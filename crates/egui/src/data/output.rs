//! All the data egui returns to the backend at the end of each frame.

use core::ops::Range;

use epaint::text::CharIndex;

use crate::{OrderedViewportIdMap, RepaintCause, Role, ViewportOutput};

/// What egui emits each frame from [`crate::Context::run_ui`].
///
/// The backend should use this.
#[derive(Clone, Default)]
pub struct FullOutput {
    /// Non-rendering related output.
    pub platform_output: PlatformOutput,

    /// Texture changes since last frame (including the font texture).
    ///
    /// The backend needs to apply [`crate::TexturesDelta::push`] _before_ painting,
    /// and free any texture in [`crate::TexturesDelta::free`] _after_ painting.
    ///
    /// It is assumed that all egui viewports share the same painter and texture namespace.
    pub textures_delta: epaint::textures::TexturesDelta,

    /// What to paint.
    ///
    /// You can use [`crate::Context::tessellate`] to turn this into triangles.
    pub shapes: Vec<epaint::ClippedShape>,

    /// The number of physical pixels per logical ui point, for the viewport that was updated.
    ///
    /// You can pass this to [`crate::Context::tessellate`] together with [`Self::shapes`].
    pub pixels_per_point: f32,

    /// All the active viewports, including the root.
    ///
    /// It is up to the integration to spawn a native window for each viewport,
    /// and to close any window that no longer has a viewport in this map.
    pub viewport_output: OrderedViewportIdMap<ViewportOutput>,
}

impl FullOutput {
    /// Add on new output.
    pub fn append(&mut self, newer: Self) {
        use alloc::collections::btree_map::Entry;

        let Self {
            platform_output,
            textures_delta,
            shapes,
            pixels_per_point,
            viewport_output,
        } = newer;

        self.platform_output.append(platform_output);
        self.textures_delta.append(textures_delta);
        self.shapes = shapes; // Only paint the latest
        self.pixels_per_point = pixels_per_point; // Use latest

        for (id, new_viewport) in viewport_output {
            match self.viewport_output.entry(id) {
                Entry::Vacant(entry) => {
                    entry.insert(new_viewport);
                }
                Entry::Occupied(mut entry) => {
                    entry.get_mut().append(new_viewport);
                }
            }
        }
    }

    /// [`epaint::textures::TexturesDelta`] will panic when dropped with still unapplied deltas,
    /// this is a helper to clear the deltas.
    pub fn drop_without_applying_deltas(mut self) {
        self.textures_delta.clear();
    }
}

/// What egui emits from [`crate::Context::run_logic`], i.e. from a tick where no ui was shown.
///
/// There is nothing to paint, but the app may still have asked the integration to do things,
/// e.g. to show a hidden window again with [`crate::ViewportCommand::Focus`].
#[derive(Clone, Default)]
pub struct LogicOutput {
    /// Non-rendering related output.
    pub platform_output: PlatformOutput,

    /// The commands sent with [`crate::Context::send_viewport_cmd`] and friends.
    ///
    /// Note that this contains no information about which viewports exist:
    /// the integration should leave its viewports as they are.
    pub viewport_commands: OrderedViewportIdMap<Vec<crate::ViewportCommand>>,
}

/// The maximum number of UTF-8 bytes in an outgoing IME surrounding-text payload.
///
/// The payload is bounded so that producing it never copies a whole large document. A
/// selection longer than this is reported as [`IMESurroundingText::Unavailable`] instead of
/// being silently truncated.
pub const MAX_IME_SURROUNDING_TEXT_BYTES: usize = 4000;

/// Text around the IME cursor, ready to be sent to the platform input method.
///
/// An empty [`Self::Available`] payload is a valid empty document and is distinct from
/// [`Self::Unavailable`], which means that the owner cannot provide surrounding text
/// (for example a password field, a too-wide selection, or an invalid editor state).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum IMESurroundingText {
    /// The owner cannot provide surrounding text.
    #[default]
    Unavailable,

    /// A bounded, committed-text snapshot around the cursor.
    Available(IMESurroundingTextPayload),
}

/// Committed text around an IME cursor and UTF-8 byte offsets into it.
///
/// `cursor` is the primary end of the selection and `anchor` is the other end, both as
/// UTF-8 byte offsets into [`Self::text`]. `anchor` may be greater than `cursor` for a
/// reversed selection.
#[derive(Clone, PartialEq, Eq)]
pub struct IMESurroundingTextPayload {
    /// Committed text, with any IME pre-edit removed.
    ///
    /// This is transient and is deliberately omitted from `Debug` output and serialization.
    pub text: String,

    /// UTF-8 byte offset of the primary cursor in [`Self::text`].
    pub cursor: usize,

    /// UTF-8 byte offset of the selection anchor in [`Self::text`].
    pub anchor: usize,
}

impl core::fmt::Debug for IMESurroundingTextPayload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IMESurroundingTextPayload")
            .field("text", &"<redacted>")
            .field("text_len_bytes", &self.text.len())
            .field("cursor", &self.cursor)
            .field("anchor", &self.anchor)
            .finish()
    }
}

impl IMESurroundingText {
    /// Build a bounded surrounding-text payload from committed editor text and an optional
    /// pre-edit range to remove.
    ///
    /// `cursor` and `anchor` are UTF-8 byte offsets into `text`, in either order. `preedit`
    /// is the byte range of the current IME composition, which is removed before offsets are
    /// remapped. Invalid byte offsets or ranges produce [`Self::Unavailable`].
    ///
    /// At most [`MAX_IME_SURROUNDING_TEXT_BYTES`] bytes of committed text are copied. Only the
    /// selected window is cloned; this does not clone the full document. If the selection itself
    /// is wider than the limit, the result is [`Self::Unavailable`].
    pub fn from_text(
        text: &str,
        cursor: usize,
        anchor: usize,
        preedit: Option<Range<usize>>,
    ) -> Self {
        if cursor > text.len()
            || anchor > text.len()
            || !text.is_char_boundary(cursor)
            || !text.is_char_boundary(anchor)
        {
            return Self::Unavailable;
        }

        let preedit = match preedit {
            Some(range)
                if range.start <= range.end
                    && range.end <= text.len()
                    && text.is_char_boundary(range.start)
                    && text.is_char_boundary(range.end) =>
            {
                Some(range)
            }
            Some(_) => return Self::Unavailable,
            None => None,
        };

        let removed_bytes = preedit.as_ref().map_or(0, |range| range.end - range.start);
        let logical_len = text.len() - removed_bytes;

        let map_to_logical = |byte: usize| -> usize {
            let Some(preedit) = &preedit else {
                return byte;
            };
            if byte > preedit.start && byte < preedit.end {
                preedit.start
            } else if byte >= preedit.end {
                byte - removed_bytes
            } else {
                byte
            }
        };

        let cursor = map_to_logical(cursor);
        let anchor = map_to_logical(anchor);
        let selection_start = cursor.min(anchor);
        let selection_end = cursor.max(anchor);

        let selection_len = selection_end - selection_start;
        if selection_len > MAX_IME_SURROUNDING_TEXT_BYTES {
            return Self::Unavailable;
        }

        // Spend the remaining byte budget on both sides of the selection, giving unused
        // room on one side to the other.
        let budget = MAX_IME_SURROUNDING_TEXT_BYTES - selection_len;
        let before_available = selection_start;
        let after_available = logical_len - selection_end;
        let mut before = (budget / 2).min(before_available);
        let mut after = (budget - before).min(after_available);
        before = (budget - after).min(before_available);
        after = (budget - before).min(after_available);

        let mut window_start = selection_start - before;
        let mut window_end = selection_end + after;

        let is_char_boundary = |logical_byte: usize| -> bool {
            let Some(preedit) = &preedit else {
                return text.is_char_boundary(logical_byte);
            };
            if logical_byte == preedit.start {
                true
            } else if logical_byte < preedit.start {
                text.is_char_boundary(logical_byte)
            } else {
                text.is_char_boundary(logical_byte + removed_bytes)
            }
        };

        // Byte-budget windows can land inside a multi-byte character. Shrink the window rather
        // than growing it, so the output cannot exceed the limit and the selection stays inside.
        while window_start < selection_start && !is_char_boundary(window_start) {
            window_start += 1;
        }
        while selection_end < window_end && !is_char_boundary(window_end) {
            window_end -= 1;
        }

        let to_original = |logical_byte: usize| -> usize {
            match &preedit {
                Some(preedit) if logical_byte > preedit.start => logical_byte + removed_bytes,
                _ => logical_byte,
            }
        };

        let original_start = to_original(window_start);
        let original_end = to_original(window_end);
        let mut window_text = String::with_capacity(window_end - window_start);

        match &preedit {
            Some(preedit) => {
                if window_start < preedit.start {
                    let first_end = original_end.min(preedit.start);
                    window_text.push_str(&text[original_start..first_end]);
                }
                if preedit.start < window_end {
                    let second_start = preedit.end.max(original_start);
                    window_text.push_str(&text[second_start..original_end]);
                }
            }
            None => window_text.push_str(&text[original_start..original_end]),
        }

        debug_assert!(
            window_text.len() <= MAX_IME_SURROUNDING_TEXT_BYTES,
            "surrounding text must fit the protocol byte limit"
        );
        debug_assert_eq!(
            window_text.len(),
            window_end - window_start,
            "removing preedit must preserve the selected logical window length"
        );

        Self::Available(IMESurroundingTextPayload {
            text: window_text,
            cursor: cursor - window_start,
            anchor: anchor - window_start,
        })
    }

    /// The available payload, if any.
    pub fn available(&self) -> Option<&IMESurroundingTextPayload> {
        match self {
            Self::Available(payload) => Some(payload),
            Self::Unavailable => None,
        }
    }
}

/// Why the committed surrounding-text snapshot changed.
///
/// The platform input method uses this to tell an accepted IME commit or deletion from an
/// application-origin edit (typing, paste, undo, automatic corrections, a document load, and so on).
/// [`Self::Other`] is the conservative default: only a producer that has verified an accepted
/// IME mutation may report [`Self::InputMethod`].
///
/// With the `serde` feature the variant names are serialized exactly as `"InputMethod"` and
/// `"Other"`. A missing field deserializes as [`Self::Other`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum ImeChangeCause {
    /// The change came from an accepted IME commit or delete-surrounding mutation.
    InputMethod,

    /// The change came from the application, or its origin could not be proven to be IME.
    #[default]
    Other,
}

/// Information about text being edited.
///
/// Useful for IME.
///
/// This is not `Copy`: an available surrounding-text payload owns a bounded copy of the
/// committed text around the cursor. The debug representation redacts that text.
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct IMEOutput {
    /// The widget that owns the IME context.
    ///
    /// This is present even when [`Self::surrounding_text`] is
    /// [`IMESurroundingText::Unavailable`].
    pub owner: crate::Id,

    /// A generation for this owner's IME context.
    ///
    /// It changes when this owner acquires focus or changes IME purpose, and stays stable for
    /// ordinary edits while the owner remains focused. Together with [`Self::owner`] it lets an
    /// integration reject stale owner/support transitions without mistaking text edits for a
    /// new context.
    pub context_generation: u64,

    /// IME's purpose.
    pub purpose: crate::IMEPurpose,

    /// Where the [`crate::TextEdit`] is located on screen.
    pub rect: crate::Rect,

    /// Where the primary cursor is.
    ///
    /// This is a very thin rectangle.
    pub cursor_rect: crate::Rect,

    /// Whether any ongoing IME composition should be interrupted.
    pub should_interrupt_composition: bool,

    /// Request-bound cancellation fence, created at the actual application interruption.
    ///
    /// The integration must wait for its actual native completion before emitting
    /// [`crate::Event::ImeResetCompleted`]. `None` retains the stock untokened interrupt API.
    #[cfg_attr(feature = "serde", serde(default))]
    pub reset_request: Option<crate::ImeResetId>,

    /// Why the surrounding-text snapshot changed.
    ///
    /// [`ImeChangeCause::Other`] is used for context transitions, application edits and any
    /// change whose origin cannot be proven. Cause alone never changes the snapshot key, so an
    /// integration must not resend identical text merely because this value differs.
    ///
    /// Defaults to [`ImeChangeCause::Other`] when deserializing older data.
    #[cfg_attr(feature = "serde", serde(default))]
    pub change_cause: ImeChangeCause,

    /// The committed text around the cursor, or an explicit reason it is unavailable.
    ///
    /// Skipped from serialization because the text is transient editor state.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub surrounding_text: IMESurroundingText,
}

impl core::fmt::Debug for IMEOutput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IMEOutput")
            .field("owner", &self.owner)
            .field("context_generation", &self.context_generation)
            .field("purpose", &self.purpose)
            .field("rect", &self.rect)
            .field("cursor_rect", &self.cursor_rect)
            .field(
                "should_interrupt_composition",
                &self.should_interrupt_composition,
            )
            .field("reset_request", &self.reset_request)
            .field("change_cause", &self.change_cause)
            .field("surrounding_text", &self.surrounding_text)
            .finish()
    }
}

impl IMEOutput {
    /// Create an IME descriptor without surrounding text.
    ///
    /// Callers that can provide surrounding text should assign
    /// [`Self::surrounding_text`] from [`IMESurroundingText::from_text`]. An explicit
    /// [`IMESurroundingText::Unavailable`] (the default here) is not the same as no IME
    /// descriptor at all.
    ///
    /// `owner` and `context_generation` identify one IME context: the generation must change
    /// when this owner acquires IME focus, and stay stable for ordinary text or selection
    /// updates. The surrounding text must be logical model text, not a visually reordered
    /// display string, with UTF-8 byte offsets into that text.
    ///
    /// The new descriptor uses [`ImeChangeCause::Other`]. A producer that has accepted an IME
    /// mutation must set [`Self::change_cause`] explicitly after the change.
    ///
    /// ```
    /// use egui::{Id, IMEPurpose, Rect};
    /// use egui::output::{IMEOutput, IMESurroundingText};
    ///
    /// let mut ime = IMEOutput::new(
    ///     Id::unique("editor"),
    ///     1,
    ///     IMEPurpose::Normal,
    ///     Rect::ZERO,
    ///     Rect::ZERO,
    /// );
    /// ime.surrounding_text = IMESurroundingText::from_text("hello", 5, 5, None);
    /// assert!(ime.surrounding_text.available().is_some());
    /// ```
    pub fn new(
        owner: crate::Id,
        context_generation: u64,
        purpose: crate::IMEPurpose,
        rect: crate::Rect,
        cursor_rect: crate::Rect,
    ) -> Self {
        Self {
            owner,
            context_generation,
            purpose,
            rect,
            cursor_rect,
            should_interrupt_composition: false,
            reset_request: None,
            change_cause: ImeChangeCause::Other,
            surrounding_text: IMESurroundingText::Unavailable,
        }
    }
}

/// Commands that the egui integration should execute at the end of a frame.
///
/// Commands that are specific to a viewport should be put in [`crate::ViewportCommand`] instead.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum OutputCommand {
    /// Put this text to the system clipboard.
    ///
    /// This is often a response to [`crate::Event::Copy`] or [`crate::Event::Cut`].
    CopyText(String),

    /// The user has finished changing the text selection, and this is what is now selected.
    ///
    /// Sent once the selection settles (e.g. when the mouse button is released after a drag),
    /// and only if it changed, so not on every frame of a drag.
    ///
    /// On X11 and Wayland, `egui-winit` puts this in the PRIMARY selection,
    /// so that it can be pasted with the middle mouse button.
    TextSelectionSettled(String),

    /// Put this image to the system clipboard.
    CopyImage(crate::ColorImage),

    /// Open this url in a browser.
    OpenUrl(OpenUrl),
}

/// The non-rendering part of what egui emits each frame.
///
/// You can access (and modify) this with [`crate::Context::output`].
///
/// The backend should use this.
#[derive(Default, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct PlatformOutput {
    /// Commands that the egui integration should execute at the end of a frame.
    pub commands: Vec<OutputCommand>,

    /// Set the cursor to this icon.
    pub cursor_icon: CursorIcon,

    /// If set, the integration should display this RGBA image as the OS
    /// cursor (via e.g. `winit::window::CustomCursor`) instead of the
    /// standard `cursor_icon`. Set per frame; integrations that don't
    /// support custom cursors fall back to `cursor_icon`.
    ///
    /// Skipped from serde because the bitmap is ephemeral and shouldn't
    /// roundtrip through persisted state.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub cursor_image: Option<CustomCursorImage>,

    /// Events that may be useful to e.g. a screen reader.
    pub events: Vec<OutputEvent>,

    /// Is there a mutable [`TextEdit`](crate::TextEdit) under the cursor?
    /// Use by `eframe` web to show/hide mobile keyboard and IME agent.
    pub mutable_text_under_cursor: bool,

    /// This is set if, and only if, the user is currently editing text.
    ///
    /// Useful for IME.
    ///
    /// This field should only be set by the widget that currently owns IME
    /// events (see [`crate::Memory::owns_ime_events`]).
    pub ime: Option<IMEOutput>,

    /// The difference in the widget tree since last frame.
    ///
    /// NOTE: this needs to be per-viewport.
    pub accesskit_update: Option<accesskit::TreeUpdate>,

    /// How many ui passes is this the sum of?
    ///
    /// See [`crate::Context::request_discard`] for details.
    ///
    /// This is incremented at the END of each frame,
    /// so this will be `0` for the first pass.
    pub num_completed_passes: usize,

    /// Was [`crate::Context::request_discard`] called during the latest pass?
    ///
    /// If so, what was the reason(s) for it?
    ///
    /// If empty, there was never any calls.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub request_discard_reasons: Vec<RepaintCause>,
}

impl PlatformOutput {
    /// This can be used by a text-to-speech system to describe the events (if any).
    pub fn events_description(&self) -> String {
        // only describe last event:
        if let Some(event) = self.events.iter().next_back() {
            match event {
                OutputEvent::Clicked(widget_info)
                | OutputEvent::DoubleClicked(widget_info)
                | OutputEvent::TripleClicked(widget_info)
                | OutputEvent::FocusGained(widget_info)
                | OutputEvent::TextSelectionChanged(widget_info)
                | OutputEvent::ValueChanged(widget_info) => {
                    return widget_info.description();
                }
            }
        }
        Default::default()
    }

    /// Add on new output.
    pub fn append(&mut self, newer: Self) {
        let Self {
            mut commands,
            cursor_icon,
            cursor_image,
            mut events,
            mutable_text_under_cursor,
            ime,
            accesskit_update,
            num_completed_passes,
            mut request_discard_reasons,
        } = newer;

        self.commands.append(&mut commands);
        self.cursor_icon = cursor_icon;
        self.cursor_image = cursor_image;
        self.events.append(&mut events);
        self.mutable_text_under_cursor = mutable_text_under_cursor;
        // The latest completed pass is authoritative, including an explicit `None`.
        // A payload/owner from an earlier pass must never survive a later pass that no
        // longer has mutable text focus.
        //
        // An interrupt is different: accesskit or another in-pass mutation can request one during
        // a discarded pass, and a later pass that recreates the same owner/generation must still
        // cancel the platform composition. Carry it only across the same IME context; an
        // interrupt for an old owner or generation must not leak into a new editor.
        let keep_interrupt = self.ime.as_ref().is_some_and(|previous| {
            ime.as_ref().is_some_and(|next| {
                previous.owner == next.owner
                    && previous.context_generation == next.context_generation
                    // A retired token must not become an untokened stock interruption.
                    // Keep the historical purpose policy for genuine untokened interrupts.
                    && (previous.reset_request.is_none() || previous.purpose == next.purpose)
                    && previous.should_interrupt_composition
            })
        });
        let previous_reset = self.ime.as_ref().and_then(|previous| {
            ime.as_ref()
                .filter(|next| {
                    previous.owner == next.owner
                        && previous.context_generation == next.context_generation
                        && previous.purpose == next.purpose
                })
                .and(previous.reset_request)
        });
        let mut ime = ime;
        if let Some(ime) = &mut ime {
            // A later actual interruption supersedes an earlier caret request in this context.
            ime.reset_request = ime.reset_request.or(previous_reset);
            if keep_interrupt || ime.reset_request.is_some() {
                ime.should_interrupt_composition = true;
            }
        }
        self.ime = ime;
        self.num_completed_passes += num_completed_passes;
        self.request_discard_reasons
            .append(&mut request_discard_reasons);

        // egui produces a complete AccessKit tree for each frame, so overwrite rather than append:
        self.accesskit_update = accesskit_update;
    }

    /// Take everything ephemeral (everything except `cursor_icon` and
    /// `cursor_image` currently)
    pub fn take(&mut self) -> Self {
        let taken = core::mem::take(self);
        self.cursor_icon = taken.cursor_icon; // sticky between frames
        self.cursor_image = taken.cursor_image.clone(); // sticky between frames
        taken
    }

    /// Was [`crate::Context::request_discard`] called?
    pub fn requested_discard(&self) -> bool {
        !self.request_discard_reasons.is_empty()
    }
}

/// What URL to open, and how.
///
/// Use with [`crate::Context::open_url`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct OpenUrl {
    pub url: String,

    /// If `true`, open the url in a new tab.
    /// If `false` open it in the same tab.
    /// Only matters when in a web browser.
    pub new_tab: bool,
}

impl OpenUrl {
    #[expect(clippy::needless_pass_by_value)]
    pub fn same_tab(url: impl ToString) -> Self {
        Self {
            url: url.to_string(),
            new_tab: false,
        }
    }

    #[expect(clippy::needless_pass_by_value)]
    pub fn new_tab(url: impl ToString) -> Self {
        Self {
            url: url.to_string(),
            new_tab: true,
        }
    }
}

/// Types of attention to request from a user when a native window is not in focus.
///
/// See [winit's documentation][user_attention_type] for platform-specific meaning of the attention types.
///
/// [user_attention_type]: https://docs.rs/winit/latest/winit/window/enum.UserAttentionType.html
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum UserAttentionType {
    /// Request an elevated amount of animations and flair for the window and the task bar or dock icon.
    Critical,

    /// Request a standard amount of attention-grabbing actions.
    Informational,

    /// Reset the attention request and interrupt related animations and flashes.
    Reset,
}

/// A bitmap cursor pushed to the integration via [`PlatformOutput::cursor_image`].
///
/// The integration is expected to upload this to the OS as a real cursor
/// (so the image is not clipped by the egui window — what `egui::Painter`
/// drawn cursors suffer from). Backends that don't support it should fall
/// back to [`PlatformOutput::cursor_icon`].
///
/// `rgba` is straight (non-premultiplied) RGBA — same encoding as
/// `winit::window::CustomCursor::from_rgba`. The buffer length must be
/// exactly `size[0] * size[1] * 4` bytes. `size` and `hotspot` use
/// `u16` to match winit's native types and avoid a lossy cast in the
/// integration layer.
///
/// `Arc<[u8]>` is used so integrations can dedupe / cache by pointer
/// identity (`Arc::ptr_eq`) and avoid re-uploading the same bitmap to
/// the OS every frame.
#[derive(Clone, PartialEq, Eq)]
pub struct CustomCursorImage {
    pub rgba: alloc::sync::Arc<[u8]>,
    pub size: [u16; 2],
    pub hotspot: [u16; 2],
}

impl core::fmt::Debug for CustomCursorImage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CustomCursorImage")
            .field("size", &self.size)
            .field("hotspot", &self.hotspot)
            .field("rgba_len", &self.rgba.len())
            .finish()
    }
}

/// A mouse cursor icon.
///
/// egui emits a [`CursorIcon`] in [`PlatformOutput`] each frame as a request to the integration.
///
/// Loosely based on <https://developer.mozilla.org/en-US/docs/Web/CSS/cursor>.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum CursorIcon {
    /// Normal cursor icon, whatever that is.
    #[default]
    Default,

    /// Show no cursor
    None,

    // ------------------------------------
    // Links and status:
    /// A context menu is available
    ContextMenu,

    /// Question mark
    Help,

    /// Pointing hand, used for e.g. web links
    PointingHand,

    /// Shows that processing is being done, but that the program is still interactive.
    Progress,

    /// Not yet ready, try later.
    Wait,

    // ------------------------------------
    // Selection:
    /// Hover a cell in a table
    Cell,

    /// For precision work
    Crosshair,

    /// Text caret, e.g. "Click here to edit text"
    Text,

    /// Vertical text caret, e.g. "Click here to edit vertical text"
    VerticalText,

    // ------------------------------------
    // Drag-and-drop:
    /// Indicated an alias, e.g. a shortcut
    Alias,

    /// Indicate that a copy will be made
    Copy,

    /// Omnidirectional move icon (e.g. arrows in all cardinal directions)
    Move,

    /// Can't drop here
    NoDrop,

    /// Forbidden
    NotAllowed,

    /// The thing you are hovering can be grabbed
    Grab,

    /// You are grabbing the thing you are hovering
    Grabbing,

    // ------------------------------------
    /// Something can be scrolled in any direction (panned).
    AllScroll,

    // ------------------------------------
    // Resizing in two directions:
    /// Horizontal resize `-` to make something wider or more narrow (left to/from right)
    ResizeHorizontal,

    /// Diagonal resize `/` (right-up to/from left-down)
    ResizeNeSw,

    /// Diagonal resize `\` (left-up to/from right-down)
    ResizeNwSe,

    /// Vertical resize `|` (up-down or down-up)
    ResizeVertical,

    // ------------------------------------
    // Resizing in one direction:
    /// Resize something rightwards (e.g. when dragging the right-most edge of something)
    ResizeEast,

    /// Resize something down and right (e.g. when dragging the bottom-right corner of something)
    ResizeSouthEast,

    /// Resize something downwards (e.g. when dragging the bottom edge of something)
    ResizeSouth,

    /// Resize something down and left (e.g. when dragging the bottom-left corner of something)
    ResizeSouthWest,

    /// Resize something leftwards (e.g. when dragging the left edge of something)
    ResizeWest,

    /// Resize something up and left (e.g. when dragging the top-left corner of something)
    ResizeNorthWest,

    /// Resize something up (e.g. when dragging the top edge of something)
    ResizeNorth,

    /// Resize something up and right (e.g. when dragging the top-right corner of something)
    ResizeNorthEast,

    // ------------------------------------
    /// Resize a column
    ResizeColumn,

    /// Resize a row
    ResizeRow,

    // ------------------------------------
    // Zooming:
    /// Enhance!
    ZoomIn,

    /// Let's get a better overview
    ZoomOut,
}

impl CursorIcon {
    pub const ALL: [Self; 35] = [
        Self::Default,
        Self::None,
        Self::ContextMenu,
        Self::Help,
        Self::PointingHand,
        Self::Progress,
        Self::Wait,
        Self::Cell,
        Self::Crosshair,
        Self::Text,
        Self::VerticalText,
        Self::Alias,
        Self::Copy,
        Self::Move,
        Self::NoDrop,
        Self::NotAllowed,
        Self::Grab,
        Self::Grabbing,
        Self::AllScroll,
        Self::ResizeHorizontal,
        Self::ResizeNeSw,
        Self::ResizeNwSe,
        Self::ResizeVertical,
        Self::ResizeEast,
        Self::ResizeSouthEast,
        Self::ResizeSouth,
        Self::ResizeSouthWest,
        Self::ResizeWest,
        Self::ResizeNorthWest,
        Self::ResizeNorth,
        Self::ResizeNorthEast,
        Self::ResizeColumn,
        Self::ResizeRow,
        Self::ZoomIn,
        Self::ZoomOut,
    ];
}

/// Things that happened during this frame that the integration may be interested in.
///
/// In particular, these events may be useful for accessibility, i.e. for screen readers.
#[derive(Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum OutputEvent {
    /// A widget was clicked.
    Clicked(WidgetInfo),

    /// A widget was double-clicked.
    DoubleClicked(WidgetInfo),

    /// A widget was triple-clicked.
    TripleClicked(WidgetInfo),

    /// A widget gained keyboard focus (by tab key).
    FocusGained(WidgetInfo),

    /// Text selection was updated.
    TextSelectionChanged(WidgetInfo),

    /// A widget's value changed.
    ValueChanged(WidgetInfo),
}

impl OutputEvent {
    pub fn widget_info(&self) -> &WidgetInfo {
        match self {
            Self::Clicked(info)
            | Self::DoubleClicked(info)
            | Self::TripleClicked(info)
            | Self::FocusGained(info)
            | Self::TextSelectionChanged(info)
            | Self::ValueChanged(info) => info,
        }
    }
}

impl core::fmt::Debug for OutputEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Clicked(wi) => write!(f, "Clicked({wi:?})"),
            Self::DoubleClicked(wi) => write!(f, "DoubleClicked({wi:?})"),
            Self::TripleClicked(wi) => write!(f, "TripleClicked({wi:?})"),
            Self::FocusGained(wi) => write!(f, "FocusGained({wi:?})"),
            Self::TextSelectionChanged(wi) => write!(f, "TextSelectionChanged({wi:?})"),
            Self::ValueChanged(wi) => write!(f, "ValueChanged({wi:?})"),
        }
    }
}

/// Describes a widget such as a [`crate::Button`] or a [`crate::TextEdit`].
#[derive(Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct WidgetInfo {
    /// The accessibility role of this widget.
    pub role: Role,

    /// Whether the widget is enabled.
    pub enabled: bool,

    /// The text on labels, buttons, checkboxes etc.
    pub label: Option<String>,

    /// The contents of some editable text (for [`TextEdit`](crate::TextEdit) fields).
    pub current_text_value: Option<String>,

    /// The previous text value.
    pub prev_text_value: Option<String>,

    /// The current value of checkboxes and radio buttons.
    pub selected: Option<bool>,

    /// The current value of sliders etc.
    pub value: Option<f64>,

    /// Selected range of characters in [`Self::current_text_value`].
    ///
    /// The range is `start..end` in *character* offsets (not bytes), with `end` exclusive.
    pub text_selection: Option<Range<CharIndex>>,

    /// The hint text for text edit fields.
    pub hint_text: Option<String>,
}

impl core::fmt::Debug for WidgetInfo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self {
            role,
            enabled,
            label,
            current_text_value: text_value,
            prev_text_value,
            selected,
            value,
            text_selection,
            hint_text,
        } = self;

        let mut s = f.debug_struct("WidgetInfo");

        s.field("role", role);

        if !enabled {
            s.field("enabled", enabled);
        }

        if let Some(label) = label {
            s.field("label", label);
        }
        if let Some(text_value) = text_value {
            s.field("text_value", text_value);
        }
        if let Some(prev_text_value) = prev_text_value {
            s.field("prev_text_value", prev_text_value);
        }
        if let Some(selected) = selected {
            s.field("selected", selected);
        }
        if let Some(value) = value {
            s.field("value", value);
        }
        if let Some(text_selection) = text_selection {
            s.field("text_selection", text_selection);
        }
        if let Some(hint_text) = hint_text {
            s.field("hint_text", hint_text);
        }

        s.finish()
    }
}

/// A human-readable, lowercase name for a widget role, e.g. for a text-to-speech system.
///
/// The [`Role`] is spelled out in `CamelCase`, so we split it into lowercase words.
/// [`Role::Unknown`] has no useful name, and gives an empty string.
///
/// ```
/// # use egui::{Role, role_description};
/// assert_eq!(role_description(Role::Button), "button");
/// assert_eq!(role_description(Role::CheckBox), "check box");
/// assert_eq!(role_description(Role::SpinButton), "spin button");
/// assert_eq!(role_description(Role::Unknown), "");
/// ```
pub fn role_description(role: Role) -> String {
    if role == Role::Unknown {
        return String::new();
    }

    let mut description = String::new();
    for ch in format!("{role:?}").chars() {
        if ch.is_ascii_uppercase() && !description.is_empty() {
            description.push(' ');
        }
        description.push(ch.to_ascii_lowercase());
    }
    description
}

impl WidgetInfo {
    pub fn new(role: Role) -> Self {
        Self {
            role,
            enabled: true,
            label: None,
            current_text_value: None,
            prev_text_value: None,
            selected: None,
            value: None,
            text_selection: None,
            hint_text: None,
        }
    }

    #[expect(clippy::needless_pass_by_value)]
    pub fn labeled(role: Role, enabled: bool, label: impl ToString) -> Self {
        Self {
            enabled,
            label: Some(label.to_string()),
            ..Self::new(role)
        }
    }

    /// checkboxes, radio-buttons etc
    #[expect(clippy::needless_pass_by_value)]
    pub fn selected(role: Role, enabled: bool, selected: bool, label: impl ToString) -> Self {
        Self {
            enabled,
            label: Some(label.to_string()),
            selected: Some(selected),
            ..Self::new(role)
        }
    }

    pub fn drag_value(enabled: bool, value: f64) -> Self {
        Self {
            enabled,
            value: Some(value),
            ..Self::new(Role::SpinButton)
        }
    }

    #[expect(clippy::needless_pass_by_value)]
    pub fn slider(enabled: bool, value: f64, label: impl ToString) -> Self {
        let label = label.to_string();
        Self {
            enabled,
            label: if label.is_empty() { None } else { Some(label) },
            value: Some(value),
            ..Self::new(Role::Slider)
        }
    }

    #[expect(clippy::needless_pass_by_value)]
    pub fn text_edit(
        enabled: bool,
        prev_text_value: impl ToString,
        text_value: impl ToString,
        hint_text: impl ToString,
    ) -> Self {
        let text_value = text_value.to_string();
        let prev_text_value = prev_text_value.to_string();
        let hint_text = hint_text.to_string();
        let prev_text_value = if text_value == prev_text_value {
            None
        } else {
            Some(prev_text_value)
        };
        Self {
            enabled,
            current_text_value: Some(text_value),
            prev_text_value,
            hint_text: Some(hint_text),
            ..Self::new(Role::TextInput)
        }
    }

    #[expect(clippy::needless_pass_by_value)]
    pub fn text_selection_changed(
        enabled: bool,
        text_selection: Range<CharIndex>,
        current_text_value: impl ToString,
    ) -> Self {
        Self {
            enabled,
            text_selection: Some(text_selection),
            current_text_value: Some(current_text_value.to_string()),
            ..Self::new(Role::TextInput)
        }
    }

    /// This can be used by a text-to-speech system to describe the widget.
    pub fn description(&self) -> String {
        let Self {
            role,
            enabled,
            label,
            current_text_value: text_value,
            prev_text_value: _,
            selected,
            value,
            text_selection: _,
            hint_text: _,
        } = self;

        let mut description = role_description(*role);

        if let Some(selected) = selected {
            if *role == Role::CheckBox {
                let state = if *selected { "checked" } else { "unchecked" };
                description = format!("{state} {description}");
            } else {
                description += if *selected { "selected" } else { "" };
            }
        }

        if let Some(label) = label {
            description = format!("{label}: {description}");
        }

        if role == &Role::TextInput {
            let text = if let Some(text_value) = text_value {
                if text_value.is_empty() {
                    "blank".into()
                } else {
                    text_value.clone()
                }
            } else {
                "blank".into()
            };
            description = format!("{text}: {description}");
        }

        if let Some(value) = value {
            description += " ";
            description += &value.to_string();
        }

        if !enabled {
            description += ": disabled";
        }
        description.trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IMEPurpose, Id, Rect, pos2};

    fn ime(owner: Id, generation: u64, text: IMESurroundingText) -> IMEOutput {
        IMEOutput {
            owner,
            context_generation: generation,
            purpose: IMEPurpose::Normal,
            rect: Rect::from_min_size(pos2(0.0, 0.0), crate::vec2(100.0, 20.0)),
            cursor_rect: Rect::from_min_size(pos2(1.0, 2.0), crate::vec2(1.0, 16.0)),
            should_interrupt_composition: false,
            reset_request: None,
            change_cause: ImeChangeCause::Other,
            surrounding_text: text,
        }
    }

    #[test]
    fn empty_available_text_is_not_unavailable() {
        let empty = IMESurroundingText::from_text("", 0, 0, None);
        let payload = empty.available().expect("empty document is available");
        assert_eq!(payload.text, "");
        assert_eq!((payload.cursor, payload.anchor), (0, 0));
        assert!(IMESurroundingText::Unavailable.available().is_none());
    }

    #[test]
    fn unicode_offsets_are_utf8_bytes_and_keep_both_selection_directions() {
        let text = "aé🇯🇵b";
        let after_accent = "aé".len();
        let after_flag = "aé🇯🇵".len();

        let forward = IMESurroundingText::from_text(text, after_accent, after_flag, None);
        let payload = forward.available().unwrap();
        assert_eq!(payload.text, text);
        assert_eq!((payload.cursor, payload.anchor), (after_accent, after_flag));

        let reversed = IMESurroundingText::from_text(text, after_flag, after_accent, None);
        let payload = reversed.available().unwrap();
        assert_eq!(payload.text, text);
        assert_eq!((payload.cursor, payload.anchor), (after_flag, after_accent));
    }

    #[test]
    fn rtl_text_keeps_logical_byte_offsets() {
        // Logical order, not visual order: right-to-left text is stored in memory order and
        // the platform receives the same byte offsets an editor uses for its model.
        let text = "a\u{200f}שלום";
        let after_rlm = "a\u{200f}".len();
        let surrounding = IMESurroundingText::from_text(text, text.len(), after_rlm, None);
        let payload = surrounding.available().expect("rtl text is available");
        assert_eq!(payload.text, text);
        assert_eq!((payload.cursor, payload.anchor), (text.len(), after_rlm));
    }

    #[test]
    fn invalid_or_non_boundary_offsets_are_unavailable() {
        assert!(matches!(
            IMESurroundingText::from_text("é", 1, 0, None),
            IMESurroundingText::Unavailable
        ));
        assert!(matches!(
            IMESurroundingText::from_text("é", 3, 0, None),
            IMESurroundingText::Unavailable
        ));
        assert!(matches!(
            IMESurroundingText::from_text("é", 0, 1, None),
            IMESurroundingText::Unavailable
        ));
        assert!(matches!(
            IMESurroundingText::from_text("é", 0, 0, Some(1..2)),
            IMESurroundingText::Unavailable
        ));
        assert!(matches!(
            IMESurroundingText::from_text("é", 0, 0, Some(0..3)),
            IMESurroundingText::Unavailable
        ));
    }

    #[test]
    fn preedit_is_removed_and_cursor_is_remapped() {
        let text = "abにほcd";
        let preedit_start = "ab".len();
        let preedit_end = preedit_start + "にほ".len();
        let payload = IMESurroundingText::from_text(
            text,
            preedit_end,
            preedit_end,
            Some(preedit_start..preedit_end),
        );
        let payload = payload.available().unwrap();
        assert_eq!(payload.text, "abcd");
        assert_eq!(
            (payload.cursor, payload.anchor),
            (preedit_start, preedit_start)
        );

        // A cursor inside the pre-edit is mapped to where the composition began.
        let payload = IMESurroundingText::from_text(
            text,
            preedit_start + "に".len(),
            preedit_start,
            Some(preedit_start..preedit_end),
        );
        let payload = payload.available().unwrap();
        assert_eq!(payload.text, "abcd");
        assert_eq!(
            (payload.cursor, payload.anchor),
            (preedit_start, preedit_start)
        );
    }

    #[test]
    fn too_wide_selection_is_unavailable_but_exact_limit_is_available() {
        let text = "a".repeat(MAX_IME_SURROUNDING_TEXT_BYTES + 1);
        assert!(matches!(
            IMESurroundingText::from_text(&text, 0, text.len(), None),
            IMESurroundingText::Unavailable
        ));

        let text = "a".repeat(MAX_IME_SURROUNDING_TEXT_BYTES);
        let payload = IMESurroundingText::from_text(&text, 0, text.len(), None);
        let payload = payload.available().unwrap();
        assert_eq!(payload.text.len(), MAX_IME_SURROUNDING_TEXT_BYTES);
        assert_eq!((payload.cursor, payload.anchor), (0, text.len()));
    }

    #[test]
    fn large_documents_are_windowed_without_splitting_utf8() {
        let text = "a".repeat(10_000);
        let payload = IMESurroundingText::from_text(&text, 6000, 6000, None);
        let payload = payload.available().unwrap();
        assert_eq!(payload.text.len(), MAX_IME_SURROUNDING_TEXT_BYTES);
        assert_eq!(payload.text, text[4000..8000]);
        assert_eq!((payload.cursor, payload.anchor), (2000, 2000));

        let text = "左".repeat(3000); // 3 bytes per character.
        let cursor = 3000;
        let payload = IMESurroundingText::from_text(&text, cursor, cursor, None);
        let payload = payload.available().unwrap();
        assert!(payload.text.len() <= MAX_IME_SURROUNDING_TEXT_BYTES);
        assert!(payload.text.is_char_boundary(payload.cursor));
        assert!(payload.text.is_char_boundary(payload.anchor));
        assert!(cursor - payload.cursor <= MAX_IME_SURROUNDING_TEXT_BYTES);
    }

    #[test]
    fn preedit_near_a_window_edge_is_excluded_and_remapped() {
        let text = format!("{}に{}", "a".repeat(5000), "b".repeat(5000));
        let preedit_start = 5000;
        let preedit_end = preedit_start + "に".len();
        let payload = IMESurroundingText::from_text(
            &text,
            preedit_end,
            preedit_start - 1,
            Some(preedit_start..preedit_end),
        );
        let payload = payload.available().unwrap();
        assert!(payload.text.len() <= MAX_IME_SURROUNDING_TEXT_BYTES);
        assert!(!payload.text.contains('に'));
        assert!(payload.anchor < payload.cursor);
        assert!(payload.text.is_char_boundary(payload.cursor));
        assert!(payload.text.is_char_boundary(payload.anchor));
    }

    #[test]
    fn debug_and_clone_do_not_leak_or_lose_transient_text() {
        let owner = Id::unique(7);
        let payload = IMESurroundingText::from_text("secret text", 6, 6, None);
        let output = ime(owner, 3, payload);
        let cloned = output.clone();
        assert_eq!(cloned, output);
        assert_eq!(cloned.owner, owner);
        assert_eq!(cloned.context_generation, 3);

        let debug = format!("{output:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret text"));
    }

    #[test]
    fn ime_output_defaults_to_other_cause_and_debug_reports_it() {
        let output = ime(
            Id::unique(11),
            2,
            IMESurroundingText::from_text("text", 4, 4, None),
        );
        assert_eq!(output.change_cause, ImeChangeCause::Other);
        let debug = format!("{output:?}");
        assert!(debug.contains("change_cause"));
        assert!(debug.contains("Other"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("text: \"text\""));
    }

    #[test]
    fn append_uses_the_latest_ime_descriptor_even_when_none() {
        let owner = Id::unique(1);
        let mut platform_output = PlatformOutput {
            ime: Some(ime(
                owner,
                1,
                IMESurroundingText::from_text("old", 3, 3, None),
            )),
            ..Default::default()
        };

        let newer = PlatformOutput::default();
        platform_output.append(newer);
        assert!(
            platform_output.ime.is_none(),
            "latest None is authoritative"
        );

        let newer = PlatformOutput {
            ime: Some(ime(
                owner,
                2,
                IMESurroundingText::from_text("new", 3, 3, None),
            )),
            ..Default::default()
        };
        platform_output.append(newer);
        let output = platform_output.ime.as_ref().unwrap();
        assert_eq!(output.context_generation, 2);
        assert_eq!(output.surrounding_text.available().unwrap().text, "new");
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn serialization_never_persists_surrounding_text() {
        let original = ime(
            Id::unique("private editor"),
            7,
            IMESurroundingText::from_text("private contents", 7, 0, None),
        );
        let serialized = ron::to_string(&original).expect("serialize IME geometry");
        assert!(!serialized.contains("private contents"));
        assert!(!serialized.contains("surrounding_text"));
        assert!(
            serialized.contains("Other"),
            "the default cause is explicit"
        );
        let restored: IMEOutput = ron::from_str(&serialized).expect("deserialize IME geometry");
        assert_eq!(restored.owner, original.owner);
        assert_eq!(restored.context_generation, original.context_generation);
        assert_eq!(restored.change_cause, ImeChangeCause::Other);
        assert_eq!(restored.surrounding_text, IMESurroundingText::Unavailable);
    }
}
