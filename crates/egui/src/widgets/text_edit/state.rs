use alloc::string::String;
use alloc::sync::Arc;

use epaint::text::cursor::CCursor;

use crate::mutex::Mutex;

use crate::{
    Context, IMEPurpose, Id, Vec2,
    output::{IMESurroundingText, ImeChangeCause},
    text_selection::{CCursorRange, TextCursorState},
};

pub type TextEditUndoer = crate::util::undoer::Undoer<(CCursorRange, String)>;

/// The text edit state stored between frames.
///
/// Attention: You also need to `store` the updated state.
/// ```
/// # egui::__run_test_ui(|ui| {
/// # let mut text = String::new();
/// use egui::text::{CCursor, CCursorRange};
///
/// let mut output = egui::TextEdit::singleline(&mut text).show(ui);
///
/// // Create a new selection range
/// let min = CCursor::new(0);
/// let max = CCursor::new(0);
/// let new_range = CCursorRange::two(min, max);
///
/// // Update the state
/// output.state.cursor.set_char_range(Some(new_range));
/// // Store the updated state
/// output.state.store(ui.ctx(), output.response.id);
/// # });
/// ```
#[derive(Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct TextEditState {
    /// Controls the text selection.
    pub cursor: TextCursorState,

    /// The purpose of the cursor.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) cursor_purpose: TextEditCursorPurpose,

    /// Wrapped in Arc for cheaper clones.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) undoer: Arc<Mutex<TextEditUndoer>>,

    // Text offset within the widget area.
    // Used for sensing and singleline text clipping.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) text_offset: Vec2,

    /// When did the user last press a key or click on the `TextEdit`.
    /// Used to pause the cursor animation when typing.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) last_interaction_time: f64,

    /// The selection last sent with [`crate::OutputCommand::TextSelectionSettled`].
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) reported_selection: Option<CCursorRange>,

    /// Identifies one focused IME context for this widget.
    ///
    /// [`Self::update_ime_context`] increments this when the widget gains focus or its IME
    /// purpose changes. Ordinary edits keep the same generation.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) ime_context_generation: u64,

    /// Whether this widget was the focused IME owner in the previous frame.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) ime_context_focused: bool,

    /// Last pass that actually showed this editor, including discarded passes.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) ime_last_seen_pass: Option<u64>,

    /// The IME purpose seen while focused, used to detect a purpose transition.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) ime_context_purpose: IMEPurpose,

    // Change-cause tracking intentionally mirrors the public `ImeContextTracker` contract,
    // but is stored in the widget's existing persisted state instead of a tracker instance.
    // A `TextEdit` is one widget among possibly many in a viewport, its state already spans
    // frames/passes, and only the focused widget may observe IME events. A per-widget copy of
    // the small frame/receipt state avoids a second generation source and keeps the existing
    // owner/generation/pre-edit lifecycle intact. Snapshots are bounded by
    // `IMESurroundingText::from_text`; no whole-document copy is made here.
    /// Last committed snapshot observed for this editor's focused IME context.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) ime_baseline: IMESurroundingText,

    /// Context generation that produced [`Self::ime_baseline`].
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) ime_baseline_generation: u64,

    /// Frame whose cause accumulation is stored in the fields below.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) ime_cause_frame_nr: Option<u64>,

    /// Whether an application-origin edit occurred anywhere in the current frame.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) ime_cause_origin_other: bool,

    /// Cause reported by the most recent observation in the current frame.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) ime_cause_last: Option<ImeChangeCause>,

    /// Compressed chain of accepted IME receipts for the current pass.
    ///
    /// Cleared after every observation; `origin_other` and `last_cause` survive all discarded
    /// passes of the frame. The chain keeps only its first `before` and latest `after`, so a
    /// batch of many commits cannot retain an unbounded number of bounded snapshots.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) ime_cause_receipt_chain: Option<ImeReceiptChain>,
}

impl TextEditState {
    pub fn load(ctx: &Context, id: Id) -> Option<Self> {
        ctx.data_mut(|d| d.get_persisted(id))
    }

    pub fn store(self, ctx: &Context, id: Id) {
        ctx.data_mut(|d| d.insert_persisted(id, self));
    }

    pub fn undoer(&self) -> TextEditUndoer {
        self.undoer.lock().clone()
    }

    #[expect(clippy::needless_pass_by_ref_mut)] // Intentionally hide interiority of mutability
    pub fn set_undoer(&mut self, undoer: TextEditUndoer) {
        *self.undoer.lock() = undoer;
    }

    pub fn clear_undoer(&mut self) {
        self.set_undoer(TextEditUndoer::default());
    }

    /// The IME composition under way: the character range of the pre-edit (marked) text in the
    /// buffer, and that text. `None` when nothing is composing.
    ///
    /// While composing, the buffer holds the pre-edit text as well as the committed text. A
    /// widget that wraps a [`crate::TextEdit`] removes this range to get the committed text, and
    /// shows the same buffer again in the next pass so the composition goes on.
    ///
    /// A composition belongs to one IME context: it ends when the editor runs without the focus,
    /// and when a new context starts (the editor gains the focus, comes back after a pass it was
    /// not shown in, or changes its IME purpose). Its pre-edit text then stays in the buffer as
    /// typed text.
    pub fn preedit(&self) -> Option<(core::ops::Range<usize>, &str)> {
        match &self.cursor_purpose {
            TextEditCursorPurpose::Selection => None,
            TextEditCursorPurpose::ImeComposition {
                preedit_range,
                preedit_text,
                ..
            } => Some((preedit_range.clone(), preedit_text.as_str())),
        }
    }

    /// Update the IME owner/focus epoch and return the current context generation.
    ///
    /// The generation is stable through ordinary text and selection updates. It changes when
    /// this widget acquires focus, reappears after a pass it was not shown in, or changes its IME
    /// purpose while focused. A new context ends the composition of the old one.
    pub(crate) fn update_ime_context(
        &mut self,
        focused: bool,
        gained_focus: bool,
        purpose: IMEPurpose,
        pass_nr: u64,
    ) -> u64 {
        if focused {
            // A hidden editor cannot update its per-widget focus flag. Consult actual focus
            // transitions too, and treat reappearance after an absent pass as a new context.
            let new_pass = self.ime_last_seen_pass != Some(pass_nr);
            let was_absent = self
                .ime_last_seen_pass
                .is_some_and(|last| last != pass_nr && last.checked_add(1) != Some(pass_nr));
            let context_changed = !self.ime_context_focused
                || self.ime_context_purpose != purpose
                || (new_pass && gained_focus)
                || was_absent;
            if context_changed {
                self.ime_context_generation =
                    crate::ime_context::next_context_generation().unwrap_or(0);
                if self.cursor_purpose.is_ime_composition() {
                    // The cursor spans the pre-edit text; it stays at its end, as typed text.
                    self.cursor_purpose = TextEditCursorPurpose::Selection;
                    let collapsed = self
                        .cursor
                        .char_range()
                        .map(|r| CCursorRange::one(r.primary));
                    self.cursor.set_char_range(collapsed);
                }
            }
            self.ime_context_purpose = purpose;
            self.ime_context_focused = true;
        } else {
            self.ime_context_focused = false;
        }
        self.ime_last_seen_pass = Some(pass_nr);
        self.ime_context_generation
    }

    /// Reset frame-scoped cause accumulation when a new frame starts.
    fn ensure_ime_cause_frame(&mut self, frame_nr: u64) {
        if self.ime_cause_frame_nr != Some(frame_nr) {
            self.ime_cause_frame_nr = Some(frame_nr);
            self.ime_cause_origin_other = false;
            self.ime_cause_last = None;
            self.ime_cause_receipt_chain = None;
        }
    }

    /// Record one accepted IME commit/delete-surrounding mutation.
    ///
    /// The receipt is accepted only when `before` differs from `after` and continues the
    /// frame's receipt chain from the last observed baseline. A mismatch marks the frame
    /// [`ImeChangeCause::Other`]. A no-op mutation is ignored without poisoning the frame.
    pub(crate) fn note_ime_input_method_apply(
        &mut self,
        frame_nr: u64,
        before: IMESurroundingText,
        after: IMESurroundingText,
    ) -> bool {
        self.ensure_ime_cause_frame(frame_nr);
        if before == after {
            return false;
        }

        let chain_is_valid = self
            .ime_cause_receipt_chain
            .as_ref()
            .map_or(before == self.ime_baseline, |chain| {
                chain.continues(&self.ime_baseline, &before)
            });

        if !chain_is_valid {
            self.ime_cause_origin_other = true;
            self.ime_cause_last = Some(ImeChangeCause::Other);
            self.ime_cause_receipt_chain = None;
            return false;
        }

        if let Some(chain) = &mut self.ime_cause_receipt_chain {
            chain.push(after);
        } else {
            self.ime_cause_receipt_chain = Some(ImeReceiptChain {
                first_before: before,
                last_after: after,
                count: 1,
            });
        }
        true
    }

    /// Record an application-origin mutation in this frame.
    ///
    /// This covers ordinary typing, paste, undo/redo, accessibility or toolbar edits, and a
    /// document mutation performed between frames. Once set, `Other` dominates every later
    /// change in the same frame, including discarded layout passes.
    pub(crate) fn note_ime_application_change(&mut self, frame_nr: u64) {
        self.ensure_ime_cause_frame(frame_nr);
        self.ime_cause_origin_other = true;
        self.ime_cause_last = Some(ImeChangeCause::Other);
        self.ime_cause_receipt_chain = None;
    }

    /// Observe the committed snapshot once at the end of a focused pass.
    ///
    /// The returned cause is [`ImeChangeCause::Other`] for context transitions and for any
    /// change not proven by an exact chain of accepted IME receipts. An unchanged pass keeps
    /// the frame's first cause; a changed pass always builds a new descriptor.
    pub(crate) fn observe_ime_committed(
        &mut self,
        frame_nr: u64,
        generation: u64,
        snapshot: &IMESurroundingText,
    ) -> ImeChangeCause {
        self.ensure_ime_cause_frame(frame_nr);

        if self.ime_baseline_generation != generation {
            // Focus/purpose/absence transition: no receipt from a previous context can apply.
            self.ime_baseline_generation = generation;
            self.ime_baseline = snapshot.clone();
            self.ime_cause_origin_other = true;
            self.ime_cause_last = Some(ImeChangeCause::Other);
            self.ime_cause_receipt_chain = None;
            return ImeChangeCause::Other;
        }

        let previous = core::mem::replace(&mut self.ime_baseline, snapshot.clone());
        let changed = previous != *snapshot;
        let cause = if changed {
            let receipts_are_exact = self
                .ime_cause_receipt_chain
                .as_ref()
                .is_some_and(|chain| chain.is_exact(&previous, snapshot));
            let cause = if self.ime_cause_origin_other {
                ImeChangeCause::Other
            } else if receipts_are_exact {
                ImeChangeCause::InputMethod
            } else {
                ImeChangeCause::Other
            };
            if cause == ImeChangeCause::Other {
                self.ime_cause_origin_other = true;
            }
            self.ime_cause_last = Some(cause);
            cause
        } else {
            self.ime_cause_last.unwrap_or(ImeChangeCause::Other)
        };

        // Receipts belong to one pass. Keep `origin_other` and `last_cause` for the rest of
        // this frame's discarded passes.
        self.ime_cause_receipt_chain = None;
        cause
    }
}

/// Compressed chain of accepted IME receipts for one pass.
///
/// Every intermediate link was already validated when it was accepted, so only the first
/// `before` (to re-check the observed baseline) and the latest `after` are needed. `count` is
/// diagnostic/test state and does not grow the retained snapshot storage.
#[derive(Clone, Debug)]
pub(crate) struct ImeReceiptChain {
    first_before: IMESurroundingText,
    last_after: IMESurroundingText,
    count: usize,
}

impl ImeReceiptChain {
    fn continues(&self, baseline: &IMESurroundingText, before: &IMESurroundingText) -> bool {
        self.first_before == *baseline && self.last_after == *before
    }

    fn is_exact(&self, baseline: &IMESurroundingText, current: &IMESurroundingText) -> bool {
        self.count > 0 && self.first_before == *baseline && self.last_after == *current
    }

    fn push(&mut self, after: IMESurroundingText) {
        self.last_after = after;
        self.count = self.count.saturating_add(1);
    }
}

#[derive(Clone, Default)]
pub(crate) enum TextEditCursorPurpose {
    /// The cursor is used for text selection.
    #[default]
    Selection,

    /// The cursor is used for IME composition. Its direction is irrelevant in
    /// this case.
    ImeComposition {
        /// An optional cursor/segment within the composing text itself,
        /// relative to the start of the composing region. Its direction is
        /// irrelevant.
        ///
        /// When `None`, no active range is displayed.
        active_range: Option<core::ops::Range<CCursor>>,

        /// The character range of the pre-edit text in the current buffer.
        ///
        /// This is kept separately because the visible cursor can move inside the composition.
        /// Surrounding text must exclude the whole pre-edit without relying on the current
        /// cursor selection.
        preedit_range: core::ops::Range<usize>,

        /// The exact pre-edit text last written at [`Self::preedit_range`].
        ///
        /// Used to detect an out-of-band `TextBuffer` mutation between passes. If the buffer no
        /// longer contains this text at the recorded range, the composition is stale and must be
        /// reset before the next surrounding-text payload is built.
        preedit_text: String,
    },
}

impl TextEditCursorPurpose {
    pub(crate) fn is_selection(&self) -> bool {
        matches!(self, Self::Selection)
    }

    pub(crate) fn is_ime_composition(&self) -> bool {
        matches!(self, Self::ImeComposition { .. })
    }

    /// Does the buffer still contain the recorded pre-edit at its recorded character range?
    ///
    /// A `Selection` purpose has no composition to validate. Invalid ranges or different text
    /// mean the application changed the `TextBuffer` out of band; the next payload must not use
    /// the stale range to remove committed bytes.
    pub(crate) fn preedit_matches(&self, text: &str) -> bool {
        match self {
            Self::Selection => true,
            Self::ImeComposition {
                preedit_range,
                preedit_text,
                ..
            } => slice_char_range(text, preedit_range) == Some(preedit_text.as_str()),
        }
    }

    /// Refresh the remembered pre-edit text from the current buffer.
    pub(crate) fn refresh_preedit_text(&mut self, text: &str) {
        if let Self::ImeComposition {
            preedit_range,
            preedit_text,
            ..
        } = self
        {
            *preedit_text = slice_char_range(text, preedit_range)
                .unwrap_or_default()
                .to_owned();
        }
    }
}

/// Slice `text` by a character range without panicking on an out-of-bounds or reversed range.
fn slice_char_range<'a>(text: &'a str, range: &core::ops::Range<usize>) -> Option<&'a str> {
    if range.start > range.end {
        return None;
    }
    let start = byte_index_from_char_index(text, range.start)?;
    let end = byte_index_from_char_index(text, range.end)?;
    text.get(start..end)
}

/// Return the UTF-8 byte offset of a character index, including the end-of-string position.
fn byte_index_from_char_index(text: &str, char_index: usize) -> Option<usize> {
    if char_index == 0 {
        return Some(0);
    }
    text.char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
        .or_else(|| (text.chars().count() == char_index).then_some(text.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preedit_reports_the_marked_text_and_its_range_until_the_commit() {
        use crate::{Event, ImeEvent, RawInput, TextEdit};

        let ctx = Context::default();
        let id = Id::unique("preedit-field");
        let mut text = String::from("ab");
        let mut frame = |events: Vec<Event>| {
            let mut state = None;
            let mut output = ctx.run_ui(
                RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    ui.memory_mut(|mem| mem.request_focus(id));
                    let output = TextEdit::singleline(&mut text)
                        .id(id)
                        .cursor_at_end(true)
                        .show(ui);
                    state = Some(output.state);
                },
            );
            output.textures_delta.clear();
            state.expect("the field was shown")
        };
        let preedit = |text: &str| {
            Event::Ime(ImeEvent::Preedit {
                text: text.to_owned(),
                active_range_chars: None,
            })
        };

        assert_eq!(frame(vec![]).preedit(), None);
        assert_eq!(frame(vec![preedit("に")]).preedit(), Some((2..3, "に")));
        assert_eq!(frame(vec![preedit("にほ")]).preedit(), Some((2..4, "にほ")));
        let loaded = TextEditState::load(&ctx, id).expect("stored");
        assert_eq!(loaded.preedit(), Some((2..4, "にほ")));
        let committed = frame(vec![Event::Ime(ImeEvent::Commit("日本".to_owned()))]);
        assert_eq!(committed.preedit(), None);
        assert_eq!(text, "ab日本");
    }

    #[test]
    fn a_composition_ends_when_its_editor_comes_back_after_a_pass_without_it() {
        use crate::{Event, ImeEvent, Key, Modifiers, RawInput, TextEdit};

        let ctx = Context::default();
        let id = Id::unique("returning-field");
        let mut text = String::from("ab");
        let mut frame = |shown: bool, events: Vec<Event>| {
            let mut state = None;
            let mut output = ctx.run_ui(
                RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    if shown {
                        ui.memory_mut(|mem| mem.request_focus(id));
                        let output = TextEdit::singleline(&mut text)
                            .id(id)
                            .cursor_at_end(true)
                            .show(ui);
                        state = Some(output.state);
                    }
                },
            );
            output.textures_delta.clear();
            (state, text.clone())
        };
        let preedit = Event::Ime(ImeEvent::Preedit {
            text: "にほ".to_owned(),
            active_range_chars: None,
        });
        let backspace = Event::Key {
            key: Key::Backspace,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };

        frame(true, vec![]);
        let (composing, _) = frame(true, vec![preedit]);
        assert_eq!(composing.expect("shown").preedit(), Some((2..4, "にほ")));
        frame(false, vec![]);
        let (back, text) = frame(true, vec![]);
        assert_eq!(back.expect("shown").preedit(), None);
        assert_eq!(text, "abにほ");
        assert_eq!(frame(true, vec![backspace]).1, "abに");
    }

    #[test]
    fn many_ime_receipts_are_compressed_to_one_bounded_chain() {
        const RECEIPTS: usize = 2_000;

        let mut state = TextEditState {
            ime_baseline: IMESurroundingText::from_text("", 0, 0, None),
            ime_baseline_generation: 7,
            ..Default::default()
        };

        let mut text = String::new();
        for _ in 0..RECEIPTS {
            let before = IMESurroundingText::from_text(&text, text.len(), text.len(), None);
            text.push('x');
            let after = IMESurroundingText::from_text(&text, text.len(), text.len(), None);
            assert!(state.note_ime_input_method_apply(3, before, after));
        }

        let chain = state
            .ime_cause_receipt_chain
            .as_ref()
            .expect("accepted receipts keep one compressed chain");
        assert_eq!(chain.count, RECEIPTS);
        assert_eq!(
            chain.last_after,
            IMESurroundingText::from_text(&text, text.len(), text.len(), None)
        );

        let current = IMESurroundingText::from_text(&text, text.len(), text.len(), None);
        assert_eq!(
            state.observe_ime_committed(3, 7, &current),
            ImeChangeCause::InputMethod
        );
        assert!(state.ime_cause_receipt_chain.is_none());
    }

    #[test]
    fn preedit_text_detects_out_of_band_buffer_mutation() {
        let mut purpose = TextEditCursorPurpose::ImeComposition {
            active_range: None,
            preedit_range: 2..4,
            preedit_text: "にほ".to_owned(),
        };
        assert!(purpose.preedit_matches("abにほ"));
        assert!(!purpose.preedit_matches("00abに"));

        // A valid IME edit at the same character range refreshes the remembered text.
        purpose.refresh_preedit_text("xyにほz");
        assert!(purpose.preedit_matches("xyにほz"));
    }
}
