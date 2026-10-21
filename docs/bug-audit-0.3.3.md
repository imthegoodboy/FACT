# FACT 0.3.3 follow-up audit

Baseline: `fc991ac` / 0.3.2. This follows the initial scrolling audit and the
request for a broader reliability pass and a less intrusive scrollbar.

## Reproduced defects

| Defect | Root cause | Fix and regression |
| --- | --- | --- |
| Clicking a right-click menu action does nothing | `handle_mouse` returned whenever any overlay was open | Context actions now use the same clipped geometry as rendering. A real-ConPTY test clicks Split right, dismisses outside the menu and clicks a menu clamped to the bottom-right. Wheel input also navigates context menus, help, palette, sessions and search. |
| Menu labels and shortcuts are unnecessarily truncated | A fixed 34-column width could not fit the existing labels and configured bindings | Size the menu to its longest actual row, clamped to the viewport. The ConPTY menu test asserts the complete restart shortcut is visible. |
| Valid large pastes are rejected | Protocol accepts 4 MiB, but paste used the ordinary 256 KiB PTY input method; bracket markers also pushed boundary-size input over that lower limit | A bounded paste method queues the complete operation atomically. The original 16 MiB queued-input byte budget remains. A real daemon/raw-input fixture verifies exactly 4 MiB of UTF-8 text, normalized newlines and complete bracket markers, and rejects input beyond the protocol limit. The test failed before the fix with `input chunk is too large`. |
| Paste can leave the user looking at history while input executes | Paste did not reset terminal scroll offset, unlike typing | Reset the offset only after successful queueing. The raw fixture deliberately emits no output; the test verifies the viewport returns to live input anyway. |
| Exit notice precedes the final process output | The daemon appended it after a bounded output tick even if more chunks remained queued | Observe reader EOF before draining, and append the notice only after a final empty read. A delayed-owner test failed with the notice between `OUTPUT_49` and `OUTPUT_50`, ahead of the final result; it passes after the fix. |

## Requested scrollbar behavior

- Ordinary shell panes reveal a history scrollbar when the pointer is within
  two columns of their right edge. It remains visible during dragging, hides
  when the pointer moves away after a drag ends, and is absent when there is
  no history. Host focus loss clears hover and local drag state; bell cues no
  longer overwrite terminal cells.
- Alternate-screen and mouse-controlled panes receive no FACT rail or rail
  click interception. The app owns its UI. Wheel reports retain the actual
  coordinates, including the last column; keyboard-only full-screen apps still
  receive page keys. Shift selection and keyboard scroll mode remain available.
- Terminal content now uses the full pane width. The scrollbar overlays one
  column only while visible. Protocol 6 prevents old clients/daemons from
  disagreeing about that geometry.
- Inline applications that advertise neither alternate-screen nor mouse modes
  share the ordinary shell history behavior. App-owned scrollbars are unchanged.

## Verification and boundaries

The release workspace suite passed all 124 tests, including 12 real-ConPTY TUI
scenarios, 10 daemon integration scenarios, eight PTY scenarios, session
lifecycle/restart/import/export, input encoding, configuration validation,
authenticated IPC and the no-network source check. A noisy-pane kill test
also passed on the baseline: it is additional regression coverage, not a claim
that a deadlock was found or fixed.

The release/installer checks and CI results are recorded in the PR. README
screenshots are captured from isolated live sessions; regeneration is documented
in `docs/screenshots/README.md`.

This is a bounded audit with reproducible fixes, not proof that every possible
bug is eliminated. Windows 10 and live model conversations were not exercised.
