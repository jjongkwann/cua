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
//! which wildcard suppression) is taken, whether the target's own activation
//! is suppressed, and whether layer 2 applies, so the three never disagree.
//! NSWorkspace does not report a synthetic activation (measured with the
//! fixture oracle: `frontmostApplication` never named the target during a
//! bound action), so the reactive layer only ever sees a real activation.
//! Key, key-and-main and key-routing bindings therefore keep suppressing the
//! target's real self-activation, as every AX action did before; raw pixel
//! input (`Active`) keeps its earlier allowance, since the click itself may
//! legitimately activate the app and `restore_user_focus_after_background_pixel`
//! hands the front back.
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
//! - [`run_blocking_under`] picks between the two from a [`FocusPolicy`],
//!   for tools whose action is one blocking closure (the key tools).

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

/// The wildcard suppression a policy's window snapshot arms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suppression {
    /// Any app that takes the front, the target included, is restored.
    Wildcard,
    /// Any app but the target.
    AllowingTarget,
    /// None: the caller fronts and restores itself.
    None,
}

impl FocusPolicy {
    /// The wildcard suppression this policy's snapshot arms.
    pub fn suppression(self) -> Suppression {
        match self {
            FocusPolicy::Suppress => Suppression::Wildcard,
            FocusPolicy::Synthetic {
                binding: Binding::Active,
                ..
            } => Suppression::AllowingTarget,
            FocusPolicy::Synthetic { .. } => Suppression::Wildcard,
            FocusPolicy::Foreground => Suppression::None,
        }
    }

    /// The window snapshot (and wildcard suppression) matching this policy,
    /// to take before the action and detect after it.
    pub fn snapshot(self, prior_front: Option<i32>, pid: i32) -> Snapshot {
        match self.suppression() {
            Suppression::Wildcard => WindowChangeDetector::snapshot(prior_front),
            Suppression::AllowingTarget => {
                WindowChangeDetector::snapshot_allowing_activation(prior_front, pid)
            }
            Suppression::None => WindowChangeDetector::snapshot_without_suppression(prior_front),
        }
    }

    /// The pid whose real self-activation a targeted lease restores under
    /// this policy: the target, except for raw pixel input and foreground
    /// delivery.
    pub fn suppressed_target(self, pid: i32) -> Option<i32> {
        matches!(self.suppression(), Suppression::Wildcard).then_some(pid)
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
/// A key tool's focus click runs nested inside the tool's own mutation
/// lease, before the tool binds the window for its keys; that click binds
/// and releases its own `Active` context, and the key binding that follows
/// starts from a released target.
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
    with_synthetic_focus_on(
        synthetic_focus::Live,
        pid,
        window_id,
        binding,
        origin,
        lease,
        f,
    )
    .await
}

/// [`with_synthetic_focus`] over an explicit host, for tests.
pub(crate) async fn with_synthetic_focus_on<H, T, E>(
    mut host: H,
    pid: i32,
    window_id: u32,
    binding: Binding,
    origin: &'static str,
    lease: Option<BackgroundMutationLease>,
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
        let (focus, result) = synthetic_focus::run_with(&mut host, pid, window_id, binding, f);
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

/// Run the blocking action `f` on `pid` under `policy`: with the window
/// bound through [`with_synthetic_focus`] (the lease moves onto the blocking
/// thread and comes back through `lease`, and the target's real
/// self-activation stays suppressed where the policy says so), or on a
/// blocking thread under [`with_focus_suppressed`]. A refused binding comes
/// back as `Ok(Err(refusal))`, the action not run; callers turn it into the
/// tool's structured refusal. `report` receives what the binding did.
pub(crate) async fn run_blocking_under<T, E>(
    pid: i32,
    prior_front: Option<i32>,
    policy: FocusPolicy,
    origin: &'static str,
    lease: &mut Option<BackgroundMutationLease>,
    report: &mut Report,
    f: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<Result<T, E>, tokio::task::JoinError>
where
    T: Send + 'static,
    E: From<Refusal> + Send + 'static,
{
    match policy {
        FocusPolicy::Synthetic { window_id, binding } => {
            let targeted = match (policy.suppressed_target(pid), prior_front) {
                (Some(target), Some(prior)) if prior != target => {
                    Some(focus_steal::begin_suppression(Some(target), prior, origin))
                }
                _ => None,
            };
            let guarded =
                with_synthetic_focus(pid, window_id, binding, origin, lease.take(), f).await?;
            *lease = guarded.lease;
            *report = guarded.focus;
            if targeted.is_some() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(guarded.result)
        }
        FocusPolicy::Suppress | FocusPolicy::Foreground => {
            with_focus_suppressed(
                policy.suppressed_target(pid),
                prior_front,
                origin,
                || async { tokio::task::spawn_blocking(f).await },
            )
            .await
        }
    }
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
        let synthetic = |binding| FocusPolicy::Synthetic {
            window_id: 1,
            binding,
        };
        // A plain AX action and every key binding suppress the target's own
        // activation with a wildcard snapshot.
        for policy in [
            FocusPolicy::Suppress,
            synthetic(Binding::Key),
            synthetic(Binding::KeyMain),
            synthetic(Binding::Keys),
        ] {
            assert_eq!(policy.suppression(), Suppression::Wildcard, "{policy:?}");
            assert_eq!(policy.suppressed_target(7), Some(7), "{policy:?}");
        }
        // Raw pixel input lets the target activate; foreground delivery
        // suppresses nothing.
        assert_eq!(
            synthetic(Binding::Active).suppression(),
            Suppression::AllowingTarget
        );
        assert_eq!(synthetic(Binding::Active).suppressed_target(7), None);
        assert_eq!(FocusPolicy::Foreground.suppression(), Suppression::None);
        assert_eq!(FocusPolicy::Foreground.suppressed_target(7), None);
    }

    #[tokio::test]
    async fn the_lease_is_released_only_after_the_binding() {
        let mut fake = Fake::new(Script::background(TARGET - 2, WINDOW));
        let gate = crate::synthetic_focus::fake::gate();
        fake.hold_deactivation = Some(gate.clone());
        let ops = fake.ops.clone();
        let lease = crate::tools::acquire_background_mutation(TARGET - 2).await;
        let action = tokio::spawn(with_synthetic_focus_on(
            fake,
            TARGET - 2,
            WINDOW,
            Binding::Key,
            "test",
            Some(lease),
            move || Ok::<_, Refusal>(()),
        ));
        // The release is blocked in its deactivation notice; the action has
        // run, and the lease must still be held.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !ops
            .lock()
            .unwrap()
            .contains(&Op::MakeKey(TARGET - 2, WINDOW))
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the binding never ran"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                crate::tools::acquire_background_mutation(TARGET - 2)
            )
            .await
            .is_err(),
            "the lease must stay held while the release is still running"
        );
        crate::synthetic_focus::fake::open(&gate);
        let guarded = tokio::time::timeout(Duration::from_secs(2), action)
            .await
            .expect("the action finishes once the release may proceed")
            .expect("the task does not panic")
            .expect("the blocking task does not panic");
        assert!(guarded.result.is_ok());
        drop(guarded);
        tokio::time::timeout(
            Duration::from_secs(2),
            crate::tools::acquire_background_mutation(TARGET - 2),
        )
        .await
        .expect("the lease is released once the release has finished");
        assert_eq!(
            ops.lock().unwrap().last(),
            Some(&Op::Deactivation(TARGET - 2, WINDOW))
        );
    }
}
