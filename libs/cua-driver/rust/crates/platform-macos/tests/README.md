# Embedded menu restoration regression

`embedded_menu_restore` runs AppKit on the process main thread and embeds the
real platform tool registry on a background Tokio worker. It launches a second
native process, invokes its Window > Minimize command, and verifies:

- the target process independently observes native minimization;
- the host becomes frontmost again;
- the host's original window is natively key; and
- its other window is not key.

The test does not accept the tool response as proof of restoration. It has
bounded readiness and completion deadlines. It exercises the embedded platform
implementation, not the public SDK authorization/lifecycle boundary.

## Build and authorize

From the repository root, build an independently identified fixture application:

```bash
bash scripts/ci/macos/build-embedded-menu-fixture.sh \
  "$HOME/Applications/CuaEmbeddedMenuFixture.app" \
  com.trycua.tests.embedded-menu-fixture
```

The builder refuses to overwrite an existing application. It uses an ad-hoc
signature; preserve the built bytes after authorization. Rebuilding can invalidate
that authorization. Do not borrow the installed CuaDriver application's identity
or modify TCC databases.

In a logged-in Aqua session, request Accessibility through the normal macOS flow:

```bash
open -W -n "$HOME/Applications/CuaEmbeddedMenuFixture.app" \
  --args --request-accessibility --report /tmp/embedded-menu-permission.txt
```

The permission mode waits up to 180 seconds for human approval and does not run
the regression. If it expires, grant access in System Settings before rerunning.

## Run the foreground regression

```bash
open -W -n "$HOME/Applications/CuaEmbeddedMenuFixture.app" \
  --args --run-gui --report /tmp/embedded-menu-result.txt
```

Run one fixture at a time. This intentionally foregrounds the fixture: a
background launcher that suppresses activation interferes with the test's
preconditions. Direct execution of the executable may also have different TCC
attribution from a LaunchServices application launch.

Require the report to contain exactly `passed`. A native abort can leave it at
`running`, so the exit status of `open` alone is not acceptance evidence. Rust
panics write `failed` with their diagnostic. Without either explicit mode, the
test executable prints a skip, allowing ordinary Cargo test/compile jobs to run
without a desktop or permission prompt.

## Negative control

In an isolated checkout, change only the call inside `focus_exact_window` from
`focus_ax_window_with_thread_affinity(pid, window_id)` to
`focus_ax_window(pid, window_id)`. Package that build under a different test
bundle identifier and obtain its own normal Accessibility grant. Restore the
source mutation before returning to the candidate.

On macOS Tahoe, the control traps during restoration with
`Must only be used from the main thread`. Its crash stack includes
`NSWindow makeKeyAndOrderFront:`, `NSResponder accessibilityPerformRaise`,
`AXUIElementPerformAction`, `focus_ax_window`, and the menu restoration closure
on a Tokio blocking worker. A missing permission or failed initial focus
precondition does not count as this negative control.

An aborted parent cannot clean up its target child; the child exits after its
bounded 45-second lifetime. Wait for that exit before the next run.

This focused regression supplements, rather than replaces, the complete
canonical desktop suite described in
[`test-harnesses-guide.md`](../../../../docs/test-harnesses-guide.md).

# Background focus oracle

`background_focus_oracle` checks that the driver acts on a background AppKit
app without the user noticing. Each case launches the repository's AppKit
fixture (`tests/fixtures/apps/macos/appkit`) in the background with its focus
oracle on (`CUA_APPKIT_FOCUS_ORACLE`), drives it through the in-process tool
registry, and compares two sides:

- the target's own view, from the fixture's journal: whether AppKit believed
  it active and which window it believed key and main when the command ran,
  and whether that belief was handed back afterwards;
- the user's side, from WindowServer, `NSWorkspace` and Accessibility: the
  front process, the process holding key focus, the user's focused window and
  element, and whether a fixture window ever sat above the user's front
  window, sampled every few milliseconds during the call.

The cases cover a background `invoke_menu` (the requested window, a sibling
window, an item with no action, a window a sheet keeps from being key), a
native `AXPress` (a button, and a sheet's own button), the key tools
(`type_text`, `type_text_chars`, `press_key`, a `hotkey` that sends keys, a
`press_key` addressed to the sibling window) and the `hotkey` menu-equivalent
route with and without an element. Nothing in it moves the pointer, raises a
window or activates an app; raw pixel clicks and drags are not covered.

## Build and run

From `libs/cua-driver`, build the fixture, then run the oracle from the Rust
workspace in a logged-in Aqua session whose terminal (or its responsible app)
has Accessibility:

```bash
bash tests/fixtures/build/macos.sh --only appkit
cd rust
cargo test -p platform-macos --test background_focus_oracle -- --run-gui --report /tmp/focus-oracle.json
```

Without `--run-gui` the binary prints a skip, so plain `cargo test` stays
hermetic. `CUA_FOCUS_ORACLE_CASE=<name>` runs one case. The report is JSON:
`passed`, and per case the failures, the call time, the refusal code, the
fixture's journal and the user's focus before and after.

The oracle runs on a Mac in use, but a user interaction during a case (a
menu-bar click, a Control Center sample holding key focus) fails that case as
a focus change; rerun it alone. What the cases established, and what they do
not, is in
[`docs/macos-synthetic-focus.md`](../../../../docs/macos-synthetic-focus.md).
