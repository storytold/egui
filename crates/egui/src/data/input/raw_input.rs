use crate::{Key, OrderedViewportIdMap, Theme, ViewportId, ViewportIdMap, emath::Rect};

use super::{DroppedFileHandle, Event, HoveredFile, SafeAreaInsets, ViewportInfo};

/// What the integrations provides to egui at the start of each frame.
///
/// Set the values that make sense, leave the rest at their `Default::default()`.
///
/// You can check if `egui` is using the inputs using
/// [`crate::Context::egui_wants_pointer_input`] and [`crate::Context::egui_wants_keyboard_input`].
///
/// All coordinates are in points (logical pixels) with origin (0, 0) in the top left .corner.
///
/// Ii "points" can be calculated from native physical pixels
/// using `pixels_per_point` = [`crate::Context::zoom_factor`] * `native_pixels_per_point`;
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct RawInput {
    /// The id of the active viewport.
    pub viewport_id: ViewportId,

    /// Information about all egui viewports.
    pub viewports: ViewportIdMap<ViewportInfo>,

    /// The insets used to only render content in a mobile safe area
    ///
    /// `None` will be treated as "same as last frame"
    pub safe_area_insets: Option<SafeAreaInsets>,

    /// Position and size of the area that egui should use, in points.
    /// Usually you would set this to
    ///
    /// `Some(Rect::from_min_size(Default::default(), screen_size_in_points))`.
    ///
    /// but you could also constrain egui to some smaller portion of your window if you like.
    ///
    /// `None` will be treated as "same as last frame", with the default being a very big area.
    pub screen_rect: Option<Rect>,

    /// Maximum size of one side of the font texture.
    ///
    /// Ask your graphics drivers about this. This corresponds to `GL_MAX_TEXTURE_SIZE`.
    ///
    /// The default is a very small (but very portable) 2048.
    pub max_texture_side: Option<usize>,

    /// Monotonically increasing time, in seconds. Relative to whatever. Used for animations.
    /// If `None` is provided, egui will assume a time delta of `predicted_dt` (default 1/60 seconds).
    pub time: Option<f64>,

    /// Should be set to the expected time between frames when painting at vsync speeds.
    /// The default for this is 1/60.
    /// Can safely be left at its default value.
    pub predicted_dt: f32,

    /// In-order events received this frame.
    ///
    /// There is currently no way to know if egui handles a particular event,
    /// but you can check if egui is using the keyboard with [`crate::Context::egui_wants_keyboard_input`]
    /// and/or the pointer (mouse/touch) with [`crate::Context::egui_is_using_pointer`].
    pub events: Vec<Event>,

    /// Dragged files hovering over egui.
    pub hovered_files: Vec<HoveredFile>,

    /// Dragged files dropped into egui.
    ///
    /// egui never reads the file contents.
    #[cfg_attr(
        not(target_arch = "wasm32"),
        doc = "Call [`crate::DroppedFile::bytes`] to read a dropped file."
    )]
    #[cfg_attr(
        target_arch = "wasm32",
        doc = "Call [`crate::DroppedFile::bytes_async`] to read a dropped file."
    )]
    ///
    /// Note: when using `eframe` on Windows, this will always be empty if drag-and-drop support has
    /// been disabled in [`crate::viewport::ViewportBuilder`].
    #[cfg_attr(feature = "serde", serde(skip))]
    pub dropped_files: Vec<DroppedFileHandle>,

    /// The native window has the keyboard focus (i.e. is receiving key presses).
    ///
    /// False when the user alt-tab away from the application, for instance.
    pub focused: bool,

    /// Does the OS use dark or light mode?
    ///
    /// `None` means "don't know".
    pub system_theme: Option<Theme>,

    /// The system or user locale, as a BCP 47 language tag or a common locale
    /// id (e.g. `"ar-EG"`).
    ///
    /// `None` means the integration doesn't know (the default, and today's
    /// behaviour). egui uses the locale to resolve the default reading
    /// direction when the direction preference is
    /// [`crate::DirectionPreference::System`]; an unknown locale falls back to
    /// a left-to-right interface. Integrations that know the locale report it
    /// here (on the web, `navigator.language`; any integration — including
    /// native ones — can fill this public field itself).
    #[cfg_attr(feature = "serde", serde(default))]
    pub system_locale: Option<String>,

    /// Does the active keyboard layout type Latin letters (US, AZERTY, Dvorak), rather than
    /// another script (Russian, Greek, Arabic)?
    ///
    /// This is the platform's answer for the whole layout, as of the last key event, for
    /// matching shortcuts by letter. `None` means "don't know": `egui-winit` reports it on
    /// Linux (X11 and Wayland) only.
    pub keyboard_layout_is_latin: Option<bool>,

    /// For keys pressed in this input whose shortcuts follow the user's Latin layout rather than
    /// the active one: the physical key, and the character it types on the Latin layout without
    /// modifiers. A character other than a letter means the key has no Latin letter.
    ///
    /// `egui-winit` fills this on Linux (X11 and Wayland) while the layout isn't Latin, and on
    /// macOS from the layout's ⌘ table while ⌘ is held. See [`Self::latin_layout_char`].
    pub latin_layout_chars: Vec<(Key, char)>,
}

impl Default for RawInput {
    fn default() -> Self {
        Self {
            viewport_id: ViewportId::ROOT,
            viewports: core::iter::once((ViewportId::ROOT, Default::default())).collect(),
            screen_rect: None,
            max_texture_side: None,
            time: None,
            predicted_dt: 1.0 / 60.0,
            events: vec![],
            hovered_files: Default::default(),
            dropped_files: Default::default(),
            focused: true, // integrations opt into global focus tracking
            system_theme: None,
            system_locale: None,
            keyboard_layout_is_latin: None,
            latin_layout_chars: Vec::new(),
            safe_area_insets: Default::default(),
        }
    }
}

impl RawInput {
    /// Does the active keyboard layout type Latin letters? `None` means "don't know".
    ///
    /// Returns the field of the same name.
    #[inline]
    pub fn keyboard_layout_is_latin(&self) -> Option<bool> {
        self.keyboard_layout_is_latin
    }

    /// The character `physical_key` types on the user's Latin layout, when it was pressed in this
    /// input and its shortcuts follow that layout. See [`Self::latin_layout_chars`].
    pub fn latin_layout_char(&self, physical_key: Key) -> Option<char> {
        self.latin_layout_chars
            .iter()
            .rev()
            .find(|(key, _)| *key == physical_key)
            .map(|&(_, c)| c)
    }

    /// Info about the active viewport
    #[inline]
    pub fn viewport(&self) -> &ViewportInfo {
        self.viewports.get(&self.viewport_id).expect("Failed to find current viewport in egui RawInput. This is the fault of the egui backend")
    }

    /// Helper: move volatile (deltas and events), clone the rest.
    ///
    /// * [`Self::hovered_files`] is cloned.
    /// * [`Self::dropped_files`] is moved.
    pub fn take(&mut self) -> Self {
        Self {
            viewport_id: self.viewport_id,
            viewports: self
                .viewports
                .iter_mut()
                .map(|(id, info)| (*id, info.take()))
                .collect(),
            screen_rect: self.screen_rect.take(),
            safe_area_insets: self.safe_area_insets.take(),
            max_texture_side: self.max_texture_side.take(),
            time: self.time,
            predicted_dt: self.predicted_dt,
            events: core::mem::take(&mut self.events),
            hovered_files: self.hovered_files.clone(),
            dropped_files: core::mem::take(&mut self.dropped_files),
            focused: self.focused,
            system_theme: self.system_theme,
            system_locale: self.system_locale.clone(),
            keyboard_layout_is_latin: self.keyboard_layout_is_latin,
            latin_layout_chars: core::mem::take(&mut self.latin_layout_chars),
        }
    }

    /// Add on new input.
    pub fn append(&mut self, newer: Self) {
        let Self {
            viewport_id: viewport_ids,
            viewports,
            screen_rect,
            max_texture_side,
            time,
            predicted_dt,
            mut events,
            mut hovered_files,
            mut dropped_files,
            focused,
            system_theme,
            system_locale,
            keyboard_layout_is_latin,
            mut latin_layout_chars,
            safe_area_insets: safe_area,
        } = newer;

        self.viewport_id = viewport_ids;
        self.viewports = viewports;
        self.screen_rect = screen_rect.or(self.screen_rect);
        self.max_texture_side = max_texture_side.or(self.max_texture_side);
        self.time = time; // use latest time
        self.predicted_dt = predicted_dt; // use latest dt
        self.events.append(&mut events);
        self.hovered_files.append(&mut hovered_files);
        self.dropped_files.append(&mut dropped_files);
        self.focused = focused;
        self.system_theme = system_theme;
        self.system_locale = system_locale.or_else(|| self.system_locale.take());
        self.keyboard_layout_is_latin = keyboard_layout_is_latin.or(self.keyboard_layout_is_latin);
        self.latin_layout_chars.append(&mut latin_layout_chars);
        self.safe_area_insets = safe_area.or(self.safe_area_insets);
    }
}

impl RawInput {
    pub fn ui(&self, ui: &mut crate::Ui) {
        let Self {
            viewport_id,
            viewports,
            screen_rect,
            max_texture_side,
            time,
            predicted_dt,
            events,
            hovered_files,
            dropped_files,
            focused,
            system_theme,
            system_locale,
            keyboard_layout_is_latin,
            latin_layout_chars,
            safe_area_insets: safe_area,
        } = self;

        ui.label(format!("Active viewport: {viewport_id:?}"));
        let ordered_viewports = viewports
            .iter()
            .map(|(id, value)| (*id, value))
            .collect::<OrderedViewportIdMap<_>>();
        for (id, viewport) in ordered_viewports {
            ui.group(|ui| {
                ui.label(format!("Viewport {id:?}"));
                ui.push_id(id, |ui| {
                    viewport.ui(ui);
                });
            });
        }
        ui.label(format!("screen_rect: {screen_rect:?} points"));

        ui.label(format!("max_texture_side: {max_texture_side:?}"));
        if let Some(time) = time {
            ui.label(format!("time: {time:.3} s"));
        } else {
            ui.label("time: None");
        }
        ui.label(format!("predicted_dt: {:.1} ms", 1e3 * predicted_dt));
        ui.label(format!("hovered_files: {}", hovered_files.len()));
        ui.label(format!("dropped_files: {}", dropped_files.len()));
        ui.label(format!("focused: {focused}"));
        ui.label(format!("system_theme: {system_theme:?}"));
        ui.label(format!("system_locale: {system_locale:?}"));
        ui.label(format!(
            "keyboard_layout_is_latin: {keyboard_layout_is_latin:?}"
        ));
        ui.label(format!("latin_layout_chars: {latin_layout_chars:?}"));
        ui.label(format!("safe_area: {safe_area:?}"));
        ui.scope(|ui| {
            ui.set_min_height(150.0);
            ui.label(format!("events: {events:#?}"))
                .on_hover_text("key presses etc");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epaint::MarginF32;

    #[test]
    fn append_preserves_latest_specified_safe_area_insets() {
        let original = SafeAreaInsets(MarginF32::same(1.0));
        let replacement = SafeAreaInsets(MarginF32::same(2.0));
        let mut input = RawInput {
            safe_area_insets: Some(original),
            ..Default::default()
        };

        input.append(RawInput::default());
        assert_eq!(input.safe_area_insets, Some(original));

        input.append(RawInput {
            safe_area_insets: Some(replacement),
            ..Default::default()
        });
        assert_eq!(input.safe_area_insets, Some(replacement));

        input.append(RawInput {
            safe_area_insets: Some(SafeAreaInsets::default()),
            ..Default::default()
        });
        assert_eq!(input.safe_area_insets, Some(SafeAreaInsets::default()));
    }

    #[test]
    fn latin_layout_chars_belong_to_one_input() {
        let mut input = RawInput {
            latin_layout_chars: vec![(Key::Q, 'q')],
            ..Default::default()
        };
        input.append(RawInput {
            latin_layout_chars: vec![(Key::Q, '\''), (Key::C, 'j')],
            ..Default::default()
        });
        assert_eq!(input.latin_layout_char(Key::Q), Some('\''));
        assert_eq!(input.latin_layout_char(Key::C), Some('j'));
        assert_eq!(input.latin_layout_char(Key::V), None);

        let frame = input.take();
        assert_eq!(frame.latin_layout_char(Key::C), Some('j'));
        assert_eq!(input.latin_layout_char(Key::C), None);
    }
}
