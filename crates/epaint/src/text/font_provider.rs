use alloc::sync::Arc;

use crate::text::{
    FontFamily, FontInsert, FontPriority, InsertFontFamily,
    face_store::{FaceStore, FontFaceKey},
    unicode::is_combining_mark,
};

/// Input to a [`FontProvider`].
pub struct FallbackRequest<'a> {
    /// A grapheme cluster that no installed font can render,
    /// or that has emoji presentation (see [`FontProvider::font_for`]).
    ///
    /// The returned font must have a glyph for its first character.
    pub cluster: &'a str,

    /// The requested font family.
    pub family: &'a FontFamily,
}

/// A source of fonts.
///
/// Fonts come from two kinds of providers, and egui treats them the same once they are installed:
/// shaped, hinted, and kerned like any other font.
///
/// * *Configured* fonts are known up front, and form the head of every family's fallback chain.
///   [`FontDefinitions`](crate::text::FontDefinitions) is itself such a provider,
///   and is always the first one asked.
/// * *Discovered* fonts are found on demand for a character no installed font has,
///   e.g. among the system fonts, and for a cluster with emoji presentation (a color emoji font).
///   They are appended to the family's fallback chain.
///
/// A provider can do either or both. Providers are asked in order, and the first answer wins.
///
/// Install one with `egui::Context::add_font_provider` or [`Fonts::with_font_providers`](crate::text::Fonts::with_font_providers).
/// `eframe` installs a system font provider on native (behind its `system_fonts` feature).
///
/// Any `Fn(&FallbackRequest<'_>) -> Option<FontInsert>` is a discovering [`FontProvider`].
pub trait FontProvider: Send + Sync {
    /// The fonts to install for `family` up front, in priority order.
    ///
    /// Called once per family (again after the providers change).
    fn fonts_for_family(&self, family: &FontFamily) -> Vec<FontInsert> {
        let _ = family;
        Vec::new()
    }

    /// Find a font with a glyph for the first character of `request.cluster`,
    /// after every installed font of the family missed.
    ///
    /// For a cluster with emoji presentation ([`has_emoji_presentation`](crate::text::has_emoji_presentation),
    /// e.g. `❤\u{FE0F}`) this is asked *before* the installed fonts of the family,
    /// so a provider can supply a color emoji font even though a text face has the same character.
    /// That font is used before the installed ones only if it has a color glyph for the character
    /// (`sbix`, `CBDT` or `COLR`, with the `color_fonts` feature); any other font from the provider
    /// only fills in when no installed font has the character. Return `None` if you have no emoji font.
    ///
    /// Called at most once per (family, character), also when returning `None`;
    /// a character with emoji presentation counts separately from the same character without.
    ///
    /// The font is appended to the fallback chain of `request.family`
    /// and of the families in [`FontInsert::families`].
    /// The [`FontPriority`] is ignored: discovered fonts always come after the configured ones
    /// in the chain, so the installed fonts have a text face's glyphs first.
    fn font_for(&self, request: &FallbackRequest<'_>) -> Option<FontInsert> {
        let _ = request;
        None
    }
}

impl<F> FontProvider for F
where
    F: Fn(&FallbackRequest<'_>) -> Option<FontInsert> + Send + Sync,
{
    fn font_for(&self, request: &FallbackRequest<'_>) -> Option<FontInsert> {
        self(request)
    }
}

/// The installed [`FontProvider`]s, in the order they are asked.
///
/// The first one is always the configured [`FontDefinitions`](crate::text::FontDefinitions).
#[derive(Default)]
pub(crate) struct FontProviders {
    providers: Vec<Arc<dyn FontProvider>>,

    /// Fonts discovered so far, in the order they were found.
    discovered: Vec<FontInsert>,
}

impl FontProviders {
    pub fn new(providers: Vec<Arc<dyn FontProvider>>) -> Self {
        Self {
            providers,
            discovered: Vec::new(),
        }
    }

    /// Fonts discovered so far, in the order they were found.
    pub fn discovered(&self) -> &[FontInsert] {
        &self.discovered
    }

    /// The fonts every provider wants installed for `family` up front, in priority order.
    pub fn fonts_for_family(&self, family: &FontFamily) -> Vec<FontInsert> {
        self.providers
            .iter()
            .flat_map(|provider| provider.fonts_for_family(family))
            .collect()
    }

    /// Ask the providers for a font with a glyph for the first char of `cluster`,
    /// and install it into `faces`.
    ///
    /// The caller caches the result, so each provider is asked at most once per (family, char),
    /// where a char with emoji presentation counts separately from the same char without.
    pub fn discover(
        &mut self,
        faces: &mut FaceStore,
        family: &FontFamily,
        cluster: &str,
    ) -> Option<FontFaceKey> {
        let base_char = cluster.chars().next()?;
        if base_char.is_control() || is_combining_mark(base_char) {
            return None;
        }

        let request = FallbackRequest { cluster, family };

        for provider in &self.providers {
            let Some(mut insert) = provider.font_for(&request) else {
                continue;
            };
            let key = match faces.install(&insert.name, &insert.data) {
                Ok(key) => key,
                Err(err) => {
                    log::warn!(
                        "Failed to parse font {:?} from a font provider: {err}",
                        insert.name
                    );
                    continue;
                }
            };
            let has_glyph = faces
                .get_mut(key)
                .is_some_and(|face| face.glyph_id_resolution(base_char).is_some());
            if !has_glyph {
                log::warn!(
                    "Font {:?} from a font provider has no glyph for {base_char:?}",
                    insert.name
                );
                continue;
            }

            if !insert.families.iter().any(|f| f.family == *family) {
                insert.families.push(InsertFontFamily {
                    family: family.clone(),
                    priority: FontPriority::Lowest,
                });
            }
            // An emoji cluster may find a font again for a family that already has it:
            let known = self.discovered.iter().any(|found| {
                found.name == insert.name
                    && insert
                        .families
                        .iter()
                        .all(|added| found.families.iter().any(|f| f.family == added.family))
            });
            if !known {
                self.discovered.push(insert);
            }
            return Some(key);
        }

        None
    }
}

#[cfg(feature = "default_fonts")]
#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use epaint_default_fonts::{HACK_REGULAR, UBUNTU_LIGHT};

    use super::*;
    use crate::{
        Color32, ColorImage,
        mutex::Mutex,
        text::{
            FontData, FontDefinitions, FontId, Fonts, GlyphBitmap, GlyphRasterizer,
            GlyphRasterizerRequest, RasterizedGlyph, TextOptions, ViewportKey,
        },
    };

    /// Latin capital letter schwa: in `Ubuntu-Light`, but not in `Hack`.
    const SCHWA: char = 'Ə';

    /// In neither `Hack` nor `Ubuntu-Light`.
    const HANGUL: char = '한';

    const DISCOVERED_FONT: &str = "discovered:Ubuntu-Light";

    /// Only `Hack`, so that e.g. [`SCHWA`] is missing.
    fn hack_only() -> FontDefinitions {
        let mut definitions = FontDefinitions::empty();
        definitions.font_data.insert(
            "Hack".to_owned(),
            Arc::new(FontData::from_static(HACK_REGULAR)),
        );
        definitions
            .families
            .insert(FontFamily::Proportional, vec!["Hack".to_owned()]);
        definitions
            .families
            .insert(FontFamily::Monospace, vec!["Hack".to_owned()]);
        definitions
    }

    /// A provider that records its requests, and returns `Ubuntu-Light` for everything if `provide`.
    fn recording_provider(
        requests: &Arc<Mutex<Vec<(FontFamily, String)>>>,
        provide: bool,
    ) -> Arc<dyn FontProvider> {
        let requests = Arc::clone(requests);
        Arc::new(move |request: &FallbackRequest<'_>| {
            requests
                .lock()
                .push((request.family.clone(), request.cluster.to_owned()));
            provide.then(|| {
                FontInsert::new(
                    DISCOVERED_FONT,
                    FontData::from_static(UBUNTU_LIGHT),
                    vec![InsertFontFamily {
                        family: request.family.clone(),
                        priority: FontPriority::Lowest,
                    }],
                )
            })
        })
    }

    fn color_rasterizer() -> GlyphRasterizer {
        GlyphRasterizer::new("test color_rasterizer", |_: &GlyphRasterizerRequest<'_>| {
            Some(RasterizedGlyph {
                bitmap: GlyphBitmap {
                    image: ColorImage::new([1, 1], vec![Color32::RED]),
                    offset_px: emath::Vec2::ZERO,
                    is_color: true,
                },
                advance_px: 10.0,
            })
        })
    }

    fn fonts_with(provider: Arc<dyn FontProvider>, rasterizer: Option<GlyphRasterizer>) -> Fonts {
        let mut fonts =
            Fonts::new(TextOptions::default(), hack_only()).with_font_providers(vec![provider]);
        fonts.set_glyph_rasterizers(rasterizer.into_iter().collect());
        fonts
    }

    fn first_glyph(fonts: &mut Fonts, c: char) -> crate::text::Glyph {
        let galley = fonts.with_pixels_per_point(1.0).layout_no_wrap(
            c.to_string(),
            FontId::proportional(14.0),
            Color32::WHITE,
        );
        galley.rows[0].row.glyphs[0]
    }

    #[test]
    fn provider_hit_installs_the_font_once() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut fonts = fonts_with(recording_provider(&requests, true), None);
        let font_id = FontId::proportional(14.0);

        assert!(fonts.has_glyph(&font_id, SCHWA));
        assert!(fonts.has_glyph(&font_id, SCHWA));
        assert!(fonts.has_glyph(&font_id, 'a'));
        assert_eq!(
            *requests.lock(),
            vec![(FontFamily::Proportional, SCHWA.to_string())]
        );
        assert_eq!(fonts.discovered_fonts().len(), 1);

        let glyph = first_glyph(&mut fonts, SCHWA);
        assert!(!glyph.uv_rect.is_nothing());
        assert!(!glyph.is_color);
        assert_eq!(requests.lock().len(), 1);

        assert_eq!(
            fonts
                .with_pixels_per_point(1.0)
                .characters(&FontFamily::Proportional)
                .get(&SCHWA),
            Some(&vec![DISCOVERED_FONT.to_owned()])
        );
    }

    #[test]
    fn provider_miss_is_remembered_per_family() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut fonts = fonts_with(recording_provider(&requests, false), None);

        assert!(!fonts.has_glyph(&FontId::proportional(14.0), HANGUL));
        assert!(!fonts.has_glyph(&FontId::proportional(14.0), HANGUL));
        assert!(!fonts.has_glyph(&FontId::monospace(14.0), HANGUL));
        assert_eq!(
            *requests.lock(),
            vec![
                (FontFamily::Proportional, HANGUL.to_string()),
                (FontFamily::Monospace, HANGUL.to_string()),
            ]
        );
        assert!(fonts.discovered_fonts().is_empty());
    }

    #[test]
    fn provider_is_not_asked_for_control_chars_and_combining_marks() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut fonts = fonts_with(recording_provider(&requests, true), None);
        let font_id = FontId::proportional(14.0);

        fonts.has_glyph(&font_id, '\n');
        fonts.has_glyph(&font_id, '\u{1AB0}'); // COMBINING DOUBLED CIRCUMFLEX ACCENT
        assert!(requests.lock().is_empty());
    }

    #[test]
    fn discovered_fonts_and_misses_survive_an_options_change() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut fonts = fonts_with(recording_provider(&requests, true), None);
        let font_id = FontId::proportional(14.0);

        assert!(fonts.has_glyph(&font_id, SCHWA));
        // The provider returns `Ubuntu-Light` for this too, but it has no glyph for it:
        assert!(!fonts.has_glyph(&font_id, HANGUL));
        assert_eq!(requests.lock().len(), 2);
        assert_eq!(fonts.discovered_fonts().len(), 1);

        // Change the text options; the faces and families must survive:
        let options = TextOptions {
            font_hinting: !TextOptions::default().font_hinting,
            ..Default::default()
        };
        fonts.begin_pass(options, ViewportKey::default());

        assert!(fonts.has_glyph(&font_id, SCHWA));
        assert!(!fonts.has_glyph(&font_id, HANGUL));
        assert_eq!(
            requests.lock().len(),
            2,
            "The provider should not be asked again"
        );
        assert_eq!(fonts.discovered_fonts().len(), 1);
    }

    #[test]
    fn discovered_font_beats_the_rasterizer() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut fonts = fonts_with(
            recording_provider(&requests, true),
            Some(color_rasterizer()),
        );

        let glyph = first_glyph(&mut fonts, SCHWA);
        assert!(!glyph.is_color, "Should come from the discovered font");
        assert_eq!(requests.lock().len(), 1);
    }

    #[test]
    fn rasterizer_is_used_when_the_provider_misses() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut fonts = fonts_with(
            recording_provider(&requests, false),
            Some(color_rasterizer()),
        );

        let glyph = first_glyph(&mut fonts, HANGUL);
        assert!(glyph.is_color, "Should come from the rasterizer");
        assert_eq!(requests.lock().len(), 1);
    }

    /// Emoji presentation: a provider's color face is asked before the installed faces.
    ///
    /// `emoji-icon-font` stands in for a text face with outlines for symbols, and
    /// `test_fonts/EmojiSbixTest.ttf` (see `make_emoji_sbix_test.py`; `sbix` bitmaps for ♥ ⚠ ☺ ✔ ▶ 👍,
    /// an outline only for ★) for the color emoji face of the system.
    #[cfg(all(feature = "monochrome_emoji_fonts", feature = "color_fonts"))]
    mod emoji_presentation {
        use epaint_default_fonts::{EMOJI_ICON, NOTO_EMOJI_REGULAR};

        use super::*;

        const COLOR_FACE: &str = "discovered:EmojiSbixTest";
        const OUTLINE_FACE: &str = "discovered:NotoEmoji";
        const TEXT_FACE: &str = "EmojiIcon";

        const SBIX_TEST_FONT: &[u8] = include_bytes!("test_fonts/EmojiSbixTest.ttf");

        /// Characters the text face and the color face both have, with a text and an emoji form.
        const TEXT_OR_EMOJI: [char; 5] = ['♥', '⚠', '☺', '✔', '▶'];

        /// One installed text face with outlines for symbols (`emoji-icon-font`).
        fn text_face_only() -> FontDefinitions {
            let mut definitions = FontDefinitions::empty();
            definitions.font_data.insert(
                TEXT_FACE.to_owned(),
                Arc::new(FontData::from_static(EMOJI_ICON)),
            );
            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                definitions
                    .families
                    .insert(family, vec![TEXT_FACE.to_owned()]);
            }
            definitions
        }

        /// A provider that records its requests and offers `face` (or nothing).
        fn provider_of(
            requests: &Arc<Mutex<Vec<(FontFamily, String)>>>,
            face: Option<(&'static str, &'static [u8])>,
        ) -> Arc<dyn FontProvider> {
            let requests = Arc::clone(requests);
            Arc::new(move |request: &FallbackRequest<'_>| {
                requests
                    .lock()
                    .push((request.family.clone(), request.cluster.to_owned()));
                face.map(|(name, bytes)| {
                    FontInsert::new(
                        name,
                        FontData::from_static(bytes),
                        vec![InsertFontFamily {
                            family: request.family.clone(),
                            priority: FontPriority::Lowest,
                        }],
                    )
                })
            })
        }

        fn color_provider(
            requests: &Arc<Mutex<Vec<(FontFamily, String)>>>,
        ) -> Arc<dyn FontProvider> {
            provider_of(requests, Some((COLOR_FACE, SBIX_TEST_FONT)))
        }

        fn fonts_with_text_face(provider: Arc<dyn FontProvider>) -> Fonts {
            Fonts::new(TextOptions::default(), text_face_only()).with_font_providers(vec![provider])
        }

        fn face_of(fonts: &mut Fonts, cluster: &str) -> String {
            fonts
                .with_pixels_per_point(1.0)
                .face_name_of_cluster(&FontFamily::Proportional, cluster)
        }

        #[test]
        fn the_text_face_has_the_characters_used_below() {
            let mut fonts = fonts_with_text_face(provider_of(&Arc::default(), None));
            let mut view = fonts.with_pixels_per_point(1.0);
            let font_id = FontId::proportional(14.0);
            for c in ['♥', '⚠', '☺', '✔', '▶', '★', '👍'] {
                assert!(view.has_glyph(&font_id, c), "{c:?} is not in {TEXT_FACE}");
            }
        }

        #[test]
        fn a_cluster_with_emoji_presentation_takes_the_providers_color_face() {
            for cluster in ["♥\u{FE0F}", "⚠\u{FE0F}", "▶\u{FE0F}", "👍", "👍\u{1F3FD}"] {
                let requests = Arc::new(Mutex::new(Vec::new()));
                let mut fonts = fonts_with_text_face(color_provider(&requests));
                assert_eq!(face_of(&mut fonts, cluster), COLOR_FACE, "{cluster:?}");
                assert_eq!(
                    *requests.lock(),
                    vec![(FontFamily::Proportional, cluster.to_owned())],
                    "{cluster:?}"
                );
            }
        }

        #[test]
        fn text_clusters_keep_the_installed_face_and_never_ask() {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let mut fonts = fonts_with_text_face(color_provider(&requests));
            // ★ has no emoji form; ♥ ⚠ ▶ have one but no U+FE0F; U+FE0E asks for text:
            for cluster in ["★", "▶", "♥", "⚠", "♥\u{FE0E}", "👍\u{FE0E}"] {
                assert_eq!(face_of(&mut fonts, cluster), TEXT_FACE, "{cluster:?}");
            }
            assert!(requests.lock().is_empty(), "{:?}", requests.lock());
        }

        #[test]
        fn a_char_with_and_without_u_fe0f_resolves_separately_in_either_order() {
            for order in [[false, true], [true, false]] {
                let mut fonts = fonts_with_text_face(color_provider(&Arc::default()));
                for emoji in order {
                    for c in TEXT_OR_EMOJI {
                        let cluster = if emoji {
                            format!("{c}\u{FE0F}")
                        } else {
                            c.to_string()
                        };
                        let expected = if emoji { COLOR_FACE } else { TEXT_FACE };
                        assert_eq!(face_of(&mut fonts, &cluster), expected, "{cluster:?}");
                    }
                }
            }
        }

        #[test]
        fn a_provider_without_a_face_leaves_todays_chain_and_is_asked_once() {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let mut fonts = fonts_with_text_face(provider_of(&requests, None));
            for _ in 0..2 {
                assert_eq!(face_of(&mut fonts, "♥\u{FE0F}"), TEXT_FACE);
                assert_eq!(face_of(&mut fonts, "👍"), TEXT_FACE);
            }
            assert_eq!(requests.lock().len(), 2);
            assert!(fonts.discovered_fonts().is_empty());
        }

        #[test]
        fn a_providers_face_without_a_color_glyph_does_not_beat_the_installed_face() {
            // The color test face has only an outline for ★, which has no emoji form anyway:
            let mut fonts = fonts_with_text_face(color_provider(&Arc::default()));
            assert_eq!(face_of(&mut fonts, "★\u{FE0F}"), TEXT_FACE);

            // A provider whose font is monochrome (here `NotoEmoji`) fills in only what is missing:
            let outline = provider_of(&Arc::default(), Some((OUTLINE_FACE, NOTO_EMOJI_REGULAR)));
            let mut fonts = fonts_with_text_face(outline);
            assert_eq!(face_of(&mut fonts, "♥\u{FE0F}"), TEXT_FACE);
            // U+2764 is not in the text face, so the provider's font is the only one with it:
            assert_eq!(face_of(&mut fonts, "❤\u{FE0F}"), OUTLINE_FACE);
        }

        #[test]
        fn an_installed_color_face_stays_ahead_of_the_providers() {
            let mut definitions = text_face_only();
            definitions.font_data.insert(
                "InstalledColor".to_owned(),
                Arc::new(FontData::from_static(SBIX_TEST_FONT)),
            );
            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                definitions
                    .families
                    .get_mut(&family)
                    .expect("family")
                    .push("InstalledColor".to_owned());
            }
            let requests = Arc::new(Mutex::new(Vec::new()));
            let mut fonts = Fonts::new(TextOptions::default(), definitions)
                .with_font_providers(vec![color_provider(&requests)]);
            assert_eq!(face_of(&mut fonts, "♥\u{FE0F}"), "InstalledColor");
            assert_eq!(face_of(&mut fonts, "👍"), "InstalledColor");
            assert!(requests.lock().is_empty(), "{:?}", requests.lock());
        }

        #[test]
        fn asking_again_for_another_emoji_installs_the_face_once() {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let mut fonts = fonts_with_text_face(color_provider(&requests));
            face_of(&mut fonts, "♥\u{FE0F}");
            // The color face has no color glyph for ★, so the provider is asked again and offers
            // the same font, which must not be listed twice:
            face_of(&mut fonts, "★\u{FE0F}");
            assert_eq!(requests.lock().len(), 2);
            assert_eq!(fonts.discovered_fonts().len(), 1);
        }
    }
}
