# Phase 0 spike: GPUI on Windows (accessibility)

Throwaway app for item 1 of Phase 0 in `docs/rust-gpui-rewrite-plan.md`. Not part of the shipped product; delete
once the results are copied into the plan.

```
cargo run -p beatseam-spike-windows-gpui
```

GPUI (`gpui` + `gpui_platform`) is pinned by git revision `a701d985a8caac9c26f33e61afff4e102a32b303` of
`zed-industries/zed`, with no local patches. `src/text_input.rs` is GPUI's own `input` example plus accessibility
attributes (Apache-2.0, see the header there).

The window has a 5,000-row `uniform_list`, a text input, a record + tonearm drawn with `canvas`/`PathBuilder`
(spinning), and a slider. Each control sets GPUI's accessibility attributes (`role`, `aria_label`, numeric value,
`on_a11y_action`) the way a real screen would.

## Results (2026-11-17, Windows 11, debug build)

Checked by dumping and operating the window through Microsoft UI Automation from PowerShell
(`System.Windows.Automation`). **Narrator itself has not been run yet**: that pass needs a person at the keyboard and
is still the deciding check (see "Still to do").

| Control | UIA control type | Name / value | Patterns | Operating it |
| --- | --- | --- | --- | --- |
| Window | Window, then a Pane for the root | titled | none needed | n/a |
| Heading | Text | "Now playing spike" | none | n/a |
| Text input | **Edit**, keyboard focusable | "Search tracks" | `Value` only (no `Text`) | `SetFocus` works; `ValuePattern.SetValue` is accepted but changes nothing |
| Record + tonearm | **Image**, labelled as one image | "Record spinning, tonearm on the record" | none | n/a |
| Slider | **Slider**, keyboard focusable | "Volume", value 60, range 0..100, small/large change 5 | `RangeValue` | reading works; `SetValue(25)` is accepted but changes nothing (only `Increment`/`Decrement` a11y actions are wired, plus the arrow/Home/End keys) |
| List | **List** | "Tracks" | none (no `Scroll`, `Selection`, `ItemContainer`) | n/a |
| List rows | **ListItem**, keyboard focusable | "Track 3 - Artist 2" | `SelectionItem` | `Select()` and `SetFocus()` work; `Invoke` unsupported |

Observations:

* The tree is virtualised along with the list: only the ~11 visible rows exist as UIA nodes, so a screen reader
  cannot enumerate all 5,000 (position-in-set/size-of-set are set on the rows, but UIA surfaced none of it through
  the patterns above).
* The first UIA query right after launch sees an empty tree; it fills in on the next query (accessibility is
  switched on lazily when a client attaches, as expected).
* The unmodified GPUI text input is the weakest control: it exposes only a value. No `TextPattern` means no
  caret/selection/line reading, which is what screen readers use to announce arrow-key navigation inside a field.
  Typing itself goes through the normal Windows text-input path (`EntityInputHandler`).

## Still to do before this can be called a pass or a fail

1. (Skipped by the maintainer, so unverified.) Run Narrator (Win+Ctrl+Enter) against the window and walk each control: Tab/arrow through the list, type in the
   text field and move the caret with the arrow keys, adjust the slider with the arrow keys and with Narrator's own
   commands, and see whether the record's label is announced.
2. Decide whether the text-field gap (no `TextPattern`) and the list gaps (no `Scroll`/`Selection` patterns) are
   acceptable "parity with today" or release blockers. Under the plan's "no local patches" rule they can't be fixed in
   GPUI, only worked around in our own elements.
3. Subjective scroll feel (fling, momentum) by hand; the numbers above only show it is cheap.

## Measurements (release build, same machine)

| Measure | Result |
| --- | --- |
| Clean release build | 94 s (GPUI and dependencies compiled from scratch) |
| Executable size | 10.3 MB (spike only: no audio, no app logic) |
| Cold start to a window UI Automation can see | ~0.7 s |
| Idle CPU with the record spinning at full frame rate | ~2% of one core |
| Memory with the 5k-row list | 63 MB working set, 69 MB private; unchanged after scrolling |
| Scrolling | 400 synthetic wheel notches reached "Track 2753" using ~0.6 s CPU in total; the list stays responsive and memory stays flat |

These are spike numbers, not app numbers: audio decoding, the library store and real cover art will add to them.
Windows only; Android and web are not measured here.
