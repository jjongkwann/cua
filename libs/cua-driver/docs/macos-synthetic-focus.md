# macOS synthetic focus for background targets

Cua Driver acts on apps the user does not have in front. On macOS a
background app is not active, so AppKit has no key window for it: its
Window-menu commands validate as disabled, `nil`-targeted controls have no
first responder, and keys posted to the process go nowhere. This document
records what was observed, what is still a hypothesis, and what the driver
does about it (`libs/cua-driver/rust/crates/platform-macos/src/synthetic_focus.rs`,
integrated through `focus_guard::FocusPolicy`).

Everything below comes from the repository, the public AppKit event
contract (`NSEvent.h`: `NSEventTypeAppKitDefined = 13`,
`NSEventSubtypeApplicationActivated = 1`,
`NSEventSubtypeApplicationDeactivated = 2`) and experiments against the
repository's own AppKit fixture. No third-party binary was inspected for it.

## Observed facts

Measured on this repository's AppKit fixture with its opt-in focus oracle
(`CUA_APPKIT_FOCUS_ORACLE`, `tests/fixtures/apps/macos/appkit/main.swift`),
which journals AppKit's own `NSApp.isActive`, key and main window changes,
key-downs and Arrange Left runs, while the user's real app held the front.
The records are the ones `input/skylight.rs` already sent; each was sent on
its own and in sequences, several runs each, on macOS 27.0 (Darwin 27.0.0).

1. An activation notice (type 13, subtype 1) to the target process makes
   `NSApp.isActive` true (`didBecomeActive`) and the app's `AXFrontmost`
   read true. No window becomes key or main by it alone. A second notice
   while active posts `didBecomeActive` again.
2. The make-key record pair (types 0x01/0x02 for a window id), posted after
   the activation notice, makes that window main and key. Window > Arrange >
   Left, which the fixture validates as `NSApp.isActive && window.isKeyWindow`,
   reads `AXEnabled = true` and `AXPress` runs it exactly once.
3. The pair for another window while a window is already key does nothing.
   Switching the key window inside the app needs a deactivation notice, a
   new activation notice naming the other window, and the pair for it. The
   switch back works the same way.
4. A deactivation notice (type 13, subtype 2) resigns the key window, the
   main window and `NSApp.isActive` in one step, whichever window id it
   names. Sent to an app that was not active, it still posts
   `didResignActive`.
5. The type-21 notices: subtype `0x4000` makes the key window resign while
   the app stays active; subtype `0x8000` after it makes the same window key
   again. Neither makes a window key in an app that has none.
6. `AXFrontmost` on the app follows the notices (true after 13/1, false after
   13/2). `AXFocusedWindow` and `AXMainWindow` already name the app's
   remembered main window before any record, so they cannot say whether that
   window is key; a sibling window shows up in them once it is bound.
7. WindowServer's front process (`_SLPSGetFrontProcess`) and key-focus
   process (`SLPSGetKeyFocusProcess`), the user's focused window and element,
   and the window order did not change in any run, sampled every 5 ms during
   the calls. One earlier run saw the user's front process change at the
   activation step; three repeats with the same steps did not reproduce it,
   and the process that had been in front no longer existed, so it is taken
   as the user switching apps.

Before this change, the driver's primers sent the activation notice and
never a deactivation: a background `invoke_menu`, a disabled item, and a
native `AXPress` each left the fixture believing it was active with no key
window (`background_focus_oracle` against ded0c1748).

## Hypotheses

- When the user later activates an app that was bound to a sibling window,
  AppKit re-keys the window that was key when it last resigned. The exit
  therefore restores the previous key window before the final deactivation.
  Only the record sequence was verified; what AppKit keys on the user's next
  real activation was not observed.
- `NSWorkspace` may report a synthetically active target as the active
  application (a comment in `tools/click.rs` says so for the earlier
  prologue). The reactive layer therefore allows the target's activation
  during a synthetic binding and still undoes any other app's. Not observed
  in these runs: WindowServer's front process never moved.
- The type-21 "key focus returned" notice is needed only to undo a key-focus
  removal, for example after AppKit opened a menu and took key focus. No
  driver path removes the target's key focus during a synthetic binding, so
  it is not sent. If a future path does, `key_focus_guard` is where the
  removal is seen.

## Implementation choices

- One module, `synthetic_focus`, owns the lifecycle per target. `enter`
  checks the window belongs to the pid, sends the activation notice and,
  for a `Key` or `KeyMain` binding, the make-key pair to that process only,
  then reads `AXFrontmost` plus the exact focused (and main) window back for
  up to 400 ms. An unconfirmed key binding is undone and refused
  (`window_not_key`); nothing is dispatched. `Active` (raw pixel input) does
  not refuse on an unconfirmed readback, as the pixel transport never did.
- Exit sends the deactivation notice; when the binding moved the key window
  and the previous one still exists, it first restores it with the sequence
  from fact 3. Nothing is sent when the target has really come to the front
  (WindowServer's front process is the target): that is the user's doing.
- An app that already believes it is active is told to resign before a key
  binding (fact 3), and left alone for an `Active` binding.
- `focus_guard::FocusPolicy` (Suppress, Synthetic, Foreground) decides the
  window snapshot's suppression and the synthetic scope together.
  `with_synthetic_focus` runs the key-focus guard, the binding, the action
  and the release on one blocking thread and moves the per-PID mutation
  lease onto it, so a caller future that is dropped (a script timeout) still
  gets the restore and the readback before the next action on that process
  can start. A nested call (a key tool's focus click inside the owner's
  lease) leaves the binding to its owner.
- The OS calls sit behind a `Host` trait so hermetic tests check the record
  order and addressing without a window server.
- Callers in this change: `invoke_menu` background (`KeyMain`, both exact
  focused/main checks kept), `hotkey`'s menu-equivalent route (through
  `invoke_menu`), a background `AXPress` in a native app (`Key`), and a
  background left pixel `click` and `drag` (`Active`). The ad-hoc primers
  they replaced are removed.

## Verified

- Hermetic: `cargo test -p platform-macos --lib` (synthetic_focus,
  focus_guard, invoke_menu, click tests) and `cargo test -p cua-driver-core --lib`.
- Native, on this Mac, `cargo test -p platform-macos --test background_focus_oracle -- --run-gui`:
  Arrange Left runs exactly once with the fixture active and its window key;
  a sibling binding and an action-less item refuse `background_unavailable`;
  a window a sheet keeps from being key refuses `window_not_key`; an
  `AXPress` increments once; every activation is paired with a resignation;
  the user's front process, key focus, focused window and window order are
  unchanged throughout.

## Not verified

- The full cross-platform desktop E2E matrix and
  `harness_appkit_invoke_menu_live_path` (its expectation was flipped from
  the oracle evidence; it needs the canonical macOS runner).
- Keyboard tools (`press_key`, `hotkey` key posting, `type_text`,
  `type_text_chars`), `set_value` type-ahead, `document_edit` and the
  browser setup path still use the earlier primitives (`WindowKeyScope`,
  `prepare_background_pixel_click`), which send the activation notice
  without a deactivation. They are the next callers to move.
- Chromium, Electron and Catalyst targets under the `Active` binding beyond
  what the pixel prologue already did; Space preservation; what AppKit keys
  on the user's next real activation of a target whose key window the driver
  restored.
