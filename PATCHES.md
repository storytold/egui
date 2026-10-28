# Craft suite patches on egui 0.37.0 (`craft/0.37`)

Base: upstream tag `0.37.0` (`841a328b`). Each patch is a `craft/<topic>` branch off `0.37.0` (or off the
topic named in its row), merged here in table order. The workspace patches `winit` to aiongg/winit
`craft/0.30.13-lang` (storytold/winit `craft/0.30.13` plus W17–W27). Apps patch every egui-workspace crate they use to this checkout, so only one egui resolves.
Drop a patch when an egui release in use contains it.

| # | Commit | Crate | Change | Upstream | Source in the suite |
|---|---|---|---|---|---|
| E1 | `8f3abf06` | eframe | `NativeOptions::viewport_hooks` (`ViewportHooks`): each viewport's native window on create/remove, and every viewport's raw input before egui (immediate viewports included); honoured by `run_native` and `create_native` | not proposed upstream yet | craft-tablet (native Wayland pen input) |
| E2+E3 | `9dd799ab` | egui-wgpu | fit a surface within `max_texture_dimension_2d`; first configure and renderer creation inside wgpu error scopes, returned as `WgpuError::ConfigureSurface` / `CreateRenderer` | not proposed; the surface fit would close emilk/egui#8361 | pdfcraft#609 (#577), #617 (#519), #1042; gridcraft#761 |
| E4 | `66bacf23` | egui-winit | Wayland: copy and paste through winit's clipboard (`winit::platform::wayland::Clipboard`, W15), so smithay-clipboard doesn't bind a second data device; PRIMARY goes through arboard meanwhile | emilk/egui#8657 (draft) | photocraft#7828, filmcraft#1115, pdfcraft#3254 (Hyprland); storytold/egui `craft/0.36.2` `c01acc2`; needs winit W15 |
| E5 | `0e27d87e`, `777977b7`, `f7e89e23`, `5b2b8e19` | epaint, egui | bidi text: right-to-left runs shaped, rows in visual order, marks, end caret and selection; word-motion underflow fix; tests build without `default_fonts` | emilk/egui#8577 (open) ported, with #8698 and review fixes | language support (craft-libs#198) |
| E6 | `0abfabcf` | egui | `request_focus` on the focused widget no longer interrupts IME composition | not proposed | IME input |
| E7 | `0670c58a` | egui-winit | Windows: letter keys from the virtual-key code on layouts without A–Z (Russian, Arabic, Hebrew) | not proposed | shortcuts on non-Latin layouts |
| E8 | `1b03c10f` | egui | `TextEdit` no longer clips glyphs above or below the row (Thai, Vietnamese, Arabic marks) | not proposed | language support |
| E9 | `b2f5cafc`, `e1341a3d`, `5105330d`, `75a5b3d3` | egui | `Style::right_to_left` mirrors menus, scroll bars, headers, title bars, combo boxes, checkboxes, popups, resize grips | not proposed | RTL UI (craft-libs#198) |
| E10 | `dc90c13e`, `c8e573f7`, `34b2c0d4` | eframe (web) | named keys not typed as text; letter keys from `KeyboardEvent.code` on non-Latin layouts; `lang` on the hidden input and speech | not proposed | web builds |
| E11 | `92e61afd` | epaint | glyphs rasterized on a single-threaded vello_cpu context, so its `multithreading` feature no longer panics | emilk/egui#8641 (open draft, flush only) | servo and VectorCraft enable `multithreading` |
| E12 | `e955bb18`, `485c0b89`, `0185887c`, `7015ac23`, `4067b2c4`, `620e070e` | epaint, egui | `LayoutJob::direction`; RTL paragraphs right-aligned; `…` at the visual end; UAX #9 rule L1; linear-time reordering in which isolate initiators, PDI and marks like RLM keep their own levels (on E5) | not proposed; follow-up to #8577 | language support |
| E13 | `5e4da7f4` | epaint, egui | caret, hit-test, Delete and Backspace by grapheme cluster (on E5) | not proposed | language support |
| E14 | `cbd77549`, `b3722ac0` | epaint, egui | `FontTweak::fallback_align`: fallback fonts on the primary baseline, rows grow to fit; default unchanged | not proposed | CJK and Indic fallback fonts |
| E15 | `f1cae609` | epaint, egui | `LineBreaker` trait (`SimpleLineBreaker` keeps the old rules), grapheme-safe emergency breaks, `LayoutJob::language` | not proposed | craft-text line breaking |
| E16 | `b3ef9771`, `bed6dfe3` | epaint, egui | visual cursor movement with affinity, Home/End in reading order, password mask per cluster, neutral paragraphs inherit direction | not proposed | language support |
| E17 | `520435df` | egui | `Context::set_language`, `UiBuilder::language`, `Response::accessible_language`; AccessKit root language; one text run per bidi level | not proposed | screen readers (craft-libs#198) |
| E18 | `89de8f3c` | eframe | `Frame::set_ui_language` sets `Context::set_language`; web falls back to `Context::language` | fork-only | craft-i18n UI language |
| E19 | `d64047bf`, `642b74f7`, `baeb4515` | egui-winit, egui | one letter-key rule for Windows, macOS and Linux (first Latin group); `RawInput::keyboard_layout_is_latin`; `RawInput::latin_layout_chars` (each key's Latin layout character, so a key without a Latin letter is no letter shortcut) | fork-only until winit has the APIs | shortcuts on non-Latin layouts; needs winit `craft/0.30.13-lang` |
| E20 | `23955535` | egui | widget text laid out in the language of its `Ui` | fork-only (goes with E15 and E17) | language support |
| E21 | `a70470b0` | egui-winit, egui | `ImeEvent::DeleteSurroundingBytes`: `TextEdit` deletes the exact bytes an input method replaces, in the commit's undo step | not proposed | macOS press-and-hold and dictation, Wayland input methods; needs winit `craft/0.30.13-lang` |
| E22 | on `craft/interact-enabled` | egui | `TextEditState::preedit`: the IME composition's pre-edit range and text, so a widget wrapping `TextEdit` can tell marked text from committed text | not proposed | craft-ui text fields (`behavior::Typing`) |

E1 changes from `e7e37ad`: the hooks are installed in `init_native`, so `create_native` uses them too, and a
unit test covers installing and replacing them. E2+E3 is the same change as `b02321d` with a `surface_fit`
test. E4 is storytold/egui `c01acc2` on 0.37's `unsafe` `Clipboard::new`; it is merged last because it
needs the `craft/0.30.13-lang` pin. The local E4/P2b follow-up below replaces E4's temporary
PRIMARY gap once the reviewed winit W15P API is pinned.

## Local migration follow-ups (not yet committed)

### Local E4/P2b: bounded asynchronous Wayland clipboard and PRIMARY bridge

Historical text-only boundary: the PNG integration status below supersedes its temporary
image refusal and pending-test statements.

`egui-winit` now consumes winit's reviewed W15/W15P transfer API for the Wayland event path
instead of calling the bounded-but-blocking `load_text`/`load_primary_text` compatibility
wrappers. A paste shortcut or middle-click starts `begin_load_text`/`begin_load_primary_text`
with a caller-owned `TransferHandle`; the transfer deadline and byte cap stay in winit, and the
worker only requests a repaint of the requesting viewport through `Context::request_repaint_of`.
The completion wake is not a generic `WindowEvent` broadcast and workers never inspect egui
state.

Each per-window `State` keeps the handle with its exact `ViewportId`, `WindowId`, focus owner
and input generation. The next `take_egui_input` drains a completed handle once, revalidates it
and emits `Event::Paste` or `Event::MiddleClickPaste`. A completion is dropped when the target
window was replaced or closed, the input generation moved on, a regular paste's focused widget
changed, or a middle-click's captured position resolves to a different egui layer (for example a
modal or popup). Another key press/edit, pointer press, focus loss, close or hide cancels the
handle. A wheel scroll cancels only a pending PRIMARY read. Key/pointer releases and repaints do
not cancel. A focus gain does not cancel a PRIMARY read from the same generation: a click-to-focus
compositor may dispatch the middle-click press before `wl_keyboard.enter`, so that `Focused(true)`
is part of the click. If winit rejects the PRIMARY read because the active seat has not focused
the window yet, the bridge keeps one deferred request and retries it when `Focused(true)` arrives.
A middle-click paste tolerates at most that one focus move; later focus churn is stale. This
preserves the old synchronous path's independence from keyboard focus without allowing a modal
opened later to receive the paste.

`PRIMARY` ownership claims and regular clipboard copy claims stay synchronous. `State` captures
winit's `InputOriginToken` while handling the triggering key press or pointer press/release and
consumes it for the next matching claim in the same generation, rather than reading the latest
serial at platform-output time. Programmatic output without a captured token falls back to winit's
latest fresh input origin; that provenance is approximate and documented here. A rejected claim
is logged at `warn` (instead of `debug`), counted by `State::clipboard_failure_count()`, leaves the
previous selection in place and is not retried. Winit permits one claim per selection family per
input turn, so a second same-pass `CopyText`/selection claim fails visibly under that contract.
`ViewportCommand::RequestPaste`, `RequestCopy` and `RequestCut` are routed through
`State::request_paste`, `request_copy` and `request_cut`; eframe no longer calls the blocking
`clipboard_text()`/`clipboard_image()` compatibility methods for those actions.

A present zero-byte regular or PRIMARY text stream is a successful empty value, not an absent
offer. Wayland image fallback is deliberately disabled on the event path until winit can carry an
image offer through a validated bounded transfer: there is no synchronous arboard read and no
arbitrary first-seat fallback there. An absent or non-text regular offer (including `InvalidData`,
which winit also uses for a stale winit self-source) and an image-only clipboard therefore become
a logged, counted no-op instead of a blocking image paste. Non-Wayland synchronous paths keep
their existing image fallback, and `get_image`/`set_image` remain arboard-only compatibility APIs
outside the event path. File-drop behavior is unchanged. A winit image-transfer API (or a
bounded image probe) and a distinct "non-text offer"/"stale self-source" error kind remain the
proper upstream follow-up; this fork does not modify winit. The focus owner used for revalidation
is egui's `Context`-global `Memory::focused`, so multiple viewports share that discriminator; the
per-state viewport, target window and input generation still prevent cross-window delivery.

Headless tests in `clipboard_bridge.rs` exercise delayed regular/PRIMARY completion and wake,
two independent targets, edit-generation/focus staleness, close and target replacement,
release/repaint non-cancellation, present-empty versus absent versus interrupted results,
synchronous once-per-family claims, exactly-once delivery, focus-gain survival, the single
middle-click focus-move tolerance, wheel/layer invalidation, deferred PRIMARY retry and captured
claim-token provenance. The production adapter is compiled only when winit provides the reviewed
W15P bounded API. These tests have not been run after this repair, and no compositor or native
Wayland evidence is claimed here: Hyprland, KWin and Mutter still need their matrix runs after the
winit and egui integration pins exist.

- Selection actions (#8763): response inspection no longer surrenders text focus. The
  click-away policy is applied when the widget is registered, inside its existing context
  lock. With `SurrenderFocusOn::Clicks`, same-pass toolbar actions can read the prior
  selection in either widget order. `Presses` still ends focus on press; an action delayed
  until release must capture its selection at press. Delayed context-menu actions own
  the selection captured when opened. Existing keyboard Copy and right-click caret
  placement are preserved. This does not extend unfocused selection lifetime.
  `crates/egui/tests/text_edit_selection_actions.rs` exercises real pointer and keyboard
  events: the three original regressions fail before the patch while both behavior controls
  pass. Eight tests now also cover restoring focus after toolbar actions, capture on press,
  and a discarded pass. The integrated egui suite passed 148 tests with clippy clean
  before the subsequent ScrollArea expansion; completion-popup tests passed all ten cases.

### Local P7: pair shortcut releases with delivered presses

`egui-winit` now remembers the actual egui key emitted for each physical/native key. Repeats retain that identity across layout or Command changes; release clears it only after the final physical alias releases. Clipboard interception retires a prior ordinary press and records that no replacement Key-down was sent. Windows IME interception retires the exact prior identity. Focus loss and viewport destruction clear tracking. Fully unidentified keys cannot be paired reliably after layout changes, so an unidentified release conservatively clears only that ambiguous subset; identified native scan codes retain normal pairing. Text and modifier reporting still uses the current OS event.

Fresh adversarial review identified clipboard-marker upgrades and defensive Process-release handling. Both are repaired. Fifteen X11-feature tests pass in `held-review-04`, including real State press/release helpers feeding egui key-down and press-count state; clippy passes in `held-lints-05`. The pinned Windows backend filters Process presses before this layer, so that branch is defensive compatibility. Default-feature/Wayland and native Windows/macOS delivery remain unqualified.

Selection follow-up review: delayed context-menu actions must capture the range when the menu opens; egui does not preserve an unfocused TextEdit range. Immediate `request_focus` before the field is shown can still be undone by its click-away policy. Toolbar actions that restore editing must apply their focus request after all relevant widgets have been shown (for example, collect the action and restore focus at the end of the UI closure). `request_focus_next_frame` is private to egui. The pointer-event condition now short-circuits before the per-widget focus lookup on idle passes. Completion-popup tests pass all ten cases. Five alternating baseline/patched idle-pass pairs completed with identical mesh hashes, but the timing coefficient of variation (5.45%/11.24%) exceeded the 5% noise gate. Performance qualification remains open; no speedup or equivalence is claimed.

### Local ScrollArea explicit-offset repair (qualification in progress)

A document zoom can request an offset beyond the previous frame's scroll bounds while also growing the content. The 0.37 beginning-of-pass kinetic step applied those old bounds to that new position: the content was painted at the requested offset, but the saved state sprung backward, moving it on the following frame. A programmatic offset also inherited an earlier gesture's velocity/animated target.

Explicit offsets now clear the previous velocity and target on the specified axes and skip the pre-layout physics step on those axes. End-of-pass clamping uses current content bounds. Other axes retain momentum. The managed `scroll-control-01` run demonstrated two failures before the repair (1800→1787.6 and 800→816.4). `scroll-fixed-01` passed the original seven tests, and WordCraft's pointer-zoom test passed in `targeted-08`. Fresh review found sticky-end and drag-release interactions; the expanded implementation clears those prior states per requested axis while preserving same-frame wheel and scroll-to actions. Sixteen ScrollArea tests and clippy pass in `scroll-review-03`. Tests drive actual drag press/move/release frames, priming the hit-test area before the press. The existing one-frame paint/state mismatch for out-of-range requests remains documented and is not claimed fixed.

### Local P7b: outgoing IME surrounding text and viewport-command ownership

`IMEOutput` now carries an IME-owning editor identity (`owner`), a per-focus
`context_generation`, the `IMEPurpose`, a bounded available/unavailable surrounding-text
payload and an `ImeChangeCause`; password contexts never publish text. `TextEdit` emits logical
committed text with UTF-8 byte cursor/anchor positions and excludes its pre-edit. `egui-winit`
delivers fresh text on Wayland through winit `text-input-v3`, using one dedicated clear for an
owner/generation/purpose boundary and preserving the composition for ordinary same-owner edits.
The bridge keeps its cache until a real platform transition or context clear; redundant external
`IMEAllowed`/unchanged `IMEPurpose` commands do not erase knowledge needed to plan
`ClearThenSet`. Focus loss and composition interruption reset the bridge cache;
`WindowEvent::Destroyed` resets that cache where present and the per-window IME
cursor/purpose/enable latches on every platform.

Immediate viewports also return their own viewport commands with the child `FullOutput`, so an
eframe immediate renderer can pre-apply `IMEAllowed` before that child publishes surrounding
text. Parent and sibling command queues are not consumed by the child.

This patch changes the public `egui_winit::process_viewport_commands` signature from the
upstream window-only form to:
`process_viewport_commands(ctx, &mut info, commands, window, &mut state, &mut actions_requested)`.
The extra `&mut State` lets `IMEAllowed` update the owning window's gate and bridge synchronously;
integrations must call it only after both the window and its `State` exist.

The workspace pins the reviewed canonical `storytold/winit` revision
`ae48c756a3b81125eb796c0cc6dccf60990d1181`. The final lock records that same immutable Git source;
the canonical lock audit found no other package drift from the qualified resolution.


### Local IME unavailable-receipt stamps (helper layer)

`ime_context::ImeMutationStamp` now carries a private optional revision on unavailable stamps.
`unavailable_at_revision`, `unavailable_revision` and the existing `unavailable` expose the
producer contract; `from_committed` and `from_surrounding_text` never attach a revision.
Available stamps still compare exact bounded text plus selection, so their identity is unchanged.
An unavailable -> unavailable receipt needs both revisions with `before < after`; a mixed
available/unavailable transition needs a revision on its unavailable side. Missing, equal,
decreasing or one-sided revisions poison the frame and clear an earlier receipt chain.
`ImeObservation` gains a private field and `unavailable_revision(rev)` builder. Available and
password observations discard the revision; password text remains unavailable and redacted.
`ContextState::baseline` and the compressed receipt chain retain stamps rather than bare
snapshots, so a revision-only unavailable change is a changed observation without a receipt.

This is the shared helper layer only. The engine-owned transaction/outcome and the WordCraft
producer adapters are separate work; no application source is changed here. Managed verification
is pending; no test result is claimed by this entry.

### Texture-delta destructor during panic unwinding

DeckCraft's strict missing-glyph regression exposed a second panic in `TexturesDelta::drop`,
which aborted the test process while the original UI failure was unwinding. The local repair
keeps the debug assertion for ordinary unhandled deltas, but suppresses that second assertion
only while the thread is already panicking. It does not count deltas as rendered or weaken the
normal ownership rule. Regression tests must preserve the original panic payload and still
reject an ordinary unhandled drop. Debug/release tests and a fresh adversarial review are required.

### Local bidi cursor affinity repair (qualification in progress)

`Galley::clamp_cursor` retains the affinity of a valid character cursor. Converting through
`LayoutCursor` otherwise loses which side of a mixed-direction boundary the cursor occupies,
so TextEdit can jump or become stuck on the next arrow-key frame. Out-of-range indices retain
the prior clamping behavior. Regressions cover both affinities, wrapped rows, out-of-range
indices, visual motion with intervening clamps, and persisted TextEdit state across idle frames.
FigCraft also exercises visual rename, selection and insertion with actual Hebrew font input.
These new checks and fresh review are pending.


### Local E4/P2b: integrated PNG clipboard status — 2026-10-24

The event path now supports bounded regular PNG copy/paste through the winit-owned Wayland
transport. `clipboard-image` implies `clipboard`; the PNG-only codec uses bounded global
encode/decode workers and memory admission. A merged PlatformOutput selects the last regular
text/image command independently of PRIMARY. Claim origins belong to the prepared input batch;
stale captures fail explicitly. A newer regular winner cancels older image work before its own
admission, including failure. Accessibility copy/cut/paste enters the same State request path.

Before this documentation update, the implementation source and required untracked modules
matched the staging snapshot qualified by `clipboard-image-smoke-20261024-025844-315945`.
The bridge/codec suite passed 124 tests and strict library/test clippy in `png-probe-build-19`;
that job then failed its native-example build for missing `winit/rwh_06`. `feature-lints-26`
passed strict native-example clippy with that feature and reduced-feature checks (the latter
retain dead-code warnings). `native-final-27` built that probe and passed 25 private-Sway checks
through real State input/output, exact external/self PNG/text, malformed/absent negatives, mixed
last-wins output, worker wakes and an unchanged external PRIMARY sentinel. The probe maps and
presents a softbuffer window; it is not a full Craft application or multi-seat acceptance test.

Other compositor/platform, slow-peer/focus-transfer, full workspace and accepted performance
gates remain open where not independently recorded. The source includes all earlier text,
selection, IME, scroll and tessellation repairs; their existing evidence is preserved separately.
The canonical immutable winit pin and lock are now aligned. Managed `canonical-lock33` and
`audit34` passed lock generation and locked/offline metadata validation; the only dependency
identity change was the qualified local winit source becoming its exact canonical Git revision.
Consumer egui-family pins and their canonical locks remain a separate landing step.
