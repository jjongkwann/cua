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
//! - Key events posted to the process with the window in their routing
//!   fields reach that window's first responder while the app is inactive
//!   and has no key window (the fixture's text field took type_text,
//!   type_text_chars, press_key and a shift+m hotkey that way). VCL
//!   (LibreOffice) instead routes keys to its focused window and drops them
//!   while it has no focused element (CUA-1767), so keys need a binding only
//!   when the app's focused window is not the requested one or it has no
//!   focused element.
//!
//! ## Lifecycle
//!
//! [`enter`] validates that the window belongs to the pid, sends the
//! activation notice and, for [`Binding::Key`], [`Binding::KeyMain`] and
//! [`Binding::Keys`], the make-key pair, then reads the app's state back
//! until it reports the binding. An unconfirmed key binding is undone and
//! refused: the caller dispatches nothing. Dropping the returned
//! [`TargetFocus`] restores the window that was focused before (when it
//! changed and still exists) and sends the deactivation notice, unless the
//! target has really come to the front in the meantime (then it is the
//! user's, and left alone).
//!
//! A [`Binding::Keys`] request for a window that already routes keys sends
//! nothing. For a target the user has in front it switches the app's key
//! window in place, as a foreground key post would, and switches it back
//! without telling the front app to resign.
//!
//! A [`Binding::Key`] request is also confirmed when the window that took key
//! is a sheet attached to the requested window: AppKit keys the sheet, and a
//! control pressed in it runs through the sheet's own responder chain.
//! `KeyMain` and `Keys` stay strict.
//!
//! The release re-reads WindowServer's front process before each record and
//! stops as soon as the target has come to the front. Whether that was the
//! user (a hardware click or modifier just before) or the target's own
//! activation is reported; both are left to the reactive layer.
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
    /// Keys posted to the process reach `window`. Nothing is sent when the
    /// app's focused window is `window` and it has a focused element (AppKit
    /// routes posted keys by window; VCL to its focused window); otherwise
    /// `window` is bound as for [`Binding::Key`].
    Keys,
}

impl Binding {
    fn label(self) -> &'static str {
        match self {
            Binding::Active => "active",
            Binding::Key => "key window",
            Binding::KeyMain => "key and main window",
            Binding::Keys => "key window for posted keys",
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
    /// The window holding `AXFocusedUIElement`. `None` for an app with no
    /// focused element, such as a LibreOffice that has never been active.
    pub element: Option<u32>,
    /// When `AXFocusedWindow` is an `AXSheet`, the window it is attached to.
    pub sheet_parent: Option<u32>,
}

impl TargetState {
    fn satisfies(self, window: u32, binding: Binding) -> bool {
        self.frontmost == Some(true)
            && match binding {
                Binding::Active => true,
                // A sheet attached to the requested window takes key in its
                // place; a control pressed in the sheet runs through the
                // sheet's responder chain, never another window's.
                Binding::Key => self.focused == Some(window) || self.sheet_parent == Some(window),
                Binding::Keys => self.focused == Some(window),
                Binding::KeyMain => self.focused == Some(window) && self.main == Some(window),
            }
    }

    /// Keys posted to the process already reach `window`: it is the app's
    /// focused window and something in the app has focus. The predictor
    /// measured on LibreOffice (keys went to `AXFocusedWindow` in every
    /// trial) and seen on AppKit (an inactive app with no key window took
    /// keys into the focused window's first responder).
    fn routes_keys(self, window: u32) -> bool {
        self.focused == Some(window) && self.element.is_some()
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

/// Who had brought the target to the front by the time the binding was
/// released, when someone had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontAtExit {
    /// A hardware click or modifier change happened just before: the user
    /// switched to the target.
    User,
    /// No hardware input: the action made the target activate itself. The
    /// reactive layer (`key_focus_guard`, `focus_steal`) hands the front
    /// back.
    Target,
}

/// What the lifecycle did, for results and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Records were sent: the target was bound (and is released on exit).
    pub synthetic: bool,
    /// The app's state read back the requested binding.
    pub confirmed: bool,
    /// The target was the front process when the binding was released, so
    /// the release stopped and sent no deactivation.
    pub front_at_exit: Option<FrontAtExit>,
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
    /// The user pressed a mouse button or a modifier on the hardware just now.
    fn user_input_recently(&mut self) -> bool;
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
            // A focused window that is a sheet names the window it is
            // attached to; `exact_target` maps a sheet to that parent.
            let sheet_parent = copy_element_attr(app, "AXFocusedWindow").and_then(|window| {
                let parent = (crate::ax::bindings::copy_string_attr(window, "AXRole").as_deref()
                    == Some("AXSheet"))
                .then(|| crate::ax::exact_target::element_window_id(window))
                .flatten();
                core_foundation::base::CFRelease(window as _);
                parent
            });
            let state = TargetState {
                frontmost: copy_bool_attr(app, "AXFrontmost"),
                focused: window_id("AXFocusedWindow"),
                main: window_id("AXMainWindow"),
                element: crate::ax::bindings::focused_element_window_id_of_pid(pid),
                sheet_parent,
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

    fn user_input_recently(&mut self) -> bool {
        crate::key_focus_guard::user_switched_recently()
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
    /// Keys posted to the process already reach the window.
    AlreadyRoutes,
}

/// A bound target.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Bound {
    pid: i32,
    window: u32,
    binding: Binding,
    /// The app's focused window before binding, when the binding moved it
    /// (restored on exit). A press binding a sheet's parent leaves the sheet
    /// focused and has nothing to restore.
    previous: Option<u32>,
    /// The app's on-screen windows before binding, front to back.
    order_before: Vec<u32>,
    confirmed: bool,
    /// The target was the front process when bound: its key window was
    /// switched in place, and it is not told to resign on exit.
    in_front: bool,
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
                front_at_exit: None,
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
    let in_front = front_before == Some(pid);
    // A target the user has in front is active for real. Only a key post may
    // still need its key window switched inside the app.
    if in_front && binding != Binding::Keys {
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
    if binding == Binding::Keys && before.routes_keys(window) {
        return Ok(Entered::Passive(Passive::AlreadyRoutes));
    }
    let order_before = host.window_order(pid);
    let mut posted = true;
    if before.frontmost == Some(true) {
        // The app already believes it is active (it is in front, or an
        // earlier action left it so). The make-key pair binds a window only
        // while the app goes active, so have it resign first.
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
        in_front,
    };
    let budget = match binding {
        Binding::Active => ACTIVE_READBACK,
        Binding::Key | Binding::KeyMain | Binding::Keys => KEY_READBACK,
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
    // Restore only a key window the binding moved. Re-binding a focused
    // window that did not move (a sheet over the requested window) sends the
    // make-key pair for a sheet, and that pair moves WindowServer's key focus
    // to the target (measured on the AppKit fixture).
    bound.previous = before
        .focused
        .filter(|previous| state.focused != Some(*previous));
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

/// Whether the target has come to the front since it was bound, and who
/// brought it there. A target bound while in front is not asked.
fn front_taken<H: Host>(host: &mut H, bound: &Bound) -> Option<FrontAtExit> {
    if bound.in_front || host.front_process() != Some(bound.pid) {
        return None;
    }
    Some(if host.user_input_recently() {
        FrontAtExit::User
    } else {
        FrontAtExit::Target
    })
}

/// Hand the activation back: the previous key window first, when the binding
/// moved it and it still exists, then the deactivation notice. A target that
/// was in front when bound keeps its activation and only gets its key window
/// back. The front process is re-read before every record: once the target
/// has come to the front, nothing more is sent (a user who just activated it
/// keeps it active; a target that activated itself is the reactive layer's).
fn exit_bound<H: Host>(host: &mut H, bound: &Bound) -> Option<FrontAtExit> {
    macro_rules! stop_if_front {
        () => {
            if let Some(by) = front_taken(host, bound) {
                tracing::debug!(
                    pid = bound.pid,
                    window = bound.window,
                    ?by,
                    "synthetic focus: the target is the front process; release stops here"
                );
                return Some(by);
            }
        };
    }
    stop_if_front!();
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
            // activate naming the previous window, bind it, then resign for
            // good unless the app is in front for real.
            host.post_deactivation(bound.pid, bound.window);
            host.settle(RECORD_SETTLE);
            stop_if_front!();
            host.post_activation(bound.pid, previous);
            host.settle(RECORD_SETTLE);
            stop_if_front!();
            host.post_make_key(bound.pid, previous);
            if !bound.in_front {
                host.settle(RECORD_SETTLE);
                stop_if_front!();
                host.post_deactivation(bound.pid, previous);
            }
        }
        None if bound.in_front => {}
        None => {
            host.post_deactivation(bound.pid, bound.window);
        }
    }
    None
}

/// Releases a binding when dropped, so the release runs after the action
/// whether the action returned or panicked.
struct Release<'a, H: Host> {
    host: &'a mut H,
    bound: Option<Bound>,
    front_at_exit: &'a mut Option<FrontAtExit>,
}

impl<H: Host> Drop for Release<'_, H> {
    fn drop(&mut self) {
        if let Some(bound) = self.bound.take() {
            *self.front_at_exit = exit_bound(self.host, &bound);
        }
    }
}

/// Run `f` with `window` bound for `pid`: key-focus guard, bind, `f`,
/// release. `f` does not run when the binding is refused.
pub(crate) fn run_with<H: Host, T, E: From<Refusal>>(
    host: &mut H,
    pid: i32,
    window: u32,
    binding: Binding,
    f: impl FnOnce() -> Result<T, E>,
) -> (Report, Result<T, E>) {
    let key_focus = host.protect_key_focus(pid);
    let entered = match enter_with(host, pid, window, binding) {
        Ok(entered) => entered,
        Err(refusal) => return (Report::default(), Err(E::from(refusal))),
    };
    let mut report = entered.report();
    let mut front_at_exit = None;
    let result = {
        let _release = Release {
            host,
            bound: match entered {
                Entered::Bound(bound) => Some(bound),
                Entered::Passive(_) => None,
            },
            front_at_exit: &mut front_at_exit,
        };
        // The release runs when `_release` drops: after `f` returns, or
        // while its panic unwinds.
        f()
    };
    report.front_at_exit = front_at_exit;
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
            let _ = exit_bound(&mut Live, &bound);
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
        /// A sheet `(sheet, parent)`: the make-key pair for `parent` keys
        /// the sheet instead, as AppKit does.
        pub sheet: Option<(u32, u32)>,
        pub user_input_recently: bool,
    }

    impl Script {
        /// A background target whose state follows every record: inactive
        /// with `main` focused and a focused element in it, then bound to
        /// whatever was last activated.
        pub fn background(owner: i32, main: u32) -> Script {
            Script {
                available: true,
                fronts: VecDeque::from([Some(1)]),
                owner: Some(owner),
                states: VecDeque::from([TargetState {
                    frontmost: Some(false),
                    focused: Some(main),
                    main: Some(main),
                    element: Some(main),
                    sheet_parent: None,
                }]),
                order: vec![main],
                posts_succeed: true,
                connection_invalid: false,
                sheet: None,
                user_input_recently: false,
            }
        }
    }

    /// Blocks a deactivation until a test says go, so the test can observe
    /// what else is still held while the release runs.
    pub type Gate = Arc<(Mutex<bool>, std::sync::Condvar)>;

    pub fn gate() -> Gate {
        Arc::new((Mutex::new(false), std::sync::Condvar::new()))
    }

    pub fn open(gate: &Gate) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    pub struct Fake {
        pub script: Script,
        pub ops: Arc<Mutex<Vec<Op>>>,
        /// Follow the records: an activation plus make-key pair binds that
        /// window, a deactivation clears everything.
        pub follow_records: bool,
        /// When set, every deactivation waits for the gate to open.
        pub hold_deactivation: Option<Gate>,
        live: TargetState,
    }

    impl Fake {
        pub fn new(script: Script) -> Fake {
            let live = script.states.front().copied().unwrap_or_default();
            Fake {
                script,
                ops: Arc::new(Mutex::new(Vec::new())),
                follow_records: true,
                hold_deactivation: None,
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
            // A sheet is an on-screen layer-0 window of the app, as the live
            // order lists it.
            let mut order = self.script.order.clone();
            if let Some((sheet, _)) = self.script.sheet {
                order.insert(0, sheet);
            }
            order
        }

        fn protect_key_focus(&mut self, pid: i32) -> Option<crate::key_focus_guard::Lease> {
            self.record(Op::ProtectKeyFocus(pid));
            None
        }

        fn user_input_recently(&mut self) -> bool {
            self.script.user_input_recently
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
            if let Some(gate) = &self.hold_deactivation {
                let mut open = gate.0.lock().unwrap();
                while !*open {
                    open = gate.1.wait(open).unwrap();
                }
            }
            self.record(Op::Deactivation(pid, window));
            if self.follow_records {
                self.live.frontmost = Some(false);
            }
            self.script.posts_succeed
        }

        fn post_make_key(&mut self, pid: i32, window: u32) -> bool {
            self.record(Op::MakeKey(pid, window));
            if self.follow_records {
                match self.script.sheet {
                    Some((sheet, parent)) if parent == window => {
                        self.live.focused = Some(sheet);
                        self.live.main = Some(window);
                        self.live.element = Some(sheet);
                        self.live.sheet_parent = Some(parent);
                    }
                    _ => {
                        self.live.focused = Some(window);
                        self.live.main = Some(window);
                        self.live.element = Some(window);
                        self.live.sheet_parent = None;
                    }
                }
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
        run_with(fake, TARGET, window, binding, || {
            ops.lock().unwrap().push(Op::Dispatch);
            Ok("dispatched")
        })
    }

    #[test]
    fn a_key_main_binding_sends_its_records_to_the_target_only_and_in_order() {
        let mut fake = Fake::new(Script::background(TARGET, MAIN));
        let ops = fake.ops.clone();
        let (report, result) = run_with(&mut fake, TARGET, MAIN, Binding::KeyMain, || {
            ops.lock().unwrap().push(Op::Dispatch);
            Ok::<_, Refusal>(())
        });
        assert_eq!(result, Ok(()));
        assert_eq!(
            report,
            Report {
                synthetic: true,
                confirmed: true,
                front_at_exit: None,
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
        let (report, result) = run_with(&mut fake, TARGET, MAIN, Binding::Active, || {
            ops.lock().unwrap().push(Op::Dispatch);
            Ok::<_, Refusal>(())
        });
        assert_eq!(result, Ok(()));
        assert_eq!(
            report,
            Report {
                synthetic: true,
                confirmed: false,
                front_at_exit: None,
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
        let (report, result) = run_with(&mut fake, TARGET, MAIN, Binding::Key, || {
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
                    element: Some(MAIN),
                    sheet_parent: None,
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
            element: Some(MAIN),
            sheet_parent: None,
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
            element: Some(MAIN),
            sheet_parent: None,
        }]
        .into();
        let mut fake = Fake::new(script);
        let (report, result) = run(&mut fake, MAIN, Binding::Active);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(report, Report::default());
        assert_eq!(fake.ops(), vec![Op::ProtectKeyFocus(TARGET), Op::Dispatch]);
    }

    #[test]
    fn keys_to_the_window_that_already_routes_them_send_nothing() {
        let mut fake = Fake::new(Script::background(TARGET, MAIN));
        let (report, result) = run(&mut fake, MAIN, Binding::Keys);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(report, Report::default());
        assert_eq!(fake.ops(), vec![Op::ProtectKeyFocus(TARGET), Op::Dispatch]);
    }

    #[test]
    fn keys_to_an_app_without_a_focused_element_bind_the_window_and_release_it() {
        // A LibreOffice that has never been active: focused window, no
        // focused element, drops every key until bound.
        let mut script = Script::background(TARGET, MAIN);
        script.states = [TargetState {
            frontmost: Some(false),
            focused: Some(MAIN),
            main: Some(MAIN),
            element: None,
            sheet_parent: None,
        }]
        .into();
        let mut fake = Fake::new(script);
        let (report, result) = run(&mut fake, MAIN, Binding::Keys);
        assert_eq!(result, Ok("dispatched"));
        assert!(report.synthetic && report.confirmed);
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
    fn keys_to_a_sibling_window_bind_it_and_restore_the_previous_one() {
        let mut script = Script::background(TARGET, MAIN);
        script.order = vec![MAIN, SIBLING];
        let mut fake = Fake::new(script);
        let (_, result) = run(&mut fake, SIBLING, Binding::Keys);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Activation(TARGET, SIBLING),
                Op::MakeKey(TARGET, SIBLING),
                Op::Dispatch,
                Op::Deactivation(TARGET, SIBLING),
                Op::Activation(TARGET, MAIN),
                Op::MakeKey(TARGET, MAIN),
                Op::Deactivation(TARGET, MAIN),
            ]
        );
    }

    #[test]
    fn keys_to_a_front_targets_other_window_switch_key_in_place_and_switch_back() {
        let mut script = Script::background(TARGET, MAIN);
        script.fronts = [Some(TARGET)].into();
        script.order = vec![MAIN, SIBLING];
        script.states = [TargetState {
            frontmost: Some(true),
            focused: Some(MAIN),
            main: Some(MAIN),
            element: Some(MAIN),
            sheet_parent: None,
        }]
        .into();
        let mut fake = Fake::new(script);
        let (report, result) = run(&mut fake, SIBLING, Binding::Keys);
        assert_eq!(result, Ok("dispatched"));
        assert!(report.synthetic && report.confirmed);
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                // In place: resign, activate naming the sibling, bind it.
                Op::Deactivation(TARGET, MAIN),
                Op::Activation(TARGET, SIBLING),
                Op::MakeKey(TARGET, SIBLING),
                Op::Dispatch,
                // Back to the previous window; the front app is not told to
                // resign at the end.
                Op::Deactivation(TARGET, SIBLING),
                Op::Activation(TARGET, MAIN),
                Op::MakeKey(TARGET, MAIN),
            ],
            "{:?}",
            fake.ops()
        );
    }

    #[test]
    fn keys_to_a_front_targets_routing_window_send_nothing() {
        let mut script = Script::background(TARGET, MAIN);
        script.fronts = [Some(TARGET)].into();
        script.states = [TargetState {
            frontmost: Some(true),
            focused: Some(MAIN),
            main: Some(MAIN),
            element: Some(MAIN),
            sheet_parent: None,
        }]
        .into();
        let mut fake = Fake::new(script);
        let (report, result) = run(&mut fake, MAIN, Binding::Keys);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(report, Report::default());
        assert_eq!(fake.ops(), vec![Op::ProtectKeyFocus(TARGET), Op::Dispatch]);
    }

    #[test]
    fn a_panicking_action_still_releases_the_binding() {
        let mut fake = Fake::new(Script::background(TARGET, MAIN));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_with(
                &mut fake,
                TARGET,
                MAIN,
                Binding::KeyMain,
                || -> Result<(), Refusal> { panic!("the action panicked") },
            )
        }));
        assert!(outcome.is_err(), "the panic propagates");
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Activation(TARGET, MAIN),
                Op::MakeKey(TARGET, MAIN),
                Op::Deactivation(TARGET, MAIN),
            ],
            "the deactivation notice goes out while the panic unwinds"
        );
    }

    #[test]
    fn a_user_switch_during_the_restore_stops_the_remaining_records() {
        let mut script = Script::background(TARGET, MAIN);
        script.order = vec![MAIN, SIBLING];
        // Reads: before and after binding (user), at the start of the
        // release (user), after the first record of the restore (target).
        script.fronts = [Some(USER), Some(USER), Some(USER), Some(TARGET)].into();
        script.user_input_recently = true;
        let mut fake = Fake::new(script);
        let (report, result) = run(&mut fake, SIBLING, Binding::KeyMain);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(report.front_at_exit, Some(FrontAtExit::User));
        assert_eq!(
            fake.ops(),
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Activation(TARGET, SIBLING),
                Op::MakeKey(TARGET, SIBLING),
                Op::Dispatch,
                Op::Deactivation(TARGET, SIBLING),
            ],
            "no activation of the previous window, no final deactivation"
        );
    }

    #[test]
    fn the_targets_own_activation_at_exit_is_told_apart_from_the_users() {
        let mut script = Script::background(TARGET, MAIN);
        script.fronts = [Some(USER), Some(USER), Some(TARGET)].into();
        script.user_input_recently = false;
        let mut fake = Fake::new(script);
        let (report, result) = run(&mut fake, MAIN, Binding::Key);
        assert_eq!(result, Ok("dispatched"));
        assert_eq!(report.front_at_exit, Some(FrontAtExit::Target));
        assert_eq!(
            fake.ops().last(),
            Some(&Op::Dispatch),
            "a target that activated itself is left to the reactive layer: {:?}",
            fake.ops()
        );
    }

    #[test]
    fn a_press_binding_accepts_the_requested_windows_sheet() {
        const SHEET: u32 = 777;
        let mut script = Script::background(TARGET, MAIN);
        script.sheet = Some((SHEET, MAIN));
        // Before the call the sheet is the app's focused window (fact 6).
        script.states = [TargetState {
            frontmost: Some(false),
            focused: Some(SHEET),
            main: Some(MAIN),
            element: Some(SHEET),
            sheet_parent: Some(MAIN),
        }]
        .into();
        let mut fake = Fake::new(script);
        let (report, result) = run(&mut fake, MAIN, Binding::Key);
        assert_eq!(result, Ok("dispatched"));
        assert!(report.confirmed);
        // The sheet stayed focused, so there is no key window to restore:
        // no activation naming the sheet, no make-key pair for it.
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
        // The document menu context and posted keys stay strict: a sheet
        // that took key is not the requested window. (With the sheet already
        // focused, keys do not route to the requested window either, so the
        // Keys binding is attempted and refused the same way.)
        for binding in [Binding::KeyMain, Binding::Keys] {
            let mut script = Script::background(TARGET, MAIN);
            script.sheet = Some((SHEET, MAIN));
            script.states = [TargetState {
                frontmost: Some(false),
                focused: Some(SHEET),
                main: Some(MAIN),
                element: Some(SHEET),
                sheet_parent: Some(MAIN),
            }]
            .into();
            let mut fake = Fake::new(script);
            let (_, result) = run(&mut fake, MAIN, binding);
            assert!(
                matches!(result, Err(Refusal::Unconfirmed { .. })),
                "{binding:?}: {result:?}"
            );
            assert!(!fake.ops().contains(&Op::Dispatch));
        }
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
            element: None,
            sheet_parent: None,
        };
        let sheet_keyed = TargetState {
            focused: Some(777),
            sheet_parent: Some(MAIN),
            ..bound
        };
        assert!(sheet_keyed.satisfies(MAIN, Binding::Key));
        assert!(!sheet_keyed.satisfies(MAIN, Binding::Keys));
        assert!(!sheet_keyed.satisfies(MAIN, Binding::KeyMain));
        assert!(bound.satisfies(MAIN, Binding::Active));
        assert!(bound.satisfies(MAIN, Binding::Key));
        assert!(bound.satisfies(MAIN, Binding::Keys));
        assert!(!bound.satisfies(MAIN, Binding::KeyMain));
        assert!(!bound.satisfies(SIBLING, Binding::Key));
        // Routing needs a focused element, not an active app.
        assert!(!bound.routes_keys(MAIN));
        assert!(TargetState {
            frontmost: Some(false),
            element: Some(MAIN),
            ..bound
        }
        .routes_keys(MAIN));
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
