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
key-downs, Arrange Left runs and sheet-button presses, while the user's real
app held the front. The records are the ones `input/skylight.rs` already
sent; each was sent on its own and in sequences, several runs each, on macOS
27.0 (Darwin 27.0.0). The oracle is
`crates/platform-macos/tests/background_focus_oracle.rs`.

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
   window is key; a sibling window shows up in them once it is bound. With a
   sheet up, `AXFocusedWindow` names the sheet and `AXMainWindow` its parent,
   before and after the parent is bound: the pair for the parent makes the
   parent main and the sheet key.
7. WindowServer's front process (`_SLPSGetFrontProcess`) and key-focus
   process (`SLPSGetKeyFocusProcess`), the user's focused window and element,
   and the window order did not change in the record-only runs, sampled every
   5 ms during the calls. The exceptions seen since are facts 10 to 12 and
   14. One earlier run saw the user's front process change at the activation
   step; three repeats with the same steps did not reproduce it, and the
   process that had been in front no longer existed, so it is taken as the
   user switching apps.
8. Key events posted to the process with the window in their routing fields
   (`type_text`, `type_text_chars`, `press_key`, a shift+m `hotkey`) reached
   the fixture's text field while the fixture was inactive and had no key
   window: the journal shows each key-down in the main window with the field
   editor (`NSTextView`) as responder, `is_active=false`, `key_window=0`, and
   the field read the typed text back. A key addressed to the sibling window
   (a label, nothing editable) reached that window (`NSWindow` responder) and
   not the text field. The driver's earlier key-window primer sent no
   records in the first four cases, because the main window was already the
   app's focused window with a focused element.
9. `NSWorkspace` does not report a synthetically activated target as the
   active application: `apps::frontmost_pid()` (`frontmostApplication`),
   sampled every 5 ms through every oracle case, named the user's app
   throughout, with the target's `NSApp.isActive` true. The comment in
   `tools/click.rs` that said otherwise described the earlier prologue and is
   not reproduced.
10. The make-key pair for a sheet (an attached window) moves WindowServer's
    key-focus process to the target, and leaves it there after the
    deactivation notice; the key-focus guard handed it back. The pair for the
    sheet's parent does not move it. Measured step by step, 400 ms apart:
    activation(parent), pair(parent), `AXPress` of the sheet's button,
    deactivation(parent), activation(sheet), pair(sheet),
    deactivation(sheet); the key focus moved at pair(sheet). That sequence is
    what the exit sent when it "restored" the sheet it had never moved. One
    full oracle run before this was known also ended a sheet-button press
    with the target as WindowServer's front process and still active; the
    step measurement did not reproduce the front change, and the full run has
    passed since the sheet restore was removed.
11. Writing `AXFocused` to the text field while the app believes it is active
    (bound) orders its window above the user's front window (from z-order 1 to
    4 among the on-screen windows), the app briefly keys a short-lived window
    of its own, and the order stays after the release. The same write while
    the app is inactive changes neither the window order nor the front
    process (the hotkey oracle case with an element).
12. The deactivation notice closes a popup button's menu that an `AXPress`
    opened while the window was bound: the menu window (layer 101) is gone
    after the notice and the journal shows the menu window resigning key.
    Opening the menu itself took WindowServer's key focus for the target and
    ordered the fixture window above the user's; the key-focus guard returned
    the focus.
13. AppKit re-validates a menu item on a schedule of its own: Arrange Left
    read `AXEnabled` true within 400 ms after a bare binding, within about 1 s
    when one of its attributes had been read through accessibility just
    before, and not within 600 ms after the hotkey route had read the menu
    bar for a key equivalent. `invoke_menu` now waits up to 2 s.
14. In one full run the sibling key case saw ControlCenter hold key focus for
    one sample. ControlCenter is a user surface, the run was on a Mac in use,
    and two reruns did not see it; it is taken as the user's input.

Before this change, the key-window primers (`invoke_menu`, native `AXPress`)
sent the activation notice and no deactivation, and the pixel prologue sent
one only when key focus had moved: a background `invoke_menu`, a disabled
item, and a native `AXPress` each left the fixture believing it was active
with no key window (`background_focus_oracle` against ded0c1748).

## Hypotheses

- When the user later activates an app that was bound to a sibling window,
  AppKit re-keys the window that was key when it last resigned. The exit
  therefore restores the previous key window before the final deactivation.
  Only the record sequence was verified; what AppKit keys on the user's next
  real activation was not observed.
- A target may still activate itself for real during a binding (an app that
  calls `activate` on its own). Not observed on the fixture. The reactive
  layer stays armed against it (below), and the exit tells the user's
  activation of the target from the target's own by recent user input; that
  distinction was exercised hermetically, not natively.
- The type-21 "key focus returned" notice is needed only to undo a key-focus
  removal, for example after AppKit opened a menu and took key focus. No
  driver path removes the target's key focus during a synthetic binding, so
  it is not sent. If a future path does, `key_focus_guard` is where the
  removal is seen.
- Posted keys reach the app's focused window: AppKit routes them by the
  window in the event, VCL (LibreOffice) to its focused window and nowhere
  while it has no focused element (CUA-1767). `AXFocusedWindow` plus a focused
  element is therefore taken as "the window already routes keys", the
  predictor the earlier primer used; it was measured on LibreOffice and seen
  on AppKit (fact 8), not on other toolkits.
- Focusing an element in a sibling window S while the app is inactive and
  has its main window M focused: whether `AXFocusedUIElement` then names S's
  element was not measured (the fixture's sibling window has no editable
  control). The hotkey menu route reads the focus back inside the binding and
  refuses when it is not the requested element, rather than assuming either.

## Implementation choices

- One module, `synthetic_focus`, owns the lifecycle per target. `enter`
  checks the window belongs to the pid, sends the activation notice and,
  for a `Key`, `KeyMain` or `Keys` binding, the make-key pair to that process
  only, then reads `AXFrontmost` plus the exact focused (and main) window
  back for up to 400 ms (200 ms for `Active`). An unconfirmed key binding is
  undone and refused (`window_not_key`); nothing is dispatched. `Active` (raw
  pixel input) does not refuse on an unconfirmed readback, as the pixel
  transport never did. A `Key` binding accepts the requested window's sheet as
  the key window (fact 6), so a sheet's own controls stay pressable; `KeyMain`
  and `Keys` stay strict.
- Exit sends the deactivation notice; when the binding moved the app's
  focused window and the previous one still exists, it first restores it with
  the sequence from fact 3. A press that bound a sheet's parent left the sheet
  focused and restores nothing (fact 10). The front process is re-read before
  every record: once the target is WindowServer's front process nothing more
  is sent, and whether the user brought it there (user input within the
  key-focus guard's recent window) or the target activated itself is reported
  as `FrontAtExit`; the latter is the reactive layer's to undo. The release
  runs from a drop guard, so an action that panics still releases.
- An app that already believes it is active is told to resign before a key
  binding (fact 3), and left alone for an `Active` binding.
- `focus_guard::FocusPolicy` (Suppress, Synthetic, Foreground) decides the
  window snapshot's suppression (`Suppression`: wildcard, allowing the target
  for `Active`, none) and the synthetic scope together. During a `Synthetic`
  key binding of a target that is not in front, the reactive layer is also
  armed against that target's own activation (`focus_steal::begin_suppression`
  naming it), as it was for the earlier prologue; fact 9 says the synthetic
  notice itself never trips it. `run_blocking_under` runs the key-focus guard,
  the binding, the action and the release on one blocking thread and moves
  the per-PID mutation lease onto it, where it is released only after the
  binding, so a caller future that is dropped (a script timeout) still gets
  the release before the next action on that process can start. There is no
  readback at exit.
- The OS calls sit behind a `Host` trait so hermetic tests check the record
  order and addressing without a window server.
- Callers: `invoke_menu` background (`KeyMain`, both exact focused/main
  checks kept); `hotkey`'s menu-equivalent route (through `invoke_menu`, which
  focuses the requested element under the same lease before the binding and
  reads the focus back inside it, fact 11); a background `AXPress` in a
  native app (`Key`; a window that cannot be made key is pressed unbound, with
  the app inactive, and the result says a control that acts on the key
  window's first responder did nothing); a background left pixel `click` and
  `drag` (`Active`, both under the per-PID lease); and the key tools
  (`press_key`, `hotkey`'s key post, `type_text`, `type_text_chars`,
  `set_value` type-ahead) with `Keys`. The ad-hoc primers and the earlier
  key-window scope (`WindowKeyScope`, `prime_exact_window_key`, `key_scope`,
  `leave_to_owner`) are removed.
- `Keys` sends nothing when the window already routes keys (fact 8 and the
  hypothesis above), so the common background key post stays record-free, as
  it was; otherwise it binds as `Key` does and refuses unconfirmed
  (`window_not_key`). The earlier primer's `AXFocusedWindow` write fallback is
  gone: that write does not make a window key, and a binding that only it
  could satisfy was never proven to route keys. For a target the user has in
  front, `Keys` switches the app's key window in place (deactivation notice,
  activation notice naming the window, pair), as the earlier primer did, and
  switches it back without telling the front app to resign.
- The key tools run their key post on the blocking thread that holds the
  binding and the per-PID lease (`focus_guard::run_blocking_under`), so a
  dropped caller future still releases the binding after the keys. A key
  tool's pixel focus click runs before that, as a nested `click`, and binds
  and releases its own `Active` context.
- Refusals name the condition: `window_not_key` (a key binding the readback
  did not confirm), `window_not_found` or `owner_pid_mismatch` (the window is
  not the target's), `front_changed` (the front process moved while binding)
  and the WindowServer-connection refusal.
- The deactivation notice at exit closes transient UI the action opened
  (fact 12). The binding is not kept open for it: a binding that outlives its
  action leaves the target believing it is active while the user works, which
  is what this module removes. Options recorded for a later decision: keep the
  exit and have menu-opening actions (`click` on a popup button) report that
  the menu closed; send the type-21 key-focus-returned notice instead of the
  deactivation while a menu is open (not measured); or let the next action on
  the same target reuse an open binding under the per-PID lease.

## Verified

- Hermetic: `cargo test -p platform-macos --lib` (synthetic_focus: record
  order, panic release, the per-record front check stopping a restore, user
  versus target at exit, sheet acceptance with no sheet restore, checked by
  reverting the fix and watching the test fail, sibling restore, the lease
  released only after the binding; focus_guard: suppression per policy;
  invoke_menu: refusal mapping; click) and `cargo test -p cua-driver-core --lib`.
- Native, on this Mac, `cargo test -p platform-macos --test background_focus_oracle -- --run-gui`,
  13 cases: Arrange Left runs exactly once with the fixture active and its
  window key, through `invoke_menu` and through a cmd+option+l `hotkey` with
  and without an element; a sibling binding and an action-less item refuse
  `background_unavailable` and the sibling case leaves the main window
  focused; a window a sheet keeps from being key refuses `window_not_key` for
  `invoke_menu`; an `AXPress` increments once; the sheet's own button presses
  once with the sheet key and the parent main; `type_text`, `type_text_chars`,
  `press_key` and a shift+m `hotkey` land in the text field with no record
  sent; a `press_key` addressed to the sibling window binds it, reaches only
  it, restores the main window and ends inactive; every activation is paired
  with a resignation; the user's front process (WindowServer and
  `NSWorkspace`), key focus, focused window and window order are unchanged
  throughout.

## Not verified

- The full cross-platform desktop E2E matrix and
  `harness_appkit_invoke_menu_live_path` (its expectation was flipped from
  the oracle evidence; it needs the canonical macOS runner). `cargo clippy` is
  not installed on this Mac.
- Raw pixel `click` and `drag` under `Active`, and the key tools' pixel focus
  click (`x,y` on `type_text`, `press_key`, `hotkey`): the oracle moves no
  pointer, and their window frame proof takes a window capture, which needs
  Screen Recording. The hermetic tests cover the sequences.
- `document_edit` and the browser setup path (`setup_ui`) still use the
  earlier primitive (`activate_without_raise_with`,
  `prepare_background_pixel_click`), which sends the activation notice
  without a deactivation; neither can be verified here (TextEdit with
  Automation, a browser). They are the next callers to move.
- `hotkey`'s key route writes the element focus inside its `Keys` binding
  (the order it had before). The binding sends no record when the window
  already routes keys, the common case; when it does send them, fact 11
  applies and the focus write should move before the binding as the menu
  route's did.
- The hotkey menu route with an element in a sibling window (hypothesis
  above); `Keys` on a LibreOffice that has never been active (the case the
  binding exists for) and on a front target's other window, beyond the
  hermetic sequence tests.
- Chromium, Electron and Catalyst targets under the `Active` binding beyond
  what the pixel prologue already did; Space preservation; what AppKit keys
  on the user's next real activation of a target whose key window the driver
  restored.
