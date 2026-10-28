//! Synthetic AppKit focus for one exact window of a background target
//! (layer 2 of [`crate::focus_guard`]).
//!
//! A background app is not active, so AppKit has no key window for it: its
//! window-menu commands validate as disabled, `nil`-targeted control actions
//! have no first responder to reach, and keys posted to the process go
//! nowhere. The driver does not activate the app (the user keeps the front
//! and the keyboard). Instead it hands the app the notices AppKit would get
//! on a real activation, addressed to that process only, and takes them back
//! when the action is done.
//!
//! ## What the AppKit fixture showed (`background_focus_oracle`, this Mac)
//!
//! - An activation notice (type 13, subtype 1) makes `NSApp.isActive` true
//!   and `AXFrontmost` readable as true; no window becomes key by it alone.
//! - The make-key record pair for a window, posted while the app goes
//!   active, makes that window main and key; its window-menu commands
//!   enable. The pair does nothing once another window is key: switching
//!   windows inside the app needs a deactivation notice, a new activation
//!   notice and the pair.
//! - A deactivation notice (type 13, subtype 2) resigns the key and main
//!   window and `NSApp.isActive` in one step, whatever window id it names.
//!   An app that was not active still posts `didResignActive` on it, so a
//!   deactivation is only sent where an activation was.
//! - WindowServer's front process and key-focus process stay the user's
//!   through all of it; the user's focused window does not change.
//! - `AXFocusedWindow` and `AXMainWindow` already name the app's remembered
//!   main window before any record, so they confirm a binding only together
//!   with `AXFrontmost`; a sibling window shows up in them once bound.
//!
//! ## Lifecycle
//!
//! [`enter`] validates that the window belongs to the pid, sends the
//! activation notice and, for [`Binding::Key`] and [`Binding::KeyMain`], the
//! make-key pair, then reads the app's state back until it reports the
//! binding. An unconfirmed key binding is undone and refused: the caller
//! dispatches nothing. Dropping the returned [`TargetFocus`] restores the
//! window that was focused before (when it changed and still exists) and
//! sends the deactivation notice, unless the target has really come to the
//! front in the meantime (then it is the user's, and left alone).
//!
//! Every record goes to the target process; nothing is sent to the user's
//! app. The type-21 key-focus notices are not used here: no path in the
//! driver removes the target's key focus during a synthetic binding.
//!
//! The OS calls are behind [`Host`] so the order and addressing of every
//! record is checked by hermetic tests; [`Live`] is the SkyLight and
//! Accessibility implementation.

use std::fmt;
use std::time::Duration;

use crate::input::skylight;

/// Pause after a record so the app handles it before the next one (measured
/// on LibreOffice: without it about half the key-window switches failed).
const RECORD_SETTLE: Duration = Duration::from_millis(120);
/// Readback poll interval.
const READBACK_POLL: Duration = Duration::from_millis(20);
/// Longest wait for a key binding to show in the app's state.
const KEY_READBACK: Duration = Duration::from_millis(400);
/// Longest wait for the app to report itself active.
const ACTIVE_READBACK: Duration = Duration::from_millis(200);

/// How far the target is bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    /// The app believes it is active. No window is made key by the driver:
    /// raw pixel input picks its own window, as a click does.
    Active,
    /// `window` is the app's key window (`AXFocusedWindow` names it).
    Key,
    /// `window` is the app's key and main window: the document context
    /// AppKit resolves window and document menus against.
    KeyMain,
}

impl Binding {
    fn label(self) -> &'static str {
        match self {
            Binding::Active => "active",
            Binding::Key => "key window",
            Binding::KeyMain => "key and main window",
        }
    }
}

/// The target's focus as Accessibility reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TargetState {
    /// `AXFrontmost`: the app believes it is active.
    pub frontmost: Option<bool>,
    /// `AXFocusedWindow`.
    pub focused: Option<u32>,
    /// `AXMainWindow`.
    pub main: Option<u32>,
}

impl TargetState {
    fn satisfies(self, window: u32, binding: Binding) -> bool {
        self.frontmost == Some(true)
            && match binding {
                Binding::Active => true,
                Binding::Key => self.focused == Some(window),
                Binding::KeyMain => self.focused == Some(window) && self.main == Some(window),
            }
    }
}

/// Why a window could not be bound. Nothing was dispatched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// WindowServer does not list `window` as a window of `pid`.
    NotOwned {
        pid: i32,
        window: u32,
        owner: Option<i32>,
    },
    /// The app did not report the binding in time; it was told to resign
    /// again.
    Unconfirmed {
        window: u32,
        binding: Binding,
        state: TargetState,
    },
    /// The user's front process changed while the window was being bound.
    FrontChanged {
        before: Option<i32>,
        after: Option<i32>,
    },
    /// Every record post failed with the invalid-connection status (1002).
    WindowServerConnection,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NotOwned { pid, window, owner } => match owner {
                Some(owner) => write!(
                    f,
                    "window {window} belongs to pid {owner}, not pid {pid}; nothing was sent"
                ),
                None => write!(
                    f,
                    "window {window} is not a window WindowServer lists for pid {pid}; nothing \
                     was sent"
                ),
            },
            Refusal::Unconfirmed {
                window,
                binding,
                state,
            } => write!(
                f,
                "window {window} could not be confirmed as the app's {} in the background (the \
                 app reports frontmost={:?}, focused window={:?}, main window={:?}); nothing was \
                 dispatched. Close the window that holds the app's focus, or use \
                 delivery_mode:\"foreground\"",
                binding.label(),
                state.frontmost,
                state.focused,
                state.main
            ),
            Refusal::FrontChanged { before, after } => write!(
                f,
                "the front app changed from pid {before:?} to pid {after:?} while the window was \
                 being bound; nothing was dispatched, retry"
            ),
            Refusal::WindowServerConnection => write!(
                f,
                "the WindowServer connection is invalid (SLPSPostEventRecordTo returned 1002); \
                 nothing was sent"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// What the lifecycle did, for results and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Records were sent: the target was bound (and is released on exit).
    pub synthetic: bool,
    /// The app's state read back the requested binding.
    pub confirmed: bool,
}

/// The OS, as the lifecycle sees it.
pub(crate) trait Host {
    /// The record-posting SPIs resolve.
    fn available(&mut self) -> bool;
    /// WindowServer's front process.
    fn front_process(&mut self) -> Option<i32>;
    /// The pid WindowServer lists as the owner of `window`.
    fn window_owner(&mut self, window: u32) -> Option<i32>;
    fn target_state(&mut self, pid: i32) -> TargetState;
    /// `pid`'s on-screen normal windows, front to back.
    fn window_order(&mut self, pid: i32) -> Vec<u32>;
    /// Keep the user's key focus while `pid` may open a menu.
    fn protect_key_focus(&mut self, pid: i32) -> Option<crate::key_focus_guard::Lease>;
    fn post_activation(&mut self, pid: i32, window: u32) -> bool;
    fn post_deactivation(&mut self, pid: i32, window: u32) -> bool;
    fn post_make_key(&mut self, pid: i32, window: u32) -> bool;
    fn settle(&mut self, duration: Duration);
    fn connection_invalid(&mut self) -> bool;
}

/// SkyLight and Accessibility.
pub(crate) struct Live;

impl Host for Live {
    fn available(&mut self) -> bool {
        skylight::is_focus_without_raise_available()
    }

    fn front_process(&mut self) -> Option<i32> {
        skylight::front_process_pid()
    }

    fn window_owner(&mut self, window: u32) -> Option<i32> {
        crate::windows::window_info_by_id(window).map(|info| info.pid)
    }

    fn target_state(&mut self, pid: i32) -> TargetState {
        use crate::ax::bindings::{
            ax_get_window_id, copy_bool_attr, copy_element_attr, AXUIElementCreateApplication,
        };
        unsafe {
            let app = AXUIElementCreateApplication(pid);
            if app.is_null() {
                return TargetState::default();
            }
            let window_id = |name| {
                let window = copy_element_attr(app, name)?;
                let id = ax_get_window_id(window);
                core_foundation::base::CFRelease(window as _);
                id
            };
            let state = TargetState {
                frontmost: copy_bool_attr(app, "AXFrontmost"),
                focused: window_id("AXFocusedWindow"),
                main: window_id("AXMainWindow"),
            };
            core_foundation::base::CFRelease(app as _);
            state
        }
    }

    fn window_order(&mut self, pid: i32) -> Vec<u32> {
        skylight::app_window_order(pid)
    }

    fn protect_key_focus(&mut self, pid: i32) -> Option<crate::key_focus_guard::Lease> {
        crate::key_focus_guard::protect(pid)
    }

    fn post_activation(&mut self, pid: i32, window: u32) -> bool {
        skylight::post_activation_notice(pid, window)
    }

    fn post_deactivation(&mut self, pid: i32, window: u32) -> bool {
        skylight::post_deactivation_notice(pid, window)
    }

    fn post_make_key(&mut self, pid: i32, window: u32) -> bool {
        skylight::post_make_key_records(pid, window)
    }

    fn settle(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn connection_invalid(&mut self) -> bool {
        skylight::windowserver_connection_invalid()
    }
}

/// Why nothing was sent and nothing is undone on exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Passive {
    /// The target is WindowServer's front process: it is active for real.
    TargetInFront,
    /// The SPIs are missing; the action runs as it always did.
    Unavailable,
    /// The app already believes it is active and only that was asked.
    AlreadyActive,
}

/// A bound target.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Bound {
    pid: i32,
    window: u32,
    binding: Binding,
    /// The app's focused window before binding.
    previous: Option<u32>,
    /// The app's on-screen windows before binding, front to back.
    order_before: Vec<u32>,
    confirmed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Entered {
    Passive(Passive),
    Bound(Bound),
}

impl Entered {
    fn report(&self) -> Report {
        match self {
            Entered::Passive(_) => Report::default(),
            Entered::Bound(bound) => Report {
                synthetic: true,
                confirmed: bound.confirmed,
            },
        }
    }
}

fn enter_with<H: Host>(
    host: &mut H,
    pid: i32,
    window: u32,
    binding: Binding,
) -> Result<Entered, Refusal> {
    let front_before = host.front_process();
    if front_before == Some(pid) {
        return Ok(Entered::Passive(Passive::TargetInFront));
    }
    if !host.available() {
        return Ok(Entered::Passive(Passive::Unavailable));
    }
    match host.window_owner(window) {
        Some(owner) if owner == pid => {}
        owner => return Err(Refusal::NotOwned { pid, window, owner }),
    }
    let before = host.target_state(pid);
    if binding == Binding::Active && before.frontmost == Some(true) {
        return Ok(Entered::Passive(Passive::AlreadyActive));
    }
    let order_before = host.window_order(pid);
    let mut posted = true;
    if before.frontmost == Some(true) {
        // The app already believes it is active (an earlier action left it
        // so). The make-key pair binds a window only while the app goes
        // active, so have it resign first.
        posted &= host.post_deactivation(pid, before.focused.unwrap_or(window));
        host.settle(RECORD_SETTLE);
    }
    posted &= host.post_activation(pid, window);
    if binding != Binding::Active {
        host.settle(RECORD_SETTLE);
        posted &= host.post_make_key(pid, window);
    }
    if !posted && host.connection_invalid() {
        return Err(Refusal::WindowServerConnection);
    }
    let mut bound = Bound {
        pid,
        window,
        binding,
        previous: before.focused,
        order_before,
        confirmed: false,
    };
    let budget = match binding {
        Binding::Active => ACTIVE_READBACK,
        Binding::Key | Binding::KeyMain => KEY_READBACK,
    };
    let max_polls = (budget.as_millis() / READBACK_POLL.as_millis()) as u32;
    let mut state = host.target_state(pid);
    let mut polls = 0u32;
    while !state.satisfies(window, binding) && polls < max_polls {
        host.settle(READBACK_POLL);
        state = host.target_state(pid);
        polls += 1;
    }
    bound.confirmed = state.satisfies(window, binding);
    // Raw pixel input never required the app to confirm its activation
    // (an app without Accessibility cannot); a key binding is proof or
    // nothing.
    if !bound.confirmed && binding != Binding::Active {
        exit_bound(host, &bound);
        return Err(Refusal::Unconfirmed {
            window,
            binding,
            state,
        });
    }
    let front_after = host.front_process();
    if front_after != front_before {
        exit_bound(host, &bound);
        return Err(Refusal::FrontChanged {
            before: front_before,
            after: front_after,
        });
    }
    Ok(Entered::Bound(bound))
}

/// Hand the activation back: the previous key window first, when the binding
/// moved it and it still exists, then the deactivation notice. Nothing when
/// the target has really come to the front: that is the user's doing.
fn exit_bound<H: Host>(host: &mut H, bound: &Bound) {
    if host.front_process() == Some(bound.pid) {
        tracing::debug!(
            pid = bound.pid,
            window = bound.window,
            "synthetic focus: the target is the front process now; leaving it active"
        );
        return;
    }
    let restore = match (bound.binding, bound.previous) {
        (Binding::Active, _) | (_, None) => None,
        (_, Some(previous)) if previous == bound.window => None,
        (_, Some(previous)) => skylight::restore_target(
            bound.window,
            Some(previous),
            &bound.order_before,
            &host.window_order(bound.pid),
        ),
    };
    match restore {
        Some(previous) => {
            // The pair binds a window only while the app goes active: resign,
            // activate naming the previous window, bind it, resign for good.
            host.post_deactivation(bound.pid, bound.window);
            host.settle(RECORD_SETTLE);
            host.post_activation(bound.pid, previous);
            host.settle(RECORD_SETTLE);
            host.post_make_key(bound.pid, previous);
            host.settle(RECORD_SETTLE);
            host.post_deactivation(bound.pid, previous);
        }
        None => {
            host.post_deactivation(bound.pid, bound.window);
        }
    }
}

/// Run `f` with `window` bound for `pid`: key-focus guard, bind, `f`,
/// release. `f` does not run when the binding is refused. With
/// `leave_to_owner` the binding is not released afterwards: an enclosing
/// background action on the same process owns the target's focus for its
/// own remaining steps.
pub(crate) fn run_with<H: Host, T, E: From<Refusal>>(
    host: &mut H,
    pid: i32,
    window: u32,
    binding: Binding,
    leave_to_owner: bool,
    f: impl FnOnce() -> Result<T, E>,
) -> (Report, Result<T, E>) {
    let key_focus = host.protect_key_focus(pid);
    let entered = match enter_with(host, pid, window, binding) {
        Ok(entered) => entered,
        Err(refusal) => return (Report::default(), Err(E::from(refusal))),
    };
    let report = entered.report();
    let result = f();
    if let Entered::Bound(bound) = &entered {
        if !leave_to_owner {
            exit_bound(host, bound);
        }
    }
    drop(key_focus);
    (report, result)
}

/// A window bound as the target's synthetic active (and key / key-and-main)
/// context. Dropping it releases the binding.
pub struct TargetFocus {
    entered: Option<Entered>,
}

impl TargetFocus {
    pub fn report(&self) -> Report {
        self.entered
            .as_ref()
            .map(Entered::report)
            .unwrap_or_default()
    }
}

impl Drop for TargetFocus {
    fn drop(&mut self) {
        if let Some(Entered::Bound(bound)) = self.entered.take() {
            exit_bound(&mut Live, &bound);
        }
    }
}

/// Bind `window` as `pid`'s synthetic context (see the module docs).
/// Blocking: records settle for up to about half a second.
pub fn enter(pid: i32, window: u32, binding: Binding) -> Result<TargetFocus, Refusal> {
    // A new binding supersedes the previous action's detached window watch,
    // as the other focus primitives do.
    crate::window_change_detector::end_tail();
    enter_with(&mut Live, pid, window, binding).map(|entered| TargetFocus {
        entered: Some(entered),
    })
}

#[cfg(test)]
pub(crate) mod fake {
    //! A recording host for the lifecycle tests.
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Op {
        ProtectKeyFocus(i32),
        Activation(i32, u32),
        Deactivation(i32, u32),
        MakeKey(i32, u32),
        Dispatch,
    }

    #[derive(Clone)]
    pub struct Script {
        pub available: bool,
        /// Front process per read; the last entry repeats.
        pub fronts: VecDeque<Option<i32>>,
        pub owner: Option<i32>,
        /// Target state per read; the last entry repeats.
        pub states: VecDeque<TargetState>,
        pub order: Vec<u32>,
        pub posts_succeed: bool,
        pub connection_invalid: bool,
    }

    impl Script {
        /// A background target whose state follows every record: inactive
        /// with `main` focused, then bound to whatever was last activated.
        pub fn background(owner: i32, main: u32) -> Script {
            Script {
                available: true,
                fronts: VecDeque::from([Some(1)]),
                owner: Some(owner),
                states: VecDeque::from([TargetState {
                    frontmost: Some(false),
                    focused: Some(main),
                    main: Some(main),
                }]),
                order: vec![main],
                posts_succeed: true,
                connection_invalid: false,
            }
        }
    }

    pub struct Fake {
        pub script: Script,
        pub ops: Arc<Mutex<Vec<Op>>>,
        /// Follow the records: an activation plus make-key pair binds that
        /// window, a deactivation clears everything.
        pub follow_records: bool,
        live: TargetState,
    }

    impl Fake {
        pub fn new(script: Script) -> Fake {
            let live = script.states.front().copied().unwrap_or_default();
            Fake {
                script,
                ops: Arc::new(Mutex::new(Vec::new())),
                follow_records: true,
                live,
            }
        }

        pub fn ops(&self) -> Vec<Op> {
            self.ops.lock().unwrap().clone()
        }

        fn record(&self, op: Op) {
            self.ops.lock().unwrap().push(op);
        }
    }

    impl Host for Fake {
        fn available(&mut self) -> bool {
            self.script.available
        }

        fn front_process(&mut self) -> Option<i32> {
            if self.script.fronts.len() > 1 {
                self.script.fronts.pop_front().unwrap()
            } else {
                self.script.fronts.front().copied().flatten()
            }
        }

        fn window_owner(&mut self, _window: u32) -> Option<i32> {
            self.script.owner
        }

        fn target_state(&mut self, _pid: i32) -> TargetState {
            if self.follow_records {
                return self.live;
            }
            if self.script.states.len() > 1 {
                self.script.states.pop_front().unwrap()
            } else {
                self.script.states.front().copied().unwrap_or_default()
            }
        }

        fn window_order(&mut self, _pid: i32) -> Vec<u32> {
            self.script.order.clone()
        }

        fn protect_key_focus(&mut self, pid: i32) -> Option<crate::key_focus_guard::Lease> {
            self.record(Op::ProtectKeyFocus(pid));
            None
        }

        fn post_activation(&mut self, pid: i32, window: u32) -> bool {
            self.record(Op::Activation(pid, window));
            if self.follow_records {
                self.live.frontmost = Some(true);
                self.live.focused = Some(window);
            }
            self.script.posts_succeed
        }

        fn post_deactivation(&mut self, pid: i32, window: u32) -> bool {
            self.record(Op::Deactivation(pid, window));
            if self.follow_records {
                self.live.frontmost = Some(false);
            }
            self.script.posts_succeed
        }

        fn post_make_key(&mut self, pid: i32, window: u32) -> bool {
            self.record(Op::MakeKey(pid, window));
            if self.follow_records {
                self.live.focused = Some(window);
                self.live.main = Some(window);
            }
            self.script.posts_succeed
        }

        fn settle(&mut self, _duration: Duration) {}

        fn connection_invalid(&mut self) -> bool {
            self.script.connection_invalid
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Fake, Op, Script};
    use super::*;

    const USER: i32 = 1;
    const TARGET: i32 = 42;
    const MAIN: u32 = 500;
    const SIBLING: u32 = 501;

    /// `run_with` holds the host, so the dispatch records through a clone of
    /// the op log.
    fn run(
        fake: &mut Fake,
        window: u32,
        binding: Binding,
    ) -> (Report, Result<&'static str, Refusal>) {
        let ops = fake.ops.clone();
        run_with(fake, TARGET, window, binding, false, || {
            ops.lock().unwrap().push(Op::Dispatch);
            Ok("dispatched")
        })
    }

    #[test]
    fn a_key_main_binding_sends_its_records_to_the_target_only_and_in_order() {
        let mut fake = Fake::new(Script::background(TARGET, MAIN));
        let ops = fake.ops.clone();
        let (report, result) = run_with(&mut fake, TARGET, MAIN, Binding::KeyMain, false, || {
            ops.lock().unwrap().push(Op::Dispatch);
            Ok::<_, Refusal>(())
        });
        assert_eq!(result, Ok(()));
        assert_eq!(
            report,
            Report {
                synthetic: true,
                confirmed: true
            }
        );
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Activation(TARGET, MAIN),
                Op::MakeKey(TARGET, MAIN),
                Op::Dispatch,
                Op::Deactivation(TARGET, MAIN),
            ]
        );
    }

    #[test]
    fn an_active_binding_sends_no_make_key_pair_and_proceeds_unconfirmed() {
        let mut fake = Fake::new(Script::background(TARGET, MAIN));
        fake.follow_records = false;
        let ops = fake.ops.clone();
        let (report, result) = run_with(&mut fake, TARGET, MAIN, Binding::Active, false, || {
            ops.lock().unwrap().push(Op::Dispatch);
            Ok::<_, Refusal>(())
        });
        assert_eq!(result, Ok(()));
        assert_eq!(
            report,
            Report {
                synthetic: true,
                confirmed: false
            }
        );
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Activation(TARGET, MAIN),
                Op::Dispatch,
                Op::Deactivation(TARGET, MAIN),
            ]
        );
    }

    #[test]
    fn an_unconfirmed_key_binding_refuses_without_dispatch_and_hands_the_activation_back() {
        let mut fake = Fake::new(Script::background(TARGET, MAIN));
        // The app never reports the binding (a sheet holds its key status).
        fake.follow_records = false;
        let ops = fake.ops.clone();
        let (report, result) = run_with(&mut fake, TARGET, MAIN, Binding::Key, false, || {
            ops.lock().unwrap().push(Op::Dispatch);
            Ok::<_, Refusal>(())
        });
        assert_eq!(
            result,
            Err(Refusal::Unconfirmed {
                window: MAIN,
                binding: Binding::Key,
                state: TargetState {
                    frontmost: Some(false),
                    focused: Some(MAIN),
                    main: Some(MAIN),
                },
            })
        );
        assert_eq!(report, Report::default());
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Activation(TARGET, MAIN),
                Op::MakeKey(TARGET, MAIN),
                Op::Deactivation(TARGET, MAIN),
            ],
            "no dispatch, and the activation is taken back"
        );
    }

    #[test]
    fn a_target_in_front_is_left_alone() {
        let mut script = Script::background(TARGET, MAIN);
        script.fronts = [Some(TARGET)].into();
        let mut fake = Fake::new(script);
        let (report, result) = run(&mut fake, MAIN, Binding::KeyMain);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(report, Report::default());
        assert_eq!(
            fake.ops(),
            vec![Op::ProtectKeyFocus(TARGET), Op::Dispatch],
            "nothing is sent to an app the user has in front"
        );
    }

    #[test]
    fn missing_spis_run_the_action_without_a_binding() {
        let mut script = Script::background(TARGET, MAIN);
        script.available = false;
        let mut fake = Fake::new(script);
        let (report, result) = run(&mut fake, MAIN, Binding::Key);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(report, Report::default());
        assert_eq!(fake.ops(), vec![Op::ProtectKeyFocus(TARGET), Op::Dispatch]);
    }

    #[test]
    fn a_window_of_another_process_is_refused_before_any_record() {
        let mut script = Script::background(TARGET, MAIN);
        script.owner = Some(7);
        let mut fake = Fake::new(script);
        let (_, result) = run(&mut fake, MAIN, Binding::Key);
        assert_eq!(
            result,
            Err(Refusal::NotOwned {
                pid: TARGET,
                window: MAIN,
                owner: Some(7)
            })
        );
        assert_eq!(fake.ops(), vec![Op::ProtectKeyFocus(TARGET)]);
    }

    #[test]
    fn exit_restores_the_previous_key_window_before_the_final_deactivation() {
        let mut script = Script::background(TARGET, MAIN);
        script.order = vec![MAIN, SIBLING];
        let mut fake = Fake::new(script);
        let (_, result) = run(&mut fake, SIBLING, Binding::KeyMain);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Activation(TARGET, SIBLING),
                Op::MakeKey(TARGET, SIBLING),
                Op::Dispatch,
                // The pair only binds a window while the app goes active, so
                // the previous window is restored with the full sequence.
                Op::Deactivation(TARGET, SIBLING),
                Op::Activation(TARGET, MAIN),
                Op::MakeKey(TARGET, MAIN),
                Op::Deactivation(TARGET, MAIN),
            ]
        );
    }

    #[test]
    fn a_previous_window_that_is_gone_is_not_restored() {
        let mut script = Script::background(TARGET, MAIN);
        // Only the sibling is on screen after the action.
        script.order = vec![SIBLING];
        let mut fake = Fake::new(script);
        let (_, result) = run(&mut fake, SIBLING, Binding::Key);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(
            fake.ops().last(),
            Some(&Op::Deactivation(TARGET, SIBLING)),
            "{:?}",
            fake.ops()
        );
        assert_eq!(
            fake.ops()
                .iter()
                .filter(|op| matches!(op, Op::Deactivation(..)))
                .count(),
            1
        );
    }

    #[test]
    fn a_user_switch_to_the_target_during_the_action_skips_the_release() {
        let mut script = Script::background(TARGET, MAIN);
        // Reads: before binding (user), after binding (user), at exit
        // (the user brought the target to the front).
        script.fronts = [Some(USER), Some(USER), Some(TARGET)].into();
        let mut fake = Fake::new(script);
        let (report, result) = run(&mut fake, MAIN, Binding::KeyMain);
        assert_eq!(result, Ok("dispatched"));
        assert!(report.synthetic);
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Activation(TARGET, MAIN),
                Op::MakeKey(TARGET, MAIN),
                Op::Dispatch,
            ],
            "an app the user activated is not told to resign"
        );
    }

    #[test]
    fn a_front_change_while_binding_refuses_and_releases() {
        let mut script = Script::background(TARGET, MAIN);
        script.fronts = [Some(USER), Some(9), Some(9)].into();
        let mut fake = Fake::new(script);
        let (_, result) = run(&mut fake, MAIN, Binding::Key);
        assert_eq!(
            result,
            Err(Refusal::FrontChanged {
                before: Some(USER),
                after: Some(9)
            })
        );
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Activation(TARGET, MAIN),
                Op::MakeKey(TARGET, MAIN),
                Op::Deactivation(TARGET, MAIN),
            ]
        );
    }

    #[test]
    fn an_app_that_already_believes_it_is_active_resigns_before_a_key_binding() {
        let mut script = Script::background(TARGET, MAIN);
        script.states = [TargetState {
            frontmost: Some(true),
            focused: Some(MAIN),
            main: Some(MAIN),
        }]
        .into();
        let mut fake = Fake::new(script);
        let (_, result) = run(&mut fake, MAIN, Binding::KeyMain);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Deactivation(TARGET, MAIN),
                Op::Activation(TARGET, MAIN),
                Op::MakeKey(TARGET, MAIN),
                Op::Dispatch,
                Op::Deactivation(TARGET, MAIN),
            ]
        );
    }

    #[test]
    fn an_app_that_already_believes_it_is_active_needs_nothing_for_an_active_binding() {
        let mut script = Script::background(TARGET, MAIN);
        script.states = [TargetState {
            frontmost: Some(true),
            focused: Some(MAIN),
            main: Some(MAIN),
        }]
        .into();
        let mut fake = Fake::new(script);
        let (report, result) = run(&mut fake, MAIN, Binding::Active);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(report, Report::default());
        assert_eq!(fake.ops(), vec![Op::ProtectKeyFocus(TARGET), Op::Dispatch]);
    }

    #[test]
    fn leaving_the_binding_to_the_owner_skips_the_release() {
        let mut fake = Fake::new(Script::background(TARGET, MAIN));
        let ops = fake.ops.clone();
        let (report, result) = run_with(&mut fake, TARGET, MAIN, Binding::Active, true, || {
            ops.lock().unwrap().push(Op::Dispatch);
            Ok::<_, Refusal>(())
        });
        assert_eq!(result, Ok(()));
        assert!(report.synthetic);
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Activation(TARGET, MAIN),
                Op::Dispatch,
            ]
        );
    }

    #[test]
    fn an_invalid_windowserver_connection_is_its_own_refusal() {
        let mut script = Script::background(TARGET, MAIN);
        script.posts_succeed = false;
        script.connection_invalid = true;
        let mut fake = Fake::new(script);
        fake.follow_records = false;
        let (_, result) = run(&mut fake, MAIN, Binding::Key);
        assert_eq!(result, Err(Refusal::WindowServerConnection));
        assert!(!fake.ops().contains(&Op::Dispatch));
    }

    #[test]
    fn the_state_readback_is_binding_specific() {
        let bound = TargetState {
            frontmost: Some(true),
            focused: Some(MAIN),
            main: Some(SIBLING),
        };
        assert!(bound.satisfies(MAIN, Binding::Active));
        assert!(bound.satisfies(MAIN, Binding::Key));
        assert!(!bound.satisfies(MAIN, Binding::KeyMain));
        assert!(!bound.satisfies(SIBLING, Binding::Key));
        let inactive = TargetState {
            frontmost: Some(false),
            ..bound
        };
        assert!(!inactive.satisfies(MAIN, Binding::Active));
        assert!(!inactive.satisfies(MAIN, Binding::Key));
    }

    #[test]
    fn refusals_say_that_nothing_was_dispatched() {
        for refusal in [
            Refusal::NotOwned {
                pid: TARGET,
                window: MAIN,
                owner: None,
            },
            Refusal::Unconfirmed {
                window: MAIN,
                binding: Binding::KeyMain,
                state: TargetState::default(),
            },
            Refusal::FrontChanged {
                before: Some(1),
                after: Some(2),
            },
            Refusal::WindowServerConnection,
        ] {
            let text = refusal.to_string();
            assert!(
                text.contains("nothing was"),
                "{refusal:?} must say nothing was sent or dispatched: {text}"
            );
        }
        assert!(Refusal::Unconfirmed {
            window: MAIN,
            binding: Binding::KeyMain,
            state: TargetState::default(),
        }
        .to_string()
        .contains("key and main window"));
    }
}
