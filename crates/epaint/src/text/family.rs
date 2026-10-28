use alloc::collections::BTreeMap;

use crate::text::{
    FontFamily,
    face_store::{FaceStore, FontFaceKey},
    font_provider::FontProviders,
    has_emoji_presentation,
};

/// Index of a [`Family`] in `FontsImpl`.
///
/// Cheaper to pass around and look up than a [`FontFamily`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FamilyKey(pub(crate) usize);

/// The fallback chain of one [`FontFamily`], and the caches for resolving chars against it.
///
/// This is the only place that decides which face renders a given char:
/// first the fonts the [`FontProviders`] install up front (the configured fonts),
/// then fonts they discover on demand, then the `.notdef` glyph ("tofu") of the primary face.
///
/// The exception is a grapheme cluster with emoji presentation (😀, ❤️, 👍🏽): there the providers
/// are asked first, so that a color emoji font can win over a text face that happens to have the
/// same code point (see [`Family::resolve_cluster`]).
#[derive(Debug)]
pub(crate) struct Family {
    name: FontFamily,

    /// Faces in priority order: the primary first, then the fallbacks.
    ///
    /// Configured fonts first, then any discovered by the [`FontProviders`].
    chain: Vec<FontFaceKey>,

    /// Cache: `char → which face in the fallback chain owns this char`.
    ///
    /// Location-independent (fallback choice depends only on charmap support,
    /// not on variation coordinates).
    face_cache: ahash::HashMap<char, FontFaceKey>,

    /// Like [`Self::face_cache`], for the base chars of clusters with emoji presentation.
    ///
    /// Separate, because ❤ and ❤️ (U+2764 followed by U+FE0F) have the same base char
    /// but are not drawn with the same face.
    emoji_face_cache: ahash::HashMap<char, FontFaceKey>,

    /// Lazily calculated: every supported char, and the names of the faces that have it.
    characters: Option<BTreeMap<char, Vec<String>>>,
}

impl Family {
    /// Install the fonts the providers want for `name` up front, in provider order.
    pub fn new(name: &FontFamily, faces: &mut FaceStore, providers: &FontProviders) -> Self {
        let mut chain: Vec<FontFaceKey> = Vec::new();
        for insert in providers.fonts_for_family(name) {
            match faces.install(&insert.name, &insert.data) {
                Ok(key) => {
                    if !chain.contains(&key) {
                        chain.push(key);
                    }
                }
                Err(err) => {
                    panic!("Error parsing {:?} TTF/OTF font file: {err}", insert.name);
                }
            }
        }
        if chain.is_empty() {
            if cfg!(feature = "default_fonts") {
                log::error!("No font provider has any font for FontFamily::{name:?}");
            } else {
                log::error!(
                    "No font provider has any font for FontFamily::{name:?}. \
                     No text will be visible. \
                     Enable the `default_fonts` feature of `epaint`/`egui` to bundle fonts, \
                     or install a `FontProvider` of your own."
                );
            }
        }

        Self {
            name: name.clone(),
            chain,
            face_cache: Default::default(),
            emoji_face_cache: Default::default(),
            characters: None,
        }
    }

    #[inline]
    pub fn name(&self) -> &FontFamily {
        &self.name
    }

    /// The primary face, if any.
    #[inline]
    pub fn primary(&self) -> Option<FontFaceKey> {
        self.chain.first().copied()
    }

    /// Find which face in the fallback chain owns `c`.
    ///
    /// Location-independent: fallback choice depends only on charmap support.
    /// Asks the [`FontProviders`] when no face has `c`
    /// (for emoji presentation, see [`Self::resolve_cluster`]),
    /// and falls back to the primary face (whose `.notdef` glyph we then render)
    /// when they have no font for it either.
    #[inline]
    pub fn resolve(
        &mut self,
        faces: &mut FaceStore,
        providers: &mut FontProviders,
        c: char,
    ) -> FontFaceKey {
        let mut utf8 = [0_u8; 4];
        let cluster = c.encode_utf8(&mut utf8);
        self.resolve_cluster(faces, providers, cluster)
    }

    /// Like [`Self::resolve`] for the first char of `cluster`,
    /// but lets the [`FontProviders`] see the whole grapheme cluster.
    ///
    /// A cluster with emoji presentation ([`has_emoji_presentation`]: 😀, ❤️ with U+FE0F, 👍🏽)
    /// asks the providers *first*: many text faces have ❤ or ⚠ as an outline, which would otherwise
    /// win over the color emoji that U+FE0F asks for. A color face in the chain (installed, or found
    /// for an earlier emoji) is used first; else the providers' face if it has a color glyph for the
    /// char; otherwise the installed faces come first, as always.
    /// A cluster with U+FE0E (text presentation) and any other cluster keep the usual order.
    #[inline]
    pub fn resolve_cluster(
        &mut self,
        faces: &mut FaceStore,
        providers: &mut FontProviders,
        cluster: &str,
    ) -> FontFaceKey {
        let Some(base_char) = cluster.chars().next() else {
            return self.notdef_face();
        };
        if !cluster.is_ascii() && has_emoji_presentation(cluster) {
            if let Some(font_key) = self.emoji_face_cache.get(&base_char) {
                return *font_key;
            }
            return self.resolve_emoji_slow(faces, providers, cluster, base_char);
        }
        if let Some(font_key) = self.face_cache.get(&base_char) {
            return *font_key;
        }
        self.resolve_slow(faces, providers, cluster, base_char)
    }

    /// [`Self::resolve_slow`] for a cluster with emoji presentation:
    /// a color face already in the chain, a provider's color face, then the installed faces
    /// (a provider's other face is in the chain by then, and fills in what they lack).
    ///
    /// Each provider is still asked at most once per (family, char) with emoji presentation.
    #[cold]
    fn resolve_emoji_slow(
        &mut self,
        faces: &mut FaceStore,
        providers: &mut FontProviders,
        cluster: &str,
        c: char,
    ) -> FontFaceKey {
        let font_key = self
            // A color face the app installed itself stays ahead of the providers':
            .find_color_face_for_char(c, faces)
            .or_else(|| self.discover(faces, providers, cluster))
            // Only a color face beats the installed ones: a provider that has no emoji font may
            // still return a text font, e.g. for `★` + U+FE0F, which has no emoji form.
            // (That text font is in the chain now, so it is found below if no installed face has `c`.)
            .filter(|key| {
                faces
                    .get_mut(*key)
                    .is_some_and(|face| face.has_color_glyph_for_char(c))
            })
            .or_else(|| self.find_face_for_char(c, faces))
            .unwrap_or_else(|| {
                log::debug!(
                    "No font for emoji {c:?} (U+{:04X}) in {:?}: rendering the tofu glyph instead",
                    c as u32,
                    self.name
                );
                self.notdef_face()
            });
        self.emoji_face_cache.insert(c, font_key);
        font_key
    }

    #[cold]
    fn resolve_slow(
        &mut self,
        faces: &mut FaceStore,
        providers: &mut FontProviders,
        cluster: &str,
        c: char,
    ) -> FontFaceKey {
        let font_key = self
            // All the installed fonts, i.e. the configured ones
            // and then the ones we have already discovered…
            .find_face_for_char(c, faces)
            // …and if none of them has the char, ask the providers for a new font:
            .or_else(|| self.discover(faces, providers, cluster))
            // No font has the char, so we will render the `.notdef` glyph ("tofu"):
            .unwrap_or_else(|| {
                log::debug!(
                    "No font for {c:?} (U+{:04X}) in {:?}: rendering the tofu glyph instead",
                    c as u32,
                    self.name
                );
                self.notdef_face()
            });
        self.face_cache.insert(c, font_key);
        font_key
    }

    /// The face whose `.notdef` glyph ("tofu") we render for chars no face has.
    #[inline]
    fn notdef_face(&self) -> FontFaceKey {
        self.primary().unwrap_or(FontFaceKey::INVALID)
    }

    /// Ask the providers for a new font with the first char of `cluster`, and append it to the chain.
    fn discover(
        &mut self,
        faces: &mut FaceStore,
        providers: &mut FontProviders,
        cluster: &str,
    ) -> Option<FontFaceKey> {
        let key = providers.discover(faces, &self.name, cluster)?;
        if !self.chain.contains(&key) {
            self.chain.push(key);
            self.characters = None;
        }
        Some(key)
    }

    /// The first face in the chain that draws `c` as a color glyph.
    fn find_color_face_for_char(&self, c: char, faces: &mut FaceStore) -> Option<FontFaceKey> {
        self.chain.iter().copied().find(|&key| {
            faces
                .get_mut(key)
                .is_some_and(|face| face.has_color_glyph_for_char(c))
        })
    }

    /// The first face in the whole chain whose charmap supports `c`.
    fn find_face_for_char(&self, c: char, faces: &mut FaceStore) -> Option<FontFaceKey> {
        Self::find_face_for_char_in(&self.chain, c, faces)
    }

    /// The first of `chain` whose charmap supports `c`.
    ///
    /// Does not touch [`Self::face_cache`].
    fn find_face_for_char_in(
        chain: &[FontFaceKey],
        c: char,
        faces: &mut FaceStore,
    ) -> Option<FontFaceKey> {
        chain.iter().copied().find(|&key| {
            faces
                .get_mut(key)
                .is_some_and(|face| face.glyph_id_resolution(c).is_some())
        })
    }

    /// The names of the faces in the fallback chain, in priority order.
    pub fn face_names(&self, faces: &FaceStore) -> Vec<String> {
        self.chain
            .iter()
            .filter_map(|key| faces.get(*key))
            .map(|face| face.name().to_owned())
            .collect()
    }

    /// All supported characters, and in which faces they are available.
    pub fn characters(&mut self, faces: &FaceStore) -> &BTreeMap<char, Vec<String>> {
        self.characters.get_or_insert_with(|| {
            let mut characters: BTreeMap<char, Vec<String>> = Default::default();
            for key in &self.chain {
                let Some(face) = faces.get(*key) else {
                    continue;
                };
                for chr in face.characters() {
                    characters
                        .entry(chr)
                        .or_default()
                        .push(face.name().to_owned());
                }
            }
            characters
        })
    }
}
