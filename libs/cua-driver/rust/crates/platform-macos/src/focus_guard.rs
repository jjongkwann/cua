//! FocusGuard — the focus posture of one background action.
//!
//! ## Layers
//!
//! 1. **Enablement** — `AXManualAccessibility` / `AXEnhancedUserInterface`
//!    on Chromium and Electron targets ([`crate::ax::enablement`], cached per
//!    process).
//! 2. **Synthetic focus** — one exact window of the target is bound as the
//!    app's synthetic active (and key, or key-and-main) context for the
//!    action and released afterwards ([`crate::synthetic_focus`]). The user's
//!    app keeps the front and the keyboard; the target believes it is active
//!    long enough for its menu validation, first responder and key routing to
//!    work as they do for a frontmost app.
//! 3. **Reactive** — a targeted focus-steal suppression lease
//!    ([`crate::focus_steal`]) that re-activates the prior frontmost when the
//!    target activates itself anyway, and the key-focus guard
//!    ([`crate::key_focus_guard`]) that hands a menu's key-focus theft back to
//!    the user's app.
//!
//! [`FocusPolicy`] decides, for one action, which window snapshot (and so
//! which wildcard suppression) is taken and whether layer 2 applies, so the
//! three never disagree: a synthetic binding is never undone by the reactive
//! layer as a theft, and a plain AX action never leaves the target believing
//! it is active.
//!
//! ## Two wrappers
//!
//! - [`with_focus_suppressed`] wraps an async action with layer 3 only
//!   (`FocusPolicy::Suppress`), as every AX tool has done; the caller runs
//!   its own blocking work inside.
//! - [`with_synthetic_focus`] runs a blocking action under layer 2 and 3 on
//!   one blocking thread: key-focus guard, bind, action, release. The per-PID
//!   mutation lease moves into that thread, so a caller whose future is
//!   dropped mid-action (a script timeout) still gets the restore and the
//!   readback before the lease is released, and the next action on that
//!   process waits for it.

use std::time::Duration;

use crate::apps;
use crate::focus_steal;
use crate::synthetic_focus::{self, Binding, Refusal, Report};
use crate::tools::BackgroundMutationLease;
use crate::window_change_detector::{Snapshot, WindowChangeDetector};

/// The focus posture of one action on a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusPolicy {
    /// Background AX dispatch: the target is suppressed if it activates
    /// itself, and the key-focus guard hands a menu's theft back.
    Suppress,
    /// Background dispatch with `window_id` bound as the target's synthetic
    /// context (`binding`) for the action, released afterwards. Any other
    /// app that takes the front is still suppressed.
    Synthetic { window_id: u32, binding: Binding },
    /// Foreground delivery: the caller fronts the exact window and restores
    /// the prior front itself; nothing is suppressed.
    Foreground,
}

impl FocusPolicy {
    /// The window snapshot (and wildcard suppression) matching this policy,
    /// to take before the action and detect after it.
    pub fn snapshot(self, prior_front: Option<i32>, pid: i32) -> Snapshot {
        match self {
            FocusPolicy::Suppress => WindowChangeDetector::snapshot(prior_front),
            // NSWorkspace can report a synthetically active target as the
            // active app; the reactive layer must not undo that binding, only
            // an activation of some other app.
            FocusPolicy::Synthetic { .. } => {
                WindowChangeDetector::snapshot_allowing_activation(prior_front, pid)
            }
            FocusPolicy::Foreground => {
                WindowChangeDetector::snapshot_without_suppression(prior_front)
            }
        }
    }

    /// The pid [`with_focus_suppressed`] arms its targeted lease for under
    /// this policy: only a plain background AX action suppresses the target.
    pub fn suppressed_target(self, pid: i32) -> Option<i32> {
        matches!(self, FocusPolicy::Suppress).then_some(pid)
    }
}

/// Wrap an async closure `f` with a targeted focus-steal suppressor.
///
/// - `target_pid` — the pid the action is dispatched to. `Some(pid)` is
///   the standard case; `None` skips the targeted entry entirely
///   (caller relies on the surrounding `WindowChangeDetector` wildcard
///   lease).
/// - `prior_frontmost` — the pid to restore focus to if the target
///   activates. Typically captured from `apps::frontmost_pid()` before
///   the snapshot.
/// - `origin` — short static label for tracing, e.g. `"click.AXPress"`.
/// - `f` — the action to run with suppression armed.
///
/// Returns whatever `f` returns. Drops the lease ~50ms after `f`
/// resolves so any in-flight reflex activation is observed before
/// suppression ends.
///
/// If `target_pid` is already the frontmost app (no point fighting
/// ourselves) or `prior_frontmost` is `None` (no app to restore to),
/// the function still runs `f` — just without the lease. This keeps
/// the call sites simple (no per-tool `if frontmost == target` ladders).
pub async fn with_focus_suppressed<F, Fut, R>(
    target_pid: Option<i32>,
    prior_frontmost: Option<i32>,
    origin: &'static str,
    f: F,
) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    // Decide whether to arm a targeted lease. Three conditions must hold:
    //   1. We have a known target pid (Some).
    //   2. We have a prior frontmost to restore to (Some).
    //   3. The target isn't already frontmost — Swift's
    //      `isTargetFrontmost` short-circuit, mirrored here. Without
    //      it we'd arm an entry that re-activates the prior frontmost
    //      against a no-op activation, fighting our own previous
    //      restore.
    let should_arm = match (target_pid, prior_frontmost) {
        (Some(tp), Some(pf)) => tp != pf,
        _ => false,
    };

    let _lease = if should_arm {
        // unwrap()s are safe — `should_arm` is true only when both are Some.
        Some(focus_steal::begin_suppression(
            target_pid,
            prior_frontmost.unwrap(),
            origin,
        ))
    } else {
        None
    };

    // Hand key focus back to the user's app if the target opens a menu
    // (popup, select, context menu) while it is in the background.
    let _key_focus = match target_pid {
        Some(tp) if prior_frontmost != Some(tp) => {
            tokio::task::spawn_blocking(move || crate::key_focus_guard::protect(tp))
                .await
                .ok()
                .flatten()
        }
        _ => None,
    };

    let result = f().await;

    // Post-action settle — give the reactive observer time to fire on
    // any side-effect activation before we drop the lease. Matches
    // Swift's 50ms sleep in `FocusGuard.withFocusSuppressed`.
    if _lease.is_some() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // _lease drops here (RAII end_suppression).
    result
}

/// Convenience wrapper that captures the current frontmost via
/// `apps::frontmost_pid()` at call time. Use when the caller doesn't
/// already have a `prior_frontmost` from a surrounding snapshot —
/// e.g. tools that only opt into the layer-3 guard without the
/// snapshot/detect cycle.
///
/// Most action tools should prefer `with_focus_suppressed` with an
/// explicit `prior_frontmost` captured *before* the snapshot, so the
/// wildcard lease + targeted lease both restore to the same pid.
pub async fn with_focus_suppressed_now<F, Fut, R>(
    target_pid: Option<i32>,
    origin: &'static str,
    f: F,
) -> R
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let prior = apps::frontmost_pid();
    with_focus_suppressed(target_pid, prior, origin, f).await
}

/// What [`with_synthetic_focus`] hands back: the per-PID lease (for the
/// caller's own postcondition checks), what the binding did, and the action's
/// result. A refused binding is `result: Err(Refusal.into())` and the action
/// did not run.
pub(crate) struct Guarded<T, E> {
    pub lease: Option<BackgroundMutationLease>,
    pub focus: Report,
    pub result: Result<T, E>,
}

/// Run the blocking action `f` on `pid` with `window_id` bound as its
/// synthetic `binding` context (see [`crate::synthetic_focus`]).
///
/// One blocking thread runs, in order: the key-focus guard, the binding
/// with its readback, `f`, the release (previous key window, deactivation),
/// then drops the guard. `lease` moves onto that thread for the whole
/// sequence and comes back in [`Guarded::lease`].
///
/// A nested call (the current task already holds this process's mutation
/// lease, as a key tool's focus click does) leaves the binding to that
/// owner instead of releasing it, so the owner's following keys still have
/// an active app and a key window.
pub(crate) async fn with_synthetic_focus<T, E>(
    pid: i32,
    window_id: u32,
    binding: Binding,
    origin: &'static str,
    lease: Option<BackgroundMutationLease>,
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<Guarded<T, E>, tokio::task::JoinError>
where
    T: Send + 'static,
    E: From<Refusal> + Send + 'static,
{
    let nested = crate::background_mutation::held_by_current_task(pid);
    with_synthetic_focus_on(
        synthetic_focus::Live,
        pid,
        window_id,
        binding,
        origin,
        lease,
        nested,
        f,
    )
    .await
}

/// [`with_synthetic_focus`] over an explicit host, for tests.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn with_synthetic_focus_on<H, T, E>(
    mut host: H,
    pid: i32,
    window_id: u32,
    binding: Binding,
    origin: &'static str,
    lease: Option<BackgroundMutationLease>,
    leave_to_owner: bool,
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<Guarded<T, E>, tokio::task::JoinError>
where
    H: synthetic_focus::Host + Send + 'static,
    T: Send + 'static,
    E: From<Refusal> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        // Owned here until the whole sequence is done, whatever happens to
        // the caller's future.
        let lease = lease;
        let (focus, result) =
            synthetic_focus::run_with(&mut host, pid, window_id, binding, leave_to_owner, f);
        tracing::debug!(
            pid,
            window_id,
            ?binding,
            origin,
            synthetic = focus.synthetic,
            confirmed = focus.confirmed,
            refused = result.is_err(),
            "synthetic focus"
        );
        Guarded {
            lease,
            focus,
            result,
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic_focus::fake::{Fake, Op, Script};
    use std::sync::{Arc, Mutex};

    const TARGET: i32 = -91_010;
    const WINDOW: u32 = 500;

    #[tokio::test]
    async fn a_dropped_caller_future_keeps_the_lease_until_the_binding_is_released() {
        let fake = Fake::new(Script::background(TARGET, WINDOW));
        let ops = fake.ops.clone();
        let dispatch_ops = ops.clone();
        let lease = crate::tools::acquire_background_mutation(TARGET).await;
        let action = with_synthetic_focus_on(
            fake,
            TARGET,
            WINDOW,
            Binding::Key,
            "test",
            Some(lease),
            false,
            move || {
                // The action outlives the caller's patience.
                std::thread::sleep(Duration::from_millis(150));
                dispatch_ops.lock().unwrap().push(Op::Dispatch);
                Ok::<_, Refusal>(())
            },
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), action)
                .await
                .is_err(),
            "the caller gives up first"
        );
        // The lease is still held by the blocking thread: a second action on
        // the process must wait.
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                crate::tools::acquire_background_mutation(TARGET)
            )
            .await
            .is_err(),
            "the lease must stay with the running action"
        );
        let next = tokio::time::timeout(
            Duration::from_secs(2),
            crate::tools::acquire_background_mutation(TARGET),
        )
        .await
        .expect("the lease is released once the action has finished");
        let recorded = ops.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![
                Op::ProtectKeyFocus(TARGET),
                Op::Activation(TARGET, WINDOW),
                Op::MakeKey(TARGET, WINDOW),
                Op::Dispatch,
                Op::Deactivation(TARGET, WINDOW),
            ],
            "the action ran to its end and the binding was released before the lease"
        );
        drop(next);
    }

    #[tokio::test]
    async fn a_refused_binding_returns_the_lease_without_running_the_action() {
        let mut script = Script::background(TARGET - 1, WINDOW);
        script.owner = Some(7);
        let fake = Fake::new(script);
        let ops = fake.ops.clone();
        let ran = Arc::new(Mutex::new(false));
        let flag = ran.clone();
        let lease = crate::tools::acquire_background_mutation(TARGET - 1).await;
        let guarded = with_synthetic_focus_on(
            fake,
            TARGET - 1,
            WINDOW,
            Binding::KeyMain,
            "test",
            Some(lease),
            false,
            move || {
                *flag.lock().unwrap() = true;
                Ok::<_, Refusal>(())
            },
        )
        .await
        .unwrap();
        assert!(matches!(guarded.result, Err(Refusal::NotOwned { .. })));
        assert!(!*ran.lock().unwrap(), "the action must not run");
        assert_eq!(guarded.focus, Report::default());
        assert!(
            guarded.lease.is_some(),
            "the lease comes back to the caller"
        );
        assert_eq!(
            ops.lock().unwrap().clone(),
            vec![Op::ProtectKeyFocus(TARGET - 1)]
        );
    }

    #[test]
    fn the_policy_decides_suppression_and_snapshot_together() {
        assert_eq!(FocusPolicy::Suppress.suppressed_target(7), Some(7));
        assert_eq!(
            FocusPolicy::Synthetic {
                window_id: 1,
                binding: Binding::Active
            }
            .suppressed_target(7),
            None
        );
        assert_eq!(FocusPolicy::Foreground.suppressed_target(7), None);
    }
}
