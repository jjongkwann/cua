//! Opt-in native check of background focus against the AppKit fixture's focus
//! oracle (`CUA_APPKIT_FOCUS_ORACLE`).
//!
//! Each case launches a fresh fixture in the background (it never activates
//! itself and orders its windows behind the user's), drives it through the
//! in-process tool registry, and compares two sides:
//!
//! - the target's own view, from the oracle journal: whether AppKit believed
//!   it active and which window it believed key/main when the command ran, and
//!   whether that belief was handed back afterwards;
//! - the user's side, from WindowServer and Accessibility: the front process,
//!   the process holding key focus, the user's focused window and element, and
//!   whether a fixture window ever sat above the user's front window. These
//!   are sampled every few milliseconds during the action, so a transient
//!   theft fails the case even when it was restored before the call returned.
//!
//! Nothing here moves the pointer, raises a window or activates an app. Pixel
//! clicks and drags are not covered: their window frame proof takes a window
//! capture, which needs Screen Recording.
//!
//! Run in a TCC-authorized Aqua session after building the fixture with
//! `tests/fixtures/build/macos.sh --only appkit`:
//!
//! ```text
//! cargo test -p platform-macos --test background_focus_oracle -- --run-gui [--report <path>]
//! ```

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|arg| arg == "--run-gui") {
        println!(
            "background_focus_oracle: skipped; pass --run-gui in a TCC-authorized Aqua session"
        );
        return;
    }
    let report = args
        .windows(2)
        .find(|pair| pair[0] == "--report")
        .map(|pair| std::path::PathBuf::from(&pair[1]));
    let summary = native::run();
    let text = serde_json::to_string_pretty(&summary).unwrap();
    if let Some(path) = report {
        std::fs::write(path, &text).unwrap();
    }
    println!("{text}");
    if summary["passed"] != serde_json::Value::Bool(true) {
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod native {
    use platform_macos::ax::bindings as ax;
    use platform_macos::input::skylight;
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// How long a case waits after the call returns before reading the
    /// journal: AppKit posts the deactivation notices on its next run-loop
    /// turns, and the key-focus guard's order watch outlives the call.
    const SETTLE: Duration = Duration::from_millis(1500);

    struct Fixture {
        child: Child,
        journal: PathBuf,
        _dir: tempfile::TempDir,
        pid: i32,
        main: u32,
        secondary: u32,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn fixture_executable() -> PathBuf {
        let app = std::env::var_os("HARNESS_APPKIT_APP")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../test-apps/harness-appkit/CuaTestHarness.AppKit.app")
            });
        app.join("Contents/MacOS/CuaTestHarness.AppKit")
    }

    impl Fixture {
        fn launch(mode: &str) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let journal = dir.path().join("focus.jsonl");
            let child = Command::new(fixture_executable())
                .env("CUA_APPKIT_FOCUS_ORACLE", &journal)
                .env("CUA_HARNESS_BRING_TO_FRONT_MODE", mode)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("launch the AppKit fixture (tests/fixtures/build/macos.sh --only appkit)");
            let mut fixture = Fixture {
                child,
                journal,
                _dir: dir,
                pid: 0,
                main: 0,
                secondary: 0,
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(ready) = fixture.events().into_iter().find(|e| e["kind"] == "ready") {
                    fixture.pid = ready["pid"].as_i64().unwrap() as i32;
                    fixture.main = ready["window"].as_u64().unwrap() as u32;
                    fixture.secondary = ready["secondary"].as_u64().unwrap_or(0) as u32;
                    break;
                }
                assert!(
                    fixture.child.try_wait().unwrap().is_none(),
                    "the fixture exited before it was ready"
                );
                assert!(Instant::now() < deadline, "the fixture never became ready");
                std::thread::sleep(Duration::from_millis(20));
            }
            // AX answers once the app has entered its run loop.
            let deadline = Instant::now() + Duration::from_secs(10);
            while ax::focused_window_id_of_pid(fixture.pid).is_none()
                && !windows_known_to_ax(fixture.pid, fixture.main)
            {
                assert!(Instant::now() < deadline, "the fixture never answered AX");
                std::thread::sleep(Duration::from_millis(50));
            }
            fixture
        }

        fn events(&self) -> Vec<Value> {
            std::fs::read_to_string(&self.journal)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        }
    }

    fn windows_known_to_ax(pid: i32, window: u32) -> bool {
        unsafe {
            let app = ax::AXUIElementCreateApplication(pid);
            if app.is_null() {
                return false;
            }
            let mut found = false;
            for element in ax::copy_ax_windows(app) {
                found |= ax::ax_get_window_id(element) == Some(window);
                core_foundation::base::CFRelease(element as _);
            }
            core_foundation::base::CFRelease(app as _);
            found
        }
    }

    /// The user's side of focus, as WindowServer and Accessibility report it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct UserFocus {
        front: Option<i32>,
        key_focus: Option<i32>,
        window: Option<u32>,
        element_window: Option<u32>,
    }

    fn user_focus() -> UserFocus {
        let front = skylight::front_process_pid();
        UserFocus {
            front,
            key_focus: skylight::key_focus_pid(),
            window: front.and_then(ax::focused_window_id_of_pid),
            element_window: front.and_then(ax::focused_element_window_id_of_pid),
        }
    }

    fn user_json(focus: UserFocus) -> Value {
        json!({
            "front_pid": focus.front,
            "key_focus_pid": focus.key_focus,
            "focused_window": focus.window,
            "focused_element_window": focus.element_window,
        })
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn CGEventSourceFlagsState(state: i32) -> u64;
    }

    /// The hardware pointer position and modifier flags. Recorded, not
    /// asserted: the user may move the mouse or hold a key during a run.
    fn pointer_and_flags() -> Value {
        use core_graphics::event::CGEvent;
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
        let location = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok()
            .and_then(|source| CGEvent::new(source).ok())
            .map(|event| event.location());
        json!({
            "pointer": location.map(|p| [p.x, p.y]),
            "flags": unsafe { CGEventSourceFlagsState(1) },
        })
    }

    /// Samples the user's front process and key-focus process every 5 ms, and
    /// whether a fixture window sits above the user's front window every
    /// 25 ms, until stopped. Returns each departure from `before` once per
    /// change.
    struct Watch {
        stop: Arc<AtomicBool>,
        handle: std::thread::JoinHandle<Vec<String>>,
    }

    impl Watch {
        fn start(before: UserFocus, fixture: i32) -> Watch {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                let mut seen = Vec::new();
                let mut last = String::new();
                let mut tick = 0u32;
                while !flag.load(Ordering::Relaxed) {
                    let front = skylight::front_process_pid();
                    let key = skylight::key_focus_pid();
                    let mut now = Vec::new();
                    if front != before.front {
                        now.push(format!("front_pid={front:?}"));
                    }
                    if key != before.key_focus {
                        now.push(format!("key_focus_pid={key:?}"));
                    }
                    if tick % 5 == 0 {
                        if let Some(user) = before.front {
                            if platform_macos::key_focus_guard::target_window_above_front(
                                fixture, user,
                            ) {
                                now.push("fixture_window_above_user_front_window".into());
                            }
                        }
                    }
                    let now = now.join(",");
                    if now != last {
                        if !now.is_empty() && seen.len() < 40 {
                            seen.push(now.clone());
                        }
                        last = now;
                    }
                    tick += 1;
                    std::thread::sleep(Duration::from_millis(5));
                }
                seen
            });
            Watch { stop, handle }
        }

        fn finish(self) -> Vec<String> {
            self.stop.store(true, Ordering::Relaxed);
            self.handle.join().unwrap()
        }
    }

    fn call(
        runtime: &tokio::runtime::Runtime,
        registry: &cua_driver_core::tool::ToolRegistry,
        tool: &str,
        args: Value,
    ) -> Value {
        let result = runtime.block_on(registry.invoke(tool, args));
        serde_json::to_value(&result).unwrap()
    }

    fn result_text(result: &Value) -> String {
        result["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn is_error(result: &Value) -> bool {
        result["isError"] == Value::Bool(true) || result["is_error"] == Value::Bool(true)
    }

    fn refusal_code(result: &Value) -> Value {
        let structured = result
            .get("structuredContent")
            .or_else(|| result.get("structured_content"))
            .cloned()
            .unwrap_or(Value::Null);
        structured
            .get("code")
            .cloned()
            .or_else(|| structured.pointer("/refusal/code").cloned())
            .unwrap_or(Value::Null)
    }

    /// The target's lifecycle as its own journal shows it.
    struct TargetView {
        events: Vec<Value>,
        menu_actions: Vec<Value>,
        activations: i64,
        resignations: i64,
        ends_active: bool,
    }

    fn target_view(fixture: &Fixture) -> TargetView {
        let events: Vec<Value> = fixture
            .events()
            .into_iter()
            .filter(|e| e["kind"] != "ready")
            .collect();
        let count = |kind: &str| events.iter().filter(|e| e["kind"] == kind).count() as i64;
        let ends_active = events
            .iter()
            .rev()
            .find(|e| e["kind"] == "app_active" || e["kind"] == "app_resign_active")
            .is_some_and(|e| e["kind"] == "app_active");
        TargetView {
            menu_actions: events
                .iter()
                .filter(|e| e["kind"] == "menu_action")
                .cloned()
                .collect(),
            activations: count("app_active"),
            resignations: count("app_resign_active"),
            ends_active,
            events,
        }
    }

    fn compact(events: &[Value]) -> Vec<Value> {
        events
            .iter()
            .map(|e| {
                json!({
                    "kind": e["kind"],
                    "window": e.get("window"),
                    "is_active": e["is_active"],
                    "key_window": e["key_window"],
                    "main_window": e["main_window"],
                })
            })
            .collect()
    }

    struct Case {
        name: &'static str,
        mode: &'static str,
        run: fn(&Ctx, &Fixture) -> Value,
        /// Case-specific expectations on the call result and the journal.
        expect: fn(&Fixture, &Value, &TargetView) -> Vec<String>,
    }

    struct Ctx {
        runtime: tokio::runtime::Runtime,
        registry: cua_driver_core::tool::ToolRegistry,
    }

    fn arrange_left(ctx: &Ctx, fixture: &Fixture, window: u32) -> Value {
        call(
            &ctx.runtime,
            &ctx.registry,
            "invoke_menu",
            json!({ "pid": fixture.pid, "window_id": window, "path": ["Window", "Arrange", "Left"] }),
        )
    }

    /// The title AX reports for the fixture's application menu.
    fn app_menu_title(pid: i32) -> String {
        unsafe {
            let app = ax::AXUIElementCreateApplication(pid);
            let bar = ax::copy_element_attr(app, "AXMenuBar");
            core_foundation::base::CFRelease(app as _);
            let bar = bar.expect("the fixture exposes AXMenuBar");
            let items = ax::copy_children(bar);
            core_foundation::base::CFRelease(bar as _);
            // The first item is the Apple menu; the application menu follows.
            let title = items
                .get(1)
                .and_then(|item| ax::copy_string_attr(*item, "AXTitle"))
                .unwrap_or_default();
            for item in items {
                core_foundation::base::CFRelease(item as _);
            }
            title
        }
    }

    fn snapshot(ctx: &Ctx, fixture: &Fixture) -> Value {
        call(
            &ctx.runtime,
            &ctx.registry,
            "get_window_state",
            json!({
                "pid": fixture.pid,
                "window_id": fixture.main,
                "include_screenshot": false,
                "tree_format": "elements",
            }),
        )
    }

    /// The value of the fixture's `lbl-counter` label, read through AX.
    fn counter_value(fixture: &Fixture) -> Option<String> {
        unsafe fn find(element: ax::AXUIElementRef, depth: u32) -> Option<String> {
            if ax::copy_string_attr(element, "AXIdentifier").as_deref() == Some("lbl-counter") {
                return ax::copy_string_attr(element, "AXValue");
            }
            if depth == 0 {
                return None;
            }
            let mut found = None;
            for child in ax::copy_children(element) {
                if found.is_none() {
                    found = find(child, depth - 1);
                }
                core_foundation::base::CFRelease(child as _);
            }
            found
        }
        unsafe {
            let app = ax::AXUIElementCreateApplication(fixture.pid);
            let mut found = None;
            for window in ax::copy_ax_windows(app) {
                if found.is_none() && ax::ax_get_window_id(window) == Some(fixture.main) {
                    found = find(window, 12);
                }
                core_foundation::base::CFRelease(window as _);
            }
            core_foundation::base::CFRelease(app as _);
            found
        }
    }

    fn cases() -> Vec<Case> {
        vec![
            Case {
                name: "invoke_menu_key_main_context",
                mode: "two-windows",
                run: |ctx, fixture| arrange_left(ctx, fixture, fixture.main),
                expect: |fixture, result, view| {
                    let mut failures = Vec::new();
                    if is_error(result) {
                        failures.push(format!("call refused: {}", result_text(result)));
                    }
                    let ran_in_context = view.menu_actions.iter().filter(|a| {
                        a["is_active"] == Value::Bool(true)
                            && a["key_window"].as_u64() == Some(u64::from(fixture.main))
                    });
                    if view.menu_actions.len() != 1 || ran_in_context.count() != 1 {
                        failures.push(format!(
                            "expected exactly one Arrange Left run with the app active and window {} key, got {:?}",
                            fixture.main, view.menu_actions
                        ));
                    }
                    failures
                },
            },
            Case {
                name: "invoke_menu_sibling_window_context",
                mode: "two-windows",
                run: |ctx, fixture| arrange_left(ctx, fixture, fixture.secondary),
                expect: |_, result, view| {
                    let mut failures = Vec::new();
                    // Arrange Left validates against the primary window being
                    // key; with the sibling bound it must refuse, never run
                    // against the primary.
                    if !is_error(result) {
                        failures
                            .push("a command bound to the sibling window reported success".into());
                    }
                    if !view.menu_actions.is_empty() {
                        failures.push(format!("the command ran: {:?}", view.menu_actions));
                    }
                    failures
                },
            },
            Case {
                name: "invoke_menu_truly_unavailable_item",
                mode: "two-windows",
                run: |ctx, fixture| {
                    let title = app_menu_title(fixture.pid);
                    call(
                        &ctx.runtime,
                        &ctx.registry,
                        "invoke_menu",
                        json!({
                            "pid": fixture.pid,
                            "window_id": fixture.main,
                            "path": [title, "Harness Test Item"]
                        }),
                    )
                },
                expect: |_, result, _| {
                    let mut failures = Vec::new();
                    if !is_error(result) || refusal_code(result) != "background_unavailable" {
                        failures.push(format!(
                            "an item with no action must refuse background_unavailable: {}",
                            result_text(result)
                        ));
                    }
                    failures
                },
            },
            Case {
                name: "invoke_menu_sheet_blocks_requested_window",
                mode: "sheet",
                run: |ctx, fixture| arrange_left(ctx, fixture, fixture.main),
                expect: |_, result, view| {
                    let mut failures = Vec::new();
                    if !is_error(result) || refusal_code(result) != "window_not_key" {
                        failures.push(format!(
                            "a window whose sheet takes key must refuse window_not_key: {}",
                            result_text(result)
                        ));
                    }
                    if !view.menu_actions.is_empty() {
                        failures.push(format!("the command ran: {:?}", view.menu_actions));
                    }
                    failures
                },
            },
            Case {
                name: "click_ax_press_native_button",
                mode: "two-windows",
                run: |ctx, fixture| {
                    let state = snapshot(ctx, fixture);
                    let token = state
                        .pointer("/structuredContent/elements")
                        .or_else(|| state.pointer("/structured_content/elements"))
                        .and_then(Value::as_array)
                        .and_then(|elements| {
                            elements
                                .iter()
                                .find(|e| e["role"] == "AXButton" && e["label"] == "Increment")
                        })
                        .and_then(|e| e["element_token"].as_str())
                        .map(str::to_owned);
                    let Some(token) = token else {
                        return json!({ "isError": true, "content": [{ "type": "text", "text": "no Increment token in get_window_state" }] });
                    };
                    let mut result = call(
                        &ctx.runtime,
                        &ctx.registry,
                        "click",
                        json!({ "element_token": token }),
                    );
                    std::thread::sleep(Duration::from_millis(300));
                    result["counter_after"] = json!(counter_value(fixture));
                    result
                },
                expect: |_, result, _| {
                    let mut failures = Vec::new();
                    if is_error(result) {
                        failures.push(format!("call refused: {}", result_text(result)));
                    }
                    if result["counter_after"] != "counter=1" {
                        failures.push(format!(
                            "expected the press to run exactly once, counter is {}",
                            result["counter_after"]
                        ));
                    }
                    failures
                },
            },
        ]
    }

    pub fn run() -> Value {
        if !unsafe { ax::AXIsProcessTrusted() } {
            return json!({
                "passed": false,
                "blocked": "Accessibility is not granted to this process's responsible app; \
                            no permission was requested",
            });
        }
        let ctx = Ctx {
            runtime: tokio::runtime::Runtime::new().unwrap(),
            registry: platform_macos::register_tools(),
        };
        let only = std::env::var("CUA_FOCUS_ORACLE_CASE").ok();
        let mut reports = Vec::new();
        let mut passed = true;
        for case in cases() {
            if only.as_deref().is_some_and(|only| only != case.name) {
                continue;
            }
            let before_launch = user_focus();
            let fixture = Fixture::launch(case.mode);
            let launched = user_focus();
            if launched.front != before_launch.front
                || launched.key_focus != before_launch.key_focus
            {
                reports.push(json!({
                    "case": case.name,
                    "aborted": "the fixture launch changed the user's front or key-focus process",
                    "before": user_json(before_launch),
                    "after_launch": user_json(launched),
                }));
                passed = false;
                break;
            }
            let pointer_before = pointer_and_flags();
            let watch = Watch::start(launched, fixture.pid);
            let started = Instant::now();
            let result = (case.run)(&ctx, &fixture);
            let call_ms = started.elapsed().as_millis() as u64;
            std::thread::sleep(SETTLE);
            let deviations = watch.finish();
            let after = user_focus();
            let view = target_view(&fixture);

            let mut failures = (case.expect)(&fixture, &result, &view);
            if view.ends_active {
                failures.push("the target still believes it is active after the call".into());
            }
            if view.activations != view.resignations {
                failures.push(format!(
                    "unpaired activation: {} app_active vs {} app_resign_active",
                    view.activations, view.resignations
                ));
            }
            if !deviations.is_empty() {
                failures.push(format!(
                    "user focus or order changed during the call: {deviations:?}"
                ));
            }
            if after != launched {
                failures.push("the user's focus differs after the call".into());
            }
            let leaked_keys: Vec<&Value> = view
                .events
                .iter()
                .filter(|e| e["kind"] == "key_down")
                .collect();
            if !leaked_keys.is_empty() {
                failures.push(format!("the fixture received key-downs: {leaked_keys:?}"));
            }
            passed &= failures.is_empty();
            reports.push(json!({
                "case": case.name,
                "passed": failures.is_empty(),
                "failures": failures,
                "call_ms": call_ms,
                "result": {
                    "is_error": is_error(&result),
                    "code": refusal_code(&result),
                    "text": result_text(&result).chars().take(400).collect::<String>(),
                    "counter_after": result.get("counter_after"),
                },
                "target": {
                    "pid": fixture.pid,
                    "main": fixture.main,
                    "secondary": fixture.secondary,
                    "journal": compact(&view.events),
                    "ax_focused_window_after": ax::focused_window_id_of_pid(fixture.pid),
                },
                "user": {
                    "before": user_json(launched),
                    "after": user_json(after),
                    "deviations_during_call": deviations,
                },
                "pointer_and_flags": { "before": pointer_before, "after": pointer_and_flags() },
            }));
        }
        json!({ "passed": passed, "cases": reports })
    }
}
