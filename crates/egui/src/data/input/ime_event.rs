/// IME event.
///
/// See <https://docs.rs/winit/latest/winit/event/enum.Ime.html>
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum ImeEvent {
    /// Notifies when the IME was enabled.
    #[deprecated = "No longer used by egui"]
    Enabled,

    /// A new IME candidate is being suggested.
    ///
    /// An empty preedit string indicates that the IME has been dismissed, while
    /// a non-empty preedit string indicates that the IME is active.
    Preedit {
        text: String,
        active_range_chars: Option<core::ops::Range<usize>>,
    },

    /// IME composition ended with this final result.
    ///
    /// The IME is considered dismissed after this event.
    Commit(String),

    /// Notifies when the text surrounding the cursor should be deleted.
    ///
    /// `before_chars` and `after_chars` are the number of characters (not
    /// bytes) to delete before and after the cursor, respectively.
    DeleteSurrounding {
        before_chars: usize,
        after_chars: usize,
    },

    /// Notifies when the text surrounding the cursor should be deleted,
    /// counted in UTF-8 bytes, the unit IMEs and windowing systems actually report
    /// (winit's `Ime::DeleteSurrounding { before_bytes, after_bytes }` and Wayland's
    /// `zwp_text_input_v3::delete_surrounding_text` are both byte-counted).
    ///
    /// `before_bytes` ends where the selection or pre-edit text starts, and `after_bytes` starts
    /// where it ends. Integrations send it right before the [`Self::Commit`] it goes with, which
    /// may be empty, so the delete and the commit stay in one undo step.
    ///
    /// Counts saturate at the text bounds and are snapped to UTF-8 character boundaries.
    /// Within a character the range is deleted exactly, not extended to whole grapheme
    /// clusters: an input method can replace the combining accent of a letter, or a letter
    /// typed after an emoji sequence.
    DeleteSurroundingBytes {
        before_bytes: usize,
        after_bytes: usize,
    },

    /// Notifies when the IME was disabled.
    #[deprecated = "No longer used by egui"]
    Disabled,
}
