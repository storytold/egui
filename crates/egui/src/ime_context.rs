//! Shared IME context tracking for custom text editors.
//!
//! Stock [`crate::TextEdit`] keeps its own focus state. Custom editors (canvas stories, type
//! tools, equation editors, ...) need the same owner/generation bookkeeping plus an explicit
//! way to say whether a committed-text change came from an accepted IME mutation or from the
//! application. [`ImeContextTracker`] provides both.
//!
//! The contract is deliberately narrow:
//!
//! * State is scoped by [`ViewportId`]. One tracker may serve several viewports; pass
//!   `ctx.viewport_id()` and tear a viewport down with [`ImeContextTracker::end_viewport`].
//! * [`ImeObservation::frame_nr`] is [`crate::Context::cumulative_frame_nr_for`] and
//!   [`ImeObservation::pass_nr`] is [`crate::Context::cumulative_pass_nr_for`]. Do not pass a
//!   pass index within a frame, and do not confuse the two counters.
//! * A context generation changes only when the owner, replacement epoch, focus or IME
//!   purpose changes, or when the editor was absent for a pass. Ordinary text, selection,
//!   caret, pre-edit and geometry changes keep it stable.
//! * The observed committed snapshot is compared as a bounded
//!   [`crate::output::IMESurroundingText`] value. No general document revision is used, and the
//!   tracker never copies a whole document.
//! * An unavailable observation may also carry one private, producer-supplied revision. It
//!   orders two unavailable stamps for one receipt transition; it is not proof of an edit.
//!   Available stamps still compare exact text plus selection, and password observations
//!   discard the revision.
//! * An IME mutation contributes [`ImeChangeCause::InputMethod`] only through an explicit
//!   receipt captured around a successful mutation. Application changes, failed/no-op
//!   receipts, context transitions and mixed edits report [`ImeChangeCause::Other`].
//! * Within one frame, `Other` dominates every discarded pass: once any change in the frame
//!   is application-origin, later changes in that same frame stay `Other`. An unchanged later
//!   pass retains the first pass's cause. A changed observation always builds a fresh
//!   descriptor; only the generation and cause are retained.
//! * [`crate::IMEPurpose::Password`] always produces
//!   [`crate::output::IMESurroundingText::Unavailable`], even if committed text is supplied.
//!   Text in retained snapshots is redacted from `Debug`.
//!
//! # Example
//!
//! ```
//! use egui::{Context, IMEPurpose, Id, RawInput};
//! use egui::ime_context::{CommittedText, ImeContextTracker, ImeObservation, ImeTarget};
//!
//! let ctx = Context::default();
//! let mut tracker = ImeContextTracker::new();
//! let target = ImeTarget::new(Id::unique("document-body"), 0);
//! let mut text = String::from("hello");
//! let mut update = None;
//!
//! let output = ctx.run_ui(RawInput::default(), |ui| {
//!     // The custom editor updates `text` and applies IME commands before this point.
//!     let viewport = ui.ctx().viewport_id();
//!     let observation = ImeObservation::new(
//!         ui.ctx().cumulative_frame_nr_for(viewport),
//!         ui.ctx().cumulative_pass_nr_for(viewport),
//!         target,
//!     )
//!     .purpose(IMEPurpose::Normal)
//!     .committed(Some(CommittedText::new(&text, text.len(), text.len())));
//!
//!     update = tracker.observe(viewport, Some(observation));
//! });
//! output.drop_without_applying_deltas();
//!
//! let update = update.expect("the editor is focused");
//! assert_eq!(update.owner, target.owner);
//! assert_eq!(update.surrounding_text.available().unwrap().text, "hello");
//! ```

use core::ops::Range;

use crate::output::{IMESurroundingText, ImeChangeCause};
use crate::{IMEPurpose, Id, ViewportId, ViewportIdMap};

/// An opaque request for one viewport's native composition cancellation fence.
///
/// Created by [`crate::Context::request_ime_reset`] at the actual application interruption.
/// A matching [`crate::Event::ImeResetCompleted`] is only an ordered admission boundary; it
/// never edits a buffer or proves that an arbitrary later IME callback is semantically fresh.
/// The application must bind its pending request to its actual target and caret and supersede
/// it after another interruption, even when owner and context generation stay unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct ImeResetId {
    viewport: ViewportId,
    serial: u64,
}

impl ImeResetId {
    pub(crate) fn allocate(viewport: ViewportId) -> Option<Self> {
        use core::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        // Process-unique even if a Context/Memory/State is replaced while a callback is queued.
        // Exhaustion remains conservative instead of wrapping into a previously accepted token.
        let serial = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .ok()?;
        Some(Self { viewport, serial })
    }

    /// The viewport that requested this fence.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport
    }

    /// Integration token passed unchanged to the native request API.
    ///
    /// A host must retain this complete id and emit it only for the actual matching completion
    /// from that same viewport/window. Calling the native API is not completion.
    pub const fn as_u64(self) -> u64 {
        self.serial
    }
}

/// The immutable identity of one editable IME target.
///
/// [`Self::owner`] must not contain the caret, selection, text, revision, rectangle, visual
/// order or a pointer address. [`Self::replacement_epoch`] distinguishes a recreated target
/// that reuses the same owner key (document replacement, a new edit session, and so on).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImeTarget {
    /// Stable owner identity, usually an [`Id`] built from document and edit-target ids.
    pub owner: Id,

    /// Changes when a target is replaced while the owner key stays equal.
    ///
    /// Zero is valid when the owner key itself changes on every replacement.
    pub replacement_epoch: u64,
}

impl ImeTarget {
    /// Create a target from a stable owner and a replacement epoch.
    pub const fn new(owner: Id, replacement_epoch: u64) -> Self {
        Self {
            owner,
            replacement_epoch,
        }
    }
}

/// A bounded committed-text snapshot captured before or after an accepted IME mutation.
///
/// Equality compares the bounded text window and both UTF-8 byte offsets. It does not use a
/// general document revision, so pre-edit, style, selection and rendering changes that do not
/// alter the committed snapshot do not poison a receipt.
///
/// An [`IMESurroundingText::Unavailable`] stamp may additionally carry an opaque,
/// producer-supplied revision. Unavailable stamps compare equal only when their revisions are
/// both absent or both equal. The revision orders two unavailable observations; it is never by
/// itself proof that an edit happened. Available stamps ignore any private revision, so the
/// receipt contract for available text remains exact text plus selection.
#[derive(Clone, Debug)]
pub struct ImeMutationStamp {
    snapshot: IMESurroundingText,
    unavailable_revision: Option<u64>,
}

impl PartialEq for ImeMutationStamp {
    fn eq(&self, other: &Self) -> bool {
        match (&self.snapshot, &other.snapshot) {
            (IMESurroundingText::Available(a), IMESurroundingText::Available(b)) => a == b,
            (IMESurroundingText::Unavailable, IMESurroundingText::Unavailable) => {
                self.unavailable_revision == other.unavailable_revision
            }
            _ => false,
        }
    }
}

impl Eq for ImeMutationStamp {}

impl ImeMutationStamp {
    /// Capture a stamp from committed logical text and UTF-8 byte offsets.
    ///
    /// `preedit` is the byte range of the current pre-edit to remove before comparison. Invalid
    /// offsets produce an [`IMESurroundingText::Unavailable`] stamp without a revision. Copying
    /// is bounded by [`crate::output::MAX_IME_SURROUNDING_TEXT_BYTES`].
    pub fn from_committed(
        text: &str,
        cursor: usize,
        anchor: usize,
        preedit: Option<Range<usize>>,
    ) -> Self {
        Self {
            snapshot: IMESurroundingText::from_text(text, cursor, anchor, preedit),
            unavailable_revision: None,
        }
    }

    /// Capture a stamp from an already-built bounded snapshot.
    ///
    /// Publicly constructed payloads with invalid offsets or excessive length become
    /// unavailable rather than retaining an unbounded or malformed receipt.
    pub fn from_surrounding_text(snapshot: IMESurroundingText) -> Self {
        if let IMESurroundingText::Available(payload) = &snapshot
            && (payload.text.len() > crate::output::MAX_IME_SURROUNDING_TEXT_BYTES
                || !payload.text.is_char_boundary(payload.cursor)
                || !payload.text.is_char_boundary(payload.anchor))
        {
            return Self::unavailable();
        }
        Self {
            snapshot,
            unavailable_revision: None,
        }
    }

    /// A stamp for an owner that cannot provide surrounding text.
    pub fn unavailable() -> Self {
        Self::unavailable_at_revision_opt(None)
    }

    /// A stamp for unavailable surrounding text with a producer-supplied revision.
    ///
    /// The revision is engine-owned and must be process-monotonic and checked by its producer.
    /// This helper only requires two revisions on an unavailable receipt to be strictly
    /// increasing (`before < after`); it cannot prove the revision was never reused.
    pub fn unavailable_at_revision(revision: u64) -> Self {
        Self::unavailable_at_revision_opt(Some(revision))
    }

    fn unavailable_at_revision_opt(unavailable_revision: Option<u64>) -> Self {
        Self {
            snapshot: IMESurroundingText::Unavailable,
            unavailable_revision,
        }
    }

    /// The revision attached to an unavailable stamp, if any.
    ///
    /// Available stamps always return `None`; their receipt identity is the bounded snapshot.
    pub fn unavailable_revision(&self) -> Option<u64> {
        self.unavailable_revision
    }

    /// The bounded snapshot stored in this stamp.
    pub fn surrounding_text(&self) -> &IMESurroundingText {
        &self.snapshot
    }
}

/// Committed logical text and selection for one observation.
///
/// `text` is borrowed for the duration of the observation. The tracker copies only the bounded
/// window used by [`IMESurroundingText::from_text`]. `cursor` and `anchor` are UTF-8 byte
/// offsets into `text`; `cursor > anchor` is a valid reversed selection.
#[derive(Clone)]
pub struct CommittedText<'a> {
    /// Committed logical model text. It must not contain the current IME pre-edit.
    pub text: &'a str,

    /// UTF-8 byte offset of the primary cursor in [`Self::text`].
    pub cursor: usize,

    /// UTF-8 byte offset of the other selection end in [`Self::text`].
    pub anchor: usize,

    /// Byte range of a pre-edit that is still present in [`Self::text`], if any.
    ///
    /// Prefer removing the pre-edit from the model query. This field exists for editors whose
    /// committed text temporarily contains the composition and must remove it before sending.
    pub preedit: Option<Range<usize>>,
}

impl core::fmt::Debug for CommittedText<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CommittedText")
            .field("text", &"<redacted>")
            .field("text_len_bytes", &self.text.len())
            .field("cursor", &self.cursor)
            .field("anchor", &self.anchor)
            .field("preedit", &self.preedit)
            .finish()
    }
}

impl<'a> CommittedText<'a> {
    /// Describe committed text with a collapsed selection at `cursor`.
    pub const fn new(text: &'a str, cursor: usize, anchor: usize) -> Self {
        Self {
            text,
            cursor,
            anchor,
            preedit: None,
        }
    }

    /// Set the byte range of a pre-edit that must be removed from the snapshot.
    #[inline]
    pub fn with_preedit(mut self, preedit: Range<usize>) -> Self {
        self.preedit = Some(preedit);
        self
    }

    /// Build the bounded outgoing snapshot for this observation.
    pub fn snapshot(&self) -> IMESurroundingText {
        IMESurroundingText::from_text(self.text, self.cursor, self.anchor, self.preedit.clone())
    }

    /// Build a receipt stamp for this committed state.
    ///
    /// The resulting stamp never carries an unavailable revision; use
    /// [`ImeObservation::unavailable_revision`] for observations that cannot provide text.
    pub fn stamp(&self) -> ImeMutationStamp {
        ImeMutationStamp {
            snapshot: self.snapshot(),
            unavailable_revision: None,
        }
    }
}

/// One focused (or unfocused) observation of a custom editor.
///
/// Construct with [`Self::new`] and the builder methods, then pass it to
/// [`ImeContextTracker::observe`] once near the end of the UI pass, after all IME events and
/// editor commands for that pass have been applied.
#[derive(Clone, Debug)]
pub struct ImeObservation<'a> {
    /// [`crate::Context::cumulative_frame_nr_for`] for this viewport.
    ///
    /// The tracker accumulates application/IME cause for the whole frame, including discarded
    /// passes.
    pub frame_nr: u64,

    /// [`crate::Context::cumulative_pass_nr_for`] for this viewport.
    ///
    /// This is not the pass index within a frame. A gap of more than one pass starts a new
    /// context generation.
    pub pass_nr: u64,

    /// Immutable owner identity and replacement epoch.
    pub target: ImeTarget,

    /// Whether this editor currently owns focus. Unfocused observations return no descriptor.
    pub focused: bool,

    /// The IME purpose for this observation.
    pub purpose: IMEPurpose,

    /// Committed text, or `None` when the owner cannot currently provide it.
    pub committed: Option<CommittedText<'a>>,

    /// Producer-supplied revision for an unavailable observation.
    ///
    /// Private so that the revision cannot be published on the wire or forged by a struct
    /// literal. It is used only when the computed snapshot is unavailable and the purpose is
    /// not [`IMEPurpose::Password`]. Password and available observations discard it.
    unavailable_revision: Option<u64>,
}

impl<'a> ImeObservation<'a> {
    /// Start a focused, normal-purpose observation with unavailable surrounding text.
    pub fn new(frame_nr: u64, pass_nr: u64, target: ImeTarget) -> Self {
        Self {
            frame_nr,
            pass_nr,
            target,
            focused: true,
            purpose: IMEPurpose::Normal,
            committed: None,
            unavailable_revision: None,
        }
    }

    /// Set whether the editor currently owns focus.
    #[inline]
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Set the IME purpose.
    #[inline]
    pub fn purpose(mut self, purpose: IMEPurpose) -> Self {
        self.purpose = purpose;
        self
    }

    /// Set the committed-text observation.
    #[inline]
    pub fn committed(mut self, committed: Option<CommittedText<'a>>) -> Self {
        self.committed = committed;
        self
    }

    /// Supply the producer's revision for an unavailable observation.
    ///
    /// The revision is retained only when the computed snapshot is
    /// [`IMESurroundingText::Unavailable`] and the purpose is not [`IMEPurpose::Password`].
    #[inline]
    pub fn unavailable_revision(mut self, revision: u64) -> Self {
        self.unavailable_revision = Some(revision);
        self
    }
}

/// The descriptor and cause produced by one observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImeContextUpdate {
    /// Stable owner identity.
    pub owner: Id,

    /// Generation for this owner and context.
    pub context_generation: u64,

    /// Purpose reported by the observation.
    pub purpose: IMEPurpose,

    /// Bounded committed surrounding text, or an explicit reason it is unavailable.
    pub surrounding_text: IMESurroundingText,

    /// Why this snapshot differs from the previously observed one.
    ///
    /// This is [`ImeChangeCause::Other`] for context transitions and any change not proven by
    /// an accepted IME receipt.
    pub change_cause: ImeChangeCause,
}

#[derive(Debug)]
struct ContextState {
    target: ImeTarget,
    generation: u64,
    focused: bool,
    purpose: IMEPurpose,
    baseline: ImeMutationStamp,
}

/// Compressed chain of accepted IME receipts for one frame.
///
/// Every intermediate link was validated when it was accepted, so only the first `before` (to
/// re-check the frame's observed baseline) and the latest `after` are needed. `count` is
/// diagnostic state; it does not grow the retained snapshot storage.
#[derive(Debug)]
struct ImeReceiptChain {
    first_before: ImeMutationStamp,
    last_after: ImeMutationStamp,
    count: usize,
}

impl ImeReceiptChain {
    fn continues(&self, baseline: &ImeMutationStamp, before: &ImeMutationStamp) -> bool {
        self.first_before == *baseline && self.last_after == *before
    }

    fn is_exact(&self, baseline: &ImeMutationStamp, current: &ImeMutationStamp) -> bool {
        self.count > 0 && self.first_before == *baseline && self.last_after == *current
    }

    fn push(&mut self, after: ImeMutationStamp) {
        self.last_after = after;
        self.count = self.count.saturating_add(1);
    }
}

/// Whether this receipt transition can prove an unavailable observation.
///
/// Available -> available still requires a changed bounded snapshot. Every transition that
/// touches an unavailable side requires that side to carry a revision; unavailable ->
/// unavailable additionally requires a strictly increasing pair.
fn receipt_transition_is_valid(before: &ImeMutationStamp, after: &ImeMutationStamp) -> bool {
    match (&before.snapshot, &after.snapshot) {
        (IMESurroundingText::Available(a), IMESurroundingText::Available(b)) => a != b,
        (IMESurroundingText::Unavailable, IMESurroundingText::Unavailable) => {
            match (before.unavailable_revision, after.unavailable_revision) {
                (Some(before), Some(after)) => before < after,
                _ => false,
            }
        }
        (IMESurroundingText::Unavailable, IMESurroundingText::Available(_)) => {
            before.unavailable_revision.is_some()
        }
        (IMESurroundingText::Available(_), IMESurroundingText::Unavailable) => {
            after.unavailable_revision.is_some()
        }
    }
}

/// Mark the frame as application-origin and discard any receipt chain.
fn poison_frame(frame: &mut FrameState) {
    frame.origin_other = true;
    frame.last_cause = Some(ImeChangeCause::Other);
    frame.receipt_chain = None;
}

#[derive(Debug)]
struct FrameState {
    frame_nr: u64,
    origin_other: bool,
    receipt_chain: Option<ImeReceiptChain>,
    last_cause: Option<ImeChangeCause>,
}

#[derive(Debug, Default)]
struct ViewportState {
    last_seen_pass: Option<u64>,
    context: Option<ContextState>,
    frame: Option<FrameState>,
}

/// Per-viewport IME owner, generation and change-cause bookkeeping.
///
/// One tracker is enough for an application with several native windows. All methods take the
/// viewport explicitly, and [`Self::end_viewport`] releases that viewport's state.
#[derive(Debug, Default)]
pub struct ImeContextTracker {
    viewports: ViewportIdMap<ViewportState>,
}

// A native bridge can outlive a custom editor's tracker. Generations therefore belong to
// the process, not the tracker, so recreating an editor cannot reuse a still-cached context.
// A mutex also works on targets without 64-bit atomics. Exhaustion fails closed.
static CONTEXT_GENERATION: std::sync::LazyLock<epaint::mutex::Mutex<u64>> =
    std::sync::LazyLock::new(|| epaint::mutex::Mutex::new(0));

pub(crate) fn next_context_generation() -> Option<u64> {
    let mut generation = CONTEXT_GENERATION.lock();
    *generation = generation.checked_add(1)?;
    Some(*generation)
}

impl ImeContextTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that an IME commit or delete-surrounding mutation was successfully applied.
    ///
    /// Call this only after the editor accepted the mutation. Do not call it for the pre-edit,
    /// a rejected or stale IME event, an empty mutation, a failed command, or an ordinary
    /// application edit (including automatic corrections). `before` must describe the committed state
    /// immediately before the mutation and `after` the state immediately after it.
    ///
    /// The receipt is accepted only when the target matches the focused context, the
    /// transition is provable, and `before` continues the frame's receipt chain from the last
    /// observed baseline. Available stamps prove a change with exact text plus selection.
    /// Unavailable stamps need a revision on every unavailable side, and an
    /// unavailable -> unavailable pair additionally needs `before < after`. A mismatch marks
    /// the frame [`ImeChangeCause::Other`] and discards any earlier receipt chain. The return
    /// value says whether the receipt was structurally accepted; an application signal
    /// elsewhere in the frame can still force `Other`.
    ///
    /// ```
    /// use egui::{IMEPurpose, Id, ViewportId};
    /// use egui::ime_context::{
    ///     CommittedText, ImeContextTracker, ImeMutationStamp, ImeObservation, ImeTarget,
    /// };
    ///
    /// let viewport = ViewportId::ROOT;
    /// let target = ImeTarget::new(Id::unique("body"), 0);
    /// let mut tracker = ImeContextTracker::new();
    /// let mut text = String::from("ab");
    ///
    /// let observation = ImeObservation::new(0, 0, target)
    ///     .purpose(IMEPurpose::Normal)
    ///     .committed(Some(CommittedText::new(&text, text.len(), text.len())));
    /// tracker.observe(viewport, Some(observation));
    ///
    /// let before = ImeMutationStamp::from_committed(&text, 2, 2, None);
    /// text.push('c'); // the accepted IME insertion
    /// let after = ImeMutationStamp::from_committed(&text, 3, 3, None);
    /// assert!(tracker.note_input_method_apply(viewport, 1, target, before, after));
    /// ```
    pub fn note_input_method_apply(
        &mut self,
        viewport: ViewportId,
        frame_nr: u64,
        target: ImeTarget,
        before: ImeMutationStamp,
        after: ImeMutationStamp,
    ) -> bool {
        let Some(state) = self.viewports.get_mut(&viewport) else {
            return false;
        };
        let Some(context) = state.context.as_ref() else {
            return false;
        };
        if !context.focused || context.target != target || context.purpose == IMEPurpose::Password {
            return false;
        }

        ensure_frame(state, frame_nr);
        let context = state.context.as_ref().expect("checked above");
        let frame = state.frame.as_mut().expect("just initialized");

        if !receipt_transition_is_valid(&before, &after) {
            poison_frame(frame);
            return false;
        }

        let chain_is_valid = frame
            .receipt_chain
            .as_ref()
            .map_or(before == context.baseline, |chain| {
                chain.continues(&context.baseline, &before)
            });

        if !chain_is_valid {
            poison_frame(frame);
            return false;
        }

        if let Some(chain) = &mut frame.receipt_chain {
            chain.push(after);
        } else {
            frame.receipt_chain = Some(ImeReceiptChain {
                first_before: before,
                last_after: after,
                count: 1,
            });
        }
        true
    }

    /// Record an application-origin mutation in this frame.
    ///
    /// Use this for ordinary typing, paste, undo, toolbar or control edits, background model
    /// changes, automatic corrections, and any other non-IME edit. The signal is not required when the
    /// changed snapshot already fails receipt validation, but it makes the origin explicit.
    /// `Other` then dominates all later changes in the same frame.
    ///
    /// Returns `false` when the viewport has no focused context or `target` does not match it.
    pub fn note_application_change(
        &mut self,
        viewport: ViewportId,
        frame_nr: u64,
        target: ImeTarget,
    ) -> bool {
        let Some(state) = self.viewports.get_mut(&viewport) else {
            return false;
        };
        let Some(context) = state.context.as_ref() else {
            return false;
        };
        if !context.focused || context.target != target {
            return false;
        }

        ensure_frame(state, frame_nr);
        let frame = state.frame.as_mut().expect("just initialized");
        poison_frame(frame);
        true
    }

    /// Observe the current editor state once near the end of a pass.
    ///
    /// The first focused observation initializes the context and its baseline. Every later
    /// observation rebuilds the bounded snapshot from the current [`CommittedText`] and
    /// refreshes the baseline; no text is ever replayed from a same-pass cache. Only the
    /// context generation and the frame cause are retained.
    ///
    /// Call this once per viewport pass for the one editor that owns IME focus. Two different
    /// owners observed in the same pass are treated as owner transitions, not as separate
    /// simultaneous contexts, because a native viewport has one platform IME target.
    ///
    /// Pass `None` to mark this viewport's context unfocused for a pass (for example when no
    /// editor is visible). The next focused observation starts a new generation. Use
    /// [`Self::end_viewport`] when the native viewport is gone.
    ///
    /// Returns `None` when the observation is absent or unfocused.
    pub fn observe(
        &mut self,
        viewport: ViewportId,
        observation: Option<ImeObservation<'_>>,
    ) -> Option<ImeContextUpdate> {
        let Some(observation) = observation else {
            if let Some(state) = self.viewports.get_mut(&viewport) {
                if let Some(context) = state.context.as_mut() {
                    context.focused = false;
                }
                state.frame = None;
            }
            return None;
        };

        let ImeObservation {
            frame_nr,
            pass_nr,
            target,
            focused,
            purpose,
            committed,
            unavailable_revision,
        } = observation;
        let state = self.viewports.entry(viewport).or_default();

        if !focused {
            if let Some(context) = state.context.as_mut() {
                context.focused = false;
            }
            state.last_seen_pass = Some(pass_nr);
            state.frame = None;
            return None;
        }

        let context_changed = match &state.context {
            None => true,
            Some(context) => {
                !context.focused
                    || context.target != target
                    || context.purpose != purpose
                    || state
                        .last_seen_pass
                        .is_some_and(|last| last != pass_nr && last.checked_add(1) != Some(pass_nr))
            }
        };

        let mut stamp = if purpose == IMEPurpose::Password {
            // Password text is never retained, and an unavailable revision must not survive
            // the privacy boundary either.
            ImeMutationStamp::unavailable()
        } else {
            committed.map_or_else(ImeMutationStamp::unavailable, |committed| committed.stamp())
        };
        if purpose != IMEPurpose::Password
            && matches!(&stamp.snapshot, IMESurroundingText::Unavailable)
            && let Some(revision) = unavailable_revision
        {
            stamp.unavailable_revision = Some(revision);
        }

        if context_changed {
            let Some(generation) = next_context_generation() else {
                state.context = None;
                state.frame = None;
                return None;
            };
            state.context = Some(ContextState {
                target,
                generation,
                focused: true,
                purpose,
                baseline: stamp.clone(),
            });
            state.last_seen_pass = Some(pass_nr);

            let frame = ensure_frame(state, frame_nr);
            frame.origin_other = true;
            frame.receipt_chain = None;
            frame.last_cause = Some(ImeChangeCause::Other);

            return Some(ImeContextUpdate {
                owner: target.owner,
                context_generation: generation,
                purpose,
                surrounding_text: stamp.snapshot,
                change_cause: ImeChangeCause::Other,
            });
        }

        ensure_frame(state, frame_nr);
        let context = state.context.as_mut().expect("context exists");
        let frame = state.frame.as_mut().expect("frame initialized");

        context.focused = true;
        context.purpose = purpose;
        let previous = core::mem::replace(&mut context.baseline, stamp.clone());
        let changed = previous != stamp;

        let cause = if changed {
            let receipts_are_exact = frame
                .receipt_chain
                .as_ref()
                .is_some_and(|chain| chain.is_exact(&previous, &stamp));
            let cause = if frame.origin_other {
                ImeChangeCause::Other
            } else if receipts_are_exact {
                ImeChangeCause::InputMethod
            } else {
                ImeChangeCause::Other
            };
            if cause == ImeChangeCause::Other {
                frame.origin_other = true;
            }
            frame.last_cause = Some(cause);
            cause
        } else {
            frame.last_cause.unwrap_or(ImeChangeCause::Other)
        };

        // Receipts belong to one pass. Keep `origin_other` and `last_cause` for the rest of the
        // frame's discarded passes.
        frame.receipt_chain = None;
        state.last_seen_pass = Some(pass_nr);

        Some(ImeContextUpdate {
            owner: context.target.owner,
            context_generation: context.generation,
            purpose: context.purpose,
            surrounding_text: stamp.snapshot,
            change_cause: cause,
        })
    }

    /// Forget all state for a closed viewport.
    ///
    /// A later context in that viewport receives a fresh generation. For an editor that is
    /// merely hidden for a pass, observe it as unfocused instead; that preserves the viewport
    /// state and follows the normal focus/gap rules.
    pub fn end_viewport(&mut self, viewport: ViewportId) {
        self.viewports.remove(&viewport);
    }
}

fn ensure_frame(state: &mut ViewportState, frame_nr: u64) -> &mut FrameState {
    let needs_reset = match &state.frame {
        Some(frame) => frame.frame_nr != frame_nr,
        None => true,
    };
    if needs_reset {
        state.frame = Some(FrameState {
            frame_nr,
            origin_other: false,
            receipt_chain: None,
            last_cause: None,
        });
    }
    state.frame.as_mut().expect("frame initialized above")
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::{Context, RawInput};

    fn target(owner: u64) -> ImeTarget {
        ImeTarget::new(Id::unique(owner), 0)
    }

    fn observation(
        frame_nr: u64,
        pass_nr: u64,
        target: ImeTarget,
        text: &str,
        cursor: usize,
        anchor: usize,
    ) -> ImeObservation<'_> {
        ImeObservation::new(frame_nr, pass_nr, target)
            .committed(Some(CommittedText::new(text, cursor, anchor)))
    }

    fn observe(
        tracker: &mut ImeContextTracker,
        viewport: ViewportId,
        frame_nr: u64,
        pass_nr: u64,
        target: ImeTarget,
        text: &str,
    ) -> ImeContextUpdate {
        tracker
            .observe(
                viewport,
                Some(observation(
                    frame_nr,
                    pass_nr,
                    target,
                    text,
                    text.len(),
                    text.len(),
                )),
            )
            .expect("focused observation")
    }

    fn receipt(
        tracker: &mut ImeContextTracker,
        viewport: ViewportId,
        frame_nr: u64,
        target: ImeTarget,
        before: &str,
        after: &str,
    ) -> bool {
        tracker.note_input_method_apply(
            viewport,
            frame_nr,
            target,
            ImeMutationStamp::from_committed(before, before.len(), before.len(), None),
            ImeMutationStamp::from_committed(after, after.len(), after.len(), None),
        )
    }

    fn unavailable_observation(
        frame_nr: u64,
        pass_nr: u64,
        target: ImeTarget,
        revision: Option<u64>,
    ) -> ImeObservation<'static> {
        let observation = ImeObservation::new(frame_nr, pass_nr, target);
        match revision {
            Some(revision) => observation.unavailable_revision(revision),
            None => observation,
        }
    }

    fn unavailable_receipt(
        tracker: &mut ImeContextTracker,
        viewport: ViewportId,
        frame_nr: u64,
        target: ImeTarget,
        before: Option<u64>,
        after: Option<u64>,
    ) -> bool {
        let stamp = |revision: Option<u64>| {
            revision.map_or_else(
                ImeMutationStamp::unavailable,
                ImeMutationStamp::unavailable_at_revision,
            )
        };
        tracker.note_input_method_apply(viewport, frame_nr, target, stamp(before), stamp(after))
    }

    fn payload(update: &ImeContextUpdate) -> &crate::output::IMESurroundingTextPayload {
        update
            .surrounding_text
            .available()
            .expect("surrounding text is available")
    }

    #[test]
    fn ordinary_text_caret_and_preedit_changes_keep_the_generation() {
        let viewport = ViewportId::ROOT;
        let target = target(1);
        let mut tracker = ImeContextTracker::new();

        let first = observe(&mut tracker, viewport, 0, 0, target, "hello");
        assert_eq!(first.owner, target.owner);
        assert!(first.context_generation > 0);

        let typed = observe(&mut tracker, viewport, 1, 1, target, "hello world");
        assert_eq!(typed.context_generation, first.context_generation);

        let reversed = tracker
            .observe(viewport, Some(observation(1, 2, target, "aé", 1, 3)))
            .unwrap();
        assert_eq!(reversed.context_generation, first.context_generation);
        assert_eq!(payload(&reversed).cursor, 1);
        assert_eq!(payload(&reversed).anchor, 3);

        let preedit = tracker
            .observe(
                viewport,
                Some(
                    observation(1, 3, target, "abに", 5, 5)
                        .committed(Some(CommittedText::new("abに", 5, 5).with_preedit(2..5))),
                ),
            )
            .unwrap();
        assert_eq!(preedit.context_generation, first.context_generation);
        assert_eq!(payload(&preedit).text, "ab");
        assert_eq!(payload(&preedit).cursor, 2);

        // Extending or clearing the pre-edit changes only the transient range. The committed
        // baseline and the context generation stay identical.
        let longer_preedit = tracker
            .observe(
                viewport,
                Some(
                    observation(1, 4, target, "abにほ", 8, 8)
                        .committed(Some(CommittedText::new("abにほ", 8, 8).with_preedit(2..8))),
                ),
            )
            .unwrap();
        assert_eq!(longer_preedit.context_generation, first.context_generation);
        assert_eq!(payload(&longer_preedit).text, "ab");
        assert_eq!(payload(&longer_preedit).cursor, 2);
    }

    #[test]
    fn repeated_same_pass_retains_accepted_input_method_cause() {
        let viewport = ViewportId::ROOT;
        let target = target(81);
        let mut tracker = ImeContextTracker::new();
        let first = observe(&mut tracker, viewport, 0, 0, target, "ab");
        assert!(receipt(&mut tracker, viewport, 1, target, "ab", "ab日"));
        let committed = observe(&mut tracker, viewport, 1, 1, target, "ab日");
        let repeated = observe(&mut tracker, viewport, 1, 1, target, "ab日");
        assert_eq!(committed.context_generation, first.context_generation);
        assert_eq!(repeated.context_generation, committed.context_generation);
        assert_eq!(repeated.surrounding_text, committed.surrounding_text);
        assert_eq!(committed.change_cause, ImeChangeCause::InputMethod);
        assert_eq!(repeated.change_cause, ImeChangeCause::InputMethod);
    }

    #[test]
    fn many_receipts_are_compressed_to_one_bounded_chain() {
        const RECEIPTS: usize = 2_000;

        let viewport = ViewportId::ROOT;
        let target = target(83);
        let mut tracker = ImeContextTracker::new();
        observe(&mut tracker, viewport, 0, 0, target, "");

        let mut text = String::new();
        for _ in 0..RECEIPTS {
            let before = text.clone();
            text.push('x');
            assert!(receipt(&mut tracker, viewport, 1, target, &before, &text));
        }

        let frame = tracker
            .viewports
            .get(&viewport)
            .and_then(|state| state.frame.as_ref())
            .expect("accepted receipts keep one frame");
        let chain = frame
            .receipt_chain
            .as_ref()
            .expect("accepted receipts keep one compressed chain");
        assert_eq!(chain.count, RECEIPTS);
        assert_eq!(
            chain.last_after,
            ImeMutationStamp::from_committed(&text, text.len(), text.len(), None)
        );

        let observed = observe(&mut tracker, viewport, 1, 1, target, &text);
        assert_eq!(observed.change_cause, ImeChangeCause::InputMethod);
        assert!(
            tracker
                .viewports
                .get(&viewport)
                .and_then(|state| state.frame.as_ref())
                .is_some_and(|frame| frame.receipt_chain.is_none()),
            "observing the pass consumes the compressed chain"
        );
    }

    #[test]
    fn recreated_tracker_cannot_reuse_a_live_bridge_context() {
        let viewport = ViewportId::ROOT;
        let target = target(82);
        let first = observe(
            &mut ImeContextTracker::new(),
            viewport,
            0,
            0,
            target,
            "same",
        );
        let recreated = observe(
            &mut ImeContextTracker::new(),
            viewport,
            0,
            0,
            target,
            "same",
        );
        assert_eq!(first.owner, recreated.owner);
        assert_ne!(first.context_generation, recreated.context_generation);
        assert_eq!(recreated.change_cause, ImeChangeCause::Other);
    }

    #[test]
    fn public_receipt_payloads_are_validated_before_retaining_them() {
        use crate::output::{IMESurroundingTextPayload, MAX_IME_SURROUNDING_TEXT_BYTES};
        for (text, cursor, anchor) in [
            ("x".repeat(MAX_IME_SURROUNDING_TEXT_BYTES + 1), 0, 0),
            ("é".to_owned(), 1, 2),
            ("é".to_owned(), 2, 1),
            ("x".to_owned(), 2, 0),
        ] {
            let stamp = ImeMutationStamp::from_surrounding_text(IMESurroundingText::Available(
                IMESurroundingTextPayload {
                    text,
                    cursor,
                    anchor,
                },
            ));
            assert_eq!(stamp, ImeMutationStamp::unavailable());
        }
        let valid = IMESurroundingText::from_text("é", 2, 0, None);
        assert_eq!(
            ImeMutationStamp::from_surrounding_text(valid.clone()).snapshot,
            valid
        );
    }

    #[test]
    fn focus_loss_refocus_and_absent_pass_start_one_new_generation() {
        let viewport = ViewportId::ROOT;
        let target = target(2);
        let mut tracker = ImeContextTracker::new();

        let first = observe(&mut tracker, viewport, 0, 0, target, "text");
        assert!(tracker.observe(viewport, None).is_none());

        let refocused = observe(&mut tracker, viewport, 1, 1, target, "text");
        assert!(refocused.context_generation > first.context_generation);

        let gap = observe(&mut tracker, viewport, 2, 5, target, "text");
        assert!(gap.context_generation > refocused.context_generation);

        let same_pass_again = observe(&mut tracker, viewport, 2, 5, target, "text");
        assert_eq!(
            same_pass_again.context_generation, gap.context_generation,
            "repeating one pass must not start another context"
        );
    }

    #[test]
    fn owner_replacement_and_purpose_transitions_start_a_new_generation() {
        let viewport = ViewportId::ROOT;
        let mut tracker = ImeContextTracker::new();

        let first = observe(&mut tracker, viewport, 0, 0, target(3), "one");
        let replacement = observe(
            &mut tracker,
            viewport,
            1,
            1,
            ImeTarget::new(target(3).owner, 1),
            "one",
        );
        assert!(replacement.context_generation > first.context_generation);

        let other_owner = observe(&mut tracker, viewport, 2, 2, target(4), "two");
        assert_ne!(other_owner.owner, first.owner);
        assert!(other_owner.context_generation > replacement.context_generation);

        let password = tracker
            .observe(
                viewport,
                Some(observation(3, 3, target(4), "secret", 6, 6).purpose(IMEPurpose::Password)),
            )
            .unwrap();
        assert_eq!(password.purpose, IMEPurpose::Password);
        assert!(password.context_generation > other_owner.context_generation);
        assert!(matches!(
            password.surrounding_text,
            IMESurroundingText::Unavailable
        ));
        assert!(!format!("{password:?}").contains("secret"));

        let normal_again = observe(&mut tracker, viewport, 4, 4, target(4), "secret");
        assert!(normal_again.context_generation > password.context_generation);
        assert_eq!(payload(&normal_again).text, "secret");
    }

    #[test]
    fn empty_and_bounded_documents_are_available_without_retaining_the_whole_model() {
        let viewport = ViewportId::ROOT;
        let target = target(5);
        let mut tracker = ImeContextTracker::new();

        let empty = observe(&mut tracker, viewport, 0, 0, target, "");
        assert_eq!(payload(&empty).text, "");
        assert_eq!((payload(&empty).cursor, payload(&empty).anchor), (0, 0));

        let huge = "a".repeat(20_000);
        let bounded = observe(&mut tracker, viewport, 1, 1, target, &huge);
        assert!(payload(&bounded).text.len() <= crate::output::MAX_IME_SURROUNDING_TEXT_BYTES);
        let debug = format!("{tracker:?}");
        assert!(debug.contains("<redacted>"));
        let long_run = "a".repeat(1000);
        assert!(!debug.contains(long_run.as_str()));
    }

    #[test]
    fn first_accepted_ime_commit_after_an_observed_baseline_is_input_method() {
        let viewport = ViewportId::ROOT;
        let target = target(6);
        let mut tracker = ImeContextTracker::new();
        observe(&mut tracker, viewport, 0, 0, target, "a");

        assert!(receipt(&mut tracker, viewport, 1, target, "a", "ab"));
        let update = observe(&mut tracker, viewport, 1, 1, target, "ab");
        assert_eq!(update.change_cause, ImeChangeCause::InputMethod);
        assert_eq!(payload(&update).text, "ab");
    }

    #[test]
    fn chained_delete_and_commit_receipts_are_input_method() {
        let viewport = ViewportId::ROOT;
        let target = target(18);
        let mut tracker = ImeContextTracker::new();
        observe(&mut tracker, viewport, 0, 0, target, "abc");

        // DeleteSurroundingBytes followed by Commit is one accepted IME batch.
        assert!(receipt(&mut tracker, viewport, 1, target, "abc", "ac"));
        assert!(receipt(&mut tracker, viewport, 1, target, "ac", "acd"));
        let update = observe(&mut tracker, viewport, 1, 1, target, "acd");
        assert_eq!(update.change_cause, ImeChangeCause::InputMethod);
        assert_eq!(payload(&update).text, "acd");
    }

    #[test]
    fn previous_application_edit_refreshes_the_baseline_before_the_next_ime_receipt() {
        let viewport = ViewportId::ROOT;
        let target = target(7);
        let mut tracker = ImeContextTracker::new();
        observe(&mut tracker, viewport, 0, 0, target, "ab");

        let application = observe(&mut tracker, viewport, 1, 1, target, "abc");
        assert_eq!(application.change_cause, ImeChangeCause::Other);

        assert!(receipt(&mut tracker, viewport, 2, target, "abc", "abcd"));
        let ime = observe(&mut tracker, viewport, 2, 2, target, "abcd");
        assert_eq!(
            ime.change_cause,
            ImeChangeCause::InputMethod,
            "the observed application edit is the new baseline, not a stale stamp"
        );
    }

    #[test]
    fn application_edit_before_ime_in_one_frame_is_other() {
        let viewport = ViewportId::ROOT;
        let target = target(8);
        let mut tracker = ImeContextTracker::new();
        observe(&mut tracker, viewport, 0, 0, target, "ab");

        // The application changes the committed text before the IME mutation and says so.
        assert!(tracker.note_application_change(viewport, 1, target));
        assert!(!receipt(&mut tracker, viewport, 1, target, "abc", "abcd"));
        let update = observe(&mut tracker, viewport, 1, 1, target, "abcd");
        assert_eq!(update.change_cause, ImeChangeCause::Other);

        // Even without the explicit signal, a receipt whose `before` is not the observed
        // baseline is rejected.
        let mut tracker = ImeContextTracker::new();
        observe(&mut tracker, viewport, 0, 0, target, "ab");
        assert!(!receipt(&mut tracker, viewport, 1, target, "abc", "abcd"));
        let update = observe(&mut tracker, viewport, 1, 1, target, "abcd");
        assert_eq!(update.change_cause, ImeChangeCause::Other);
    }

    #[test]
    fn application_edit_after_ime_and_ime_after_application_are_other() {
        let viewport = ViewportId::ROOT;
        let target = target(9);
        let mut tracker = ImeContextTracker::new();
        observe(&mut tracker, viewport, 0, 0, target, "ab");

        assert!(receipt(&mut tracker, viewport, 1, target, "ab", "abc"));
        // The application edits after the IME commit, without another receipt.
        let app_after = observe(&mut tracker, viewport, 1, 1, target, "abcd");
        assert_eq!(app_after.change_cause, ImeChangeCause::Other);

        let mut tracker = ImeContextTracker::new();
        observe(&mut tracker, viewport, 0, 0, target, "ab");
        assert!(receipt(&mut tracker, viewport, 1, target, "ab", "abc"));
        assert!(tracker.note_application_change(viewport, 1, target));
        assert!(!receipt(&mut tracker, viewport, 1, target, "abcd", "abcde"));
        let ime_after_app = observe(&mut tracker, viewport, 1, 1, target, "abcde");
        assert_eq!(ime_after_app.change_cause, ImeChangeCause::Other);
    }

    #[test]
    fn noop_mismatched_and_failed_receipts_never_earn_input_method() {
        let viewport = ViewportId::ROOT;
        let target = target(10);
        let mut tracker = ImeContextTracker::new();
        observe(&mut tracker, viewport, 0, 0, target, "abc");

        assert!(
            !receipt(&mut tracker, viewport, 1, target, "abc", "abc"),
            "a no-op receipt is rejected"
        );
        assert!(
            !tracker.note_input_method_apply(
                viewport,
                1,
                ImeTarget::new(Id::unique(99), 0),
                ImeMutationStamp::from_committed("abc", 3, 3, None),
                ImeMutationStamp::from_committed("abcd", 4, 4, None),
            ),
            "a receipt for another owner is rejected"
        );
        assert!(
            !tracker.note_input_method_apply(
                viewport,
                1,
                ImeTarget::new(target.owner, 1),
                ImeMutationStamp::from_committed("abc", 3, 3, None),
                ImeMutationStamp::from_committed("abcd", 4, 4, None),
            ),
            "a receipt for a replaced target is rejected"
        );

        // A structurally accepted receipt whose result never reached the model is a failure.
        assert!(receipt(&mut tracker, viewport, 2, target, "abc", "abcd"));
        let failed = observe(&mut tracker, viewport, 2, 2, target, "abce");
        assert_eq!(failed.change_cause, ImeChangeCause::Other);
    }

    #[test]
    fn password_context_rejects_receipts_and_never_retains_text() {
        let viewport = ViewportId::ROOT;
        let target = target(11);
        let mut tracker = ImeContextTracker::new();
        let password = tracker
            .observe(
                viewport,
                Some(observation(0, 0, target, "secret", 6, 6).purpose(IMEPurpose::Password)),
            )
            .unwrap();
        assert_eq!(password.purpose, IMEPurpose::Password);
        assert!(matches!(
            password.surrounding_text,
            IMESurroundingText::Unavailable
        ));

        assert!(!receipt(
            &mut tracker,
            viewport,
            1,
            target,
            "secret",
            "secret2"
        ));
        let debug = format!("{tracker:?}");
        assert!(!debug.contains("secret"));
    }

    #[test]
    fn unchanged_second_pass_retains_cause_but_changed_same_pass_is_fresh() {
        let viewport = ViewportId::ROOT;
        let target = target(12);
        let mut tracker = ImeContextTracker::new();
        observe(&mut tracker, viewport, 0, 0, target, "a");

        assert!(receipt(&mut tracker, viewport, 1, target, "a", "ab"));
        let first_pass = observe(&mut tracker, viewport, 1, 1, target, "ab");
        assert_eq!(first_pass.change_cause, ImeChangeCause::InputMethod);
        assert_eq!(payload(&first_pass).text, "ab");

        let unchanged_second_pass = observe(&mut tracker, viewport, 1, 2, target, "ab");
        assert_eq!(
            unchanged_second_pass.change_cause,
            ImeChangeCause::InputMethod,
            "an unchanged discarded pass retains the frame cause"
        );
        assert_eq!(
            unchanged_second_pass.context_generation,
            first_pass.context_generation
        );

        // A later change in the same pass is rebuilt from the current observation, never from
        // a cached first-pass descriptor.
        assert!(receipt(&mut tracker, viewport, 1, target, "ab", "abc"));
        let changed_same_pass = observe(&mut tracker, viewport, 1, 2, target, "abc");
        assert_eq!(changed_same_pass.change_cause, ImeChangeCause::InputMethod);
        assert_eq!(payload(&changed_same_pass).text, "abc");
        assert_eq!(
            changed_same_pass.context_generation,
            first_pass.context_generation
        );
    }

    #[test]
    fn other_dominates_all_later_mutations_in_one_frame() {
        let viewport = ViewportId::ROOT;
        let target = target(13);
        let mut tracker = ImeContextTracker::new();
        observe(&mut tracker, viewport, 0, 0, target, "a");

        assert!(receipt(&mut tracker, viewport, 1, target, "a", "ab"));
        let ime = observe(&mut tracker, viewport, 1, 1, target, "ab");
        assert_eq!(ime.change_cause, ImeChangeCause::InputMethod);

        assert!(tracker.note_application_change(viewport, 1, target));
        let application = observe(&mut tracker, viewport, 1, 2, target, "ab");
        assert_eq!(application.change_cause, ImeChangeCause::Other);

        assert!(receipt(&mut tracker, viewport, 1, target, "ab", "abc"));
        let later_ime = observe(&mut tracker, viewport, 1, 3, target, "abc");
        assert_eq!(later_ime.change_cause, ImeChangeCause::Other);
        assert_eq!(payload(&later_ime).text, "abc");

        let unchanged = observe(&mut tracker, viewport, 1, 4, target, "abc");
        assert_eq!(unchanged.change_cause, ImeChangeCause::Other);
    }

    #[test]
    fn teardown_forgets_owner_and_generation_for_only_that_viewport() {
        let root = ViewportId::ROOT;
        let aux = ViewportId::from_hash_of("aux");
        let target = target(14);
        let mut tracker = ImeContextTracker::new();

        let root_first = observe(&mut tracker, root, 0, 0, target, "root");
        let aux_first = observe(&mut tracker, aux, 0, 0, target, "aux");
        assert_ne!(root_first.context_generation, aux_first.context_generation);

        tracker.end_viewport(root);
        let root_again = observe(&mut tracker, root, 1, 1, target, "root");
        assert!(root_again.context_generation > root_first.context_generation);
        let aux_again = observe(&mut tracker, aux, 1, 1, target, "aux");
        assert_eq!(aux_again.context_generation, aux_first.context_generation);
    }

    #[test]
    fn two_real_viewports_keep_independent_generations_and_passes() {
        let ctx = Context::default();
        ctx.set_embed_viewports(false);
        let root = ViewportId::ROOT;
        let aux = ViewportId::from_hash_of("ime-context-aux");
        let target = target(15);
        let mut tracker = ImeContextTracker::new();
        let mut root_updates = Vec::new();
        let mut aux_updates = Vec::new();

        for (viewport, text, updates) in [
            (root, "root", &mut root_updates),
            (aux, "aux", &mut aux_updates),
        ] {
            let mut raw = RawInput {
                viewport_id: viewport,
                ..Default::default()
            };
            raw.viewports.entry(viewport).or_default();
            let output = ctx.run_ui(raw, |ui| {
                if viewport == root {
                    ui.ctx().show_viewport_deferred(
                        aux,
                        crate::ViewportBuilder::default(),
                        |_, _| {},
                    );
                }
                let actual_viewport = ui.ctx().viewport_id();
                assert_eq!(actual_viewport, viewport);
                let observation = observation(
                    ui.ctx().cumulative_frame_nr_for(actual_viewport),
                    ui.ctx().cumulative_pass_nr_for(actual_viewport),
                    target,
                    text,
                    text.len(),
                    text.len(),
                );
                updates.push(tracker.observe(actual_viewport, Some(observation)).unwrap());
            });
            output.drop_without_applying_deltas();
        }

        assert_eq!(root_updates.len(), 1);
        assert_eq!(aux_updates.len(), 1);
        let root_generation = root_updates[0].context_generation;
        let aux_generation = aux_updates[0].context_generation;
        assert_ne!(root_generation, aux_generation);

        // Both viewports have their own pass counter, so both are at pass 1 here. Their state
        // must not collide even though the numeric pass values are equal.
        for (viewport, text, updates) in [
            (root, "root2", &mut root_updates),
            (aux, "aux2", &mut aux_updates),
        ] {
            let mut raw = RawInput {
                viewport_id: viewport,
                ..Default::default()
            };
            raw.viewports.entry(viewport).or_default();
            let output = ctx.run_ui(raw, |ui| {
                if viewport == root {
                    ui.ctx().show_viewport_deferred(
                        aux,
                        crate::ViewportBuilder::default(),
                        |_, _| {},
                    );
                }
                let actual_viewport = ui.ctx().viewport_id();
                assert_eq!(ui.ctx().cumulative_pass_nr_for(actual_viewport), 1);
                let observation = observation(
                    ui.ctx().cumulative_frame_nr_for(actual_viewport),
                    ui.ctx().cumulative_pass_nr_for(actual_viewport),
                    target,
                    text,
                    text.len(),
                    text.len(),
                );
                updates.push(tracker.observe(actual_viewport, Some(observation)).unwrap());
            });
            output.drop_without_applying_deltas();
        }

        assert_eq!(root_updates[1].context_generation, root_generation);
        assert_eq!(aux_updates[1].context_generation, aux_generation);
        assert_eq!(payload(&root_updates[1]).text, "root2");
        assert_eq!(payload(&aux_updates[1]).text, "aux2");
    }

    #[test]
    fn real_forced_discard_mixes_ime_and_application_changes_conservatively() {
        let ctx = Context::default();
        let target = target(16);
        let tracker = core::cell::RefCell::new(ImeContextTracker::new());
        let mut text = String::from("base");
        let mut updates = Vec::new();

        let baseline = ctx.run_ui(RawInput::default(), |ui| {
            let viewport = ui.ctx().viewport_id();
            let observation = observation(
                ui.ctx().cumulative_frame_nr_for(viewport),
                ui.ctx().cumulative_pass_nr_for(viewport),
                target,
                &text,
                text.len(),
                text.len(),
            );
            updates.push(
                tracker
                    .borrow_mut()
                    .observe(viewport, Some(observation))
                    .unwrap(),
            );
        });
        baseline.drop_without_applying_deltas();
        updates.clear();

        let output = ctx.run_ui(RawInput::default(), |ui| {
            let viewport = ui.ctx().viewport_id();
            let pass = ui.ctx().cumulative_pass_nr_for(viewport);
            let frame_nr = ui.ctx().cumulative_frame_nr_for(viewport);
            if pass == 1 {
                let before = ImeMutationStamp::from_committed(&text, text.len(), text.len(), None);
                text.push('x');
                let after = ImeMutationStamp::from_committed(&text, text.len(), text.len(), None);
                assert!(
                    tracker
                        .borrow_mut()
                        .note_input_method_apply(viewport, frame_nr, target, before, after,)
                );
                ui.ctx().request_discard("exercise a second IME pass");
            } else {
                text.push('y');
                assert!(
                    tracker
                        .borrow_mut()
                        .note_application_change(viewport, frame_nr, target)
                );
            }

            let observation = observation(frame_nr, pass, target, &text, text.len(), text.len());
            updates.push(
                tracker
                    .borrow_mut()
                    .observe(viewport, Some(observation))
                    .unwrap(),
            );
        });
        output.drop_without_applying_deltas();

        assert_eq!(updates.len(), 2, "the frame ran two real passes");
        assert_eq!(updates[0].change_cause, ImeChangeCause::InputMethod);
        assert_eq!(updates[1].change_cause, ImeChangeCause::Other);
        assert_eq!(
            updates[1].surrounding_text.available().unwrap().text,
            "basexy"
        );
        assert_eq!(
            updates[0].context_generation, updates[1].context_generation,
            "a discarded pass is not a new context"
        );
    }

    #[test]
    fn real_forced_discard_application_first_still_dominates_later_ime() {
        let ctx = Context::default();
        let target = target(17);
        let tracker = core::cell::RefCell::new(ImeContextTracker::new());
        let mut text = String::from("base");
        let mut updates = Vec::new();

        let baseline = ctx.run_ui(RawInput::default(), |ui| {
            let viewport = ui.ctx().viewport_id();
            let observation = observation(
                ui.ctx().cumulative_frame_nr_for(viewport),
                ui.ctx().cumulative_pass_nr_for(viewport),
                target,
                &text,
                text.len(),
                text.len(),
            );
            tracker
                .borrow_mut()
                .observe(viewport, Some(observation))
                .unwrap();
        });
        baseline.drop_without_applying_deltas();

        let output = ctx.run_ui(RawInput::default(), |ui| {
            let viewport = ui.ctx().viewport_id();
            let pass = ui.ctx().cumulative_pass_nr_for(viewport);
            let frame_nr = ui.ctx().cumulative_frame_nr_for(viewport);
            if pass == 1 {
                text.push('y');
                assert!(
                    tracker
                        .borrow_mut()
                        .note_application_change(viewport, frame_nr, target)
                );
                ui.ctx()
                    .request_discard("exercise a second application pass");
            } else {
                let before = ImeMutationStamp::from_committed(&text, text.len(), text.len(), None);
                text.push('x');
                let after = ImeMutationStamp::from_committed(&text, text.len(), text.len(), None);
                assert!(
                    tracker
                        .borrow_mut()
                        .note_input_method_apply(viewport, frame_nr, target, before, after,)
                );
            }

            let observation = observation(frame_nr, pass, target, &text, text.len(), text.len());
            updates.push(
                tracker
                    .borrow_mut()
                    .observe(viewport, Some(observation))
                    .unwrap(),
            );
        });
        output.drop_without_applying_deltas();

        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].change_cause, ImeChangeCause::Other);
        assert_eq!(updates[1].change_cause, ImeChangeCause::Other);
        assert_eq!(
            updates[1].surrounding_text.available().unwrap().text,
            "baseyx"
        );
    }

    #[test]
    fn stamp_equality_follows_the_transition_table() {
        let snapshot = IMESurroundingText::from_text("ab", 2, 2, None);
        let available = ImeMutationStamp {
            snapshot: snapshot.clone(),
            unavailable_revision: None,
        };
        let available_with_private_revision = ImeMutationStamp {
            snapshot: snapshot.clone(),
            unavailable_revision: Some(99),
        };
        assert_eq!(
            available, available_with_private_revision,
            "available identity is exact text plus selection, ignoring any private revision"
        );
        assert_ne!(
            available,
            ImeMutationStamp::from_committed("ab", 1, 1, None),
            "available selection is part of the identity"
        );

        assert_eq!(
            ImeMutationStamp::unavailable_at_revision(3),
            ImeMutationStamp::unavailable_at_revision(3)
        );
        assert_ne!(
            ImeMutationStamp::unavailable_at_revision(3),
            ImeMutationStamp::unavailable_at_revision(4)
        );
        assert_ne!(
            ImeMutationStamp::unavailable(),
            ImeMutationStamp::unavailable_at_revision(3)
        );
        assert_ne!(ImeMutationStamp::unavailable(), available);
        assert_ne!(available, ImeMutationStamp::unavailable());
    }

    #[test]
    fn unavailable_revisions_are_present_and_strictly_increasing() {
        let viewport = ViewportId::ROOT;
        let target = target(90);
        let mut tracker = ImeContextTracker::new();
        let initial = tracker
            .observe(
                viewport,
                Some(unavailable_observation(0, 0, target, Some(10))),
            )
            .unwrap();
        assert!(matches!(
            initial.surrounding_text,
            IMESurroundingText::Unavailable
        ));
        assert_eq!(initial.change_cause, ImeChangeCause::Other);

        assert!(unavailable_receipt(
            &mut tracker,
            viewport,
            1,
            target,
            Some(10),
            Some(11)
        ));
        let update = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 1, target, Some(11))),
            )
            .unwrap();
        assert_eq!(update.change_cause, ImeChangeCause::InputMethod);
        assert!(matches!(
            update.surrounding_text,
            IMESurroundingText::Unavailable
        ));
    }

    #[test]
    fn equal_and_backwards_unavailable_revisions_poison_the_frame() {
        let viewport = ViewportId::ROOT;
        let target = target(91);

        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(
                viewport,
                Some(unavailable_observation(0, 0, target, Some(10))),
            )
            .unwrap();
        assert!(
            !unavailable_receipt(&mut tracker, viewport, 1, target, Some(10), Some(10)),
            "an equal unavailable pair cannot prove a mutation"
        );
        let equal = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 1, target, Some(10))),
            )
            .unwrap();
        assert_eq!(equal.change_cause, ImeChangeCause::Other);
        assert!(
            tracker
                .viewports
                .get(&viewport)
                .and_then(|state| state.frame.as_ref())
                .is_some_and(|frame| frame.receipt_chain.is_none()),
            "an invalid receipt clears the chain"
        );

        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(
                viewport,
                Some(unavailable_observation(0, 0, target, Some(11))),
            )
            .unwrap();
        assert!(
            !unavailable_receipt(&mut tracker, viewport, 1, target, Some(11), Some(10)),
            "a decreasing unavailable pair cannot prove a mutation"
        );
        let backwards = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 1, target, Some(11))),
            )
            .unwrap();
        assert_eq!(backwards.change_cause, ImeChangeCause::Other);
    }

    #[test]
    fn missing_unavailable_revisions_cannot_prove_a_receipt() {
        let viewport = ViewportId::ROOT;
        let target = target(92);

        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(viewport, Some(unavailable_observation(0, 0, target, None)))
            .unwrap();
        assert!(!unavailable_receipt(
            &mut tracker,
            viewport,
            1,
            target,
            None,
            None
        ));
        let missing_both = tracker
            .observe(viewport, Some(unavailable_observation(1, 1, target, None)))
            .unwrap();
        assert_eq!(missing_both.change_cause, ImeChangeCause::Other);

        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(viewport, Some(unavailable_observation(0, 0, target, None)))
            .unwrap();
        assert!(
            !unavailable_receipt(&mut tracker, viewport, 1, target, None, Some(1)),
            "a revision on only the after side is not a proof"
        );
        let missing_before = tracker
            .observe(viewport, Some(unavailable_observation(1, 1, target, None)))
            .unwrap();
        assert_eq!(missing_before.change_cause, ImeChangeCause::Other);

        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(
                viewport,
                Some(unavailable_observation(0, 0, target, Some(1))),
            )
            .unwrap();
        assert!(
            !unavailable_receipt(&mut tracker, viewport, 1, target, Some(1), None),
            "a revision on only the before side is not a proof"
        );
        let missing_after = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 1, target, Some(1))),
            )
            .unwrap();
        assert_eq!(missing_after.change_cause, ImeChangeCause::Other);
    }

    #[test]
    fn unavailable_receipt_chain_requires_exact_continuity() {
        let viewport = ViewportId::ROOT;
        let target = target(93);
        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(
                viewport,
                Some(unavailable_observation(0, 0, target, Some(1))),
            )
            .unwrap();

        assert!(unavailable_receipt(
            &mut tracker,
            viewport,
            1,
            target,
            Some(1),
            Some(2)
        ));
        assert!(unavailable_receipt(
            &mut tracker,
            viewport,
            1,
            target,
            Some(2),
            Some(3)
        ));
        assert!(
            !unavailable_receipt(&mut tracker, viewport, 1, target, Some(4), Some(5)),
            "a structurally valid pair that skips the chain's last revision is rejected"
        );
        let update = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 1, target, Some(3))),
            )
            .unwrap();
        assert_eq!(update.change_cause, ImeChangeCause::Other);
    }

    #[test]
    fn mixed_availability_receipts_require_the_unavailable_revision() {
        let viewport = ViewportId::ROOT;
        let target = target(94);

        // Unavailable -> available without a before revision is rejected.
        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(
                viewport,
                Some(unavailable_observation(0, 0, target, Some(5))),
            )
            .unwrap();
        assert!(!tracker.note_input_method_apply(
            viewport,
            1,
            target,
            ImeMutationStamp::unavailable(),
            ImeMutationStamp::from_committed("a", 1, 1, None),
        ));
        let rejected = tracker
            .observe(viewport, Some(observation(1, 1, target, "a", 1, 1)))
            .unwrap();
        assert_eq!(rejected.change_cause, ImeChangeCause::Other);

        // The same transition is accepted with the unavailable revision.
        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(
                viewport,
                Some(unavailable_observation(0, 0, target, Some(5))),
            )
            .unwrap();
        assert!(tracker.note_input_method_apply(
            viewport,
            1,
            target,
            ImeMutationStamp::unavailable_at_revision(5),
            ImeMutationStamp::from_committed("a", 1, 1, None),
        ));
        let available = tracker
            .observe(viewport, Some(observation(1, 1, target, "a", 1, 1)))
            .unwrap();
        assert_eq!(available.change_cause, ImeChangeCause::InputMethod);
        assert_eq!(payload(&available).text, "a");

        // Available -> unavailable without an after revision is rejected.
        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(viewport, Some(observation(0, 0, target, "a", 1, 1)))
            .unwrap();
        assert!(!tracker.note_input_method_apply(
            viewport,
            1,
            target,
            ImeMutationStamp::from_committed("a", 1, 1, None),
            ImeMutationStamp::unavailable(),
        ));
        let rejected = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 1, target, Some(6))),
            )
            .unwrap();
        assert_eq!(rejected.change_cause, ImeChangeCause::Other);

        // The reverse transition is accepted with the unavailable revision.
        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(viewport, Some(observation(0, 0, target, "a", 1, 1)))
            .unwrap();
        assert!(tracker.note_input_method_apply(
            viewport,
            1,
            target,
            ImeMutationStamp::from_committed("a", 1, 1, None),
            ImeMutationStamp::unavailable_at_revision(6),
        ));
        let unavailable = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 1, target, Some(6))),
            )
            .unwrap();
        assert_eq!(unavailable.change_cause, ImeChangeCause::InputMethod);
        assert!(matches!(
            unavailable.surrounding_text,
            IMESurroundingText::Unavailable
        ));
    }

    #[test]
    fn observation_revision_is_discarded_for_available_text_and_passwords() {
        let viewport = ViewportId::ROOT;
        let target = target(95);
        let mut tracker = ImeContextTracker::new();

        let available = tracker
            .observe(
                viewport,
                Some(observation(0, 0, target, "abc", 3, 3).unavailable_revision(7)),
            )
            .unwrap();
        assert_eq!(payload(&available).text, "abc");
        let baseline = &tracker
            .viewports
            .get(&viewport)
            .and_then(|state| state.context.as_ref())
            .expect("context")
            .baseline;
        assert_eq!(baseline.unavailable_revision(), None);

        let unavailable = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 1, target, Some(42))),
            )
            .unwrap();
        assert!(matches!(
            unavailable.surrounding_text,
            IMESurroundingText::Unavailable
        ));
        let baseline = &tracker
            .viewports
            .get(&viewport)
            .and_then(|state| state.context.as_ref())
            .expect("context")
            .baseline;
        assert_eq!(baseline.unavailable_revision(), Some(42));

        let password = tracker
            .observe(
                viewport,
                Some(
                    observation(2, 2, target, "secret", 6, 6)
                        .purpose(IMEPurpose::Password)
                        .unavailable_revision(9),
                ),
            )
            .unwrap();
        assert_eq!(password.purpose, IMEPurpose::Password);
        assert!(matches!(
            password.surrounding_text,
            IMESurroundingText::Unavailable
        ));
        let baseline = &tracker
            .viewports
            .get(&viewport)
            .and_then(|state| state.context.as_ref())
            .expect("context")
            .baseline;
        assert_eq!(baseline.unavailable_revision(), None);
        assert!(!format!("{tracker:?}").contains("secret"));
    }

    #[test]
    fn observation_revision_is_retained_for_a_computed_unavailable_snapshot() {
        let viewport = ViewportId::ROOT;
        let target = target(99);
        let mut tracker = ImeContextTracker::new();
        let update = tracker
            .observe(
                viewport,
                Some(observation(0, 0, target, "é", 1, 1).unavailable_revision(11)),
            )
            .unwrap();
        assert!(matches!(
            update.surrounding_text,
            IMESurroundingText::Unavailable
        ));
        let baseline = &tracker
            .viewports
            .get(&viewport)
            .and_then(|state| state.context.as_ref())
            .expect("context")
            .baseline;
        assert_eq!(baseline.unavailable_revision(), Some(11));
    }

    #[test]
    fn invalid_receipt_after_an_accepted_receipt_poisons_the_frame() {
        let viewport = ViewportId::ROOT;
        let target = target(96);
        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(
                viewport,
                Some(unavailable_observation(0, 0, target, Some(1))),
            )
            .unwrap();
        assert!(unavailable_receipt(
            &mut tracker,
            viewport,
            1,
            target,
            Some(1),
            Some(2)
        ));
        assert!(
            !unavailable_receipt(&mut tracker, viewport, 1, target, Some(2), Some(2)),
            "an equal receipt after an accepted one is still invalid"
        );
        let update = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 1, target, Some(2))),
            )
            .unwrap();
        assert_eq!(update.change_cause, ImeChangeCause::Other);
        assert!(
            tracker
                .viewports
                .get(&viewport)
                .and_then(|state| state.frame.as_ref())
                .is_some_and(|frame| frame.receipt_chain.is_none()),
            "the accepted chain does not survive an invalid receipt"
        );
    }

    #[test]
    fn revision_only_unavailable_changes_are_changed_observations_without_a_receipt() {
        let viewport = ViewportId::ROOT;
        let target = target(97);
        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(
                viewport,
                Some(unavailable_observation(0, 0, target, Some(1))),
            )
            .unwrap();

        assert!(unavailable_receipt(
            &mut tracker,
            viewport,
            1,
            target,
            Some(1),
            Some(2)
        ));
        let accepted = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 1, target, Some(2))),
            )
            .unwrap();
        assert_eq!(accepted.change_cause, ImeChangeCause::InputMethod);

        // The snapshot is still Unavailable, so only the revision shows this change. Without a
        // receipt it must not inherit the previous pass's InputMethod cause.
        let advanced = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 2, target, Some(3))),
            )
            .unwrap();
        assert_eq!(advanced.change_cause, ImeChangeCause::Other);
        assert!(matches!(
            advanced.surrounding_text,
            IMESurroundingText::Unavailable
        ));

        let unchanged = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 3, target, Some(3))),
            )
            .unwrap();
        assert_eq!(unchanged.change_cause, ImeChangeCause::Other);
    }

    #[test]
    fn exhausted_or_absent_observation_revisions_cannot_mint_receipts() {
        let viewport = ViewportId::ROOT;
        let target = target(98);

        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(viewport, Some(unavailable_observation(0, 0, target, None)))
            .unwrap();
        assert!(!unavailable_receipt(
            &mut tracker,
            viewport,
            1,
            target,
            None,
            None
        ));
        let exhausted = tracker
            .observe(viewport, Some(unavailable_observation(1, 1, target, None)))
            .unwrap();
        assert_eq!(exhausted.change_cause, ImeChangeCause::Other);

        let mut tracker = ImeContextTracker::new();
        tracker
            .observe(
                viewport,
                Some(unavailable_observation(0, 0, target, Some(u64::MAX))),
            )
            .unwrap();
        assert!(
            !unavailable_receipt(&mut tracker, viewport, 1, target, Some(u64::MAX), None),
            "a maximum revision with no successor cannot prove a pair"
        );
        let maximum = tracker
            .observe(
                viewport,
                Some(unavailable_observation(1, 1, target, Some(u64::MAX))),
            )
            .unwrap();
        assert_eq!(maximum.change_cause, ImeChangeCause::Other);
    }
}
