#![expect(clippy::unwrap_used)] // TODO(emilk): remove unwraps

use alloc::sync::Arc;
use core::{iter, ops::Range};

use emath::{Align, GuiRounding as _, NumExt as _, Pos2, Rect, Vec2, pos2, vec2};
use smallvec::SmallVec;

use crate::{
    Color32, Mesh, Stroke, Vertex,
    stroke::PathStroke,
    text::{
        ByteIndex, ByteRange, FontPriority,
        face_store::FontFaceKey,
        fonts::FontsImpl,
        styled_metrics::StyledMetrics,
        unicode::{is_cjk, is_combining_mark},
    },
};

use unicode_segmentation::UnicodeSegmentation as _;

use super::{
    ByteRangeExt as _, FallbackAlign, Galley, Glyph, LayoutJob, LayoutSection, LineBreakKind,
    LineBreaker, PlacedRow, Row, RowVisuals, TextDirection, VariationCoords,
    family::FamilyKey,
    font_face::{ClusterGlyph, FontFace, GlyphInfo, ShapedCluster, ShapedGlyph},
    glyph_atlas::{GlyphAllocation, OutlineGlyph, RasterGlyphAllocation},
    line_break::is_mandatory_break,
};

// ----------------------------------------------------------------------------

/// Represents GUI scale and convenience methods for rounding to pixels.
#[derive(Clone, Copy)]
struct PointScale {
    pub pixels_per_point: f32,
}

impl PointScale {
    #[inline(always)]
    pub fn new(pixels_per_point: f32) -> Self {
        Self { pixels_per_point }
    }

    #[inline(always)]
    pub fn pixels_per_point(&self) -> f32 {
        self.pixels_per_point
    }

    #[inline(always)]
    pub fn round_to_pixel(&self, point: f32) -> f32 {
        (point * self.pixels_per_point).round() / self.pixels_per_point
    }

    #[inline(always)]
    pub fn floor_to_pixel(&self, point: f32) -> f32 {
        (point * self.pixels_per_point).floor() / self.pixels_per_point
    }
}

// ----------------------------------------------------------------------------

/// Temporary storage before line-wrapping.
#[derive(Clone)]
struct Paragraph {
    /// Start of the next glyph to be added. In screen-space / physical pixels.
    pub cursor_x_px: f32,

    /// This is included in case there are no glyphs
    pub section_index_at_start: u32,

    pub glyphs: Vec<Glyph>,

    /// In case of an empty paragraph ("\n"), use this as height.
    pub empty_paragraph_height: f32,

    /// Is the base direction of this paragraph right-to-left?
    pub rtl: bool,

    /// The `LayoutSection::leading_space` advance consumed before this
    /// paragraph's first glyph, in logical pixels. Remembered so row
    /// anchoring can keep it in the ink of the row that carries the
    /// paragraph's first glyph.
    pub leading_advance: f32,
}

impl Paragraph {
    pub fn from_section_index(section_index_at_start: u32) -> Self {
        Self {
            cursor_x_px: 0.0,
            section_index_at_start,
            glyphs: vec![],
            empty_paragraph_height: 0.0,
            rtl: false,
            leading_advance: 0.0,
        }
    }

    /// The paragraph's resolved base reading direction (UAX #9 P2/P3),
    /// as computed by [`BidiLevels`] when the paragraphs were split.
    pub fn base_direction(&self) -> TextDirection {
        if self.rtl {
            TextDirection::RightToLeft
        } else {
            TextDirection::LeftToRight
        }
    }
}

/// Layout text into a [`Galley`].
///
/// In most cases you should use [`crate::FontsView::layout_job`] instead
/// since that memoizes the input, making subsequent layouting of the same text much faster.
pub(crate) fn layout(fonts: &mut FontsImpl, pixels_per_point: f32, job: Arc<LayoutJob>) -> Galley {
    profiling::function_scope!();

    job.debug_sanity_check();

    if job.wrap.max_rows == 0 {
        // Early-out: no text
        return Galley {
            job,
            rows: Default::default(),
            rect: Rect::ZERO,
            mesh_bounds: Rect::NOTHING,
            num_vertices: 0,
            num_indices: 0,
            pixels_per_point,
            elided: true,
            intrinsic_size: Vec2::ZERO,
        };
    }

    // For most of this we ignore the y coordinate:

    let bidi = BidiLevels::new(&job.text, job.direction);

    let mut paragraphs = vec![Paragraph::from_section_index(0)];
    paragraphs[0].rtl = bidi.is_rtl_paragraph(0);
    {
        let mut shape_buffer = fonts.take_shape_buffer();
        for section_index in 0..job.sections.len() {
            shape_buffer = layout_section(
                fonts,
                shape_buffer,
                pixels_per_point,
                &job,
                section_index as u32,
                &bidi,
                &mut paragraphs,
            );
        }
        fonts.return_shape_buffer(shape_buffer);
    }

    let point_scale = PointScale::new(pixels_per_point);

    let intrinsic_size = calculate_intrinsic_size(point_scale, &job, &paragraphs);

    let mut elided = false;
    let mut rows = rows_from_paragraphs(
        paragraphs,
        &job,
        pixels_per_point,
        fonts.line_breaker.as_ref(),
        &mut elided,
    );
    if elided && let Some(last_placed) = rows.last_mut() {
        let last_row = Arc::make_mut(&mut last_placed.row);
        replace_last_glyph_with_overflow_character(fonts, pixels_per_point, &job, last_row);
        if let Some(last) = last_row.glyphs.last() {
            last_row.size.x = last.max_x();
        }
    }

    // Up to here every glyph sits at its logical position. Now place
    // right-to-left text where a reader expects it:
    if bidi.levels.is_some() {
        for placed_row in &mut rows {
            let row = Arc::make_mut(&mut placed_row.row);
            reset_whitespace_levels(row);
            if row.glyphs.iter().any(Glyph::is_rtl) {
                reorder_row_visually(point_scale, row);
            }
        }
    }

    let justify = job.justify && job.wrap.max_width.is_finite();

    let num_rows = rows.len();
    for (i, placed_row) in rows.iter_mut().enumerate() {
        // Aligned around x = 0 for now, see `align_rtl_rows_to_start`:
        let halign = if starts_on_the_right(&job, &placed_row.row) {
            Align::RIGHT
        } else {
            job.halign
        };
        let is_last_row = i + 1 == num_rows;
        let ends_with_hard_break = placed_row
            .row
            .glyphs
            .last()
            .is_some_and(|glyph| is_mandatory_break(glyph.chr));
        let justify_row =
            justify && !placed_row.ends_with_newline && !ends_with_hard_break && !is_last_row;
        if justify || halign != Align::LEFT {
            halign_and_justify_row(
                point_scale,
                placed_row,
                halign,
                job.wrap.max_width,
                justify_row,
                job.keep_trailing_whitespace,
            );
        }
    }
    align_rtl_rows_to_start(&job, &mut rows, pixels_per_point);

    // Calculate the Y positions and tessellate the text:
    galley_from_rows(point_scale, job, rows, elided, intrinsic_size)
}

/// Shared context for emitting shaped glyphs into a [`Paragraph`].
struct ShapingContext {
    /// The family of the section being shaped.
    family: FamilyKey,
    pixels_per_point: f32,
    font_size: f32,
    line_height: f32,
    extra_letter_spacing: f32,
    section_index: u32,
    font_metrics: StyledMetrics,
    is_first_glyph_in_section: bool,

    /// The UAX #9 embedding level of the bidi run being laid out (odd = RTL).
    /// Stamped onto every glyph the context creates, so continuation glyphs
    /// carry their cluster's level too.
    bidi_level: u8,
}

impl ShapingContext {
    fn glyph(
        &self,
        chr: char,
        physical_x: i32,
        advance_width_px: f32,
        face_metrics: &StyledMetrics,
        alloc: GlyphAllocation,
    ) -> Glyph {
        let GlyphAllocation { uv_rect, is_color } = alloc;
        Glyph {
            chr,
            pos: pos2(physical_x as f32 / self.pixels_per_point, f32::NAN),
            advance_width: advance_width_px / self.pixels_per_point,
            line_height: self.line_height,
            font_face_height: face_metrics.row_height,
            font_face_ascent: face_metrics.ascent,
            font_face_align: face_metrics.fallback_align,
            font_height: self.font_metrics.row_height,
            font_ascent: self.font_metrics.ascent,
            uv_rect,
            is_color,
            bidi_level: self.bidi_level,
            section_index: self.section_index,
            first_vertex: 0,
        }
    }
}

/// Produced by [`segment_into_runs`] for text shaping.
#[derive(Debug)]
struct TextRun {
    /// Which font face should shape this run.
    ///
    /// Ignored if `raster` is set.
    font_key: FontFaceKey,

    /// Byte range within the section text.
    byte_range: ByteRange,

    /// Bidi embedding level (UAX #9) shared by every char in the run.
    /// Odd means right-to-left.
    bidi_level: u8,

    /// Set if a priority [`GlyphRasterizer`](crate::text::GlyphRasterizer) rendered this run.
    ///
    /// Such a run is exactly one grapheme cluster, and is not shaped.
    raster: Option<RasterGlyphAllocation>,
}

/// Emit shaped glyphs from a [`harfrust::GlyphBuffer`] into a [`Paragraph`].
///
/// Every character gets exactly one [`Glyph`] (`glyphs.len() == char_count`),
/// an invariant all cursor and selection code relies on, whatever the shaper did:
///
/// * a cluster with fewer glyphs than characters (flag emojis, ligatures) gets
///   zero-width "continuation" glyphs for the extra characters;
/// * a cluster with more glyphs than characters (a letter the font composes
///   from a base and marks) is drawn as one bitmap, see [`emit_cluster_bitmap`].
///
/// The glyphs are emitted in _logical_ order with increasing x, whatever the
/// run's direction; a right-to-left row is put in visual order later, by
/// [`reorder_row_visually`], once the rows are known.
fn layout_shaped_run(
    fonts: &mut FontsImpl,
    run: &TextRun,
    run_text: &str,
    glyph_buffer: &harfrust::GlyphBuffer,
    face_metrics: &StyledMetrics,
    ctx: &mut ShapingContext,
    paragraph: &mut Paragraph,
) {
    let shaped = ShapedRun {
        run,
        text: run_text,
        face_metrics,
        infos: glyph_buffer.glyph_infos(),
        positions: glyph_buffer.glyph_positions(),
    };

    // The start byte of the previously emitted cluster, for descending
    // transitions: a real shaper emits clusters in monotone byte order per
    // run, so a descending transition (only reachable from a hand-shaped,
    // non-monotone vector) means the outgoing cluster extends to the previous
    // larger cluster start — or, at the run's first transition and for the
    // logically-last cluster of such a vector, to the end of the run text.
    // Seeded only once a cluster has actually been emitted, never from a
    // dummy start:
    let mut previous_cluster_start: Option<usize> = None;
    let mut clusters = logical_clusters(shaped.infos, run.bidi_level % 2 == 1).peekable();
    while let Some(glyph_range) = clusters.next() {
        let cluster_start = shaped.infos[glyph_range.start].cluster as usize;
        let byte_range = {
            let raw = shaped.cluster_bytes(&glyph_range, clusters.peek());
            match (clusters.peek(), previous_cluster_start) {
                // Ascending: the next cluster's start is this one's end:
                (Some(_), _) if raw.start < raw.end => raw,
                // Descending transition: end at the previous larger start,
                // or the run text at the first transition:
                (Some(_), previous) => raw.start..previous.unwrap_or(shaped.text.len()),
                // The logically-last cluster of an ascending run runs to the
                // end of the run text; a descending vector's last cluster
                // ends at the previous larger start:
                (None, Some(previous)) if raw.start < previous => raw.start..previous,
                (None, _) => raw,
            }
        };
        let char_count = run_text
            .get(byte_range.clone())
            .map_or(0, |cluster_text| cluster_text.chars().count());

        // Apply extra_letter_spacing only at cluster boundaries,
        // never between glyphs within the same cluster (e.g. base + mark).
        if !ctx.is_first_glyph_in_section {
            paragraph.cursor_x_px += ctx.extra_letter_spacing * ctx.pixels_per_point;
        }
        ctx.is_first_glyph_in_section = false;

        let first_glyph = paragraph.glyphs.len();
        let as_one_bitmap = if char_count < glyph_range.len() {
            emit_cluster_bitmap(fonts, &shaped, glyph_range.clone(), ctx, paragraph)
        } else {
            None
        };
        let emitted = as_one_bitmap
            .unwrap_or_else(|| emit_cluster_glyphs(fonts, &shaped, glyph_range, ctx, paragraph));

        // Every glyph of a cluster was emitted for the cluster's first char.
        // Give each its own char, in logical order, so the row's chars are the text.
        if let Some(cluster_text) = run_text.get(byte_range.clone()) {
            for (glyph, chr) in core::iter::zip(
                paragraph.glyphs[first_glyph..].iter_mut(),
                cluster_text.chars(),
            ) {
                glyph.chr = chr;
            }
        }

        emit_continuation_glyphs(ctx, paragraph, run_text, byte_range, emitted, face_metrics);
        previous_cluster_start = Some(cluster_start);
    }
}

/// A shaped run being emitted: what every glyph in it shares.
#[derive(Clone, Copy)]
struct ShapedRun<'a> {
    run: &'a TextRun,

    /// The run's text; [`harfrust::GlyphInfo::cluster`] is a byte offset into it.
    text: &'a str,

    face_metrics: &'a StyledMetrics,
    infos: &'a [harfrust::GlyphInfo],
    positions: &'a [harfrust::GlyphPosition],
}

impl ShapedRun<'_> {
    /// The bytes of the cluster shaped into `glyph_range`,
    /// given the cluster after it in logical order (if any).
    ///
    /// Clusters ascend in logical order, so the outgoing cluster ends where
    /// the next one starts; the caller redirects descending transitions (a
    /// hand-shaped vector can be non-monotone — a real shaper never produces
    /// that) to the previous larger start or the end of the run text.
    fn cluster_bytes(
        &self,
        glyph_range: &Range<usize>,
        next: Option<&Range<usize>>,
    ) -> Range<usize> {
        let start = self.infos[glyph_range.start].cluster as usize;
        let end = next.map_or(self.text.len(), |next| {
            self.infos[next.start].cluster as usize
        });
        start..end
    }

    /// The first character of the cluster starting at byte `cluster`.
    fn char_at(&self, cluster: u32) -> char {
        self.text
            .get(cluster as usize..)
            .and_then(|s| s.chars().next())
            .unwrap_or('\u{FFFD}') // Unicode Replacement Character
    }
}

/// Emit a cluster that has more glyphs than characters as one glyph carrying one bitmap.
///
/// Some fonts compose a letter from a base glyph and mark glyphs (Hack's `ǻ`,
/// Noto Naskh Arabic's `ب`), so one character shapes to several glyphs. Emitting
/// one [`Glyph`] per shaped glyph would break `glyphs.len() == char_count`, so
/// the cluster is rendered into one atlas entry ([`FontsImpl::allocate_cluster`])
/// carried by its first character, with the cluster's whole advance.
///
/// `None` if the cluster cannot be drawn that way (a `.notdef` or a color glyph
/// in it); the caller then falls back to one glyph per shaped glyph.
fn emit_cluster_bitmap(
    fonts: &mut FontsImpl,
    shaped: &ShapedRun<'_>,
    glyph_range: Range<usize>,
    ctx: &ShapingContext,
    paragraph: &mut Paragraph,
) -> Option<usize> {
    let px_scale = shaped.face_metrics.px_scale_factor;
    let chr = shaped.char_at(shaped.infos[glyph_range.start].cluster);

    let mut advance_px = 0.0;
    let mut glyphs = Vec::with_capacity(glyph_range.len());
    for (info, pos) in core::iter::zip(
        shaped.infos[glyph_range.clone()].iter(),
        &shaped.positions[glyph_range],
    ) {
        let glyph_id = skrifa::GlyphId::new(info.glyph_id);
        if glyph_id == skrifa::GlyphId::NOTDEF {
            return None;
        }
        glyphs.push(ClusterGlyph {
            glyph_id,
            offset_px: vec2(
                advance_px + pos.x_offset as f32 * px_scale,
                -(pos.y_offset as f32 * px_scale), // harfrust Y+ up → screen Y+ down
            ),
        });
        advance_px += pos.x_advance as f32 * px_scale;
    }

    let OutlineGlyph { allocation, x_px } = fonts.allocate_cluster(
        shaped.run.font_key,
        shaped.face_metrics,
        &ShapedCluster {
            glyphs: &glyphs,
            h_pos: paragraph.cursor_x_px,
            is_cjk: is_cjk(chr),
        },
    )?;

    paragraph.cursor_x_px += advance_px;
    paragraph
        .glyphs
        .push(ctx.glyph(chr, x_px, advance_px, shaped.face_metrics, allocation));
    Some(1)
}

/// Emit one [`Glyph`] per shaped glyph of a cluster, and return how many that was.
///
/// Fewer than the cluster's glyphs when the shaper could not map a character:
/// its combining marks and repeated `.notdef` glyphs are dropped.
fn emit_cluster_glyphs(
    fonts: &mut FontsImpl,
    shaped: &ShapedRun<'_>,
    glyph_range: Range<usize>,
    ctx: &ShapingContext,
    paragraph: &mut Paragraph,
) -> usize {
    let ShapedRun {
        run,
        text: run_text,
        face_metrics,
        infos,
        positions,
    } = *shaped;
    let px_scale = face_metrics.px_scale_factor;
    let mut emitted = 0;
    let first_glyph = paragraph.glyphs.len();

    // The glyph that stands for the cluster. Not always the first:
    // a right-to-left shaper puts a cluster's marks before its base.
    let base = if run.bidi_level % 2 == 1 {
        glyph_range
            .clone()
            .find(|&idx| 0 < positions[idx].x_advance)
            .unwrap_or(glyph_range.start)
    } else {
        glyph_range.start
    };

    for idx in glyph_range.clone() {
        let (info, pos) = (&infos[idx], &positions[idx]);
        let glyph_id = skrifa::GlyphId::new(info.glyph_id);
        let cluster = info.cluster;
        let mut advance_width_px = pos.x_advance as f32 * px_scale;
        let x_offset_px = pos.x_offset as f32 * px_scale;
        let y_offset_px = -(pos.y_offset as f32 * px_scale); // harfrust Y+ up → screen Y+ down

        let chr = shaped.char_at(cluster);

        // Tab is a layout concept, not a glyph — the shaper doesn't know about tab stops.
        // Override the advance width using the font's configured tab size.
        if chr == '\t' {
            let tweak = fonts.face(run.font_key).map(|face| face.tweak());
            let tab_size = tweak.map_or(4.0, |t| t.tab_size);
            let (_, space_info) = fonts.glyph_info(ctx.family, ' ', face_metrics);
            let space_width_px = space_info.advance_width_unscaled.0 * px_scale;
            advance_width_px = tab_size * space_width_px;
        }

        // Thin space (U+2009) and narrow no-break space (U+202F):
        // override the shaper's advance width with the configured fraction of a space.
        if chr == '\u{2009}' || chr == '\u{202F}' {
            let tweak = fonts.face(run.font_key).map(|face| face.tweak());
            let thin_space_width = tweak.map_or(0.5, |t| t.thin_space_width);
            let (_, space_info) = fonts.glyph_info(ctx.family, ' ', face_metrics);
            let space_width_px = space_info.advance_width_unscaled.0 * px_scale;
            advance_width_px = thin_space_width * space_width_px;
        }

        let glyph = if glyph_id == skrifa::GlyphId::NOTDEF {
            // The shaper couldn't map this character. Drop combining marks and duplicate
            // NOTDEF glyphs within the same cluster. The base character is rasterized,
            // or rendered as the `.notdef` glyph ("tofu") if no rasterizer can handle the cluster.
            if is_combining_mark(chr) || idx != base {
                continue;
            }

            let cluster_text = run_text
                .get(cluster as usize..)
                .and_then(|text| {
                    unicode_segmentation::UnicodeSegmentation::graphemes(text, true).next()
                })
                .unwrap_or_default();

            if let Some(raster) = fonts.rasterize_cluster(
                FontPriority::Lowest,
                ctx.family,
                cluster_text,
                ctx.pixels_per_point,
                ctx.font_size,
            ) {
                raster_glyph(ctx, paragraph, chr, &raster, face_metrics)
            } else {
                // Use the fallback font face (not run.font_key which returned NOTDEF).
                let fallback_key = fonts.resolve_face(ctx.family, chr);
                let fallback_metrics = fonts
                    .face(fallback_key)
                    .map(|face| {
                        face.styled_metrics(
                            ctx.pixels_per_point,
                            ctx.font_size,
                            &Default::default(),
                        )
                    })
                    .unwrap_or_default();
                let (_, glyph_info) = fonts.glyph_info(ctx.family, chr, &fallback_metrics);

                // The shaper had no glyph, but the face may still resolve the character
                // (e.g. `\t` and thin spaces become a space with a custom advance).
                // Only a genuine `.notdef` is a missing glyph.
                if glyph_info.id == Some(skrifa::GlyphId::NOTDEF) {
                    fonts.on_missing_glyph(ctx.family, cluster_text);
                }
                let advance_width_px =
                    glyph_info.advance_width_unscaled.0 * fallback_metrics.px_scale_factor;
                let OutlineGlyph { allocation, x_px } = allocate_glyph_info(
                    fonts,
                    fallback_key,
                    &fallback_metrics,
                    glyph_info,
                    paragraph.cursor_x_px,
                    chr,
                );

                paragraph.cursor_x_px += advance_width_px;

                ctx.glyph(chr, x_px, advance_width_px, &fallback_metrics, allocation)
            }
        } else {
            let OutlineGlyph {
                allocation: mut glyph_alloc,
                x_px,
            } = fonts.allocate_glyph(
                run.font_key,
                face_metrics,
                &ShapedGlyph {
                    glyph_id,
                    h_pos: paragraph.cursor_x_px + x_offset_px,
                    is_cjk: is_cjk(chr),
                },
            );

            // Apply shaper y_offset — this varies per glyph instance so it
            // is not part of the cached ShapedGlyph / GlyphAllocation.
            glyph_alloc.uv_rect.offset.y += y_offset_px / ctx.pixels_per_point;

            paragraph.cursor_x_px += advance_width_px;

            ctx.glyph(chr, x_px, advance_width_px, face_metrics, glyph_alloc)
        };
        paragraph.glyphs.push(glyph);
        emitted += 1;
    }

    // A right-to-left shaper puts a cluster's marks before its base. Put the base first,
    // so it carries the cluster's first char and the marks move with it in
    // `reorder_row_visually`. Positions are already final, so only the order changes.
    if run.bidi_level % 2 == 1
        && let Some(cluster) = paragraph.glyphs.get_mut(first_glyph..)
        && let Some(base) = cluster.iter().position(|glyph| 0.0 < glyph.advance_width)
    {
        cluster.rotate_left(base);
    }

    emitted
}

/// Put `glyph_info` in the atlas, or nothing at all if it is invisible.
fn allocate_glyph_info(
    fonts: &mut FontsImpl,
    face_key: FontFaceKey,
    metrics: &StyledMetrics,
    glyph_info: GlyphInfo,
    h_pos_px: f32,
    chr: char,
) -> OutlineGlyph {
    let Some(glyph_id) = glyph_info.id else {
        return OutlineGlyph {
            allocation: GlyphAllocation::default(),
            x_px: h_pos_px.round() as i32,
        };
    };
    fonts.allocate_glyph(
        face_key,
        metrics,
        &ShapedGlyph {
            glyph_id,
            h_pos: h_pos_px,
            is_cjk: is_cjk(chr),
        },
    )
}

/// Emit one glyph for a rasterized cluster and advance the cursor.
fn raster_glyph(
    ctx: &ShapingContext,
    paragraph: &mut Paragraph,
    chr: char,
    raster: &RasterGlyphAllocation,
    face_metrics: &StyledMetrics,
) -> Glyph {
    let physical_x = paragraph.cursor_x_px.round() as i32;
    paragraph.cursor_x_px += raster.advance_px;
    ctx.glyph(
        chr,
        physical_x,
        raster.advance_px,
        face_metrics,
        raster.allocation,
    )
}

/// Emit the glyphs of a run that a [`GlyphRasterizer`](crate::text::GlyphRasterizer) rendered,
/// or that stands in for a cluster in a family without fonts (see [`FontsImpl::fontless_cluster`]).
///
/// The run is one grapheme cluster: its first char gets the bitmap,
/// and the rest zero-width continuation glyphs.
fn layout_raster_run(
    ctx: &mut ShapingContext,
    paragraph: &mut Paragraph,
    run_text: &str,
    raster: &RasterGlyphAllocation,
) {
    let Some(chr) = run_text.chars().next() else {
        return;
    };
    if !ctx.is_first_glyph_in_section {
        paragraph.cursor_x_px += ctx.extra_letter_spacing * ctx.pixels_per_point;
    }
    ctx.is_first_glyph_in_section = false;

    let face_metrics = ctx.font_metrics.clone();
    let glyph = raster_glyph(ctx, paragraph, chr, raster, &face_metrics);
    paragraph.glyphs.push(glyph);
    emit_continuation_glyphs(
        ctx,
        paragraph,
        run_text,
        0..run_text.len(),
        1,
        &face_metrics,
    );
}

/// Emit zero-width continuation glyphs when a cluster has more characters than
/// shaped glyphs.
///
/// This preserves the invariant `glyphs.len() == char_count` that all cursor
/// and text-selection code depends on. Continuation glyphs have
/// [`GlyphAllocation::default()`] so [`tessellate_glyphs`] skips them entirely.
fn emit_continuation_glyphs(
    ctx: &ShapingContext,
    paragraph: &mut Paragraph,
    run_text: &str,
    cluster_bytes: Range<usize>,
    cluster_glyph_count: usize,
    face_metrics: &StyledMetrics,
) {
    let Some(cluster_text) = run_text.get(cluster_bytes) else {
        return;
    };
    let char_count = cluster_text.chars().count();
    if char_count <= cluster_glyph_count {
        return;
    }

    let physical_x = paragraph.cursor_x_px.round() as i32;

    for chr in cluster_text.chars().skip(cluster_glyph_count) {
        paragraph.glyphs.push(ctx.glyph(
            chr,
            physical_x,
            0.0,
            face_metrics,
            GlyphAllocation::default(),
        ));
    }
}

// Ignores the Y coordinate.
#[must_use]
fn layout_section(
    fonts: &mut FontsImpl,
    mut shape_buffer: harfrust::UnicodeBuffer,
    pixels_per_point: f32,
    job: &LayoutJob,
    section_index: u32,
    bidi: &BidiLevels,
    out_paragraphs: &mut Vec<Paragraph>,
) -> harfrust::UnicodeBuffer {
    let section = &job.sections[section_index as usize];
    let LayoutSection {
        leading_space,
        byte_range,
        format,
    } = section;

    let family = fonts.family_key(&format.font_id.family);
    let font_size = format.font_id.size;
    let font_metrics = fonts.family_metrics(family, pixels_per_point, font_size, &format.coords);
    let line_height = section
        .format
        .line_height
        .unwrap_or(font_metrics.row_height);
    let extra_letter_spacing = section.format.extra_letter_spacing;

    let mut paragraph = out_paragraphs.last_mut().unwrap();
    if paragraph.glyphs.is_empty() {
        paragraph.empty_paragraph_height = line_height;
        // The advance precedes this paragraph's first glyph (a section that
        // starts here): remember it so row anchoring can keep it in the ink.
        paragraph.leading_advance += leading_space;
    }
    paragraph.cursor_x_px += leading_space * pixels_per_point;

    let section_text = &job.text[byte_range.as_usize()];
    let mut ctx = ShapingContext {
        family,
        pixels_per_point,
        font_size,
        line_height,
        extra_letter_spacing,
        section_index,
        font_metrics,
        is_first_glyph_in_section: paragraph.glyphs.is_empty(),
        bidi_level: 0,
    };
    let mut runs = Vec::new();

    // Byte offset of the current segment within `job.text`, to look up bidi levels:
    let mut segment_start = byte_range.start.0;

    // Process each paragraph segment (split on newlines — the shaper can't handle them).
    for (seg_idx, segment) in SplitOrWhole::new(section_text, job.break_on_newline).enumerate() {
        let segment_levels = bidi
            .levels
            .as_deref()
            .map(|levels| &levels[segment_start..segment_start + segment.len()]);

        if 0 < seg_idx {
            paragraph = out_paragraphs.push_mut(Paragraph::from_section_index(section_index));
            paragraph.empty_paragraph_height = line_height;
            paragraph.rtl = bidi.is_rtl_paragraph(segment_start);
            ctx.is_first_glyph_in_section = true;
        }
        segment_start += segment.len() + 1; // skip the `\n` we split on

        if segment.is_empty() {
            continue;
        }

        segment_into_runs(fonts, &ctx, segment, segment_levels, &mut runs);

        let num_runs = runs.len();
        for (run_idx, run) in runs.iter().enumerate() {
            let run_text = &segment[run.byte_range.as_usize()];
            ctx.bidi_level = run.bidi_level;
            if let Some(raster) = &run.raster {
                layout_raster_run(&mut ctx, paragraph, run_text, raster);
                continue;
            }
            let Some(font_face) = fonts.face(run.font_key) else {
                debug_assert!(
                    false,
                    "`segment_into_runs` should have turned this fontless cluster into a raster run"
                );
                continue;
            };

            let face_metrics =
                font_face.styled_metrics(pixels_per_point, font_size, &format.coords);

            // Set buffer flags for paragraph boundary context.
            let mut flags = harfrust::BufferFlags::empty();
            if run_idx == 0 {
                flags |= harfrust::BufferFlags::BEGINNING_OF_TEXT;
            }
            if run_idx + 1 == num_runs {
                flags |= harfrust::BufferFlags::END_OF_TEXT;
            }

            let glyph_buffer = shape_text(
                font_face,
                run_text,
                run.bidi_level % 2 == 1,
                &format.coords,
                shape_buffer,
                flags,
            );

            layout_shaped_run(
                fonts,
                run,
                run_text,
                &glyph_buffer,
                &face_metrics,
                &mut ctx,
                paragraph,
            );

            shape_buffer = glyph_buffer.clear();
        }
    }

    shape_buffer
}

/// Iterator that either splits on `'\n'` or yields the whole string once.
/// Avoids `Box<dyn Iterator>` and `Vec<&str>` allocation.
enum SplitOrWhole<'a> {
    Split(core::str::Split<'a, char>),
    Whole(iter::Once<&'a str>),
}

impl<'a> SplitOrWhole<'a> {
    fn new(text: &'a str, split: bool) -> Self {
        if split {
            Self::Split(text.split('\n'))
        } else {
            Self::Whole(iter::once(text))
        }
    }
}

impl<'a> Iterator for SplitOrWhole<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        match self {
            Self::Split(iter) => iter.next(),
            Self::Whole(iter) => iter.next(),
        }
    }
}

/// Calculate the intrinsic size of the text.
///
/// The result is eventually passed to `Response::intrinsic_size`.
/// This works by calculating the size of each `Paragraph` (instead of each `Row`).
fn calculate_intrinsic_size(
    point_scale: PointScale,
    job: &LayoutJob,
    paragraphs: &[Paragraph],
) -> Vec2 {
    let mut intrinsic_size = Vec2::ZERO;
    for (idx, paragraph) in paragraphs.iter().enumerate() {
        // Use the precise cursor position instead of `last_glyph.max_x()`,
        // because glyph positions are pixel-snapped but the cursor tracks
        // the exact subpixel advance. This ensures that when two galleys are
        // placed side-by-side, the gap matches what it would be within a
        // single galley.
        let width = paragraph.cursor_x_px / point_scale.pixels_per_point;
        intrinsic_size.x = f32::max(intrinsic_size.x, width);

        let mut height = paragraph
            .glyphs
            .iter()
            .map(|g| g.line_height)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal))
            .unwrap_or(paragraph.empty_paragraph_height);
        if idx == 0 {
            height = f32::max(height, job.first_row_min_height);
        }
        intrinsic_size.y += point_scale.round_to_pixel(height);
    }
    intrinsic_size
}

// Ignores the Y coordinate.
fn rows_from_paragraphs(
    paragraphs: Vec<Paragraph>,
    job: &LayoutJob,
    pixels_per_point: f32,
    line_breaker: &dyn LineBreaker,
    elided: &mut bool,
) -> Vec<PlacedRow> {
    let num_paragraphs = paragraphs.len();

    let mut rows = vec![];

    for (i, paragraph) in paragraphs.into_iter().enumerate() {
        if job.wrap.max_rows <= rows.len() {
            *elided = true;
            break;
        }

        let is_last_paragraph = (i + 1) == num_paragraphs;

        if paragraph.glyphs.is_empty() {
            rows.push(PlacedRow {
                pos: pos2(0.0, f32::NAN),
                row: Arc::new(Row {
                    section_index_at_start: paragraph.section_index_at_start,
                    base_direction: paragraph.base_direction(),
                    leading_advance: 0.0,
                    glyphs: vec![],
                    rtl_paragraph: paragraph.rtl,
                    visual_order: Vec::new(),
                    visuals: Default::default(),
                    size: vec2(0.0, paragraph.empty_paragraph_height),
                }),
                ends_with_newline: !is_last_paragraph,
            });
        } else {
            // Use precise cursor position for width instead of pixel-snapped
            // `last_glyph.max_x()`, so that side-by-side galleys have the same
            // spacing as characters within a single galley.
            let paragraph_width = paragraph.cursor_x_px / pixels_per_point;
            let fits = paragraph_width <= job.effective_wrap_width();
            let may_break_hard =
                job.break_on_newline && paragraph.glyphs.iter().any(|g| is_mandatory_break(g.chr));
            // Early-out optimization: don't look for breaks in a paragraph that fits on one row.
            let breaks = (!fits || may_break_hard)
                .then(|| break_opportunities(&paragraph, job, line_breaker))
                .filter(|breaks| !fits || breaks.iter().any(|after| after.hard));
            if let Some(breaks) = breaks {
                line_break(&paragraph, &breaks, job, &mut rows, elided);
                if let Some(placed_row) = rows.last_mut() {
                    placed_row.ends_with_newline = !is_last_paragraph;
                }
            } else {
                rows.push(PlacedRow {
                    pos: pos2(0.0, f32::NAN),
                    row: Arc::new(Row {
                        section_index_at_start: paragraph.section_index_at_start,
                        base_direction: paragraph.base_direction(),
                        leading_advance: paragraph.leading_advance,
                        glyphs: paragraph.glyphs,
                        rtl_paragraph: paragraph.rtl,
                        visual_order: Vec::new(),
                        visuals: Default::default(),
                        size: vec2(paragraph_width, 0.0),
                    }),
                    ends_with_newline: !is_last_paragraph,
                });
            }
        }
    }

    rows
}

/// Where a row may end after a glyph.
#[derive(Clone, Copy, Default)]
struct BreakAfter {
    /// The lowest penalty of the soft breaks the [`LineBreaker`] found here.
    penalty: Option<u8>,

    /// The row must end here.
    hard: bool,

    /// The next glyph starts a new grapheme cluster.
    cluster_end: bool,
}

/// What may follow each glyph of `paragraph`: one entry per glyph.
fn break_opportunities(
    paragraph: &Paragraph,
    job: &LayoutJob,
    line_breaker: &dyn LineBreaker,
) -> Vec<BreakAfter> {
    // There is one glyph per char, so the text of the glyphs is the text of the paragraph.
    let text: String = paragraph.glyphs.iter().map(|glyph| glyph.chr).collect();
    let glyph_starts: Vec<usize> = text.char_indices().map(|(offset, _)| offset).collect();
    let glyph_before = |offset: usize| match glyph_starts.binary_search(&offset) {
        Ok(next_glyph) => next_glyph.checked_sub(1),
        Err(_) => None, // Not on a char boundary, or the end of the text.
    };

    let mut after = vec![BreakAfter::default(); glyph_starts.len()];
    for (offset, _) in text.grapheme_indices(true) {
        if let Some(glyph) = glyph_before(offset) {
            after[glyph].cluster_end = true;
        }
    }

    let mut breaks = Vec::new();
    line_breaker.line_breaks(&text, job.language.as_deref(), &mut breaks);
    for line_break in breaks {
        let Some(glyph) = glyph_before(line_break.offset) else {
            continue;
        };
        let penalty = match line_break.kind {
            LineBreakKind::Hard
                if job.break_on_newline && is_mandatory_break(paragraph.glyphs[glyph].chr) =>
            {
                after[glyph].hard = true;
                continue;
            }
            LineBreakKind::Hard => 0,
            LineBreakKind::Soft { penalty } => penalty,
        };
        let entry = &mut after[glyph].penalty;
        *entry = Some(entry.map_or(penalty, |p| p.min(penalty)));
    }
    after
}

/// The glyphs `range` of `paragraph` as a row, moved left by `row_start_x`.
///
/// `leading_advance` is the `LayoutSection::leading_space` advance the row's
/// first glyph carries in its position (only the row holding the paragraph's
/// first glyph has one; later rows are re-based without it), kept on the row
/// so anchoring can re-inset it at the reading start.
fn row_of_glyphs(
    paragraph: &Paragraph,
    range: Range<usize>,
    row_start_x: f32,
    leading_advance: f32,
) -> PlacedRow {
    let glyphs: Vec<Glyph> = paragraph.glyphs[range]
        .iter()
        .copied()
        .map(|mut glyph| {
            glyph.pos.x -= row_start_x;
            glyph
        })
        .collect();

    let section_index_at_start = glyphs[0].section_index;
    let paragraph_max_x = glyphs.last().unwrap().max_x();

    PlacedRow {
        pos: pos2(0.0, f32::NAN),
        row: Arc::new(Row {
            section_index_at_start,
            base_direction: paragraph.base_direction(),
            leading_advance,
            glyphs,
            rtl_paragraph: paragraph.rtl,
            visual_order: Vec::new(),
            visuals: Default::default(),
            size: vec2(paragraph_max_x, 0.0),
        }),
        ends_with_newline: false,
    }
}

/// `breaks` has one entry per glyph of `paragraph`, see [`break_opportunities`].
fn line_break(
    paragraph: &Paragraph,
    breaks: &[BreakAfter],
    job: &LayoutJob,
    out_rows: &mut Vec<PlacedRow>,
    elided: &mut bool,
) {
    let wrap_width = job.effective_wrap_width();

    // Keeps track of good places to insert row break if we exceed `wrap_width`.
    let mut row_break_candidates = RowBreakCandidates::default();

    let mut first_row_indentation = paragraph.glyphs[0].pos.x;
    let mut row_start_x = 0.0;
    let mut row_start_idx = 0;

    // The `leading_space` advance the paragraph's first glyph carries: the row
    // holding that glyph keeps it (its glyphs carry it in their positions), so
    // row anchoring can re-inset it at the reading start; every later row has
    // it re-based out of its glyphs and gets 0.0.
    let mut leading_advance = paragraph.leading_advance;

    for (i, &after) in breaks.iter().enumerate() {
        if job.wrap.max_rows <= out_rows.len() {
            *elided = true;
            break;
        }

        let potential_row_width = paragraph.glyphs[i].max_x() - row_start_x;

        if wrap_width < potential_row_width {
            // Row break:

            if first_row_indentation > 0.0
                && !row_break_candidates.has_good_candidate(job.wrap.break_anywhere)
            {
                // Allow the first row to be completely empty, because we know there will be more space on the next row:
                // TODO(emilk): this records the height of this first row as zero, though that is probably fine since first_row_indentation usually comes with a first_row_min_height.
                out_rows.push(PlacedRow {
                    pos: pos2(0.0, f32::NAN),
                    row: Arc::new(Row {
                        section_index_at_start: paragraph.section_index_at_start,
                        base_direction: paragraph.base_direction(),
                        leading_advance: 0.0,
                        glyphs: vec![],
                        rtl_paragraph: paragraph.rtl,
                        visual_order: Vec::new(),
                        visuals: Default::default(),
                        size: Vec2::ZERO,
                    }),
                    ends_with_newline: false,
                });
                row_start_x += first_row_indentation;
                first_row_indentation = 0.0;
                // The advance was consumed with the indent (via `row_start_x`),
                // so no row keeps it:
                leading_advance = 0.0;
            } else if let Some(last_kept_index) = row_break_candidates.get(job.wrap.break_anywhere)
            {
                out_rows.push(row_of_glyphs(
                    paragraph,
                    row_start_idx..last_kept_index + 1,
                    row_start_x,
                    leading_advance,
                ));

                // Start a new row:
                row_start_idx = last_kept_index + 1;
                row_start_x = paragraph.glyphs[row_start_idx].pos.x;
                row_break_candidates.forget_before_idx(row_start_idx);
                // The advance rode the row that carried the paragraph's first
                // glyph; later rows' glyphs are re-based without it:
                leading_advance = 0.0;
            } else {
                // Found no place to break, so we have to overrun wrap_width.
            }
        }

        if after.hard && i + 1 < paragraph.glyphs.len() {
            if job.wrap.max_rows <= out_rows.len() {
                *elided = true;
                break;
            }
            out_rows.push(row_of_glyphs(
                paragraph,
                row_start_idx..i + 1,
                row_start_x,
                leading_advance,
            ));
            row_start_idx = i + 1;
            row_start_x = paragraph.glyphs[row_start_idx].pos.x;
            row_break_candidates.forget_before_idx(row_start_idx);
            // The advance rode the row that carried the paragraph's first
            // glyph; later rows' glyphs are re-based without it:
            leading_advance = 0.0;
        } else {
            row_break_candidates.add(i, after);
        }
    }

    if row_start_idx < paragraph.glyphs.len() {
        // Final row of text:

        if job.wrap.max_rows <= out_rows.len() {
            *elided = true; // can't fit another row
        } else {
            let paragraph_min_x = paragraph.glyphs[row_start_idx].pos.x - row_start_x;
            let paragraph_max_x = paragraph.glyphs.last().unwrap().max_x() - row_start_x;

            let glyphs: Vec<Glyph> = paragraph.glyphs[row_start_idx..]
                .iter()
                .copied()
                .map(|mut glyph| {
                    glyph.pos.x -= row_start_x + paragraph_min_x;
                    glyph
                })
                .collect();

            let section_index_at_start = glyphs[0].section_index;

            out_rows.push(PlacedRow {
                pos: pos2(paragraph_min_x, 0.0),
                row: Arc::new(Row {
                    section_index_at_start,
                    base_direction: paragraph.base_direction(),
                    leading_advance,
                    glyphs,
                    rtl_paragraph: paragraph.rtl,
                    visual_order: Vec::new(),
                    visuals: Default::default(),
                    size: vec2(paragraph_max_x - paragraph_min_x, 0.0),
                }),
                ends_with_newline: false,
            });
            // This is the last row: for the single-row overrun the advance has
            // moved from the glyphs into `paragraph_min_x`/`pos.x`, and the row
            // records it so row anchoring can re-inset it at the reading start.
        }
    }
}

/// Trims the last glyphs in the row and replaces it with an overflow character (e.g. `…`).
///
/// Called before we have any Y coordinates.
fn replace_last_glyph_with_overflow_character(
    fonts: &mut FontsImpl,
    pixels_per_point: f32,
    job: &LayoutJob,
    row: &mut Row,
) {
    let Some(overflow_character) = job.wrap.overflow_character else {
        return;
    };

    let mut section_index = row
        .glyphs
        .last()
        .map(|g| g.section_index)
        .unwrap_or(row.section_index_at_start);
    loop {
        let section = &job.sections[section_index as usize];
        let extra_letter_spacing = section.format.extra_letter_spacing;
        let family = fonts.family_key(&section.format.font_id.family);
        let font_size = section.format.font_id.size;

        let font_metrics =
            fonts.family_metrics(family, pixels_per_point, font_size, &section.format.coords);

        // A priority rasterizer may override the overflow character, just like any other glyph:
        let mut utf8 = [0_u8; 4];
        let raster = fonts.rasterize_cluster(
            FontPriority::Highest,
            family,
            overflow_character.encode_utf8(&mut utf8),
            pixels_per_point,
            font_size,
        );

        let (face_key, font_face_metrics, glyph_info) = if raster.is_some() {
            (
                FontFaceKey::INVALID,
                font_metrics.clone(),
                GlyphInfo::INVISIBLE,
            )
        } else {
            let face_key = fonts.resolve_face(family, overflow_character);
            let font_face_metrics = fonts
                .face(face_key)
                .map(|face| {
                    face.styled_metrics(pixels_per_point, font_size, &section.format.coords)
                })
                .unwrap_or_default();
            let (_, glyph_info) = fonts.glyph_info(family, overflow_character, &font_face_metrics);
            (face_key, font_face_metrics, glyph_info)
        };

        let overflow_glyph_x = if let Some(prev_glyph) = row.glyphs.last() {
            prev_glyph.max_x() + extra_letter_spacing
        } else {
            0.0 // TODO(emilk): heed paragraph leading_space 😬
        };
        // At the paragraph level the character goes to the visual end of the row
        // (the left end of a right-to-left row), whatever direction the text before it has:
        let bidi_level = u8::from(row.rtl_paragraph);

        let advance_width_px = match &raster {
            Some(raster) => raster.advance_px,
            None => glyph_info.advance_width_unscaled.0 * font_face_metrics.px_scale_factor,
        };
        let replacement_glyph_width = advance_width_px / pixels_per_point;

        // Check if we're within width budget:
        if overflow_glyph_x + replacement_glyph_width <= job.effective_wrap_width()
            || row.glyphs.is_empty()
        {
            // we are done

            let OutlineGlyph {
                allocation: replacement_glyph_alloc,
                x_px,
            } = match &raster {
                Some(raster) => OutlineGlyph {
                    allocation: raster.allocation,
                    x_px: (overflow_glyph_x * pixels_per_point).round() as i32,
                },
                None => allocate_glyph_info(
                    fonts,
                    face_key,
                    &font_face_metrics,
                    glyph_info,
                    overflow_glyph_x * pixels_per_point,
                    overflow_character,
                ),
            };

            let line_height = section
                .format
                .line_height
                .unwrap_or(font_metrics.row_height);

            row.glyphs.push(Glyph {
                chr: overflow_character,
                pos: pos2(x_px as f32 / pixels_per_point, f32::NAN),
                advance_width: advance_width_px / pixels_per_point,
                line_height,
                font_face_height: font_face_metrics.row_height,
                font_face_ascent: font_face_metrics.ascent,
                font_face_align: font_face_metrics.fallback_align,
                font_height: font_metrics.row_height,
                font_ascent: font_metrics.ascent,
                uv_rect: replacement_glyph_alloc.uv_rect,
                is_color: replacement_glyph_alloc.is_color,
                bidi_level,
                section_index,
                first_vertex: 0, // filled in later
            });
            return;
        }

        // We didn't fit - pop the last glyph and try again.
        if let Some(last_glyph) = row.glyphs.pop() {
            section_index = last_glyph.section_index;
        } else {
            section_index = row.section_index_at_start;
        }
    }
}

/// Horizontally aligned the text on a row.
///
/// Ignores the Y coordinate.
fn halign_and_justify_row(
    point_scale: PointScale,
    placed_row: &mut PlacedRow,
    halign: Align,
    wrap_width: f32,
    justify: bool,
    keep_trailing_whitespace: bool,
) {
    #![expect(clippy::useless_let_if_seq)] // False positive

    let row = Arc::make_mut(&mut placed_row.row);

    if row.glyphs.is_empty() {
        return;
    }

    // The natural width of the row, whitespace included. Alignment trims the
    // ink below, but the row keeps this size so the space after (or before,
    // in right-to-left rows) the text is allocated like in a left-aligned row.
    let natural_width = row.size.x;

    // Read before the `Row` destructure below holds the mutable borrow:
    // the `LayoutSection::leading_space` advance the row's first glyph
    // carries in its position, and the row's resolved base reading
    // direction.
    let row_leading_advance = row.leading_advance;
    let row_base_direction = row.base_direction;

    let num_leading_spaces = row
        .glyphs
        .iter()
        .take_while(|glyph| glyph.chr.is_whitespace())
        .count();

    let glyph_range = if num_leading_spaces == row.glyphs.len() {
        // There is only whitespace
        (0, row.glyphs.len())
    } else if keep_trailing_whitespace {
        (num_leading_spaces, row.glyphs.len())
    } else {
        let num_trailing_spaces = row
            .glyphs
            .iter()
            .rev()
            .take_while(|glyph| glyph.chr.is_whitespace())
            .count();

        (num_leading_spaces, row.glyphs.len() - num_trailing_spaces)
    };
    let num_glyphs_in_range = glyph_range.1 - glyph_range.0;
    assert!(num_glyphs_in_range > 0, "Should have at least one glyph");

    // Glyphs are indexed in logical order but placed in visual order
    // (see `reorder_row_visually`), so measure and shift them by x:
    let Row {
        glyphs,
        visual_order,
        ..
    } = &mut *row;
    let num_glyphs = glyphs.len();
    let order = || visual_indices(visual_order, num_glyphs);
    let kept = |i: usize| glyph_range.0 <= i && i < glyph_range.1;
    let first_kept = order().position(kept).unwrap_or(0);

    // The ink span of the row, over ALL glyphs in the kept range — not just
    // the endpoints in visual order. Shaped complex scripts (e.g. Arabic
    // ligature glyphs with negative offsets, plus the zero-advance
    // continuation placeholders at both ends of the glyph vector) make the
    // glyph vector non-monotonic in x, so the first and last glyph do not
    // bracket the ink. `f32::min`/`max` ignore NaN, so a stray NaN position
    // cannot poison the span.
    let original_min_x = glyphs[glyph_range.0..glyph_range.1]
        .iter()
        .map(|glyph| glyph.logical_rect().min.x)
        .fold(f32::INFINITY, f32::min);
    let original_max_x = glyphs[glyph_range.0..glyph_range.1]
        .iter()
        .map(|glyph| glyph.logical_rect().max.x)
        .fold(f32::NEG_INFINITY, f32::max);
    let original_width = original_max_x - original_min_x;

    let target_width = if justify && num_glyphs_in_range > 1 {
        wrap_width
    } else {
        original_width
    };

    let (target_min_x, target_max_x) = match halign {
        Align::LEFT => (0.0, target_width),
        Align::Center => (-target_width * 0.5, target_width * 0.5),
        Align::RIGHT => (-target_width, 0.0),
    };

    let num_spaces_in_range = glyphs[glyph_range.0..glyph_range.1]
        .iter()
        .filter(|glyph| glyph.chr.is_whitespace())
        .count();

    let mut extra_x_per_glyph = if num_glyphs_in_range == 1 {
        0.0
    } else {
        (target_width - original_width) / (num_glyphs_in_range as f32 - 1.0)
    };
    extra_x_per_glyph = extra_x_per_glyph.at_least(0.0); // Don't contract

    let mut extra_x_per_space = 0.0;
    if 0 < num_spaces_in_range && num_spaces_in_range < num_glyphs_in_range {
        // Add an integral number of pixels between each glyph,
        // and add the balance to the spaces:

        extra_x_per_glyph = point_scale.floor_to_pixel(extra_x_per_glyph);

        extra_x_per_space = (target_width
            - original_width
            - extra_x_per_glyph * (num_glyphs_in_range as f32 - 1.0))
            / (num_spaces_in_range as f32);
    }

    // The paragraph's leading advance sits before the first glyph. For a
    // left-anchored row it survives inside the ink positions; for a
    // right-anchored right-to-left row the reading start is the row's RIGHT
    // edge, so the anchored ink is inset by the advance instead of the
    // advance being absorbed into the row box.
    let reading_start_inset = if halign == Align::RIGHT
        && row_base_direction == TextDirection::RightToLeft
        && row_leading_advance != 0.0
    {
        row_leading_advance
    } else {
        0.0
    };
    placed_row.pos.x = point_scale.round_to_pixel(target_min_x) - reading_start_inset;
    let mut translate_x = -original_min_x - extra_x_per_glyph * first_kept as f32;

    for i in order() {
        let glyph = &mut glyphs[i];
        glyph.pos.x += translate_x;
        glyph.pos.x = point_scale.round_to_pixel(glyph.pos.x);
        translate_x += extra_x_per_glyph;
        if glyph.chr.is_whitespace() {
            translate_x += extra_x_per_space;
        }
    }

    // The ink range above is only for positioning: centered and right-anchored
    // rows align around their ink, so trailing whitespace must not skew them.
    // The row's allocated size keeps counting the whitespace (except for
    // justified rows, which fill the wrap width, and rows that
    // `keep_trailing_whitespace`, whose aligned span is their size), so
    // inter-widget spacing is the same in both reading directions.
    row.size.x = if justify || keep_trailing_whitespace {
        target_max_x - target_min_x
    } else {
        natural_width
    };
}

/// Is `row` aligned to the right because its start is there?
///
/// See [`LayoutJob::halign_min_is_start`].
fn starts_on_the_right(job: &LayoutJob, row: &Row) -> bool {
    job.halign == Align::Min
        && job.halign_min_is_start
        && row.rtl_paragraph
        && !row.glyphs.is_empty()
}

/// The right edge of a row's aligned ink, in the row's own coordinates.
///
/// [`halign_and_justify_row`] anchors the kept ink at `[0.0, right]`;
/// whitespace that the alignment trimmed (see
/// [`LayoutJob::keep_trailing_whitespace`], and leading whitespace, which a
/// right-to-left row renders at its right edge) sits outside it. A row that
/// hangs from its reading start must therefore hang from this edge, not from
/// its allocated width, which keeps counting the whitespace.
fn aligned_ink_right(row: &Row) -> f32 {
    let ink_right = row
        .glyphs
        .iter()
        .filter(|glyph| !glyph.chr.is_whitespace())
        .map(Glyph::max_x)
        .fold(f32::NEG_INFINITY, f32::max);
    if ink_right.is_finite() {
        ink_right
    } else {
        // Only whitespace: the whole row is the ink.
        row.size.x
    }
}

/// Puts each row that [`starts_on_the_right`] flush with the right edge of the widest row.
///
/// Such a row has been aligned as `Align::RIGHT`, with its ink anchored at the
/// right edge of its `row.size.x`; the other rows stay where they are.
/// Returns `true` if any row moved.
///
/// A row that carries a `leading_advance` (the `LayoutSection::leading_space`
/// consumed before the paragraph's first glyph) hangs from its ink right edge
/// plus that advance: the advance stays inside the reading-start edge, exactly
/// like the leading whitespace a right-to-left row renders at its right edge,
/// instead of floating past it as a box overhang.
pub(crate) fn align_rtl_rows_to_start(
    job: &LayoutJob,
    rows: &mut [PlacedRow],
    pixels_per_point: f32,
) -> bool {
    if !rows
        .iter()
        .any(|placed_row| starts_on_the_right(job, &placed_row.row))
    {
        return false;
    }
    let hang_from = |placed_row: &PlacedRow| {
        aligned_ink_right(&placed_row.row) + placed_row.row.leading_advance
    };
    let width = rows
        .iter()
        .map(|placed_row| {
            if starts_on_the_right(job, &placed_row.row) {
                hang_from(placed_row)
            } else {
                placed_row.pos.x + placed_row.size.x
            }
        })
        .fold(0.0, f32::max);
    let point_scale = PointScale::new(pixels_per_point);
    for placed_row in rows {
        if starts_on_the_right(job, &placed_row.row) {
            placed_row.pos.x = point_scale.round_to_pixel(width - hang_from(placed_row));
        }
    }
    true
}

/// Calculate the Y positions and tessellate the text.
fn galley_from_rows(
    point_scale: PointScale,
    job: Arc<LayoutJob>,
    mut rows: Vec<PlacedRow>,
    elided: bool,
    intrinsic_size: Vec2,
) -> Galley {
    let mut first_row_min_height = job.first_row_min_height;
    let mut cursor_y = 0.0;

    for placed_row in &mut rows {
        let mut max_row_height = first_row_min_height.at_least(placed_row.height());
        let row = Arc::make_mut(&mut placed_row.row);

        first_row_min_height = 0.0;
        let overshoot = BaselineOvershoot::of_row(&row.glyphs);
        for glyph in &row.glyphs {
            let (_, grow) = overshoot.of_section(glyph.section_index);
            max_row_height = max_row_height.at_least(glyph.line_height + grow);
        }
        max_row_height = point_scale.round_to_pixel(max_row_height);

        // Now position each glyph vertically:
        for glyph in &mut row.glyphs {
            let format = &job.sections[glyph.section_index as usize].format;
            let (shift_down, grow) = overshoot.of_section(glyph.section_index);

            let (ascent, centering) = match glyph.font_face_align {
                FallbackAlign::Center => (
                    glyph.font_face_ascent,
                    // When mixing different `FontImpl` (e.g. latin and emojis),
                    // we center the difference:
                    0.5 * (glyph.font_height - glyph.font_face_height),
                ),
                FallbackAlign::Baseline => (glyph.font_ascent, 0.0),
            };

            glyph.pos.y = ascent

                // Apply valign to the difference in height of the entire row,
                // and the line height of this glyph (which is the font height, unless explicitly set),
                // grown to fit the fallback faces that share its baseline:
                + format.valign.to_factor() * (max_row_height - (glyph.line_height + grow))

                // If the line height was set explicitly (e.g. taller than the font),
                // we always center the text within that line height, like CSS does.
                // When `line_height` is not set, this is zero.
                + 0.5 * (glyph.line_height - glyph.font_height)

                + centering

                // Make room above the baseline for taller fallback faces:
                + shift_down;

            glyph.pos.y = point_scale.round_to_pixel(glyph.pos.y);
        }

        placed_row.pos.y = cursor_y;
        row.size.y = max_row_height;

        cursor_y += max_row_height;
        cursor_y = point_scale.round_to_pixel(cursor_y); // TODO(emilk): it would be better to do the calculations in pixels instead.
    }

    let format_summary = format_summary(&job);

    let mut rect = Rect::ZERO;
    let mut mesh_bounds = Rect::NOTHING;
    let mut num_vertices = 0;
    let mut num_indices = 0;

    for placed_row in &mut rows {
        rect |= placed_row.rect();

        let row = Arc::make_mut(&mut placed_row.row);
        row.visuals = tessellate_row(point_scale, &job, &format_summary, row);

        mesh_bounds |= row.visuals.mesh_bounds.translate(placed_row.pos.to_vec2());
        num_vertices += row.visuals.mesh.vertices.len();
        num_indices += row.visuals.mesh.indices.len();

        row.section_index_at_start = u32::MAX; // No longer in use.
        for glyph in &mut row.glyphs {
            glyph.section_index = u32::MAX; // No longer in use.
        }
    }

    let mut galley = Galley {
        job,
        rows,
        elided,
        rect,
        mesh_bounds,
        num_vertices,
        num_indices,
        pixels_per_point: point_scale.pixels_per_point,
        intrinsic_size,
    };

    if galley.job.round_output_to_gui {
        galley.round_output_to_gui();
    }

    galley
}

/// How far the fallback faces with [`FallbackAlign::Baseline`] in one row reach
/// above and below the font box of their section, per section.
///
/// Sections without such faces are left out, so their glyphs are placed
/// with exactly the arithmetic used when no face shares the baseline.
struct BaselineOvershoot(SmallVec<[BaselineOvershootOfSection; 2]>);

struct BaselineOvershootOfSection {
    section_index: u32,
    above: f32,
    below: f32,
}

impl BaselineOvershoot {
    fn of_row(glyphs: &[Glyph]) -> Self {
        let mut sections = SmallVec::<[BaselineOvershootOfSection; 2]>::new();
        for glyph in glyphs {
            if glyph.font_face_align != FallbackAlign::Baseline {
                continue;
            }
            let above = (glyph.font_face_ascent - glyph.font_ascent).at_least(0.0);
            let below = ((glyph.font_face_height - glyph.font_face_ascent)
                - (glyph.font_height - glyph.font_ascent))
                .at_least(0.0);
            if let Some(section) = sections
                .iter_mut()
                .find(|section| section.section_index == glyph.section_index)
            {
                section.above = section.above.at_least(above);
                section.below = section.below.at_least(below);
            } else {
                sections.push(BaselineOvershootOfSection {
                    section_index: glyph.section_index,
                    above,
                    below,
                });
            }
        }
        Self(sections)
    }

    /// How far the section's baseline moves down, and how much taller its line gets.
    fn of_section(&self, section_index: u32) -> (f32, f32) {
        self.0
            .iter()
            .find(|section| section.section_index == section_index)
            .map_or((0.0, 0.0), |section| {
                (section.above, section.above + section.below)
            })
    }
}

#[derive(Default)]
struct FormatSummary {
    any_background: bool,
    any_underline: bool,
    any_strikethrough: bool,
}

fn format_summary(job: &LayoutJob) -> FormatSummary {
    let mut format_summary = FormatSummary::default();
    for section in &job.sections {
        format_summary.any_background |= section.format.background != Color32::TRANSPARENT;
        format_summary.any_underline |= section.format.underline != Stroke::NONE;
        format_summary.any_strikethrough |= section.format.strikethrough != Stroke::NONE;
    }
    format_summary
}

fn tessellate_row(
    point_scale: PointScale,
    job: &LayoutJob,
    format_summary: &FormatSummary,
    row: &mut Row,
) -> RowVisuals {
    if row.glyphs.is_empty() {
        return Default::default();
    }

    let mut mesh = Mesh::default();

    mesh.reserve_triangles(row.glyphs.len() * 2);
    mesh.reserve_vertices(row.glyphs.len() * 4);

    if format_summary.any_background {
        add_row_backgrounds(point_scale, job, row, &mut mesh);
    }

    let glyph_index_start = mesh.indices.len();
    let glyph_vertex_start = mesh.vertices.len();
    tessellate_glyphs(point_scale, job, row, &mut mesh);
    let glyph_vertex_end = mesh.vertices.len();

    if format_summary.any_underline {
        add_row_hline(point_scale, row, &mut mesh, |glyph| {
            let format = &job.sections[glyph.section_index as usize].format;
            let stroke = format.underline;
            let y = glyph.logical_rect().bottom();
            (stroke, y)
        });
    }

    if format_summary.any_strikethrough {
        add_row_hline(point_scale, row, &mut mesh, |glyph| {
            let format = &job.sections[glyph.section_index as usize].format;
            let stroke = format.strikethrough;
            let y = glyph.logical_rect().center().y;
            (stroke, y)
        });
    }

    let mesh_bounds = mesh.calc_bounds();

    RowVisuals {
        mesh,
        mesh_bounds,
        glyph_index_start,
        glyph_vertex_range: glyph_vertex_start..glyph_vertex_end,
    }
}

/// Create background for glyphs that have them.
/// Creates as few rectangular regions as possible.
fn add_row_backgrounds(point_scale: PointScale, job: &LayoutJob, row: &Row, mesh: &mut Mesh) {
    if row.glyphs.is_empty() {
        return;
    }

    let mut end_run = |start: Option<(Color32, Rect, f32)>, stop_x: f32| {
        if let Some((color, start_rect, expand)) = start {
            let rect = Rect::from_min_max(start_rect.left_top(), pos2(stop_x, start_rect.bottom()));
            let rect = rect.expand(expand);
            let rect = rect.round_to_pixels(point_scale.pixels_per_point());
            mesh.add_colored_rect(rect, color);
        }
    };

    let mut run_start = None;
    let mut last_rect = Rect::NAN;

    for i in row.visual_order() {
        let glyph = &row.glyphs[i];
        let format = &job.sections[glyph.section_index as usize].format;
        let color = format.background;
        let rect = glyph.logical_rect();

        if color == Color32::TRANSPARENT {
            end_run(run_start.take(), last_rect.right());
        } else if let Some((existing_color, start, expand)) = run_start {
            if existing_color == color
                && start.top() == rect.top()
                && start.bottom() == rect.bottom()
                && format.expand_bg == expand
            {
                // continue the same background rectangle
            } else {
                end_run(run_start.take(), last_rect.right());
                run_start = Some((color, rect, format.expand_bg));
            }
        } else {
            run_start = Some((color, rect, format.expand_bg));
        }

        last_rect = rect;
    }

    end_run(run_start.take(), last_rect.right());
}

fn tessellate_glyphs(point_scale: PointScale, job: &LayoutJob, row: &mut Row, mesh: &mut Mesh) {
    for glyph in &mut row.glyphs {
        glyph.first_vertex = mesh.vertices.len() as u32;
        let uv_rect = glyph.uv_rect;
        if !uv_rect.is_nothing() {
            let mut left_top = glyph.pos + uv_rect.offset;
            left_top.x = point_scale.round_to_pixel(left_top.x);
            left_top.y = point_scale.round_to_pixel(left_top.y);

            let rect = Rect::from_min_max(left_top, left_top + uv_rect.size);
            let uv = Rect::from_min_max(
                pos2(uv_rect.min[0] as f32, uv_rect.min[1] as f32),
                pos2(uv_rect.max[0] as f32, uv_rect.max[1] as f32),
            );

            let format = &job.sections[glyph.section_index as usize].format;

            // Color glyphs (e.g. color emoji) are untinted by the tessellator,
            // which is also where `Color32::PLACEHOLDER` and `override_text_color` are resolved.
            let color = format.color;

            if format.italics {
                let idx = mesh.vertices.len() as u32;
                mesh.add_triangle(idx, idx + 1, idx + 2);
                mesh.add_triangle(idx + 2, idx + 1, idx + 3);

                let top_offset = rect.height() * 0.25 * Vec2::X;

                mesh.vertices.push(Vertex {
                    pos: rect.left_top() + top_offset,
                    uv: uv.left_top(),
                    color,
                });
                mesh.vertices.push(Vertex {
                    pos: rect.right_top() + top_offset,
                    uv: uv.right_top(),
                    color,
                });
                mesh.vertices.push(Vertex {
                    pos: rect.left_bottom(),
                    uv: uv.left_bottom(),
                    color,
                });
                mesh.vertices.push(Vertex {
                    pos: rect.right_bottom(),
                    uv: uv.right_bottom(),
                    color,
                });
            } else {
                mesh.add_rect_with_uv(rect, uv, color);
            }
        }
    }
}

/// Add a horizontal line over a row of glyphs with a stroke and y decided by a callback.
fn add_row_hline(
    point_scale: PointScale,
    row: &Row,
    mesh: &mut Mesh,
    stroke_and_y: impl Fn(&Glyph) -> (Stroke, f32),
) {
    let mut path = crate::tessellator::Path::default(); // reusing path to avoid re-allocations.

    let mut end_line = |start: Option<(Stroke, Pos2)>, stop_x: f32| {
        if let Some((stroke, start)) = start {
            let stop = pos2(stop_x, start.y);
            path.clear();
            path.add_line_segment([start, stop]);
            let feathering = 1.0 / point_scale.pixels_per_point();
            path.stroke_open(feathering, &PathStroke::from(stroke), mesh);
        }
    };

    let mut line_start = None;
    let mut last_right_x = f32::NAN;

    for i in row.visual_order() {
        let glyph = &row.glyphs[i];
        let (stroke, mut y) = stroke_and_y(glyph);
        stroke.round_center_to_pixel(point_scale.pixels_per_point, &mut y);

        if stroke.is_empty() {
            end_line(line_start.take(), last_right_x);
        } else if let Some((existing_stroke, start)) = line_start {
            if existing_stroke == stroke && start.y == y {
                // continue the same line
            } else {
                end_line(line_start.take(), last_right_x);
                line_start = Some((stroke, pos2(glyph.pos.x, y)));
            }
        } else {
            line_start = Some((stroke, pos2(glyph.pos.x, y)));
        }

        last_right_x = glyph.max_x();
    }

    end_line(line_start.take(), last_right_x);
}

// ----------------------------------------------------------------------------

/// Keeps track of good places to break a long row of text.
#[derive(Clone, Default)]
struct RowBreakCandidates {
    /// The last soft break of each penalty, by increasing penalty.
    soft: Vec<(u8, usize)>,

    /// The last break between two grapheme clusters, for when a word doesn't fit on a row.
    any: Option<usize>,
}

impl RowBreakCandidates {
    fn add(&mut self, index: usize, after: BreakAfter) {
        if let Some(penalty) = after.penalty {
            match self.soft.binary_search_by_key(&penalty, |&(p, _)| p) {
                Ok(i) => self.soft[i].1 = index,
                Err(i) => self.soft.insert(i, (penalty, index)),
            }
        }
        if after.cluster_end {
            self.any = Some(index);
        }
    }

    fn has_good_candidate(&self, break_anywhere: bool) -> bool {
        if break_anywhere {
            self.any.is_some()
        } else {
            self.soft.first().is_some_and(|&(penalty, _)| penalty == 0)
        }
    }

    fn get(&self, break_anywhere: bool) -> Option<usize> {
        if break_anywhere {
            self.any
        } else {
            self.soft.first().map(|&(_, index)| index).or(self.any)
        }
    }

    fn forget_before_idx(&mut self, index: usize) {
        self.soft.retain(|&(_, i)| index <= i);
        if self.any.is_some_and(|i| i < index) {
            self.any = None;
        }
    }
}

// ----------------------------------------------------------------------------

/// Segment text into runs where each run uses a single font face.
///
/// Grapheme clusters are never split across runs: if a combining mark
/// falls back to a different font than its base character, it stays
/// with the base character's font (the shaper will handle it).
///
/// Clusters that a priority [`GlyphRasterizer`](crate::text::GlyphRasterizer)
/// handles get a run of their own, with [`TextRun::raster`] set.
///
/// A run also never crosses a change of bidi embedding level (`levels`, one
/// per byte of `text`, or `None` for text without right-to-left characters),
/// so each run is shaped in one direction.
///
/// NOTE: Segmentation is by font face and direction, not by Unicode script.
/// A run may mix scripts (e.g. Latin + Cyrillic) when they share the same
/// font, which is acceptable for scripts with similar shaping rules.
///
/// Results are appended to `out` (which is cleared first) to allow
/// the caller to reuse the allocation across calls.
fn segment_into_runs(
    fonts: &mut FontsImpl,
    ctx: &ShapingContext,
    text: &str,
    levels: Option<&[u8]>,
    out: &mut Vec<TextRun>,
) {
    out.clear();

    for (byte_offset, grapheme_str) in text.grapheme_indices(true) {
        let bidi_level = levels.map_or(0, |levels| levels[byte_offset]);
        let byte_offset = ByteIndex(byte_offset);
        let byte_end = byte_offset + grapheme_str.len();

        if let Some(raster) = fonts.rasterize_cluster(
            FontPriority::Highest,
            ctx.family,
            grapheme_str,
            ctx.pixels_per_point,
            ctx.font_size,
        ) {
            out.push(TextRun {
                font_key: FontFaceKey::INVALID,
                byte_range: byte_offset..byte_end,
                bidi_level,
                raster: Some(raster),
            });
            continue;
        }

        let font_key = fonts.resolve_cluster_face(ctx.family, grapheme_str);

        if fonts.face(font_key).is_none() {
            // The family has no font at all, so there is nothing to shape with.
            // Still emit a glyph per cluster, or cursors and selections break.
            let raster = fonts.fontless_cluster(
                ctx.family,
                grapheme_str,
                ctx.pixels_per_point,
                ctx.font_size,
            );
            out.push(TextRun {
                font_key,
                byte_range: byte_offset..byte_end,
                bidi_level,
                raster: Some(raster),
            });
            continue;
        }

        if let Some(last_run) = out.last_mut()
            && last_run.raster.is_none()
            && last_run.font_key == font_key
            && last_run.bidi_level == bidi_level
        {
            last_run.byte_range.end = byte_end;
            continue;
        }
        out.push(TextRun {
            font_key,
            byte_range: byte_offset..byte_end,
            bidi_level,
            raster: None,
        });
    }
}

/// The result of the bidi algorithm (UAX #9) on the text of a job, up to rule I2.
struct BidiLevels {
    /// The resolved embedding level of every byte of the text, or `None`
    /// when the text has no right-to-left character and every paragraph is
    /// left-to-right, which keeps the common case free of any bidi work.
    levels: Option<Vec<u8>>,

    /// The byte ranges of the right-to-left paragraphs, in order.
    rtl_paragraphs: Vec<Range<usize>>,

    /// Is the last paragraph right-to-left? The empty paragraph after a final `\n` follows it.
    last_rtl: bool,

    text_len: usize,

    direction: TextDirection,
}

impl BidiLevels {
    fn new(text: &str, direction: TextDirection) -> Self {
        let mut bidi = Self {
            levels: None,
            rtl_paragraphs: Vec::new(),
            last_rtl: direction == TextDirection::RightToLeft,
            text_len: text.len(),
            direction,
        };
        let paragraph_level = match direction {
            TextDirection::Auto | TextDirection::LeftToRight if text.is_ascii() => return bidi,
            TextDirection::Auto => None,
            TextDirection::LeftToRight => Some(unicode_bidi::Level::ltr()),
            TextDirection::RightToLeft => Some(unicode_bidi::Level::rtl()),
        };
        let info = unicode_bidi::BidiInfo::new(text, paragraph_level);
        let mut levels: Vec<u8> = info.levels.iter().map(|level| level.number()).collect();
        for paragraph in &info.paragraphs {
            let range = paragraph.range.clone();
            let mut rtl = paragraph.level.is_rtl();
            if direction == TextDirection::Auto && is_neutral(&text[range.clone()]) {
                // No strong character: take the direction of the paragraph before,
                // so that a new line in right-to-left text starts on the right.
                rtl = bidi.last_rtl;
                if rtl {
                    let paragraph_info = unicode_bidi::BidiInfo::new(
                        &text[range.clone()],
                        Some(unicode_bidi::Level::rtl()),
                    );
                    for (level, resolved) in
                        core::iter::zip(levels[range.clone()].iter_mut(), &paragraph_info.levels)
                    {
                        *level = resolved.number();
                    }
                }
            }
            if rtl {
                bidi.rtl_paragraphs.push(range);
            }
            bidi.last_rtl = rtl;
        }
        // A right-to-left paragraph needs its levels even when they are all even,
        // e.g. for trailing whitespace (rule L1).
        if info.has_rtl() || !bidi.rtl_paragraphs.is_empty() {
            bidi.levels = Some(levels);
        }
        bidi
    }

    /// Is the paragraph that contains the byte at `byte_offset` right-to-left?
    ///
    /// The empty paragraph after a final `\n` has the direction of the one before it.
    fn is_rtl_paragraph(&self, byte_offset: usize) -> bool {
        match self.direction {
            TextDirection::LeftToRight => false,
            TextDirection::RightToLeft => true,
            TextDirection::Auto if self.text_len <= byte_offset => self.last_rtl,
            TextDirection::Auto => {
                let i = self
                    .rtl_paragraphs
                    .partition_point(|range| range.end <= byte_offset);
                self.rtl_paragraphs
                    .get(i)
                    .is_some_and(|range| range.contains(&byte_offset))
            }
        }
    }
}

/// Does `paragraph` lack a strong character (UAX #9 rule P2), e.g. an empty line or only digits?
pub(crate) fn is_neutral(paragraph: &str) -> bool {
    unicode_bidi::get_base_direction(paragraph) == unicode_bidi::Direction::Mixed
}

/// UAX #9 rule L1: segment separators (tab), paragraph separators, and any sequence
/// of whitespace before one of them or at the end of the row take the paragraph level.
///
/// So the spaces where a right-to-left row wraps go to its left end,
/// not next to the last word wherever that is on the row.
fn reset_whitespace_levels(row: &mut Row) {
    use unicode_bidi::BidiClass::{B, BN, FSI, LRE, LRI, LRO, PDF, PDI, RLE, RLI, RLO, S, WS};

    let paragraph_level = u8::from(row.rtl_paragraph);
    // Walking backwards: is everything after this glyph, up to a separator
    // or the end of the row, whitespace?
    let mut in_trailing_whitespace = true;
    for glyph in row.glyphs.iter_mut().rev() {
        match unicode_bidi::bidi_class(glyph.chr) {
            B | S => {
                glyph.bidi_level = paragraph_level;
                in_trailing_whitespace = true;
            }
            // Whitespace, isolate formatting characters, and the characters
            // that rule X9 removes:
            WS | FSI | LRI | RLI | PDI | LRE | RLE | LRO | RLO | PDF | BN => {
                if in_trailing_whitespace {
                    glyph.bidi_level = paragraph_level;
                }
            }
            _ => in_trailing_whitespace = false,
        }
    }
}

/// The clusters of a shaped buffer, as ranges of glyph indices, in _logical_ order.
///
/// The shaper returns glyphs in visual order, which for a right-to-left run
/// is cluster-by-cluster reversed. Walking the clusters backwards restores
/// logical order while keeping the glyphs inside a cluster (a base and its
/// marks) in shaper order, because the marks' offsets are relative to the base.
fn logical_clusters(infos: &[harfrust::GlyphInfo], rtl: bool) -> LogicalClusters<'_> {
    LogicalClusters {
        infos,
        remaining: 0..infos.len(),
        rtl,
    }
}

/// See [`logical_clusters`].
struct LogicalClusters<'a> {
    infos: &'a [harfrust::GlyphInfo],

    /// The glyphs not yet yielded, consumed from the front (left-to-right)
    /// or from the back (right-to-left).
    remaining: Range<usize>,

    rtl: bool,
}

impl Iterator for LogicalClusters<'_> {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Range<usize>> {
        let infos = self.infos;
        let unyielded = &infos[self.remaining.clone()];
        let (first, last) = (unyielded.first()?, unyielded.last()?);
        let offset = self.remaining.start;

        if self.rtl {
            let start = unyielded
                .iter()
                .rposition(|info| info.cluster != last.cluster)
                .map_or(0, |i| i + 1);
            self.remaining.end = offset + start;
            Some(offset + start..offset + unyielded.len())
        } else {
            let end = unyielded
                .iter()
                .position(|info| info.cluster != first.cluster)
                .unwrap_or(unyielded.len());
            self.remaining.start = offset + end;
            Some(offset..offset + end)
        }
    }
}

#[cfg(test)]
std::thread_local! {
    /// Counts the steps [`reorder_row_visually`] takes, for a test that keeps it linear.
    static REORDER_STEPS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[inline]
fn count_reorder_steps(steps: usize) {
    #[cfg(test)]
    REORDER_STEPS.with(|count| count.set(count.get() + steps));
    #[cfg(not(test))]
    let _ = steps;
}

/// Place the glyphs of a row in visual order (UAX #9 rule L2) by rewriting their x,
/// and store that order in [`Row::visual_order`].
///
/// On entry `row.glyphs` is in logical order with increasing x, as
/// [`layout_shaped_run`] left it. It stays in logical order, so a glyph's
/// index is still its char index; only the positions move.
///
/// A block is one advancing glyph and the zero-width glyphs after it at the same
/// level (combining marks, the continuation glyphs of a cluster). Blocks move as
/// units so a mark stays over its base. A zero-width glyph at another level,
/// such as an isolate initiator, a PDI or a mark like RLM, is a block of its own:
/// its level may be the only thing that separates two runs.
///
/// Takes time linear in the number of glyphs: each pass of L2 reverses level runs
/// (maximal sequences of blocks at one level), not single blocks, and there are
/// at most 126 levels.
fn reorder_row_visually(point_scale: PointScale, row: &mut Row) {
    let glyphs = &mut row.glyphs;
    if glyphs.len() < 2 {
        return;
    }

    let mut blocks: Vec<Range<usize>> = Vec::new();
    for (i, glyph) in glyphs.iter().enumerate() {
        match blocks.last_mut() {
            Some(last)
                if glyph.advance_width <= 0.0
                    && glyph.bidi_level == glyphs[last.start].bidi_level =>
            {
                last.end = i + 1;
            }
            _ => blocks.push(i..i + 1),
        }
    }
    count_reorder_steps(glyphs.len());
    let origins: Vec<f32> = blocks
        .iter()
        .map(|block| glyphs[block.start].pos.x)
        .collect();
    // A block's width is the distance to the next one, which includes any
    // extra letter spacing; the last block ends where its glyphs end.
    let widths: Vec<f32> = blocks
        .iter()
        .enumerate()
        .map(|(b, block)| match origins.get(b + 1) {
            Some(next_origin) => next_origin - origins[b],
            None => {
                glyphs[block.clone()]
                    .iter()
                    .map(Glyph::max_x)
                    .fold(origins[b], f32::max)
                    - origins[b]
            }
        })
        .collect();

    // Level runs, as ranges of block indices:
    let mut runs: Vec<Range<usize>> = Vec::new();
    let mut run_levels: Vec<u8> = Vec::new();
    for (b, block) in blocks.iter().enumerate() {
        let level = glyphs[block.start].bidi_level;
        match runs.last_mut() {
            Some(last) if run_levels.last() == Some(&level) => last.end = b + 1,
            _ => {
                runs.push(b..b + 1);
                run_levels.push(level);
            }
        }
    }
    count_reorder_steps(blocks.len());

    // L2: from the highest level down to the lowest odd level,
    // reverse every maximal sequence of runs at that level or higher.
    let Some(lowest_odd) = run_levels.iter().copied().filter(|l| l % 2 == 1).min() else {
        return;
    };
    let highest = run_levels.iter().copied().max().unwrap_or(0);
    let mut order: Vec<usize> = (0..runs.len()).collect();
    for level in (lowest_odd..=highest).rev() {
        let mut i = 0;
        while i < order.len() {
            if run_levels[order[i]] < level {
                i += 1;
                continue;
            }
            let start = i;
            while i < order.len() && level <= run_levels[order[i]] {
                i += 1;
            }
            order[start..i].reverse();
        }
        count_reorder_steps(order.len());
    }

    // Each pass at or below a run's level reversed it as a whole, so its blocks
    // are in reverse order if that happened an odd number of times:
    let mut visual_order = Vec::with_capacity(glyphs.len());
    let mut pen = origins[0];
    for &r in &order {
        let reversed =
            lowest_odd <= run_levels[r] && (run_levels[r] - lowest_odd).is_multiple_of(2);
        let run = runs[r].clone();
        let mut place = |b: usize| {
            for i in blocks[b].clone() {
                let glyph = &mut glyphs[i];
                glyph.pos.x = point_scale.round_to_pixel(pen + (glyph.pos.x - origins[b]));
                visual_order.push(i as u32);
            }
            pen += widths[b];
        };
        if reversed {
            run.rev().for_each(&mut place);
        } else {
            run.for_each(&mut place);
        }
    }
    count_reorder_steps(glyphs.len());
    row.visual_order = visual_order;
}

/// The indices of `len` glyphs from left to right, given a [`Row::visual_order`].
pub(crate) fn visual_indices(
    visual_order: &[u32],
    len: usize,
) -> impl DoubleEndedIterator<Item = usize> + Clone + '_ {
    // Rows that need no reordering have no `visual_order`; a row whose glyphs were
    // changed after layout gets the logical order rather than a panic.
    let identity = if visual_order.len() == len { 0 } else { len };
    let reordered = if identity == 0 { visual_order } else { &[] };
    core::iter::chain(0..identity, reordered.iter().map(|&i| i as usize))
}

/// Shape a text run and return the raw [`harfrust::GlyphBuffer`].
///
/// The caller should iterate `glyph_infos()` / `glyph_positions()` (both
/// `Copy` slices) and convert font units to pixels using `metrics.px_scale_factor`.
/// After iteration, recycle the buffer via `glyph_buffer.clear()`.
fn shape_text(
    font_face: &FontFace,
    text: &str,
    rtl: bool,
    coords: &VariationCoords,
    mut buffer: harfrust::UnicodeBuffer,
    flags: harfrust::BufferFlags,
) -> harfrust::GlyphBuffer {
    let font_ref = font_face.skrifa_font_ref();
    let tweak = font_face.tweak();

    // Build shaper with variable font instance if variation coordinates are set.
    let variations: Vec<harfrust::Variation> = iter::chain(tweak.coords.as_ref(), coords.as_ref())
        .map(|&(tag, value)| harfrust::Variation { tag, value })
        .collect();

    let instance = if variations.is_empty() {
        None
    } else {
        Some(harfrust::ShaperInstance::from_variations(
            font_ref, variations,
        ))
    };

    let shaper = font_face
        .shaper_data()
        .shaper(font_ref)
        .instance(instance.as_ref())
        .build();

    buffer.set_flags(flags);
    buffer.push_str(text);
    // The direction is decided by the bidi algorithm, per run;
    // the shaper only guesses the script (and would guess the direction wrong
    // for digits inside right-to-left text).
    buffer.set_direction(if rtl {
        harfrust::Direction::RightToLeft
    } else {
        harfrust::Direction::LeftToRight
    });
    buffer.guess_segment_properties();

    shaper.shape(buffer, harfrust::ShapeOptions::new())
}

// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::iter;

    use super::{super::*, *};
    use crate::{ColorImage, text::cursor::CCursor, text::cursor::LayoutCursor};
    use emath::Rangef;

    /// The bundled fonts, installed exactly as `FontDefinitions::default` does
    /// with the `default_fonts` feature (a no-op when the feature is on), so
    /// the text tests exercise real shaping in every configuration.
    fn test_font_definitions() -> FontDefinitions {
        let mut definitions = FontDefinitions::default();
        if definitions.families.values().all(Vec::is_empty) {
            // The `default_fonts` feature is off (a plain `cargo test -p epaint`),
            // so `FontDefinitions::default` installed nothing. Install the bundled
            // fonts exactly as it does with the feature (see `font_definitions.rs`).
            definitions.font_data.insert(
                "Hack".to_owned(),
                Arc::new(FontData::from_static(epaint_default_fonts::HACK_REGULAR)),
            );
            definitions.font_data.insert(
                "Ubuntu-Light".to_owned(),
                Arc::new(FontData::from_static(epaint_default_fonts::UBUNTU_LIGHT)),
            );
            definitions.font_data.insert(
                "egui-icons".to_owned(),
                Arc::new(
                    FontData::from_static(epaint_default_fonts::EGUI_ICONS).tweak(FontTweak {
                        scale: 0.90, // Make smaller
                        ..Default::default()
                    }),
                ),
            );
            definitions.families.insert(
                FontFamily::Monospace,
                vec![
                    "Hack".to_owned(),
                    "Ubuntu-Light".to_owned(), // fallback for √ etc
                    "egui-icons".to_owned(),
                ],
            );
            definitions.families.insert(
                FontFamily::Proportional,
                vec!["Ubuntu-Light".to_owned(), "egui-icons".to_owned()],
            );
        }
        definitions
    }

    fn test_fonts() -> FontsImpl {
        FontsImpl::new(TextOptions::default(), test_font_definitions())
    }

    fn layout_without_fonts(text: &str, policy: MissingGlyphPolicy) -> Arc<Galley> {
        let mut fonts = Fonts::new(TextOptions::default(), FontDefinitions::empty())
            .with_missing_glyph_policy(policy);
        fonts.with_pixels_per_point(2.0).layout_no_wrap(
            text.to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
        )
    }

    /// Chinese closes a clause with full-width marks (，：；？！）), and those must never start a row:
    /// <https://en.wikipedia.org/wiki/Line_breaking_rules_in_East_Asian_languages>
    #[test]
    fn cjk_row_never_starts_with_full_width_closing_punctuation() {
        let mut fonts = Fonts::new(TextOptions::default(), FontDefinitions::empty())
            .with_missing_glyph_policy(MissingGlyphPolicy::Tofu);
        let mut fonts = fonts.with_pixels_per_point(2.0);
        let font_id = FontId::proportional(14.0);
        let text = "一二三，四五六：七八九？十一二！三四五）六七八";
        let one = fonts
            .layout_no_wrap("一".to_owned(), font_id.clone(), Color32::WHITE)
            .size()
            .x;
        // Every width that fits a few glyphs, so some row would end just before each mark.
        for glyphs in 2..12 {
            let width = one * glyphs as f32 + 0.5 * one;
            let galley = fonts.layout(text.to_owned(), font_id.clone(), Color32::WHITE, width);
            for placed in galley.rows.iter().skip(1) {
                let first = placed.row.glyphs.first().map(|g| g.chr);
                assert!(
                    !matches!(first, Some('，' | '：' | '？' | '！' | '）')),
                    "a row starts with {first:?} at {glyphs} glyphs wide"
                );
            }
        }
    }

    /// With no font at all we still need one glyph per character and rows with height,
    /// or text cursors get clamped to 0 and text edits lose their contents.
    #[test]
    fn no_font_still_gives_one_glyph_per_char() {
        let galley = layout_without_fonts("ab c", MissingGlyphPolicy::Tofu);

        assert_eq!(galley.rows.len(), 1);
        let row = &galley.rows[0].row;
        assert_eq!(row.glyphs.len(), 4);
        assert!(0.0 < row.height(), "row should have height: {row:?}");
        assert!(0.0 < row.size.x, "row should have width: {row:?}");

        // Every character gets a tofu box, whitespace included:
        for (i, glyph) in row.glyphs.iter().enumerate() {
            assert!(!glyph.uv_rect.is_nothing(), "glyph {i}: {glyph:?}");
            assert!(0.0 < glyph.advance_width, "glyph {i}: {glyph:?}");
            assert_eq!(galley.clamp_cursor(&CCursor::new(i)).index.0, i);
            assert!(
                glyph.pos.x.is_finite() && glyph.pos.y.is_finite(),
                "{glyph:?}"
            );
        }
        assert_eq!(galley.clamp_cursor(&CCursor::new(4)).index.0, 4);
        assert_eq!(galley.end().index.0, 4);
    }

    #[test]
    fn no_font_tofu_is_cached_across_layouts() {
        let mut fonts = Fonts::new(TextOptions::default(), FontDefinitions::empty());
        let mut view = fonts.with_pixels_per_point(1.0);
        let a = view.layout_no_wrap("a".into(), FontId::proportional(14.0), Color32::WHITE);
        let b = view.layout_no_wrap("b".into(), FontId::proportional(14.0), Color32::WHITE);
        assert_eq!(
            a.rows[0].row.glyphs[0].uv_rect.min, b.rows[0].row.glyphs[0].uv_rect.min,
            "the same synthetic tofu box should be reused"
        );
    }

    #[test]
    #[should_panic(expected = "No glyph for \"a\" (U+0061) in Proportional")]
    fn missing_glyph_policy_panic_without_fonts() {
        let _ = layout_without_fonts("a", MissingGlyphPolicy::Panic);
    }

    #[test]
    #[should_panic(expected = "No glyph for \" \" (U+0020) in Proportional")]
    fn missing_glyph_policy_panic_without_fonts_includes_whitespace() {
        let _ = layout_without_fonts(" ", MissingGlyphPolicy::Panic);
    }

    #[test]
    fn no_font_newline_still_breaks_rows() {
        let galley = layout_without_fonts(" \t\n ", MissingGlyphPolicy::Tofu);
        assert_eq!(galley.rows.len(), 2);
        assert_eq!(galley.rows[0].row.glyphs.len(), 2);
        assert_eq!(galley.rows[1].row.glyphs.len(), 1);
    }

    #[test]
    #[cfg(feature = "default_fonts")] // Needs `hack_only`
    #[should_panic(
        expected = "No glyph for \"😀\" (U+1F600) in Proportional. Installed fonts: [\"Hack\"]"
    )]
    fn missing_glyph_policy_panic_with_fonts() {
        let mut fonts = Fonts::new(TextOptions::default(), hack_only())
            .with_missing_glyph_policy(MissingGlyphPolicy::Panic);
        let _ = fonts.with_pixels_per_point(1.0).layout_no_wrap(
            "a😀".into(),
            FontId::proportional(14.0),
            Color32::WHITE,
        );
    }

    /// Fonts have no glyphs for control characters like `\t`; that is not a missing glyph.
    #[test]
    #[cfg(feature = "default_fonts")] // Needs `hack_only`
    fn missing_glyph_policy_panic_ignores_tab_with_fonts() {
        let mut fonts = Fonts::new(TextOptions::default(), hack_only())
            .with_missing_glyph_policy(MissingGlyphPolicy::Panic);
        let galley = fonts.with_pixels_per_point(1.0).layout_no_wrap(
            "a\tb".into(),
            FontId::proportional(14.0),
            Color32::WHITE,
        );
        assert_eq!(galley.rows[0].row.glyphs.len(), 3);
    }

    #[test]
    #[cfg(feature = "default_fonts")] // Needs `hack_only`
    fn missing_glyph_policy_panic_is_satisfied_by_a_fallback_rasterizer() {
        let rasterizer = GlyphRasterizer::new("test", |_: &GlyphRasterizerRequest<'_>| {
            Some(white_raster_glyph())
        });
        let mut fonts = Fonts::new(TextOptions::default(), hack_only())
            .with_glyph_rasterizer(rasterizer)
            .with_missing_glyph_policy(MissingGlyphPolicy::Panic);
        let galley = fonts.with_pixels_per_point(1.0).layout_no_wrap(
            "a😀".into(),
            FontId::proportional(14.0),
            Color32::WHITE,
        );
        assert_eq!(galley.rows[0].row.glyphs.len(), 2);
    }

    /// Only `Hack`, which has no emoji.
    #[cfg(feature = "default_fonts")]
    fn hack_only() -> FontDefinitions {
        let mut definitions = FontDefinitions::empty();
        definitions.font_data.insert(
            "Hack".to_owned(),
            Arc::new(FontData::from_static(epaint_default_fonts::HACK_REGULAR)),
        );
        definitions
            .families
            .insert(FontFamily::Proportional, vec!["Hack".to_owned()]);
        definitions
    }

    /// Hack composes `ǻ` from `å` plus an acute: two glyphs for one character.
    #[test]
    #[cfg(feature = "default_fonts")]
    fn a_letter_the_font_composes_from_several_glyphs_is_one_glyph() {
        let mut fonts = FontsImpl::new(TextOptions::default(), hack_only());
        let composed = layout_simple(&mut fonts, "aǻb");
        let plain = layout_simple(&mut fonts, "aåb");

        let glyphs = &composed.rows[0].row.glyphs;
        let chars: Vec<char> = glyphs.iter().map(|glyph| glyph.chr).collect();
        assert_eq!(chars, ['a', 'ǻ', 'b']);

        let letter = &glyphs[1];
        assert!(!letter.uv_rect.is_nothing(), "the composed letter is drawn");
        assert!(
            plain.rows[0].row.glyphs[1].uv_rect.size.y < letter.uv_rect.size.y,
            "its bitmap covers the acute above the ring"
        );
        assert_eq!(
            composed.size(),
            plain.size(),
            "the cluster keeps the shaper's advance"
        );

        for i in 0..=3 {
            let cursor = CCursor {
                index: CharIndex(i),
                prefer_next_row: false,
            };
            let rect = composed.pos_from_cursor(cursor);
            let back = composed.cursor_from_pos(rect.center().to_vec2());
            assert_eq!(back.index.0, i, "cursor round-trip at {i}");
        }
    }

    #[test]
    fn color_raster_glyph_is_not_tinted() {
        let rasterizer = GlyphRasterizer::new("test", |request: &GlyphRasterizerRequest<'_>| {
            (request.cluster == "한").then(color_raster_glyph)
        });
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default())
            .with_glyph_rasterizer(rasterizer);

        // Returns the tessellated vertex color of 'a' (a normal glyph) and '한' (a color glyph).
        let mut glyph_colors = |text_color: Color32, override_text_color: Option<Color32>| {
            let job = LayoutJob::simple(
                "a한".into(),
                FontId::proportional(14.0),
                text_color,
                f32::INFINITY,
            );
            let galley = layout(&mut fonts, 1.0, Arc::new(job));
            let row = &galley.rows[0].row;
            assert!(!row.glyphs[0].is_color);
            assert!(row.glyphs[1].is_color);
            let first_vertices = [
                row.glyphs[0].first_vertex as usize,
                row.glyphs[1].first_vertex as usize,
            ];

            let mut text_shape =
                crate::TextShape::new(Pos2::ZERO, Arc::new(galley), Color32::GREEN);
            text_shape.override_text_color = override_text_color;
            let mut mesh = crate::Mesh::default();
            crate::Tessellator::new(1.0, Default::default(), [1024, 1024], vec![])
                .tessellate_text(&text_shape, &mut mesh);
            first_vertices.map(|i| mesh.vertices[i].color)
        };

        let half_alpha = Color32::from_white_alpha(128);

        // Explicit text color:
        assert_eq!(
            glyph_colors(Color32::BLUE, None),
            [Color32::BLUE, Color32::WHITE]
        );

        // Delayed text color, resolved by the tessellator (used by most widgets):
        assert_eq!(
            glyph_colors(Color32::PLACEHOLDER, None),
            [Color32::GREEN, Color32::WHITE]
        );

        // Override, e.g. for disabled widgets. The alpha should still apply:
        assert_eq!(
            glyph_colors(Color32::BLUE, Some(Color32::from_black_alpha(128))),
            [Color32::from_black_alpha(128), half_alpha]
        );
    }

    #[test]
    fn color_raster_glyph_keeps_color_in_atlas() {
        let rasterizer = GlyphRasterizer::new("test", |_: &GlyphRasterizerRequest<'_>| {
            Some(color_raster_glyph())
        });
        // The default transfer function discards color for regular (white) glyphs:
        let options = TextOptions {
            color_transfer_function: crate::FontColorTransferFunction::TwoCoverageMinusCoverageSq,
            ..Default::default()
        };
        let mut fonts =
            Fonts::new(options, FontDefinitions::default()).with_glyph_rasterizer(rasterizer);
        let galley = fonts.with_pixels_per_point(1.0).layout_no_wrap(
            "한".into(),
            FontId::proportional(14.0),
            Color32::WHITE,
        );

        let [x, y] = galley.rows[0].row.glyphs[0].uv_rect.min;
        assert_eq!(fonts.image()[(x as usize, y as usize)], Color32::RED);
    }

    /// A rasterizer that records the requests it gets and returns a fixed glyph.
    fn recording_rasterizer(
        requests: &Arc<crate::mutex::Mutex<Vec<(String, FontFamily)>>>,
        result: Option<RasterizedGlyph>,
    ) -> GlyphRasterizer {
        let requests = Arc::clone(requests);
        GlyphRasterizer::new(
            unique_test_key(),
            move |request: &GlyphRasterizerRequest<'_>| {
                requests
                    .lock()
                    .push((request.cluster.to_owned(), request.family.clone()));
                result.clone()
            },
        )
    }

    fn white_raster_glyph() -> RasterizedGlyph {
        RasterizedGlyph {
            bitmap: GlyphBitmap {
                image: ColorImage::new([1, 1], vec![Color32::WHITE]),
                offset_px: Vec2::ZERO,
                is_color: false,
            },
            advance_px: 10.0,
        }
    }

    /// Each test rasterizer needs its own [`GlyphRasterizer::key`], or they would replace each other.
    fn unique_test_key() -> String {
        static COUNTER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        format!("test rasterizer {n}")
    }

    fn color_raster_glyph() -> RasterizedGlyph {
        RasterizedGlyph {
            bitmap: GlyphBitmap {
                image: ColorImage::new([1, 1], vec![Color32::RED]),
                is_color: true,
                ..white_raster_glyph().bitmap
            },
            ..white_raster_glyph()
        }
    }

    /// `(chr, is_color, has_pixels)` for each glyph.
    #[cfg(feature = "default_fonts")]
    fn glyph_summary(galley: &Galley) -> Vec<(char, bool, bool)> {
        galley
            .rows
            .iter()
            .flat_map(|row| &row.row.glyphs)
            .map(|glyph| (glyph.chr, glyph.is_color, !glyph.uv_rect.is_nothing()))
            .collect()
    }

    #[test]
    #[cfg(feature = "default_fonts")] // Needs `hack_only`
    fn rasterized_sequence_keeps_one_glyph_per_char() {
        let requests = Arc::new(crate::mutex::Mutex::new(Vec::new()));
        let rasterizer = recording_rasterizer(&requests, Some(color_raster_glyph()));
        // No font has the emoji, so the rasterizer gets the whole cluster:
        let mut fonts =
            FontsImpl::new(TextOptions::default(), hack_only()).with_glyph_rasterizer(rasterizer);
        let family = "👨\u{200D}👩\u{200D}👧";
        let job = LayoutJob::simple(
            family.to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(&mut fonts, 1.0, Arc::new(job));

        // One visible glyph for the whole cluster, plus zero-width continuation glyphs:
        let summary = glyph_summary(&galley);
        assert_eq!(summary.len(), family.chars().count());
        assert_eq!(summary[0], ('👨', true, true));
        assert!(summary[1..].iter().all(|(_, _, has_pixels)| !has_pixels));
        assert_eq!(
            *requests.lock(),
            [(family.to_owned(), FontFamily::Proportional)]
        );
    }

    #[test]
    fn raster_glyph_cache_is_keyed_by_family() {
        let requests = Arc::new(crate::mutex::Mutex::new(Vec::new()));
        let rasterizer = recording_rasterizer(&requests, Some(white_raster_glyph()));
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default())
            .with_glyph_rasterizer(rasterizer);

        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let job = LayoutJob::simple(
                "한".into(),
                FontId::new(14.0, family),
                Color32::WHITE,
                f32::INFINITY,
            );
            layout(&mut fonts, 1.0, Arc::new(job));
        }

        assert_eq!(
            *requests.lock(),
            vec![
                ("한".to_owned(), FontFamily::Proportional),
                ("한".to_owned(), FontFamily::Monospace),
            ]
        );
    }

    #[test]
    fn raster_glyph_failure_is_cached_and_falls_back_to_replacement() {
        let requests = Arc::new(crate::mutex::Mutex::new(Vec::new()));
        let rasterizer = recording_rasterizer(&requests, None);
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::default())
            .with_glyph_rasterizer(rasterizer);
        let job = Arc::new(LayoutJob::simple(
            "한".into(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        ));

        let galley = layout(&mut fonts, 1.0, Arc::clone(&job));
        layout(&mut fonts, 1.0, job);

        assert_eq!(requests.lock().len(), 1, "Failures should be cached");
        let glyph = &galley.rows[0].row.glyphs[0];
        assert_eq!(glyph.chr, '한');
        assert!(
            !glyph.uv_rect.is_nothing(),
            "Should render the `.notdef` glyph"
        );
    }

    /// A priority rasterizer that only handles `cluster`, and records every request.
    fn priority_rasterizer_for(
        cluster: &'static str,
        requests: &Arc<crate::mutex::Mutex<Vec<(String, FontFamily)>>>,
    ) -> GlyphRasterizer {
        let requests = Arc::clone(requests);
        GlyphRasterizer::new(
            unique_test_key(),
            move |request: &GlyphRasterizerRequest<'_>| {
                requests
                    .lock()
                    .push((request.cluster.to_owned(), request.family.clone()));
                (request.cluster == cluster).then(color_raster_glyph)
            },
        )
        .with_priority(FontPriority::Highest)
    }

    fn layout_simple(fonts: &mut FontsImpl, text: &str) -> Galley {
        let job = LayoutJob::simple(
            text.to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        layout(fonts, 1.0, Arc::new(job))
    }

    #[test]
    #[cfg(feature = "default_fonts")] // The font must have `…` for the override to mean anything
    fn priority_rasterizer_overrides_installed_glyph() {
        let requests = Arc::new(crate::mutex::Mutex::new(Vec::new()));
        let mut fonts = test_fonts().with_glyph_rasterizer(priority_rasterizer_for("…", &requests));
        assert!(fonts.has_glyph(&FontFamily::Proportional, '…'));

        let galley = layout_simple(&mut fonts, "a…b");

        assert_eq!(
            glyph_summary(&galley),
            [('a', false, true), ('…', true, true), ('b', false, true)]
        );
        let clusters: Vec<_> = requests
            .lock()
            .iter()
            .map(|(cluster, _)| cluster.clone())
            .collect();
        assert_eq!(clusters, ["a", "…", "b"], "Asked about every cluster");
    }

    #[test]
    #[cfg(feature = "default_fonts")] // Needs `glyph_summary`
    fn priority_rasterizer_miss_falls_through_to_font_before_fallback() {
        let priority_requests = Arc::new(crate::mutex::Mutex::new(Vec::new()));
        let fallback_requests = Arc::new(crate::mutex::Mutex::new(Vec::new()));
        let mut fonts = test_fonts()
            .with_glyph_rasterizer(priority_rasterizer_for("never", &priority_requests))
            .with_glyph_rasterizer(recording_rasterizer(
                &fallback_requests,
                Some(color_raster_glyph()),
            ));

        let galley = layout_simple(&mut fonts, "a한");

        // 'a' comes from the font, '한' from the fallback:
        assert_eq!(
            glyph_summary(&galley),
            [('a', false, true), ('한', true, true)]
        );
        assert_eq!(priority_requests.lock().len(), 2);
        assert_eq!(
            *fallback_requests.lock(),
            [("한".to_owned(), FontFamily::Proportional)],
            "The fallback should only see what the fonts lack"
        );
    }

    #[test]
    fn rasterizers_are_asked_in_order_until_one_succeeds() {
        let first_requests = Arc::new(crate::mutex::Mutex::new(Vec::new()));
        let second_requests = Arc::new(crate::mutex::Mutex::new(Vec::new()));
        let third_requests = Arc::new(crate::mutex::Mutex::new(Vec::new()));
        let mut fonts = test_fonts()
            .with_glyph_rasterizer(recording_rasterizer(&first_requests, None))
            .with_glyph_rasterizer(recording_rasterizer(
                &second_requests,
                Some(white_raster_glyph()),
            ))
            .with_glyph_rasterizer(recording_rasterizer(
                &third_requests,
                Some(white_raster_glyph()),
            ));

        let galley = layout_simple(&mut fonts, "한");

        assert!(!galley.rows[0].row.glyphs[0].uv_rect.is_nothing());
        assert_eq!(first_requests.lock().len(), 1);
        assert_eq!(second_requests.lock().len(), 1);
        assert_eq!(
            third_requests.lock().len(),
            0,
            "The second one already succeeded"
        );
    }

    #[test]
    #[cfg(feature = "default_fonts")] // Needs `glyph_summary`
    fn priority_rasterizer_keeps_one_glyph_per_char() {
        let requests = Arc::new(crate::mutex::Mutex::new(Vec::new()));
        let family = "👨\u{200D}👩\u{200D}👧";
        let mut fonts =
            test_fonts().with_glyph_rasterizer(priority_rasterizer_for(family, &requests));

        let galley = layout_simple(&mut fonts, family);

        let summary = glyph_summary(&galley);
        assert_eq!(summary.len(), family.chars().count());
        assert_eq!(summary[0], ('👨', true, true));
        assert!(summary[1..].iter().all(|(_, _, has_pixels)| !has_pixels));
    }

    #[test]
    fn overflow_character_uses_priority_rasterizer() {
        let requests = Arc::new(crate::mutex::Mutex::new(Vec::new()));
        let mut fonts = test_fonts().with_glyph_rasterizer(priority_rasterizer_for("…", &requests));

        let mut job = LayoutJob::simple(
            "Hello wide world".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        job.wrap = TextWrapping::truncate_at_width(40.0);
        let galley = layout(&mut fonts, 1.0, Arc::new(job));

        let glyphs = &galley.rows[0].row.glyphs;
        let last = &glyphs[glyphs.len() - 1];
        assert_eq!(last.chr, '…');
        assert!(last.is_color, "Should come from the priority rasterizer");
        assert!(!last.uv_rect.is_nothing());
    }

    #[test]
    fn test_zero_max_width() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();
        let mut layout_job = LayoutJob::single_section("W".into(), TextFormat::default());
        layout_job.wrap.max_width = 0.0;
        let galley = layout(&mut fonts, pixels_per_point, layout_job.into());
        assert_eq!(galley.rows.len(), 1);
    }

    #[test]
    fn test_truncate_with_newline() {
        // No matter where we wrap, we should be appending the newline character.

        let pixels_per_point = 1.0;

        let mut fonts = test_fonts();
        let text_format = TextFormat {
            font_id: FontId::monospace(12.0),
            ..Default::default()
        };

        for text in ["Hello\nworld", "\nfoo"] {
            for break_anywhere in [false, true] {
                for max_width in [0.0, 5.0, 10.0, 20.0, f32::INFINITY] {
                    let mut layout_job =
                        LayoutJob::single_section(text.into(), text_format.clone());
                    layout_job.wrap.max_width = max_width;
                    layout_job.wrap.max_rows = 1;
                    layout_job.wrap.break_anywhere = break_anywhere;

                    let galley = layout(&mut fonts, pixels_per_point, layout_job.into());

                    assert!(galley.elided);
                    assert_eq!(galley.rows.len(), 1);
                    let row_text = galley.rows[0].text();
                    assert!(
                        row_text.ends_with('…'),
                        "Expected row to end with `…`, got {row_text:?} when line-breaking the text {text:?} with max_width {max_width} and break_anywhere {break_anywhere}.",
                    );
                }
            }
        }

        {
            let mut layout_job = LayoutJob::single_section("Hello\nworld".into(), text_format);
            layout_job.wrap.max_width = 50.0;
            layout_job.wrap.max_rows = 1;
            layout_job.wrap.break_anywhere = false;

            let galley = layout(&mut fonts, pixels_per_point, layout_job.into());

            assert!(galley.elided);
            assert_eq!(galley.rows.len(), 1);
            let row_text = galley.rows[0].text();
            assert_eq!(row_text, "Hello…");
        }
    }

    #[test]
    #[cfg(feature = "default_fonts")]
    // No bundled font has CJK glyphs, so the line breaks depend on
    // the width of the `.notdef` glyph of the primary font.
    fn test_cjk() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();
        let mut layout_job = LayoutJob::single_section(
            "日本語とEnglishの混在した文章".into(),
            TextFormat::default(),
        );
        layout_job.wrap.max_width = 90.0;
        let galley = layout(&mut fonts, pixels_per_point, layout_job.into());
        assert_eq!(
            galley.rows.iter().map(|row| row.text()).collect::<Vec<_>>(),
            vec!["日本語とEnglishの混", "在した文章"]
        );
    }

    #[test]
    #[cfg(feature = "default_fonts")]
    // No bundled font has CJK glyphs, so the line breaks depend on
    // the width of the `.notdef` glyph of the primary font.
    fn test_pre_cjk() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();
        let mut layout_job = LayoutJob::single_section(
            "日本語とEnglishの混在した文章".into(),
            TextFormat::default(),
        );
        layout_job.wrap.max_width = 110.0;
        let galley = layout(&mut fonts, pixels_per_point, layout_job.into());
        assert_eq!(
            galley.rows.iter().map(|row| row.text()).collect::<Vec<_>>(),
            vec!["日本語とEnglishの混在した", "文章"]
        );
    }

    #[test]
    fn test_truncate_width() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();
        let mut layout_job =
            LayoutJob::single_section("# DNA\nMore text".into(), TextFormat::default());
        layout_job.wrap.max_width = f32::INFINITY;
        layout_job.wrap.max_rows = 1;
        layout_job.round_output_to_gui = false;
        let galley = layout(&mut fonts, pixels_per_point, layout_job.into());
        assert!(galley.elided);
        assert_eq!(
            galley.rows.iter().map(|row| row.text()).collect::<Vec<_>>(),
            vec!["# DNA…"]
        );
        let row = &galley.rows[0];
        assert_eq!(row.pos, Pos2::ZERO);
        assert_eq!(row.rect().max.x, row.glyphs.last().unwrap().max_x());
    }

    #[test]
    fn test_truncate_with_pixels_per_point() {
        let mut fonts = test_fonts();

        for pixels_per_point in [
            0.33, 0.5, 0.67, 1.0, 1.25, 1.33, 1.5, 1.75, 2.0, 3.0, 4.0, 5.0,
        ] {
            for ch in ['W', 'A', 'n', 't', 'i'] {
                let target_width = 50.0;
                let text = (0..20).map(|_| ch).collect::<String>();

                let mut job = LayoutJob::single_section(text, TextFormat::default());
                job.wrap.max_width = target_width;
                job.wrap.max_rows = 1;
                let elided_galley = layout(&mut fonts, pixels_per_point, job.into());
                assert!(elided_galley.elided);

                let test_galley = layout(
                    &mut fonts,
                    pixels_per_point,
                    Arc::new(LayoutJob::single_section(
                        iter::chain(
                            (0..elided_galley.rows[0].char_count_excluding_newline().0).map(|_| ch),
                            iter::once('…'),
                        )
                        .collect::<String>(),
                        TextFormat::default(),
                    )),
                );

                assert!(elided_galley.size().x >= 0.0);
                assert!(elided_galley.size().x <= target_width);
                assert!(test_galley.size().x > target_width);
            }
        }
    }

    #[test]
    fn test_empty_row() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();

        let font_id = FontId::default();
        let family = fonts.family_key(&font_id.family);
        let font_height = fonts
            .family_metrics(
                family,
                pixels_per_point,
                font_id.size,
                &VariationCoords::default(),
            )
            .row_height;

        let job = LayoutJob::simple(String::new(), font_id, Color32::WHITE, f32::INFINITY);

        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 1, "Expected one row");
        assert_eq!(
            galley.rows[0].row.glyphs.len(),
            0,
            "Expected no glyphs in the empty row"
        );
        assert_eq!(
            galley.size(),
            Vec2::new(0.0, font_height.round()),
            "Unexpected galley size"
        );
        assert_eq!(
            galley.intrinsic_size(),
            Vec2::new(0.0, font_height.round()),
            "Unexpected intrinsic size"
        );
    }

    #[test]
    #[cfg(feature = "default_fonts")]
    fn test_end_with_newline() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();

        let font_id = FontId::default();
        let family = fonts.family_key(&font_id.family);
        let font_height = fonts
            .family_metrics(
                family,
                pixels_per_point,
                font_id.size,
                &VariationCoords::default(),
            )
            .row_height;

        let job = LayoutJob::simple("Hi!\n".to_owned(), font_id, Color32::WHITE, f32::INFINITY);

        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 2, "Expected two rows");
        assert_eq!(
            galley.rows[1].row.glyphs.len(),
            0,
            "Expected no glyphs in the empty row"
        );
        assert_eq!(
            galley.size().round(),
            Vec2::new(17.0, font_height.round() * 2.0),
            "Unexpected galley size"
        );
        assert_eq!(
            galley.intrinsic_size().round(),
            Vec2::new(17.0, font_height.round() * 2.0),
            "Unexpected intrinsic size"
        );
    }

    #[test]
    fn test_valign_with_custom_line_height() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();

        let font_id = FontId::default();
        let family = fonts.family_key(&font_id.family);
        let font_height = fonts
            .family_metrics(
                family,
                pixels_per_point,
                font_id.size,
                &VariationCoords::default(),
            )
            .row_height;
        let extra_height = 20.0;
        let line_height = (font_height + extra_height).round();
        let free_space = line_height - font_height;

        // Row height, and y-position of the last glyph (in the last section):
        let mut glyph_y = |sections: &[(Option<f32>, Align)]| {
            let mut job = LayoutJob::default();
            for &(line_height, valign) in sections {
                job.append(
                    "Hello",
                    0.0,
                    TextFormat {
                        font_id: font_id.clone(),
                        line_height,
                        valign,
                        ..Default::default()
                    },
                );
            }
            let galley = layout(&mut fonts, pixels_per_point, job.into());
            assert_eq!(galley.rows.len(), 1);
            let glyph = galley.rows[0].row.glyphs.last().unwrap();
            (galley.size().y, glyph.pos.y)
        };

        let (default_height, baseline_y) = glyph_y(&[(None, Align::default())]);
        assert_eq!(default_height, font_height.round());

        // A custom line height always centers the text, regardless of `valign`
        // (including the default `valign`):
        for valign in [
            TextFormat::default().valign,
            Align::TOP,
            Align::Center,
            Align::BOTTOM,
        ] {
            let (height, y) = glyph_y(&[(Some(line_height), valign)]);
            assert_eq!(
                height, line_height,
                "Row should have the custom line height"
            );
            assert!(
                (y - baseline_y - 0.5 * free_space).abs() <= 1.0,
                "{valign:?}: text should be centered in the line: baseline_y={baseline_y}, y={y}, free_space={free_space}"
            );
        }

        // `valign` still governs how a section without a custom line height
        // is placed within a taller row:
        let tall = (Some(line_height), Align::Center);
        let (_, top) = glyph_y(&[tall, (None, Align::TOP)]);
        let (_, center) = glyph_y(&[tall, (None, Align::Center)]);
        let (_, bottom) = glyph_y(&[tall, (None, Align::BOTTOM)]);
        assert!((top - baseline_y).abs() <= 1.0, "top={top}");
        assert!(
            (center - top - 0.5 * free_space).abs() <= 1.0,
            "top={top}, center={center}, free_space={free_space}"
        );
        assert!(
            (bottom - top - free_space).abs() <= 1.0,
            "top={top}, bottom={bottom}, free_space={free_space}"
        );
    }

    #[test]
    fn test_combining_diacritics() {
        // ɔ̃ = U+0254 (LATIN SMALL LETTER OPEN O) + U+0303 (COMBINING TILDE)
        // With text shaping, the combining tilde should NOT produce a separate
        // advance — it should be positioned above ɔ via GPOS anchors.
        // Note: the default fonts don't contain U+0254, so the `.notdef` glyph
        // is used. The key test is that the combining mark does NOT add extra width.
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();

        let job_combined = LayoutJob::simple(
            "ɔ\u{0303}".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley_combined = layout(&mut fonts, pixels_per_point, job_combined.into());

        let job_base = LayoutJob::simple(
            "ɔ".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley_base = layout(&mut fonts, pixels_per_point, job_base.into());

        let width_combined = galley_combined.size().x;
        let width_base = galley_base.size().x;

        assert!(
            (width_combined - width_base).abs() < 2.0,
            "Combining diacritic should not add significant width. \
             Base width: {width_base}, Combined width: {width_combined}"
        );

        let glyphs = &galley_combined.rows[0].row.glyphs;
        assert!(!glyphs.is_empty(), "Expected at least 1 glyph for ɔ̃");
    }

    #[test]
    fn test_shaping_basic_latin() {
        // Basic test: shaped Latin text should produce the same number of glyphs as characters.
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();

        let job = LayoutJob::simple(
            "Hello".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 1);
        assert_eq!(galley.rows[0].row.glyphs.len(), 5);
        assert!(galley.size().x > 0.0);
    }

    #[test]
    fn test_shaping_empty_string() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();

        let job = LayoutJob::simple(
            String::new(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 1);
        assert_eq!(galley.rows[0].row.glyphs.len(), 0);
    }

    #[test]
    fn test_shaping_multiple_newlines() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();

        let job = LayoutJob::simple(
            "A\n\nB".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 3, "Expected 3 rows for 'A\\n\\nB'");
        assert_eq!(galley.rows[0].row.glyphs.len(), 1); // "A"
        assert_eq!(galley.rows[1].row.glyphs.len(), 0); // empty line
        assert_eq!(galley.rows[2].row.glyphs.len(), 1); // "B"
    }

    #[test]
    fn test_shaping_mixed_font_fallback() {
        // Text with both Latin and emoji should work without panicking,
        // even though they use different font faces.
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();

        let job = LayoutJob::simple(
            "Hi 🎉 bye".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(&mut fonts, pixels_per_point, job.into());

        assert_eq!(galley.rows.len(), 1);
        // "Hi " (3) + "🎉" (1) + " bye" (4) = at least 8 glyphs
        assert!(
            galley.rows[0].row.glyphs.len() >= 8,
            "Expected >= 8 glyphs, got {}",
            galley.rows[0].row.glyphs.len()
        );
    }

    #[test]
    #[cfg(feature = "default_fonts")]
    fn test_gpos_kerning() {
        // GPOS kerning: pairs like "AV", "VA", "AT" should be tighter than
        // the sum of individual character widths. Without text shaping, egui
        // only uses the legacy `kern` table, so these pairs had diff ≈ 0.
        // With harfrust, GPOS kerning applies proper negative adjustments.
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();
        let font_id = FontId::proportional(14.0);

        for pair in ["AV", "VA", "AT"] {
            let (pair_w, _, _) = measure_text(&mut fonts, pair, &font_id, pixels_per_point);
            let chars: Vec<char> = pair.chars().collect();
            let (w1, _, _) = measure_text(
                &mut fonts,
                &chars[0].to_string(),
                &font_id,
                pixels_per_point,
            );
            let (w2, _, _) = measure_text(
                &mut fonts,
                &chars[1].to_string(),
                &font_id,
                pixels_per_point,
            );
            let sum = w1 + w2;
            let kern_adjustment = sum - pair_w;

            assert!(
                kern_adjustment > 0.5,
                "GPOS kerning for '{pair}': expected pair to be noticeably tighter \
                 than sum of individuals. pair_width={pair_w:.2}, sum={sum:.2}, \
                 kern_adjustment={kern_adjustment:.2} (should be > 0.5)",
            );
        }
    }

    /// Regression test for <https://github.com/emilk/egui/issues/8087>.
    ///
    /// Multi-codepoint grapheme clusters (flag emojis, combining marks) must
    /// produce exactly as many glyphs as characters so that cursor positioning
    /// and text selection remain correct.
    #[test]
    fn test_grapheme_cluster_glyph_count() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();
        let font_id = FontId::default();

        // Each test case: (input text, expected char count)
        let cases: &[(&str, usize)] = &[
            // Flag emoji: two Regional Indicator codepoints → one visual glyph
            ("\u{1F1EF}\u{1F1F5}", 2), // 🇯🇵
            // Flag surrounded by ASCII
            ("A\u{1F1EB}\u{1F1F7}B", 4), // A🇫🇷B
            // Base char + combining acute accent
            ("e\u{0301}", 2), // é as decomposed
            // Multiple combining marks
            ("o\u{0302}\u{0323}", 3), // ộ
            // Plain ASCII (sanity check)
            ("Hello", 5),
        ];

        for &(text, expected_chars) in cases {
            let job = LayoutJob::simple(
                text.to_owned(),
                font_id.clone(),
                Color32::WHITE,
                f32::INFINITY,
            );
            let galley = layout(&mut fonts, pixels_per_point, job.into());

            let total_glyphs: usize = galley.rows.iter().map(|r| r.row.glyphs.len()).sum();

            assert_eq!(
                total_glyphs,
                expected_chars,
                "Glyph count mismatch for {text:?}: \
                 expected {expected_chars} glyphs (one per char), got {total_glyphs}. \
                 Glyphs: {:?}",
                galley.rows[0]
                    .row
                    .glyphs
                    .iter()
                    .map(|g| (g.chr, g.advance_width))
                    .collect::<Vec<_>>(),
            );

            // Verify that Row::text() reconstructs the input text.
            let row_text: String = galley.rows.iter().map(|r| r.text()).collect();
            assert_eq!(row_text, text, "Row::text() mismatch for {text:?}");

            // Verify cursor round-trip: end cursor index == char count.
            assert_eq!(
                galley.end().index.0,
                expected_chars,
                "Galley::end().index mismatch for {text:?}",
            );
        }
    }

    /// Verify that cursor positioning round-trips correctly for text
    /// containing multi-codepoint grapheme clusters (regression test for #8087).
    #[test]
    fn test_grapheme_cluster_cursor_roundtrip() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();
        let font_id = FontId::default();

        // "A" + flag emoji (2 codepoints) + "B" = 4 chars
        let text = "A\u{1F1EF}\u{1F1F5}B";
        let job = LayoutJob::simple(
            text.to_owned(),
            font_id.clone(),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(&mut fonts, pixels_per_point, job.into());

        // Walking through every cursor index should produce valid positions.
        for i in 0..=galley.end().index.0 {
            let cursor = CCursor {
                index: CharIndex(i),
                prefer_next_row: false,
            };
            let rect = galley.pos_from_cursor(cursor);
            assert!(
                rect.is_finite(),
                "pos_from_cursor returned non-finite rect for index {i}",
            );

            // Round-trip: position → cursor → position should be stable.
            let cursor2 = galley.cursor_from_pos(Vec2::new(rect.center().x, rect.center().y));
            let rect2 = galley.pos_from_cursor(cursor2);
            assert!(
                (rect.min.x - rect2.min.x).abs() < 1.0,
                "Cursor round-trip unstable at index {i}: \
                 first={}, second={}, cursor2.index={}",
                rect.min.x,
                rect2.min.x,
                cursor2.index,
            );
        }
    }

    /// The bundled fonts have no Hebrew, so these glyphs are `.notdef`;
    /// levels, order, glyph count and cursor geometry do not depend on that.
    #[test]
    fn bidi_text_keeps_one_glyph_per_char_in_logical_order() {
        let mut fonts = test_fonts();
        for text in ["שלום 42 עולם", "abc אבג 12", "אב", "١٢٣ עברית"] {
            let galley = layout_simple(&mut fonts, text);
            let row = &galley.rows[0].row;
            assert_eq!(row.glyphs.len(), text.chars().count(), "{text:?}");
            assert_eq!(row.text(), text);
            assert_eq!(galley.end().index.0, text.chars().count());
        }
    }

    #[test]
    fn rtl_rows_are_placed_in_visual_order() {
        let mut fonts = test_fonts();
        let x_of = |galley: &Galley, chr: char| {
            galley.rows[0]
                .row
                .glyphs
                .iter()
                .find(|g| g.chr == chr)
                .map(|g| g.pos.x)
                .unwrap_or_else(|| panic!("no glyph for {chr:?}"))
        };

        // A right-to-left paragraph: the first word ends up on the right,
        // and the number inside it still reads left to right.
        let galley = layout_simple(&mut fonts, "אב 12");
        let xs = ['1', '2', ' ', 'ב', 'א'].map(|c| x_of(&galley, c));
        assert!(xs.is_sorted(), "visual order of \"אב 12\": {xs:?}");
        // The space takes the paragraph direction (UAX #9 N1/N2); the digits do not.
        for glyph in &galley.rows[0].row.glyphs {
            assert_eq!(
                glyph.is_rtl(),
                matches!(glyph.chr, 'א' | 'ב' | ' '),
                "{:?}",
                glyph.chr
            );
        }

        // A left-to-right paragraph with a Hebrew word in it:
        // the word is mirrored, the line is not.
        let galley = layout_simple(&mut fonts, "ab אב");
        let xs = ['a', 'b', ' ', 'ב', 'א'].map(|c| x_of(&galley, c));
        assert!(xs.is_sorted(), "visual order of \"ab אב\": {xs:?}");

        // Plain text is untouched:
        let galley = layout_simple(&mut fonts, "ab 12");
        let xs = ['a', 'b', ' ', '1', '2'].map(|c| x_of(&galley, c));
        assert!(xs.is_sorted(), "{xs:?}");
    }

    /// A row ending in whitespace allocates the same width in both reading
    /// directions: the ink is trimmed for alignment, but the row's size keeps
    /// counting the whitespace, so a widget following at zero item spacing
    /// keeps its gap under right-to-left layout too.
    #[test]
    fn rtl_row_allocates_trailing_whitespace_like_its_ltr_twin() {
        let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
        let pixels_per_point = 2.0;
        let font_id = FontId::proportional(14.0);
        // Half a physical pixel: glyph positions are pixel-rounded.
        let tol = 0.5 / pixels_per_point;

        let mut ltr_job =
            LayoutJob::simple_singleline("abc ".into(), font_id.clone(), Color32::WHITE);
        ltr_job.direction = TextDirection::LeftToRight;
        let ltr = layout(&mut fonts, pixels_per_point, ltr_job.into());

        let mut rtl_job = LayoutJob::simple_singleline("abc ".into(), font_id, Color32::WHITE);
        rtl_job.direction = TextDirection::RightToLeft;
        let rtl = layout(&mut fonts, pixels_per_point, rtl_job.into());

        assert_eq!(ltr.rows.len(), 1);
        assert_eq!(rtl.rows.len(), 1);
        assert_eq!(
            rtl.size().x,
            ltr.size().x,
            "the rtl row must allocate the same width as its ltr twin"
        );
        // The ink is the same in both directions: the trailing whitespace sits
        // outside it, but the row still allocates room for it. A single row
        // hangs from its own reading start, so both rows anchor at x = 0:
        let ltr_ink = ink_rect(&ltr);
        let rtl_ink = ink_rect(&rtl);
        assert!(
            (rtl_ink.left() - ltr_ink.left()).abs() <= tol,
            "{rtl_ink:?}"
        );
        assert!(
            (rtl_ink.right() - ltr_ink.right()).abs() <= tol,
            "{rtl_ink:?}"
        );
        assert!(
            (rtl_ink.left() - rtl.rect.left()).abs() <= tol,
            "the row rect starts at the ink: {:?} vs {:?}",
            rtl_ink,
            rtl.rect
        );
        assert!(
            rtl.size().x > rtl_ink.width(),
            "the whitespace must be allocated: {:?} vs {rtl_ink:?}",
            rtl.size()
        );
    }

    /// The bounding rect of the non-whitespace glyphs ("the ink"), in galley
    /// space.
    fn ink_rect(galley: &Galley) -> Rect {
        galley
            .rows
            .iter()
            .map(|row| {
                row.glyphs
                    .iter()
                    .filter(|glyph| !glyph.chr.is_whitespace())
                    .map(|glyph| glyph.logical_rect())
                    .fold(Rect::NOTHING, Rect::union)
                    .translate(row.pos.to_vec2())
            })
            .fold(Rect::NOTHING, Rect::union)
    }

    /// Trailing whitespace never skews the ink of centered or right-anchored
    /// rows: they align around their ink (whitespace excluded), while the
    /// allocated width still includes it.
    #[test]
    fn trailing_whitespace_does_not_skew_center_or_right_ink() {
        let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
        let pixels_per_point = 2.0;
        let font_id = FontId::proportional(14.0);
        // Half a physical pixel: glyph positions are pixel-rounded.
        let tol = 0.5 / pixels_per_point;

        let mut left_job =
            LayoutJob::simple_singleline("abc ".into(), font_id.clone(), Color32::WHITE);
        left_job.direction = TextDirection::LeftToRight;
        let ink_width = ink_rect(&layout(&mut fonts, pixels_per_point, left_job.into())).width();

        for halign in [Align::Center, Align::RIGHT] {
            let mut job =
                LayoutJob::simple_singleline("abc ".into(), font_id.clone(), Color32::WHITE);
            job.direction = TextDirection::LeftToRight;
            job.halign = halign;
            let galley = layout(&mut fonts, pixels_per_point, job.into());
            assert_eq!(galley.rows.len(), 1);

            // The ink, in galley space: the non-whitespace glyphs, offset by
            // the row position.
            let ink = ink_rect(&galley);
            match halign {
                Align::Center => {
                    assert!(
                        (ink.center().x).abs() <= tol,
                        "center ink skewed: {:?}",
                        ink.center().x
                    );
                }
                Align::RIGHT => {
                    assert!(
                        ink.right().abs() <= tol,
                        "right ink skewed: {:?}",
                        ink.right()
                    );
                }
                Align::LEFT => unreachable!(),
            }
            assert!(
                (ink.width() - ink_width).abs() <= tol,
                "ink width changed: {:?} vs {ink_width:?}",
                ink.width()
            );
        }
    }

    #[test]
    fn rtl_cursor_sits_on_the_right_side_of_its_glyph() {
        let mut fonts = test_fonts();
        let galley = layout_simple(&mut fonts, "אב");
        let row = &galley.rows[0].row;
        let [alef, bet] = [&row.glyphs[0], &row.glyphs[1]];
        assert!(bet.pos.x < alef.pos.x, "bet is drawn left of alef");
        assert_eq!(row.x_offset(CharIndex(0)), alef.max_x());
        assert_eq!(row.x_offset(CharIndex(1)), bet.max_x());
        assert_eq!(row.x_offset(CharIndex(2)), bet.pos.x);

        for i in 0..=2 {
            let cursor = CCursor {
                index: CharIndex(i),
                prefer_next_row: false,
            };
            let rect = galley.pos_from_cursor(cursor);
            let back = galley.cursor_from_pos(rect.center().to_vec2());
            assert_eq!(back.index.0, i, "cursor round-trip at {i}");
        }
    }

    /// Regression test for <https://github.com/emilk/egui/issues/8576>.
    ///
    /// The shaper returns a right-to-left run in visual order, with decreasing
    /// cluster offsets. Such a run must still produce one glyph per character.
    #[test]
    fn test_right_to_left_run_glyph_count() {
        let pixels_per_point = 1.0;
        let mut fonts = test_fonts();
        let font_id = FontId::default();

        let cases = [
            "ب",
            "بب",
            "ببب",
            "אב",
            "a אבג b",
            "שלום 42 עולם",
            // Base char + combining mark inside a right-to-left run
            "ב\u{05BC}א",
        ];

        for text in cases {
            let job = LayoutJob::simple(
                text.to_owned(),
                font_id.clone(),
                Color32::WHITE,
                f32::INFINITY,
            );
            let galley = layout(&mut fonts, pixels_per_point, job.into());

            let total_glyphs: usize = galley.rows.iter().map(|r| r.row.glyphs.len()).sum();
            let num_chars = text.chars().count();

            assert_eq!(total_glyphs, num_chars, "Glyph count mismatch for {text:?}");
            assert_eq!(
                galley.end().index.0,
                num_chars,
                "Galley::end().index mismatch for {text:?}",
            );
        }
    }

    /// Arabic-Indic digits belong to the Arabic script, so a shaper left to guess the direction
    /// shapes them right to left and "٤٥" comes out as "٥٤". Outside right-to-left text the bidi
    /// algorithm keeps them left to right (<https://github.com/crmne/zapfast/issues/184>).
    #[test]
    fn arabic_indic_digits_without_rtl_text_stay_left_to_right() {
        let mut fonts = test_fonts();
        for text in ["\u{664}\u{665}", "\u{664}\u{665} messages"] {
            let galley = layout_simple(&mut fonts, text);
            let glyphs = &galley.rows[0].row.glyphs;
            assert_eq!(glyphs.len(), text.chars().count(), "{text:?}");
            assert_eq!(galley.rows[0].row.text(), text);
            assert!(
                glyphs[0].pos.x < glyphs[1].pos.x,
                "{text:?}: four is drawn left of five"
            );
        }
    }

    fn layout_with_direction(
        fonts: &mut FontsImpl,
        text: &str,
        direction: TextDirection,
    ) -> Galley {
        let mut job = LayoutJob::simple(
            text.to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        job.direction = direction;
        layout(fonts, 1.0, Arc::new(job))
    }

    #[test]
    fn paragraph_direction_is_auto_or_forced() {
        let mut fonts = test_fonts();

        // Each paragraph takes the direction of its first strong character,
        // or of the paragraph before it if it has none:
        let galley =
            layout_with_direction(&mut fonts, "abc אבג\nאבג abc\n123", TextDirection::Auto);
        let rtl: Vec<bool> = galley.rows.iter().map(|row| row.rtl_paragraph).collect();
        assert_eq!(rtl, [false, true, true]);
        assert_visual_order(&galley.rows[0].row, &[0, 1, 2, 6, 5, 4]);
        assert_visual_order(&galley.rows[1].row, &[4, 5, 6, 2, 1, 0]);

        // A forced direction overrides the first strong character:
        let galley = layout_with_direction(&mut fonts, "abc אבג", TextDirection::RightToLeft);
        assert!(galley.rows[0].rtl_paragraph);
        assert_visual_order(&galley.rows[0].row, &[6, 5, 4, 0, 1, 2]);

        let galley = layout_with_direction(&mut fonts, "אבג abc", TextDirection::LeftToRight);
        assert!(!galley.rows[0].rtl_paragraph);
        assert_visual_order(&galley.rows[0].row, &[2, 1, 0, 4, 5, 6]);

        // Text without a right-to-left character can still be in a right-to-left paragraph:
        let galley = layout_with_direction(&mut fonts, "abc", TextDirection::RightToLeft);
        assert!(galley.rows[0].rtl_paragraph);
        assert!(
            galley.rows[0]
                .glyphs
                .iter()
                .all(|glyph| glyph.bidi_level == 2)
        );
        assert_visual_order(&galley.rows[0].row, &[0, 1, 2]);
    }

    /// With `Auto`, a paragraph without a strong character takes the direction of the one before,
    /// both when the job is laid out whole and when `Fonts` lays out each paragraph on its own.
    #[test]
    fn neutral_paragraphs_take_the_direction_before_them() {
        let cases: [(&str, &[bool]); 5] = [
            // An empty line after Hebrew, and the empty line after a final newline:
            ("אבג\n\nabc\n\n", &[true, true, false, false, false]),
            ("אבג\n", &[true, true]),
            // Digits after Arabic, and after English:
            ("ثمن\n123 456\nabc\n123", &[true, true, false, false]),
            // The first paragraph has nothing before it:
            ("123\nאבג\n", &[false, true, true]),
            ("\n\nאבג", &[false, false, true]),
        ];
        let mut fonts = test_fonts();
        let mut cached = Fonts::new(TextOptions::default(), FontDefinitions::default());
        for (text, expected) in cases {
            let whole = layout_with_direction(&mut fonts, text, TextDirection::Auto);
            let job = LayoutJob::simple(
                text.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
                f32::INFINITY,
            );
            let split = cached.with_pixels_per_point(1.0).layout_job(job);
            for galley in [&whole, &*split] {
                let rtl: Vec<bool> = galley.rows.iter().map(|row| row.rtl_paragraph).collect();
                assert_eq!(rtl, expected, "{text:?}");
            }
        }

        // Digits in a right-to-left paragraph get the levels of one:
        let galley = layout_with_direction(&mut fonts, "ثمن\n12 34", TextDirection::Auto);
        let row = &galley.rows[1].row;
        let levels: Vec<u8> = row.glyphs.iter().map(|g| g.bidi_level).collect();
        assert_eq!(levels, [2, 2, 1, 2, 2]);
        assert_visual_order(row, &[3, 4, 2, 0, 1]);
    }

    /// `Fonts` lays out each paragraph of a multi-paragraph job on its own;
    /// every one of them must keep the job's direction.
    #[test]
    fn paragraph_direction_survives_the_paragraph_cache() {
        let mut fonts = Fonts::new(TextOptions::default(), FontDefinitions::default());
        let mut job = LayoutJob::simple(
            "abc\ndef\n".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        job.direction = TextDirection::RightToLeft;
        let galley = fonts.with_pixels_per_point(1.0).layout_job(job);
        assert_eq!(galley.rows.len(), 3);
        assert!(galley.rows.iter().all(|row| row.rtl_paragraph));
    }

    /// The left and right edge of the visible glyphs of a row, in galley coordinates.
    fn visible_x_range(placed_row: &PlacedRow) -> Rangef {
        let visible = placed_row
            .glyphs
            .iter()
            .filter(|glyph| !glyph.chr.is_whitespace());
        let min = visible
            .clone()
            .map(|g| g.pos.x)
            .fold(f32::INFINITY, f32::min);
        let max = visible.map(Glyph::max_x).fold(f32::NEG_INFINITY, f32::max);
        Rangef::new(placed_row.pos.x + min, placed_row.pos.x + max)
    }

    #[test]
    fn rtl_paragraphs_start_on_the_right() {
        let text = "abcdefghij\nאב גד\nאב";
        let job = |halign: Align, halign_min_is_start: bool| {
            let mut job = LayoutJob::simple(
                text.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
                f32::INFINITY,
            );
            job.halign = halign;
            job.halign_min_is_start = halign_min_is_start;
            job
        };
        let mut fonts = test_fonts();
        let close = |a: f32, b: f32| (a - b).abs() <= 0.5;

        // The start of a right-to-left paragraph is on the right,
        // flush with the widest row; the galley still starts at x = 0:
        let galley = layout(&mut fonts, 1.0, Arc::new(job(Align::LEFT, true)));
        let ranges: Vec<Rangef> = galley.rows.iter().map(visible_x_range).collect();
        assert_eq!(galley.rect.min.x, 0.0);
        assert!(close(ranges[0].min, 0.0), "{ranges:?}");
        for range in &ranges[1..] {
            assert!(close(range.max, ranges[0].max), "{ranges:?}");
            assert!(0.0 < range.min, "{ranges:?}");
        }

        // `Fonts` lays out each paragraph on its own, then joins them:
        let joined = Fonts::new(TextOptions::default(), test_font_definitions())
            .with_pixels_per_point(1.0)
            .layout_job(job(Align::LEFT, true));
        let joined_ranges: Vec<Rangef> = joined.rows.iter().map(visible_x_range).collect();
        assert_eq!(joined_ranges, ranges);
        assert_eq!(joined.rect.min.x, 0.0);

        // An explicit left alignment:
        let galley = layout(&mut fonts, 1.0, Arc::new(job(Align::LEFT, false)));
        for placed_row in &galley.rows {
            assert!(close(visible_x_range(placed_row).min, 0.0));
        }

        // Right and center alignment do not depend on the direction:
        let galley = layout(&mut fonts, 1.0, Arc::new(job(Align::RIGHT, true)));
        for placed_row in &galley.rows {
            assert!(close(visible_x_range(placed_row).max, 0.0));
        }
        let galley = layout(&mut fonts, 1.0, Arc::new(job(Align::Center, true)));
        for placed_row in &galley.rows {
            let range = visible_x_range(placed_row);
            assert!((range.min + range.max).abs() <= 1.0, "{range:?}");
        }
    }

    #[test]
    fn elision_is_at_the_visual_end_of_the_row() {
        let mut fonts = test_fonts();
        for (text, rtl) in [
            ("אבגד הוזח טיכל מנסע", true),
            ("שלום hello world again", true),
            ("ab אבגד הוזח טיכל מנסע", false),
        ] {
            let full_width = layout_simple(&mut fonts, text).size().x;
            let mut job = LayoutJob::simple_singleline(
                text.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
            );
            job.wrap = TextWrapping::truncate_at_width(full_width / 2.0);
            let galley = layout(&mut fonts, 1.0, Arc::new(job));
            assert!(galley.elided);
            let row = &galley.rows[0].row;
            assert_eq!(row.rtl_paragraph, rtl);

            // The logically first text is kept, followed by the overflow character:
            let kept = row.text();
            assert!(kept.ends_with('…'), "{kept:?}");
            assert!(text.starts_with(kept.trim_end_matches('…')), "{kept:?}");

            let ellipsis = row.glyphs.last().unwrap();
            let others = &row.glyphs[..row.glyphs.len() - 1];
            // The bundled fonts installed by `test_font_definitions` include the
            // overflow character, so it always has width:
            assert!(0.0 < ellipsis.advance_width, "{kept:?}");
            let at_visual_end = |g: &Glyph| {
                if rtl {
                    ellipsis.max_x() <= g.pos.x
                } else {
                    g.max_x() <= ellipsis.pos.x
                }
            };
            assert!(others.iter().all(at_visual_end), "{kept:?}");
        }
    }

    #[test]
    fn whitespace_at_the_end_of_a_row_takes_the_paragraph_level() {
        let mut fonts = test_fonts();
        let mut layout_left = |text: &str, max_width: f32| {
            let mut job = LayoutJob::simple(
                text.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
                max_width,
            );
            job.halign_min_is_start = false;
            layout(&mut fonts, 1.0, Arc::new(job))
        };
        for (first_row, rest, rtl) in [("אבג abc ", "def", true), ("abc אבג ", "דהו", false)]
        {
            let width = layout_left(first_row, f32::INFINITY).size().x;
            let galley = layout_left(&format!("{first_row}{rest}"), width + 1.0);
            assert_eq!(galley.rows.len(), 2);
            let row = &galley.rows[0].row;
            assert_eq!(row.text(), first_row);
            assert_eq!(row.rtl_paragraph, rtl);

            // Inside the paragraph this space is between two words of the other direction,
            // but where the row wraps it goes to the visual end of the row:
            let space = row.glyphs.last().unwrap();
            assert_eq!(space.bidi_level, u8::from(rtl));
            let others = &row.glyphs[..row.glyphs.len() - 1];
            if rtl {
                assert!(others.iter().all(|g| space.pos.x < g.pos.x));
            } else {
                assert!(others.iter().all(|g| g.pos.x < space.pos.x));
            }
        }

        // A tab is a segment separator and also takes the paragraph level:
        let galley = layout_with_direction(&mut fonts, "abc \tdef", TextDirection::RightToLeft);
        let levels: Vec<u8> = galley.rows[0].glyphs.iter().map(|g| g.bidi_level).collect();
        assert_eq!(levels, [2, 2, 2, 1, 1, 2, 2, 2]);
    }

    #[test]
    fn reordering_a_long_mixed_paragraph_takes_linear_steps() {
        let mut fonts = test_fonts();
        let text: String = iter::repeat_n("abc אבג 123 (דה) x: ", 500).collect();
        let num_chars = text.chars().count();
        assert!(10_000 <= num_chars);

        for max_width in [f32::INFINITY, 300.0] {
            REORDER_STEPS.with(|steps| steps.set(0));
            let job = LayoutJob::simple(
                text.clone(),
                FontId::proportional(14.0),
                Color32::WHITE,
                max_width,
            );
            let galley = layout(&mut fonts, 1.0, Arc::new(job));
            let steps = REORDER_STEPS.with(core::cell::Cell::get);
            assert!(
                0 < steps && steps <= 6 * num_chars,
                "{steps} steps for {num_chars} chars"
            );

            // The cached order is a permutation that matches where the glyphs are:
            for placed_row in &galley.rows {
                let row = &placed_row.row;
                let mut order: Vec<usize> = row.visual_order().collect();
                let xs: Vec<f32> = order
                    .iter()
                    .filter(|&&i| 0.0 < row.glyphs[i].advance_width)
                    .map(|&i| row.glyphs[i].pos.x)
                    .collect();
                assert!(xs.is_sorted(), "{:?}", row.text());
                order.sort_unstable();
                assert!(order.into_iter().eq(0..row.glyphs.len()));
            }
        }
    }

    /// Asserts that the glyphs at these char indices are placed left to right.
    #[track_caller]
    fn assert_visual_order(row: &Row, left_to_right: &[usize]) {
        let xs: Vec<f32> = left_to_right.iter().map(|&i| row.glyphs[i].pos.x).collect();
        assert!(
            xs.is_sorted_by(|a, b| a < b),
            "{:?}: glyphs {left_to_right:?} are at {xs:?}",
            row.text()
        );
    }

    #[test]
    #[cfg(feature = "default_fonts")]
    fn arabic_with_digits_and_brackets() {
        let mut fonts = Fonts::new(TextOptions::default(), FontDefinitions::default());
        let mut layout = |text: &str| {
            fonts.with_pixels_per_point(1.0).layout_no_wrap(
                text.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
            )
        };
        let text = "عدد (123) هنا";
        let galley = layout(text);
        let row = &galley.rows[0].row;
        assert_eq!(row.text(), text);

        // The number reads left to right, the words and the brackets right to left:
        assert_visual_order(row, &[12, 11, 10, 9, 8, 5, 6, 7, 4, 3, 2, 1, 0]);

        // The shaper mirrors a bracket in right-to-left text,
        // so the logical "(" is drawn as ")" on the right of the number:
        let ltr = layout("(x)");
        let image = fonts.image();
        // Which bracket a bitmap shows, by the side its middle row is inked on.
        // (Comparing bitmaps directly fails on their different sub-pixel offsets.)
        let shape = |glyph: &Glyph| {
            let [x0, y0] = glyph.uv_rect.min.map(usize::from);
            let [x1, y1] = glyph.uv_rect.max.map(usize::from);
            let y = usize::midpoint(y0, y1);
            let ink: Vec<f32> = (x0..x1).map(|x| f32::from(image[(x, y)].a())).collect();
            let weighted: f32 = ink.iter().enumerate().map(|(i, a)| i as f32 * a).sum();
            let centroid = weighted / ink.iter().sum::<f32>();
            if centroid < (ink.len() - 1) as f32 / 2.0 {
                '('
            } else {
                ')'
            }
        };
        assert_eq!(shape(&ltr.rows[0].row.glyphs[0]), '(');
        assert_eq!(shape(&ltr.rows[0].row.glyphs[2]), ')');
        assert_eq!(shape(&row.glyphs[4]), ')');
        assert_eq!(shape(&row.glyphs[8]), '(');
    }

    #[test]
    fn mixed_hebrew_and_latin_in_one_paragraph() {
        let mut fonts = test_fonts();

        // Left-to-right paragraph: only the Hebrew word is reversed.
        let galley = layout_simple(&mut fonts, "Hello שלום world");
        let ltr: Vec<usize> =
            core::iter::chain(core::iter::chain(0..=5, [9, 8, 7, 6]), 10..=15).collect();
        assert_visual_order(&galley.rows[0].row, &ltr);

        // Right-to-left paragraph: the words run right to left, the Latin word inside reads left to right.
        let galley = layout_simple(&mut fonts, "שלום Hello עולם");
        let rtl: Vec<usize> =
            core::iter::chain(core::iter::chain((10..=14).rev(), 5..=9), (0..=4).rev()).collect();
        assert_visual_order(&galley.rows[0].row, &rtl);

        // Each wrapped row is reordered on its own, and keeps its logical chars:
        let width = layout_simple(&mut fonts, "Hello ").size().x;
        let job = LayoutJob::simple(
            "שלום Hello עולם".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            width + 1.0,
        );
        let galley = layout(&mut fonts, 1.0, Arc::new(job));
        let texts: Vec<String> = galley.rows.iter().map(|row| row.row.text()).collect();
        assert_eq!(texts, ["שלום ", "Hello ", "עולם"]);
        assert_visual_order(&galley.rows[0].row, &[4, 3, 2, 1, 0]);
        // The space after "Hello" is right to left, like the paragraph, so it ends up on the left:
        assert_visual_order(&galley.rows[1].row, &[5, 0, 1, 2, 3, 4]);
        assert_visual_order(&galley.rows[2].row, &[3, 2, 1, 0]);
    }

    /// A click on the left or right half of a glyph puts the cursor on that side of it,
    /// in the glyph's own direction, also where the direction changes.
    #[test]
    fn cursor_hit_testing_across_a_direction_boundary() {
        let mut fonts = test_fonts();
        for text in ["abc אבג def", "אבג abc דהו"] {
            let galley = layout_simple(&mut fonts, text);
            let row = &galley.rows[0].row;
            let y = row.size.y / 2.0;
            for (i, glyph) in row.glyphs.iter().enumerate() {
                let rect = glyph.logical_rect();
                let left = galley.cursor_from_pos(vec2(rect.lerp_inside([0.25, 0.5]).x, y));
                let right = galley.cursor_from_pos(vec2(rect.lerp_inside([0.75, 0.5]).x, y));
                let (before, after) = if glyph.is_rtl() {
                    (right, left)
                } else {
                    (left, right)
                };
                assert_eq!(before.index.0, i, "{text:?}: before {:?}", glyph.chr);
                assert_eq!(after.index.0, i + 1, "{text:?}: after {:?}", glyph.chr);
            }
        }
    }

    /// A selection that is contiguous in logical order can be split on screen.
    #[test]
    fn selection_across_a_direction_boundary() {
        let mut fonts = test_fonts();
        let galley = layout_simple(&mut fonts, "abc אבג def");
        let row = &galley.rows[0].row;
        let span = |chars: Range<usize>| {
            let glyphs = &row.glyphs[chars];
            let min = glyphs.iter().map(|g| g.pos.x).fold(f32::INFINITY, f32::min);
            let max = glyphs
                .iter()
                .map(Glyph::max_x)
                .fold(f32::NEG_INFINITY, f32::max);
            Rangef::new(min, max)
        };
        let ranges =
            |chars: Range<usize>| row.x_ranges(CharIndex(chars.start)..CharIndex(chars.end));

        // "abc א" is "abc " on the left and "א" at the right end of the Hebrew word:
        assert_eq!(ranges(0..5).as_slice(), [span(0..4), span(4..5)]);
        // The Hebrew word alone:
        assert_eq!(ranges(4..7).as_slice(), [span(4..7)]);
        // "בג def" is the left of the Hebrew word, and everything right of it:
        assert_eq!(ranges(5..11).as_slice(), [span(5..7), span(7..11)]);

        // Left-to-right text: one range, from cursor to cursor.
        let galley = layout_simple(&mut fonts, "abc def");
        let row = &galley.rows[0].row;
        assert_eq!(
            row.x_ranges(CharIndex(1)..CharIndex(5)).as_slice(),
            [Rangef::new(
                row.x_offset(CharIndex(1)),
                row.x_offset(CharIndex(5))
            )]
        );
    }

    /// The end of a right-to-left row that ends in left-to-right text is after that text.
    #[test]
    fn rtl_row_ending_in_ltr_text_has_its_end_cursor_after_it() {
        let mut fonts = test_fonts();
        for text in ["ملف PDF", "صفحة 12 من 340"] {
            let galley = layout_simple(&mut fonts, text);
            let row = &galley.rows[0].row;
            let last = row.glyphs.last().unwrap();
            assert!(!last.is_rtl());
            assert_eq!(
                row.x_offset(row.char_count_excluding_newline()),
                last.max_x(),
                "{text:?}"
            );
        }
    }

    /// The cursor layer maps *logical* columns over the bidi-reordered
    /// row: under a right-to-left base direction the text-start caret (logical
    /// column 0) sits at the visual RIGHT edge and the text-end caret in the
    /// visual left half, clicks map visual x back to the logical column
    /// (clicking the visual middle of the Arabic word lands on the right
    /// grapheme boundary), and every column round-trips through its caret x.
    #[test]
    fn bidi_cursor_hit_test_follows_logical_order() {
        let mut fonts = test_fonts();
        let galley = layout_with_direction(&mut fonts, "عربي 123", TextDirection::Auto);
        assert_eq!(galley.rows.len(), 1);
        let row = &galley.rows[0];
        let n = row.row.char_count_excluding_newline().0;
        assert_eq!(n, 8, "one cursor column per character");
        let mid_y = (row.min_y() + row.max_y()) * 0.5;

        // Text start (logical column 0) sits at the visual RIGHT edge, the text
        // end (logical column n) in the visual left half:
        let pos = |column: usize| {
            galley
                .pos_from_layout_cursor(&LayoutCursor {
                    row: 0,
                    column: CharIndex(column),
                })
                .min
                .x
        };
        assert!(
            pos(0) > galley.rect.center().x,
            "the text-start caret must sit in the visual right half, got {} (rect {:?})",
            pos(0),
            galley.rect
        );
        assert!(
            pos(n) < galley.rect.center().x,
            "the text-end caret must sit in the visual left half, got {} (rect {:?})",
            pos(n),
            galley.rect
        );

        // Caret x decreases as the logical column advances through the
        // right-to-left run (reading right to left). Inside an embedded
        // left-to-right run the caret reads forward with that run — the
        // per-glyph caret mapping follows each glyph's own direction — so the
        // strict decrease is pinned for the Arabic prefix only:
        for column in 0..4 {
            assert!(
                pos(column) > pos(column + 1),
                "caret x must decrease from column {column} to {}: {} vs {}",
                column + 1,
                pos(column),
                pos(column + 1)
            );
        }

        // Clicking the visual middle of the Arabic word: the center of the `ب`
        // glyph (logical column 2) snaps the caret onto `ب`'s reading-start
        // (right) edge, between `ر` and `ب`:
        let ba = &row.row.glyphs[2];
        assert_eq!(ba.chr, 'ب');
        let ba_center = ba.pos.x + ba.advance_width * 0.5 + row.pos.x;
        let clicked = galley.cursor_from_pos(vec2(ba_center, mid_y));
        assert_eq!(
            clicked.index.0, 2,
            "clicking the visual center of `ب` must put the caret at its reading-start edge"
        );

        // x round-trip: every caret position maps back to its own column:
        for column in 0..=n {
            let back = galley.cursor_from_pos(vec2(pos(column), mid_y));
            assert_eq!(
                back.index.0, column,
                "clicking the caret x of column {column} must return that column"
            );
        }
    }

    /// One glyph slot per character: the continuation-glyph bookkeeping of RTL
    /// runs (whose shaper output is in descending cluster order) used to
    /// over-emit zero-advance duplicates for every character but the first,
    /// breaking the `glyphs.len() == char_count` invariant that all cursor
    /// code relies on.
    #[test]
    fn rtl_rows_have_one_glyph_slot_per_char() {
        let mut fonts = test_fonts();
        for text in ["تقرير", "عربي 123", "سلام"] {
            for direction in [TextDirection::Auto, TextDirection::RightToLeft] {
                let galley = layout_with_direction(&mut fonts, text, direction);
                assert_eq!(galley.rows.len(), 1);
                let count = galley.rows[0].row.glyphs.len();
                assert_eq!(
                    count,
                    text.chars().count(),
                    "glyph slots must equal the char count for {text:?} (direction {direction:?})"
                );
            }
        }
    }

    /// Every glyph is stamped with its bidi run's UAX #9 embedding level, so a
    /// row's visual glyph stream can be split back into directional runs at
    /// level changes (consumers: the accessibility `TextRun` builder). LTR rows
    /// stay all-zero (pure metadata — rendering is unaffected).
    #[test]
    fn bidi_levels_partition_rows_into_directional_runs() {
        let mut fonts = test_fonts();
        fn level_runs(galley: &Galley) -> Vec<(u8, String)> {
            assert_eq!(galley.rows.len(), 1, "expected a single-row galley");
            let mut runs: Vec<(u8, String)> = Vec::new();
            for glyph in &galley.rows[0].row.glyphs {
                match runs.last_mut() {
                    Some((level, text)) if *level == glyph.bidi_level => {
                        text.push(glyph.chr);
                    }
                    _ => runs.push((glyph.bidi_level, glyph.chr.to_string())),
                }
            }
            runs
        }

        // LTR base with an embedded RTL run: 0 / 1 / 0. Glyphs are stored
        // in logical order, so the runs come out in logical order:
        assert_eq!(
            level_runs(&layout_with_direction(
                &mut fonts,
                "Hello שלום world",
                TextDirection::Auto
            )),
            vec![
                (0, "Hello ".to_owned()),
                (1, "שלום".to_owned()),
                (0, " world".to_owned()),
            ]
        );
        // Two runs suffice when the RTL text ends the row:
        assert_eq!(
            level_runs(&layout_with_direction(
                &mut fonts,
                "Hello שלום",
                TextDirection::Auto
            )),
            vec![(0, "Hello ".to_owned()), (1, "שלום".to_owned())]
        );

        // RTL base with European numbers: the space between them resolves to
        // the base level, so it belongs to the RTL run, and the digits sit at
        // level 2 (even). Logical order: the Arabic run first, the digits after:
        assert_eq!(
            level_runs(&layout_with_direction(
                &mut fonts,
                "عربي 123",
                TextDirection::Auto
            )),
            vec![(1, "عربي ".to_owned()), (2, "123".to_owned())]
        );

        // Pure RTL: every glyph at the odd base level:
        for direction in [TextDirection::Auto, TextDirection::RightToLeft] {
            for glyph in &layout_with_direction(&mut fonts, "تقرير", direction).rows[0]
                .row
                .glyphs
            {
                assert!(
                    glyph.is_rtl(),
                    "pure-RTL glyph {:?} must carry an odd level",
                    glyph.chr
                );
            }
        }

        // Pure LTR: every glyph at level 0 (also with an explicit LTR pin):
        for direction in [TextDirection::Auto, TextDirection::LeftToRight] {
            for glyph in &layout_with_direction(&mut fonts, "Hello World 123", direction).rows[0]
                .row
                .glyphs
            {
                assert_eq!(
                    glyph.bidi_level, 0,
                    "pure-LTR glyph {:?} must carry level 0",
                    glyph.chr
                );
            }
        }
    }

    /// Continuation glyphs (zero-width stand-ins that keep
    /// `glyphs.len() == char_count` for multi-char clusters) carry their
    /// cluster's bidi level: they belong to the same directional run as the
    /// glyphs before them. No bundled font shapes a multi-glyph cluster, so
    /// this uses the synthetic shaper vector (see
    /// `descending_run_first_emitted_cluster_keeps_its_continuation_glyphs`).
    #[test]
    fn continuation_glyphs_carry_their_cluster_bidi_level() {
        let run_text = "ab\u{E000}";
        let mut fonts = test_fonts();
        let codepoints = [('b', 1), ('\u{E000}', 1), ('a', 0)];
        let (glyph_buffer, face_key, face_metrics) = shape_synthetic_run(&mut fonts, &codepoints);

        let family = fonts.family_key(&FontFamily::Proportional);
        // An odd level stands in for any RTL run: the point is that every
        // glyph of the run — continuations included — carries the run's level.
        let mut ctx = ShapingContext {
            family,
            pixels_per_point: 1.0,
            font_size: 14.0,
            line_height: face_metrics.row_height,
            extra_letter_spacing: 0.0,
            section_index: 0,
            font_metrics: face_metrics.clone(),
            is_first_glyph_in_section: true,
            bidi_level: 3,
        };
        let run = TextRun {
            font_key: face_key,
            byte_range: ByteRangeExt::full(run_text),
            bidi_level: 3,
            raster: None,
        };
        let mut paragraph = Paragraph::from_section_index(0);
        layout_shaped_run(
            &mut fonts,
            &run,
            run_text,
            &glyph_buffer,
            &face_metrics,
            &mut ctx,
            &mut paragraph,
        );

        assert_eq!(paragraph.glyphs.len(), run_text.chars().count());
        for glyph in &paragraph.glyphs {
            assert_eq!(glyph.bidi_level, 3, "glyph {:?}", glyph.chr);
            assert!(glyph.is_rtl());
        }
        // The continuation glyph (the zero-advance one) is covered too:
        assert!(paragraph.glyphs.iter().any(|g| g.advance_width == 0.0));
    }

    /// Shape `codepoints` — (char, cluster byte offset) pairs in output order —
    /// with hand-picked cluster values, so a cluster can be given fewer glyphs
    /// than characters. No bundled font produces a multi-char→fewer-glyphs
    /// cluster (no lam-alef ligature; flag RIs keep one glyph per char), so the
    /// continuation-glyph bookkeeping needs a synthetic glyph vector to
    /// exercise that shape. Non-monotone cluster levels keep the hand-picked
    /// (descending) values instead of rewriting them.
    fn shape_synthetic_run(
        fonts: &mut FontsImpl,
        codepoints: &[(char, u32)],
    ) -> (harfrust::GlyphBuffer, FontFaceKey, StyledMetrics) {
        let family = fonts.family_key(&FontFamily::Proportional);
        let face_key = fonts.resolve_face(family, 'a');
        let coords = VariationCoords::default();
        let face_metrics = fonts
            .face(face_key)
            .expect("the test font must resolve 'a'")
            .styled_metrics(1.0, 14.0, &coords);

        let mut buffer = harfrust::UnicodeBuffer::default();
        for (chr, cluster) in codepoints {
            buffer.add(*chr, *cluster);
        }
        buffer.set_cluster_level(harfrust::BufferClusterLevel::Characters);
        buffer.set_direction(harfrust::Direction::LeftToRight);
        buffer.guess_segment_properties();

        let face = fonts
            .face(face_key)
            .expect("the test font must resolve 'a'");
        let shaper = face.shaper_data().shaper(face.skrifa_font_ref()).build();
        let glyph_buffer = shaper.shape(buffer, harfrust::ShapeOptions::new());
        (glyph_buffer, face_key, face_metrics)
    }

    /// A multi-char cluster with fewer glyphs than chars at the logically-last
    /// position of a descending-order run keeps its continuation glyph.
    ///
    /// The synthetic vector emits two clusters in descending byte order:
    /// cluster 1 covers the two chars "b" + U+E000 with a single glyph (the
    /// second codepoint maps to no glyph, like a dropped mark), then cluster 0
    /// covers "a". The first descending transition's outgoing cluster extends
    /// to the end of the run text; the previous-cluster bookkeeping used to
    /// seed itself with the dummy start `0` there, emitting an inverted
    /// `1..0` range that silently dropped the continuation glyph (2 slots for
    /// 3 chars).
    #[test]
    fn descending_run_first_emitted_cluster_keeps_its_continuation_glyphs() {
        let run_text = "ab\u{E000}";
        let mut fonts = test_fonts();
        let codepoints = [('b', 1), ('\u{E000}', 1), ('a', 0)];
        let (glyph_buffer, face_key, face_metrics) = shape_synthetic_run(&mut fonts, &codepoints);

        // Precondition: the synthetic vector is what the bookkeeping expects —
        // three glyphs in descending clusters, the second one unmapped (so the
        // run emits one glyph for the two-char cluster 1):
        let infos = glyph_buffer.glyph_infos();
        assert_eq!(infos.len(), 3, "infos: {infos:?}");
        let clusters: Vec<u32> = infos.iter().map(|info| info.cluster).collect();
        assert_eq!(clusters, [1, 1, 0], "infos: {infos:?}");
        assert_eq!(
            infos[1].glyph_id, 0,
            "U+E000 must have no glyph in the test font for this vector to be synthetic"
        );

        let family = fonts.family_key(&FontFamily::Proportional);
        let mut ctx = ShapingContext {
            family,
            pixels_per_point: 1.0,
            font_size: 14.0,
            line_height: face_metrics.row_height,
            extra_letter_spacing: 0.0,
            section_index: 0,
            font_metrics: face_metrics.clone(),
            is_first_glyph_in_section: true,
            bidi_level: 0,
        };
        let run = TextRun {
            font_key: face_key,
            byte_range: ByteRangeExt::full(run_text),
            bidi_level: 0,
            raster: None,
        };
        let mut paragraph = Paragraph::from_section_index(0);
        layout_shaped_run(
            &mut fonts,
            &run,
            run_text,
            &glyph_buffer,
            &face_metrics,
            &mut ctx,
            &mut paragraph,
        );

        let chars = run_text.chars().count();
        assert_eq!(
            paragraph.glyphs.len(),
            chars,
            "the continuation glyph of the logically-last cluster must not be lost: \
             glyphs {:?}",
            paragraph.glyphs.iter().map(|g| g.chr).collect::<Vec<_>>()
        );

        // The continuation is grouped with its cluster: it carries the cluster's
        // byte start (1, like the 'b' glyph before it, not 0), zero advance (it
        // draws nothing), and the char it stands in for:
        // The continuation is grouped with its cluster: the glyph slice lists
        // the cluster's chars in logical order, then 'a' of cluster 0:
        assert_eq!(
            paragraph.glyphs.iter().map(|g| g.chr).collect::<Vec<_>>(),
            ['b', '\u{E000}', 'a'],
            "the continuation glyph must belong to cluster 1, with 'a' in cluster 0"
        );
        let continuation = &paragraph.glyphs[1];
        assert_eq!(continuation.chr, '\u{E000}');
        assert_eq!(
            continuation.advance_width, 0.0,
            "a continuation glyph is zero-width"
        );
        assert!(
            paragraph.glyphs[2].pos.x >= continuation.pos.x,
            "the glyph vector stays in visual order: {:?}",
            paragraph.glyphs.iter().map(|g| g.pos.x).collect::<Vec<_>>()
        );
    }

    /// The caret boundaries of a pure-RTL row mirror the reading direction: the
    /// first column sits at the ink's right edge, the last at the ink's left
    /// edge, and the LTR twin keeps the historic left-to-right geometry.
    #[test]
    fn rtl_caret_positions_mirror_the_reading_direction() {
        let mut fonts = test_fonts();
        let rtl = layout_with_direction(&mut fonts, "تقرير", TextDirection::Auto);
        assert_eq!(rtl.rows.len(), 1);
        let row = &rtl.rows[0];
        let n = row.row.char_count_excluding_newline().0;
        let ink_min = row
            .glyphs
            .iter()
            .map(|glyph| glyph.pos.x)
            .fold(f32::INFINITY, f32::min);
        let ink_max = row
            .glyphs
            .iter()
            .map(|glyph| glyph.max_x())
            .fold(f32::NEG_INFINITY, f32::max);

        let x = |column: usize| row.row.x_offset(CharIndex(column));
        assert!(
            (x(0) - ink_max).abs() < 1.0,
            "column 0 (reading start) at the ink right edge: {} vs {ink_max}",
            x(0)
        );
        assert!(
            (x(n) - ink_min).abs() < 1.0,
            "column {n} (reading end) at the ink left edge: {} vs {ink_min}",
            x(n)
        );

        // The LTR twin keeps the historic geometry: column 0 at the ink's left
        // edge, column n at the row width:
        let ltr = layout_with_direction(&mut fonts, "abcde", TextDirection::LeftToRight);
        let ltr_row = &ltr.rows[0];
        let ltr_n = ltr_row.row.char_count_excluding_newline().0;
        let ltr_x = |column: usize| ltr_row.row.x_offset(CharIndex(column));
        assert!(
            (ltr_x(0) - ltr_row.glyphs[0].pos.x).abs() < 1e-3,
            "LTR column 0 keeps its glyph-left caret"
        );
        assert!(
            (ltr_x(ltr_n) - ltr_row.size.x).abs() < 1e-3,
            "LTR column {ltr_n} keeps the end-of-row caret"
        );
    }

    /// A mixed row under an RTL base direction keeps exact caret geometry at
    /// the reading edges (the per-glyph caret mapping follows each glyph's own
    /// bidi level), and clicks inside the Arabic run land on the right
    /// grapheme boundary. This pins the edges.
    #[test]
    fn mixed_rtl_base_caret_edges() {
        let mut fonts = test_fonts();
        let galley = layout_with_direction(&mut fonts, "عربي abc", TextDirection::Auto);
        assert_eq!(galley.rows.len(), 1);
        let row = &galley.rows[0];
        let n = row.row.char_count_excluding_newline().0;
        assert_eq!(n, 8);
        let mid_y = (row.min_y() + row.max_y()) * 0.5;

        let pos = |column: usize| {
            galley
                .pos_from_layout_cursor(&LayoutCursor {
                    row: 0,
                    column: CharIndex(column),
                })
                .min
                .x
        };
        assert!(
            pos(0) > galley.rect.center().x,
            "reading start at the visual right"
        );
        assert!(
            pos(n) < galley.rect.center().x,
            "reading end at the visual left"
        );

        // Clicking the visual center of `ب` (logical column 2) puts the caret
        // at `ب`'s reading-start (right) edge, between `ر` and `ب`:
        let ba = &row.glyphs[2];
        assert_eq!(ba.chr, 'ب');
        let ba_center = ba.pos.x + ba.advance_width * 0.5 + row.pos.x;
        let clicked = galley.cursor_from_pos(vec2(ba_center, mid_y));
        assert_eq!(
            clicked.index.0, 2,
            "the click inside the Arabic run must land on the right grapheme boundary"
        );
    }

    /// The caret geometry of a mixed row maps every logical column by the
    /// *local* bidi level of the glyph displaying it (not the row's base
    /// parity): inside an embedded run the carets walk in that run's own
    /// reading direction, the reading-exit caret sits at the last-read
    /// glyph's exit edge — strictly inside the row, not at the opposite
    /// visual edge — and every column round-trips through its caret x.
    /// Relational pins only: absolute pixels are font-dependent.
    #[test]
    fn mixed_row_caret_geometry_maps_by_local_level() {
        let mut fonts = test_fonts();

        // RTL base with an embedded LTR run (first-strong inference):
        let galley = layout_with_direction(&mut fonts, "ملف PDF", TextDirection::Auto);
        assert_eq!(galley.rows.len(), 1);
        let placed = &galley.rows[0];
        let row = &placed.row;
        assert!(
            row.glyphs[0].is_rtl(),
            "first-strong inference must resolve the row right-to-left"
        );
        let n = row.char_count_excluding_newline().0;
        assert_eq!(n, 7, "one cursor column per character");
        let mid_y = (placed.min_y() + placed.max_y()) * 0.5;
        let x_of = |column: usize| {
            galley
                .pos_from_layout_cursor(&LayoutCursor {
                    row: 0,
                    column: CharIndex(column),
                })
                .min
                .x
        };
        let ink_min = row
            .glyphs
            .iter()
            .map(|g| g.pos.x)
            .fold(f32::INFINITY, f32::min)
            + placed.pos.x;
        let ink_max = row
            .glyphs
            .iter()
            .map(|g| g.max_x())
            .fold(f32::NEG_INFINITY, f32::max)
            + placed.pos.x;

        // The embedded Latin run reads left-to-right: its caret columns walk
        // with *increasing* x:
        assert!(
            x_of(4) < x_of(5) && x_of(5) < x_of(6) && x_of(6) < x_of(7),
            "the Latin run's carets must walk left-to-right: {} {} {} {}",
            x_of(4),
            x_of(5),
            x_of(6),
            x_of(7)
        );
        // The Arabic run shares the base direction and walks right-to-left;
        // its reading-start caret (column 0) sits at the ink's right edge:
        assert!(
            (x_of(0) - ink_max).abs() < 1.0,
            "column 0 (the reading start) must sit at the ink right edge: {} vs {ink_max}",
            x_of(0)
        );
        assert!(
            x_of(0) > x_of(1) && x_of(1) > x_of(2) && x_of(2) > x_of(3),
            "the Arabic run's carets must walk right-to-left: {} {} {} {}",
            x_of(0),
            x_of(1),
            x_of(2),
            x_of(3)
        );
        // The reading exit (column 7) sits at the last-read Latin glyph's
        // right edge — strictly inside the row, not at the visual left edge:
        assert!(
            x_of(7) > ink_min && x_of(7) < x_of(3),
            "the reading exit must anchor at the Latin run's right edge: {} \
             (ink min {ink_min}, the Arabic run's exit {})",
            x_of(7),
            x_of(3)
        );
        // The run boundary teleports the caret from the Arabic run's exit
        // edge to the Latin run's entry edge:
        assert!(
            x_of(3) > x_of(4),
            "the boundary must drop the caret from the Arabic run's exit edge \
             to the Latin run's entry edge: {} vs {}",
            x_of(3),
            x_of(4)
        );
        // Hit-test round-trips: every run-owned caret x maps back to its
        // column. The reading exit's caret x is not run-owned on this row: it
        // coincides with the run-boundary space's shaped box, so its click
        // resolves by the space's parity rather than back to the exit.
        for column in 0..n {
            let back = galley.cursor_from_pos(vec2(x_of(column), mid_y));
            assert_eq!(
                back.index.0, column,
                "clicking the caret x of column {column} must return that column"
            );
        }
        // Direct hit-test pins (row-relative `char_at`): the ink's left edge
        // lands on the first-read Latin glyph, the ink's right edge on the
        // Arabic reading start, a click past the run-boundary space's center
        // snaps to the space's reading-start edge (the space resolves to the
        // paragraph level, so it reads with the Arabic run), and a click on
        // the last-read Latin glyph resolves to the reading exit:
        let char_at = |x: f32| row.char_at(x).0;
        assert_eq!(
            char_at(ink_min - placed.pos.x),
            4,
            "a click at the ink's left edge must land on the first-read Latin glyph (P)"
        );
        assert_eq!(
            char_at(ink_max - placed.pos.x),
            0,
            "a click at the ink's right edge must land on the reading start (م)"
        );
        let space = &row.glyphs[3];
        assert_eq!(space.chr, ' ', "logical slot 3 is the run-boundary space");
        assert!(
            space.is_rtl(),
            "the run-boundary space must take the paragraph level"
        );
        let space_center = space.pos.x + space.advance_width * 0.5;
        assert_eq!(
            char_at(space_center),
            3,
            "a click past the space's center must snap to its reading-start edge"
        );
        let last_latin = &row.glyphs[6];
        assert_eq!(last_latin.chr, 'F');
        let last_latin_center = last_latin.pos.x + last_latin.advance_width * 0.5;
        assert_eq!(
            char_at(last_latin_center),
            n,
            "a click inside the last-read Latin glyph must land on the reading exit"
        );

        // The LTR twin: an embedded RTL run under a left-to-right base reads
        // right-to-left — its carets DESCEND with the column — and the
        // reading start (ع's entry) anchors at the ink's right edge while the
        // reading exit anchors at the run's visual left edge:
        let galley = layout_with_direction(&mut fonts, "abc عربي", TextDirection::LeftToRight);
        assert_eq!(galley.rows.len(), 1);
        let placed = &galley.rows[0];
        let row = &placed.row;
        let n = row.char_count_excluding_newline().0;
        assert_eq!(n, 8);
        let x_of = |column: usize| {
            galley
                .pos_from_layout_cursor(&LayoutCursor {
                    row: 0,
                    column: CharIndex(column),
                })
                .min
                .x
        };
        let ink_max = row
            .glyphs
            .iter()
            .map(|g| g.max_x())
            .fold(f32::NEG_INFINITY, f32::max)
            + placed.pos.x;
        let run_left = row
            .glyphs
            .iter()
            .filter(|g| g.is_rtl())
            .map(|g| g.pos.x)
            .fold(f32::INFINITY, f32::min)
            + placed.pos.x;
        assert!(
            x_of(4) > x_of(5) && x_of(5) > x_of(6) && x_of(6) > x_of(7) && x_of(7) > x_of(8),
            "the embedded RTL run's carets must walk right-to-left: {} {} {} {} {}",
            x_of(4),
            x_of(5),
            x_of(6),
            x_of(7),
            x_of(8)
        );
        assert!(
            (x_of(4) - ink_max).abs() < 1.0,
            "column 4 (ع's entry) must sit at the ink right edge: {} vs {ink_max}",
            x_of(4)
        );
        assert!(
            (x_of(8) - run_left).abs() < 1.0,
            "the reading exit must anchor at the RTL run's visual left edge: {} vs {run_left}",
            x_of(8)
        );
        // The LTR prefix keeps today's ascending geometry:
        assert!(
            x_of(0) < x_of(1) && x_of(1) < x_of(2) && x_of(2) < x_of(3),
            "the LTR prefix's carets must keep their left-to-right walk"
        );

        // All-even twin under an RTL base (every Latin/digit value in an RTL
        // field): the content counteracts the base, so the mapping is by the
        // glyphs' own level — column 0 paints at the ink's left edge and the
        // reading exit at the ink's right edge:
        let galley = layout_with_direction(&mut fonts, "Hello", TextDirection::RightToLeft);
        assert_eq!(galley.rows.len(), 1);
        let placed = &galley.rows[0];
        let row = &placed.row;
        let n = row.char_count_excluding_newline().0;
        assert_eq!(n, 5);
        let x_of = |column: usize| {
            galley
                .pos_from_layout_cursor(&LayoutCursor {
                    row: 0,
                    column: CharIndex(column),
                })
                .min
                .x
        };
        let ink_min = row
            .glyphs
            .iter()
            .map(|g| g.pos.x)
            .fold(f32::INFINITY, f32::min)
            + placed.pos.x;
        let ink_max = row
            .glyphs
            .iter()
            .map(|g| g.max_x())
            .fold(f32::NEG_INFINITY, f32::max)
            + placed.pos.x;
        assert!(
            (x_of(0) - ink_min).abs() < 1.0,
            "column 0 must sit at the ink's left edge for all-even content in \
             an RTL base: {} vs {ink_min}",
            x_of(0)
        );
        assert!(
            (x_of(n) - ink_max).abs() < 1.0,
            "column {n} must sit at the ink's right edge for all-even content \
             in an RTL base: {} vs {ink_max}",
            x_of(n)
        );
    }

    /// Arrow-key movement is visual: under a right-to-left base direction,
    /// moving toward the visual left steps to the logically NEXT character and
    /// vice versa ([`Galley::cursor_left_visually`] /
    /// [`Galley::cursor_right_visually`], what the arrow keys call). The
    /// logical one-character step ([`Galley::cursor_left_one_character`])
    /// keeps grapheme order for the Mac Cmd bindings.
    #[test]
    fn rtl_arrow_keys_move_visually() {
        let mut fonts = test_fonts();
        let rtl = layout_with_direction(&mut fonts, "تقرير", TextDirection::RightToLeft);
        assert_eq!(rtl.rows.len(), 1);
        assert_eq!(rtl.end().index.0, 5);
        assert_eq!(
            rtl.cursor_left_visually(&rtl.begin()).index.0,
            1,
            "visual left from the reading start enters the text"
        );
        assert_eq!(
            rtl.cursor_right_visually(&rtl.begin()).index.0,
            0,
            "the reading start is the visual right edge"
        );
        assert_eq!(
            rtl.cursor_right_visually(&rtl.end()).index.0,
            4,
            "visual right from the reading end steps back into the text"
        );

        let ltr = layout_with_direction(&mut fonts, "abcde", TextDirection::LeftToRight);
        assert_eq!(ltr.cursor_left_visually(&ltr.begin()).index.0, 0);
        assert_eq!(ltr.cursor_right_visually(&ltr.end()).index.0, 5);
    }

    /// A right-to-left shaper puts a mark before its base. The base must still be drawn
    /// and carry the char index, and the mark move with it when the row is reordered.
    #[test]
    #[cfg(feature = "default_fonts")]
    fn rtl_mark_stays_on_its_base() {
        // Hack has the combining acute but no Hebrew, so alef is drawn as `.notdef`.
        let mut fonts = FontsImpl::new(TextOptions::default(), hack_only());
        let galley = layout_simple(&mut fonts, "\u{5D0}\u{301}\u{5D1}");
        let row = &galley.rows[0].row;
        let [alef, acute, bet] = [0, 1, 2].map(|i| &row.glyphs[i]);
        assert!(0.0 < alef.advance_width, "alef carries its own index");
        assert!(!alef.uv_rect.is_nothing(), "alef is drawn");
        assert_eq!(acute.advance_width, 0.0);
        assert!(!acute.uv_rect.is_nothing(), "the acute is drawn");
        assert!(bet.pos.x < alef.pos.x, "bet is drawn left of alef");
        let acute_x = acute.pos.x + acute.uv_rect.offset.x;
        assert!(
            alef.pos.x <= acute_x && acute_x <= alef.max_x(),
            "the acute at {acute_x} is over alef at {}..{}",
            alef.pos.x,
            alef.max_x()
        );
    }

    /// A base and a mark that the font draws as two glyphs of one cluster
    /// each keep their own char.
    #[test]
    #[cfg(feature = "default_fonts")]
    fn multi_glyph_cluster_keeps_its_chars() {
        // Hack has the combining acute, and no precomposed form of either cluster.
        let mut fonts = FontsImpl::new(TextOptions::default(), hack_only());
        for text in ["q\u{301}x", "\u{5D0}\u{301}\u{5D1}"] {
            let galley = layout_simple(&mut fonts, text);
            assert_eq!(galley.rows[0].row.text(), text);
        }
    }

    /// Left and Right step over whole grapheme clusters.
    #[test]
    fn cursor_moves_by_grapheme_cluster() {
        let mut fonts = test_fonts();
        let text = "e\u{301}👨\u{200D}👩\u{200D}👧🇸🇪1\u{FE0F}\u{20E3}क्षกี่\u{627}\u{651}";
        let boundaries = [0, 2, 7, 9, 12, 15, 18, 20];
        let galley = layout_simple(&mut fonts, text);

        let mut cursor = galley.begin();
        let mut right = vec![cursor.index.0];
        while cursor != galley.end() {
            cursor = galley.cursor_right_one_character(&cursor);
            right.push(cursor.index.0);
        }
        assert_eq!(right, boundaries);

        let mut left = vec![cursor.index.0];
        while cursor != galley.begin() {
            cursor = galley.cursor_left_one_character(&cursor);
            left.push(cursor.index.0);
        }
        left.reverse();
        assert_eq!(left, boundaries);
    }

    /// Hit testing never puts the cursor inside a grapheme cluster,
    /// in left-to-right and in right-to-left rows.
    #[test]
    fn hit_testing_lands_on_cluster_boundaries() {
        let mut font_sets = vec![test_fonts()];
        #[cfg(feature = "default_fonts")]
        font_sets.push(FontsImpl::new(TextOptions::default(), hack_only()));

        let texts = [
            "\u{628}\u{651}\u{627}\u{64B} \u{5D0}\u{301}\u{5D1}\u{302}\u{303}", // right-to-left
            "\u{5D0}\u{301} q\u{301}e\u{301} 🇸🇪 \u{5D1}\u{302}",                // mixed directions
            "q\u{301}e\u{301}🇸🇪1\u{FE0F}\u{20E3}कि",                            // left-to-right
        ];
        for fonts in &mut font_sets {
            for text in texts {
                let galley = layout_simple(fonts, text);
                let row = &galley.rows[0];
                let mut x = row.rect().left() - 5.0;
                while x < row.rect().right() + 5.0 {
                    let cursor = galley.cursor_from_pos(vec2(x, row.rect().center().y));
                    assert!(
                        grapheme::cluster_around(text, cursor.index).is_none(),
                        "{text:?}: the cursor at x = {x} is at {}, inside a cluster",
                        cursor.index
                    );
                    x += 0.25;
                }
            }
        }
    }

    /// The chars of the advancing glyphs of a row, left to right as drawn.
    #[cfg(feature = "default_fonts")]
    fn drawn(row: &Row) -> String {
        let mut glyphs: Vec<&Glyph> = row
            .glyphs
            .iter()
            .filter(|g| 0.0 < g.advance_width)
            .collect();
        glyphs.sort_by(|a, b| a.pos.x.total_cmp(&b.pos.x));
        glyphs.iter().map(|g| g.chr).collect()
    }

    /// Isolate initiators, PDI and marks like RLM have no width, but their levels
    /// still separate runs (UAX #9 X5a–X6a, L2).
    #[test]
    #[cfg(feature = "default_fonts")]
    fn zero_width_bidi_controls_keep_their_levels() {
        const LRI: char = '\u{2066}';
        const RLI: char = '\u{2067}';
        const FSI: char = '\u{2068}';
        const PDI: char = '\u{2069}';
        const RLM: char = '\u{200F}';

        let mut fonts = test_fonts();
        let mut drawn_with = |text: &str, direction| {
            let galley = layout_with_direction(&mut fonts, text, direction);
            assert_eq!(galley.rows.len(), 1);
            let row = &galley.rows[0].row;
            for glyph in &row.glyphs {
                if matches!(glyph.chr, LRI | RLI | FSI | PDI | RLM) {
                    assert_eq!(glyph.advance_width, 0.0, "{:?}", glyph.chr);
                }
            }
            drawn(row)
        };
        let rtl = TextDirection::RightToLeft;

        // A right-to-left menu path: a Hebrew part, then a Latin id, each in its own isolate.
        assert_eq!(
            drawn_with(&format!("{FSI}קובץ{PDI}{FSI}abc{PDI}"), rtl),
            "abcץבוק"
        );
        assert_eq!(
            drawn_with(&format!("קובץ {FSI}abc{PDI}"), TextDirection::Auto),
            "abc ץבוק"
        );

        // Two Latin isolates in right-to-left text stay two runs, the first on the right:
        assert_eq!(
            drawn_with(&format!("{FSI}abc{PDI}{FSI}def{PDI}"), rtl),
            "defabc"
        );
        assert_eq!(
            drawn_with(&format!("א {LRI}abc{PDI}{LRI}def{PDI} ב"), rtl),
            "ב defabc א"
        );

        // A number in an isolate inside Arabic:
        assert_eq!(
            drawn_with(&format!("عدد {LRI}123{PDI} هنا"), TextDirection::Auto),
            "انه 123 ددع"
        );

        // Nested isolates: two left-to-right isolates inside a right-to-left one,
        // inside a left-to-right paragraph.
        assert_eq!(
            drawn_with(
                &format!("x {RLI}אב {LRI}cd{PDI}{LRI}ef{PDI}{PDI} y"),
                TextDirection::Auto
            ),
            "x efcd בא y"
        );

        // An RLM at the start makes the paragraph right to left, and an RLM between
        // Latin words splits them into two runs:
        assert_eq!(
            drawn_with(&format!("{RLM}abc אבג"), TextDirection::Auto),
            "גבא abc"
        );
        assert_eq!(
            drawn_with(&format!("{RLM}abc{RLM}def"), TextDirection::Auto),
            "defabc"
        );
        let galley = layout_simple(&mut fonts, &format!("{RLM}abc אבג"));
        let row = &galley.rows[0].row;
        let right_end = row.glyphs[1..]
            .iter()
            .map(Glyph::max_x)
            .fold(f32::MIN, f32::max);
        let rlm_x = row.glyphs[0].pos.x;
        assert!(
            (rlm_x - right_end).abs() < 1.0,
            "the RLM at {rlm_x} is at the right end, {right_end}"
        );
    }

    /// Glyph and row positions of left-to-right text: no wrapping, centered and justified.
    #[test]
    #[cfg(feature = "default_fonts")]
    fn ltr_layout_is_unchanged_by_bidi() {
        let mut fonts = test_fonts();
        let text = "Hello, wörld! fi ffi 123 (x) — ok\tdone";
        let mut lines = Vec::new();
        for (max_width, halign, justify) in [
            (f32::INFINITY, Align::LEFT, false),
            (80.0, Align::Center, false),
            (80.0, Align::LEFT, true),
        ] {
            let mut job = LayoutJob::simple(
                text.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
                max_width,
            );
            job.halign = halign;
            job.justify = justify;
            let galley = layout(&mut fonts, 1.0, Arc::new(job));
            for placed_row in &galley.rows {
                assert!(placed_row.row.glyphs.iter().all(|g| g.bidi_level == 0));
                let xs: Vec<String> = placed_row
                    .row
                    .glyphs
                    .iter()
                    .map(|glyph| format!("{:.2}", glyph.pos.x))
                    .collect();
                lines.push(format!(
                    "{:.2},{:.2} {:.2}x{:.2}: {}",
                    placed_row.pos.x,
                    placed_row.pos.y,
                    placed_row.row.size.x,
                    placed_row.row.size.y,
                    xs.join(" ")
                ));
            }
        }
        // As laid out by egui 0.37.0, before bidi. Since the trailing-whitespace
        // fix, centered rows keep their natural whitespace-included size (their
        // ink is unchanged, and so are all glyph positions):
        let expected = [
            "0.00,0.00 233.75x16.00: 0.00 9.00 17.00 21.00 24.00 32.00 36.00 39.00 50.00 58.00 63.00 67.00 75.00 79.00 82.00 91.00 90.00 94.00 108.00 108.00 108.00 111.00 119.00 127.00 134.00 138.00 142.00 149.00 153.00 156.00 170.00 174.00 182.00 189.00 201.00 210.00 218.00 226.00",
            "-18.00,0.00 39.19x16.00: 0.00 9.00 17.00 21.00 24.00 32.00 36.00",
            "-35.00,16.00 72.19x16.00: 0.00 11.00 19.00 24.00 28.00 36.00 40.00 43.00 52.00 51.00 55.00 69.00 69.00 69.00",
            "-29.00,32.00 62.19x16.00: 0.00 8.00 16.00 23.00 27.00 31.00 38.00 42.00 45.00 59.00",
            "-30.00,48.00 59.69x16.00: 0.00 8.00 15.00 27.00 36.00 44.00 52.00",
            "0.00,0.00 80.00x16.00: 0.00 18.00 35.00 48.00 60.00 77.00 89.00",
            "0.00,16.00 80.00x16.00: 0.00 11.00 19.00 24.00 28.00 36.00 40.00 49.00 58.00 57.00 66.00 80.00 80.00 80.00",
            "0.00,32.00 80.00x16.00: 0.00 10.00 20.00 29.00 38.00 44.00 53.00 59.00 66.00 82.00",
            "0.00,48.00 59.69x16.00: 0.00 8.00 15.00 27.00 36.00 44.00 52.00",
        ];
        assert_eq!(lines, expected);
    }

    /// Every row and glyph position of a galley, exactly.
    #[cfg(feature = "default_fonts")]
    fn vertical_geometry(galley: &Galley) -> Vec<String> {
        galley
            .rows
            .iter()
            .map(|placed_row| {
                let ys: Vec<String> = placed_row
                    .row
                    .glyphs
                    .iter()
                    .map(|glyph| format!("{:?}", glyph.pos.y))
                    .collect();
                format!(
                    "{:?},{:?} {:?}x{:?}: {}",
                    placed_row.pos.x,
                    placed_row.pos.y,
                    placed_row.row.size.x,
                    placed_row.row.size.y,
                    ys.join(" ")
                )
            })
            .collect()
    }

    /// Latin with a fallback face and an icon, in two sizes, one with its own line height.
    #[cfg(feature = "default_fonts")]
    fn mixed_face_job() -> LayoutJob {
        let mut job = LayoutJob::default();
        job.wrap.max_width = 120.0;
        let body = TextFormat::simple(FontId::proportional(14.0), Color32::WHITE);
        job.append("Text \u{e618} with ", 0.0, body.clone());
        job.append(
            "mono ƀƁ\u{e61f}",
            0.0,
            TextFormat {
                valign: Align::Center,
                ..TextFormat::simple(FontId::monospace(20.0), Color32::WHITE)
            },
        );
        job.append(
            " tall ƀ rows\nand more",
            0.0,
            TextFormat {
                line_height: Some(24.0),
                ..body
            },
        );
        job
    }

    /// Galley geometry for text that mixes faces, with the fonts egui ships.
    #[test]
    #[cfg(feature = "default_fonts")]
    fn mixed_face_layout_is_unchanged_by_default() {
        let mut fonts = test_fonts();
        let galley = layout(&mut fonts, 1.5, Arc::new(mixed_face_job()));
        // As laid out before fallback faces could share the baseline:
        let expected = [
            "0.0,0.0 76.53125x16.0: 12.666667 12.666667 12.666667 12.666667 12.666667 13.333333 12.666667 12.666667 12.666667 12.666667 12.666667 12.666667",
            "0.0,16.0 107.1875x24.0: 18.666666 18.666666 18.666666 18.666666 18.666666 19.333334 19.333334 20.0 17.333334",
            "0.0,40.0 64.65625x24.0: 17.333334 17.333334 17.333334 17.333334 17.333334 17.333334 17.333334 17.333334 17.333334 17.333334 17.333334",
            "0.0,64.0 59.59375x24.0: 17.333334 17.333334 17.333334 17.333334 17.333334 17.333334 17.333334 17.333334",
        ];
        assert_eq!(vertical_geometry(&galley), expected);
    }

    /// The proportional family `primary`, `fallback` (aligned with `align`), `egui-icons`.
    #[cfg(feature = "default_fonts")]
    fn fonts_with_fallback(
        primary: &'static [u8],
        fallback: &'static [u8],
        align: FallbackAlign,
    ) -> FontsImpl {
        let mut definitions = FontDefinitions::empty();
        let tweaked =
            |data: &'static [u8], tweak| Arc::new(FontData::from_static(data).tweak(tweak));
        definitions.font_data.insert(
            "primary".to_owned(),
            Arc::new(FontData::from_static(primary)),
        );
        definitions.font_data.insert(
            "fallback".to_owned(),
            tweaked(
                fallback,
                FontTweak {
                    fallback_align: align,
                    ..Default::default()
                },
            ),
        );
        definitions.font_data.insert(
            "egui-icons".to_owned(),
            tweaked(
                epaint_default_fonts::EGUI_ICONS,
                FontTweak {
                    scale: 0.9,
                    ..Default::default()
                },
            ),
        );
        definitions.families.insert(
            FontFamily::Proportional,
            ["primary", "fallback", "egui-icons"]
                .map(str::to_owned)
                .into(),
        );
        FontsImpl::new(TextOptions::default(), definitions)
    }

    /// Hack and Ubuntu differ in ascent and descent: either one, as the fallback
    /// of the other, sits on the first one's baseline, and the row fits both.
    #[test]
    #[cfg(feature = "default_fonts")]
    fn fallback_on_the_baseline_grows_the_row() {
        use epaint_default_fonts::{HACK_REGULAR, UBUNTU_LIGHT};

        // `Ա` is only in Hack, `ƀ` only in Ubuntu, `\u{e618}` only in egui-icons.
        for (primary, fallback, text) in [
            (UBUNTU_LIGHT, HACK_REGULAR, "xԱ\u{e618}"),
            (HACK_REGULAR, UBUNTU_LIGHT, "xƀ\u{e618}"),
        ] {
            let layout_with = |align, text: &str| {
                let mut fonts = fonts_with_fallback(primary, fallback, align);
                let job = LayoutJob::simple_singleline(
                    text.to_owned(),
                    FontId::proportional(100.0),
                    Color32::WHITE,
                );
                layout(&mut fonts, 1.0, Arc::new(job))
            };
            let centered = layout_with(FallbackAlign::Center, text);
            let on_baseline = layout_with(FallbackAlign::Baseline, text);

            let [x, other, icon] = &on_baseline.rows[0].row.glyphs[..] else {
                panic!("one row of three glyphs");
            };
            assert_ne!(x.font_face_height, other.font_face_height);
            assert_eq!(x.pos.y, other.pos.y, "{text}: one baseline");
            assert_ne!(
                centered.rows[0].row.glyphs[0].pos.y, centered.rows[0].row.glyphs[1].pos.y,
                "{text}: centered fallbacks sit elsewhere"
            );

            let ascent = x.font_ascent.max(other.font_face_ascent);
            let descent = (x.font_height - x.font_ascent)
                .max(other.font_face_height - other.font_face_ascent);
            assert_eq!(x.pos.y, ascent.round(), "{text}");
            assert_eq!(
                on_baseline.rows[0].height(),
                (ascent + descent).round(),
                "{text}"
            );
            assert!(
                centered.rows[0].height() < on_baseline.rows[0].height(),
                "{text}: the row grows"
            );

            // The icon keeps its place relative to the text:
            let icon_to_text = |galley: &Galley| {
                let glyphs = &galley.rows[0].row.glyphs;
                glyphs[2].pos.y - glyphs[0].pos.y
            };
            assert!(
                (icon_to_text(&on_baseline) - icon_to_text(&centered)).abs() <= 1.0,
                "{text}: icon at {} with text at {}",
                icon.pos.y,
                x.pos.y
            );

            // Text in the first font alone doesn't move:
            assert_eq!(
                vertical_geometry(&layout_with(FallbackAlign::Baseline, "x\u{e618} y")),
                vertical_geometry(&layout_with(FallbackAlign::Center, "x\u{e618} y")),
            );
        }
    }

    #[cfg(feature = "default_fonts")]
    fn measure_text(
        fonts: &mut FontsImpl,
        text: &str,
        font_id: &FontId,
        pixels_per_point: f32,
    ) -> (f32, usize, Vec<(char, f32)>) {
        let job = LayoutJob::simple(
            text.to_owned(),
            font_id.clone(),
            Color32::WHITE,
            f32::INFINITY,
        );
        let galley = layout(fonts, pixels_per_point, job.into());
        let glyphs = &galley.rows[0].row.glyphs;
        let details: Vec<_> = glyphs.iter().map(|g| (g.chr, g.advance_width)).collect();
        (galley.size().x, glyphs.len(), details)
    }

    // ------------------------------------------------------------------------
    // Line breaking

    /// Allows rows to end only at the given breaks, and remembers the language it was asked about.
    #[derive(Default)]
    struct BreakAt {
        breaks: Vec<LineBreak>,
        languages: crate::mutex::Mutex<Vec<Option<String>>>,
    }

    impl BreakAt {
        fn soft(offsets: &[usize]) -> Self {
            Self {
                breaks: offsets
                    .iter()
                    .map(|&offset| LineBreak::soft(offset))
                    .collect(),
                ..Default::default()
            }
        }
    }

    impl LineBreaker for BreakAt {
        fn line_breaks(&self, _text: &str, language: Option<&str>, out: &mut Vec<LineBreak>) {
            self.languages.lock().push(language.map(str::to_owned));
            out.extend_from_slice(&self.breaks);
        }
    }

    /// No fonts, so every grapheme cluster is a box of the same width.
    fn tofu_fonts(line_breaker: Arc<dyn LineBreaker>) -> FontsImpl {
        let mut fonts = FontsImpl::new(TextOptions::default(), FontDefinitions::empty());
        fonts.line_breaker = line_breaker;
        fonts
    }

    /// `job.text` wrapped at `glyphs` and a half glyph widths: the text of each row.
    fn wrapped_rows(fonts: &mut FontsImpl, mut job: LayoutJob, glyphs: u32) -> Vec<String> {
        let one = layout_simple(fonts, "x").size().x;
        job.wrap.max_width = one * glyphs as f32 + 0.5 * one;
        let galley = layout(fonts, 1.0, Arc::new(job));
        galley.rows.iter().map(|row| row.row.text()).collect()
    }

    fn simple_job(text: &str) -> LayoutJob {
        LayoutJob::simple(
            text.to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        )
    }

    /// [`SimpleLineBreaker`] wraps text where epaint always did: these rows are what
    /// epaint made before line breakers could be swapped.
    #[test]
    fn simple_line_breaker_keeps_the_old_breaks() {
        #[rustfmt::skip]
        const CASES: &[(&str, bool, u32, &str)] = &[
            ("The quick brown fox jumps over the lazy dog", false, 3, "The| |qui|ck |bro|wn |fox| |jum|ps |ove|r |the| |laz|y |dog"),
            ("The quick brown fox jumps over the lazy dog", false, 5, "The |quick| |brown| fox |jumps| |over |the |lazy |dog"),
            ("The quick brown fox jumps over the lazy dog", false, 7, "The |quick |brown |fox |jumps |over |the |lazy |dog"),
            ("The quick brown fox jumps over the lazy dog", false, 10, "The quick |brown fox |jumps |over the |lazy dog"),
            ("The quick brown fox jumps over the lazy dog", true, 3, "The| qu|ick| br|own| fo|x j|ump|s o|ver| th|e l|azy| do|g"),
            ("The quick brown fox jumps over the lazy dog", true, 5, "The q|uick |brown| fox |jumps| over| the |lazy |dog"),
            ("The quick brown fox jumps over the lazy dog", true, 7, "The qui|ck brow|n fox j|umps ov|er the |lazy do|g"),
            ("The quick brown fox jumps over the lazy dog", true, 10, "The quick |brown fox |jumps over| the lazy |dog"),
            ("well-known self-contained hyphen-ated", false, 3, "wel|l-|kno|wn |sel|f-|con|tai|ned| |hyp|hen|-|ate|d"),
            ("well-known self-contained hyphen-ated", false, 5, "well-|known| |self-|conta|ined |hyphe|n-|ated"),
            ("well-known self-contained hyphen-ated", false, 7, "well-|known |self-|contain|ed |hyphen-|ated"),
            ("well-known self-contained hyphen-ated", false, 10, "well-|known |self-|contained |hyphen-|ated"),
            ("see www.example.com/path?q=1 or a-b.c", false, 3, "see| |www|.|exa|mpl|e.|com|/|pat|h?|q=|1 |or |a-|b.c"),
            ("see www.example.com/path?q=1 or a-b.c", false, 5, "see |www.|examp|le.|com/|path?|q=1 |or |a-b.c"),
            ("see www.example.com/path?q=1 or a-b.c", false, 7, "see |www.|example|.com/|path?q=|1 or |a-b.c"),
            ("see www.example.com/path?q=1 or a-b.c", false, 10, "see |www.|example.|com/path?|q=1 or |a-b.c"),
            ("一二三，四五六：七八九？十一二！三四五）六七八", false, 3, "一二|三，四|五六：|七八|九？十|一二！|三四|五）六|七八"),
            ("一二三，四五六：七八九？十一二！三四五）六七八", false, 5, "一二三，四|五六：七八|九？十一|二！三四|五）六七八"),
            ("一二三，四五六：七八九？十一二！三四五）六七八", false, 7, "一二三，四五|六：七八九？十|一二！三四五）|六七八"),
            ("一二三，四五六：七八九？十一二！三四五）六七八", false, 10, "一二三，四五六：七八|九？十一二！三四五）|六七八"),
            ("一二三，四五六：七八九？十一二！三四五）六七八", true, 3, "一二三|，四五|六：七|八九？|十一二|！三四|五）六|七八"),
            ("一二三，四五六：七八九？十一二！三四五）六七八", true, 5, "一二三，四|五六：七八|九？十一二|！三四五）|六七八"),
            ("一二三，四五六：七八九？十一二！三四五）六七八", true, 7, "一二三，四五六|：七八九？十一|二！三四五）六|七八"),
            ("一二三，四五六：七八九？十一二！三四五）六七八", true, 10, "一二三，四五六：七八|九？十一二！三四五）|六七八"),
            ("日本語のテキストとEnglish wordsの混在-です", false, 3, "日本語|のテキ|ストと|Eng|lis|h |wor|dsの|混在|-です"),
            ("日本語のテキストとEnglish wordsの混在-です", false, 5, "日本語のテ|キストと|Engli|sh |words|の混在-で|す"),
            ("日本語のテキストとEnglish wordsの混在-です", false, 7, "日本語のテキス|トと|English| wordsの|混在-です"),
            ("日本語のテキストとEnglish wordsの混在-です", false, 10, "日本語のテキストと|English |wordsの混在-で|す"),
            ("Supercalifragilisticexpialidocious", false, 3, "Sup|erc|ali|fra|gil|ist|ice|xpi|ali|doc|iou|s"),
            ("Supercalifragilisticexpialidocious", false, 5, "Super|calif|ragil|istic|expia|lidoc|ious"),
            ("Supercalifragilisticexpialidocious", false, 7, "Superca|lifragi|listice|xpialid|ocious"),
            ("Supercalifragilisticexpialidocious", false, 10, "Supercalif|ragilistic|expialidoc|ious"),
            ("a\u{a0}b c\u{a0}d e\u{a0}f\u{a0}g", false, 3, "a\u{a0}b| |c\u{a0}d| |e\u{a0}f|\u{a0}g"),
            ("a\u{a0}b c\u{a0}d e\u{a0}f\u{a0}g", false, 5, "a\u{a0}b |c\u{a0}d |e\u{a0}f\u{a0}g"),
            ("a\u{a0}b c\u{a0}d e\u{a0}f\u{a0}g", false, 10, "a\u{a0}b c\u{a0}d |e\u{a0}f\u{a0}g"),
            ("a\u{a0}b c\u{a0}d e\u{a0}f\u{a0}g", true, 3, "a\u{a0}b| c\u{a0}|d e|\u{a0}f\u{a0}|g"),
        ];
        let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
        for &(text, break_anywhere, glyphs, expected) in CASES {
            let mut job = simple_job(text);
            job.wrap.break_anywhere = break_anywhere;
            let rows = wrapped_rows(&mut fonts, job, glyphs).join("|");
            assert_eq!(
                rows, expected,
                "{text:?} at {glyphs} glyphs, break_anywhere: {break_anywhere}"
            );
        }
    }

    #[test]
    fn rows_end_only_where_the_line_breaker_allows() {
        // Allowed: before "def" and before "ghi". Not at the spaces.
        let text = "abc def ghi jkl";
        let mut fonts = tofu_fonts(Arc::new(BreakAt::soft(&[4, 8])));
        assert_eq!(
            wrapped_rows(&mut fonts, simple_job(text), 9),
            ["abc def ", "ghi jkl"]
        );
        assert_eq!(
            wrapped_rows(&mut fonts, simple_job(text), 5),
            ["abc ", "def ", "ghi j", "kl"]
        );

        // A lower penalty wins over a later break:
        let mut fonts = tofu_fonts(Arc::new(BreakAt {
            breaks: vec![LineBreak::soft(4), LineBreak::soft_with_penalty(8, 1)],
            ..Default::default()
        }));
        assert_eq!(
            wrapped_rows(&mut fonts, simple_job("abc def ghi"), 9),
            ["abc ", "def ghi"]
        );

        // Too long a piece breaks between grapheme clusters:
        let mut fonts = tofu_fonts(Arc::new(BreakAt::soft(&[])));
        assert_eq!(
            wrapped_rows(&mut fonts, simple_job("abcdefgh"), 3),
            ["abc", "def", "gh"]
        );
    }

    #[test]
    fn line_breaker_gets_the_paragraph_and_its_language() {
        let line_breaker = Arc::new(BreakAt::soft(&[]));
        let mut fonts = tofu_fonts(Arc::<BreakAt>::clone(&line_breaker));
        let mut job = simple_job("ภาษาไทย ภาษาไทย");
        job.language = Some("th".to_owned());
        wrapped_rows(&mut fonts, job, 5);
        assert_eq!(*line_breaker.languages.lock(), [Some("th".to_owned())]);

        // A paragraph that fits on one row has no breaks to look for:
        line_breaker.languages.lock().clear();
        wrapped_rows(&mut fonts, simple_job("ไทย"), 5);
        assert!(line_breaker.languages.lock().is_empty());
    }

    #[test]
    fn hard_breaks_end_the_row() {
        let text = "abc\u{2028}def ghi";
        let after_separator = "abc\u{2028}".len();
        let line_breaker = Arc::new(BreakAt {
            breaks: vec![
                LineBreak::hard(after_separator),
                LineBreak::hard(text.len()),
            ],
            ..Default::default()
        });
        let mut fonts = tofu_fonts(line_breaker);
        // Even when the paragraph fits on one row:
        assert_eq!(
            wrapped_rows(&mut fonts, simple_job(text), 20),
            ["abc\u{2028}", "def ghi"]
        );

        // Only after a line separator:
        let mut fonts = tofu_fonts(Arc::new(BreakAt {
            breaks: vec![LineBreak::hard(2)],
            ..Default::default()
        }));
        assert_eq!(
            wrapped_rows(&mut fonts, simple_job("abcd\u{2028}"), 20),
            ["abcd\u{2028}"]
        );
        assert_eq!(
            wrapped_rows(&mut fonts, simple_job("abcd\u{2028}"), 3),
            ["ab", "cd\u{2028}"]
        );

        // Only with `break_on_newline`; otherwise it is a soft break:
        let mut fonts = tofu_fonts(Arc::new(BreakAt {
            breaks: vec![LineBreak::hard(after_separator)],
            ..Default::default()
        }));
        let mut job = simple_job(text);
        job.break_on_newline = false;
        assert_eq!(wrapped_rows(&mut fonts, job.clone(), 20), [text]);
        assert_eq!(
            wrapped_rows(&mut fonts, job, 6),
            ["abc\u{2028}", "def gh", "i"]
        );
    }

    /// With no break opportunity in a word, the row breaks between grapheme clusters:
    /// never inside an emoji ZWJ sequence, a flag, or a letter with its marks.
    #[test]
    fn emergency_breaks_keep_grapheme_clusters_whole() {
        use crate::text::grapheme::cluster_around;

        let texts = [
            "👨\u{200D}👩\u{200D}👧\u{200D}👦👩\u{200D}🔬🇯🇵🇰🇷👍🏽",
            "e\u{301}e\u{301}\u{323}a\u{308}o\u{302}\u{301}u\u{328}",
            "ที่นี่มีน้ำใสสะอาด",
            "नमस्ते क्षत्रिय",
            "ကျွန်တော်မြန်မာစာ",
        ];
        // With the default fonts, the emoji in a sequence the emoji font has no ligature for
        // are glyphs of their own, each with its own width.
        let line_breakers = [
            Arc::new(SimpleLineBreaker) as Arc<dyn LineBreaker>,
            Arc::new(BreakAt::soft(&[])),
        ];
        for (definitions, line_breaker) in [FontDefinitions::empty(), FontDefinitions::default()]
            .into_iter()
            .flat_map(|definitions| line_breakers.clone().map(|lb| (definitions.clone(), lb)))
        {
            let mut fonts = FontsImpl::new(TextOptions::default(), definitions);
            fonts.line_breaker = line_breaker;
            for text in texts {
                for break_anywhere in [false, true] {
                    for glyphs in 1..8 {
                        let mut job = simple_job(text);
                        job.wrap.break_anywhere = break_anywhere;
                        let rows = wrapped_rows(&mut fonts, job, glyphs);
                        assert_eq!(rows.concat(), text);
                        let mut row_start = 0;
                        for row in &rows[..rows.len() - 1] {
                            row_start += row.chars().count();
                            assert!(
                                cluster_around(text, CharIndex(row_start)).is_none(),
                                "{text:?} at {glyphs} glyphs: {rows:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Rows that end where a line breaker says still get reordered for right-to-left text.
    #[test]
    fn rtl_rows_are_reordered_after_custom_breaks() {
        // Letters are two bytes each. Break only before "ה", inside the second word.
        let text = "אבג דהו זחט";
        let before_he = "אבג ד".len();
        let mut fonts = tofu_fonts(Arc::new(BreakAt::soft(&[before_he])));
        let one = layout_simple(&mut fonts, "x").size().x;
        let mut job = simple_job(text);
        job.wrap.max_width = 6.5 * one;
        let galley = layout(&mut fonts, 1.0, Arc::new(job));
        let texts: Vec<String> = galley.rows.iter().map(|row| row.row.text()).collect();
        assert_eq!(texts, ["אבג ד", "הו זחט"]);
        for placed in &galley.rows {
            assert!(placed.row.rtl_paragraph);
        }
        assert_visual_order(&galley.rows[0].row, &[4, 3, 2, 1, 0]);
        assert_visual_order(&galley.rows[1].row, &[5, 4, 3, 2, 1, 0]);
        // Right-aligned, like the rest of a right-to-left paragraph:
        assert!(
            (galley.rows[0].rect().right() - galley.rows[1].rect().right()).abs() < 0.01,
            "{:?} {:?}",
            galley.rows[0].rect(),
            galley.rows[1].rect()
        );
    }

    /// Shaped complex scripts emit glyph vectors that are NOT sorted by x:
    /// zero-advance continuation placeholders at both ends and real glyphs
    /// with negative x offsets in between. `halign_and_justify_row` used to
    /// take the ink span from only the first and last glyph, so a right- or
    /// center-aligned row (what an RTL label gets) computed a too-narrow
    /// width and left its ink positioned outside the allocated rect —
    /// button hover fills then failed to cover the ink.
    ///
    /// No Arabic font is bundled (fonts are never committed), so this test
    /// builds a row by hand with the geometry measured from shaping the
    /// Arabic word "ملف" with a real Arabic font (Noto Sans Arabic, 14 px):
    /// a placeholder at 0, the ligature glyph drawn 10 px left of the pen,
    /// the remaining glyphs, and trailing placeholders at the natural width.
    /// The row keeps its glyphs in stored order (`visual_order` empty, i.e.
    /// the identity), which is what `halign_and_justify_row` measures over.
    #[test]
    fn halign_uses_full_ink_span_of_non_monotonic_glyph_vector() {
        /// Only self-consistent metrics are needed: `logical_rect` spans
        /// `[pos.x, pos.x + advance_width]` around the baseline.
        fn glyph(chr: char, x: f32, advance: f32) -> Glyph {
            Glyph {
                chr,
                pos: pos2(x, 0.0),
                advance_width: advance,
                line_height: 16.0,
                font_ascent: 13.0,
                font_height: 16.0,
                font_face_ascent: 13.0,
                font_face_height: 16.0,
                font_face_align: FallbackAlign::default(),
                uv_rect: Default::default(),
                is_color: false,
                bidi_level: 0,
                section_index: 0,
                first_vertex: 0,
            }
        }

        fn placed_row(
            glyphs: Vec<Glyph>,
            natural_width: f32,
            base_direction: TextDirection,
            leading_advance: f32,
        ) -> PlacedRow {
            PlacedRow {
                pos: pos2(0.0, 0.0),
                row: Arc::new(Row {
                    section_index_at_start: 0,
                    base_direction,
                    leading_advance,
                    glyphs,
                    rtl_paragraph: false,
                    visual_order: Vec::new(),
                    size: vec2(natural_width, 16.0),
                    visuals: RowVisuals::default(),
                }),
                ends_with_newline: false,
            }
        }

        fn assert_close(got: f32, want: f32) {
            assert!((got - want).abs() < 1e-3, "got {got:.4}, want {want:.4}");
        }

        fn ink_span(row: &Row) -> (f32, f32) {
            (
                row.glyphs
                    .iter()
                    .map(|glyph| glyph.logical_rect().min.x)
                    .fold(f32::INFINITY, f32::min),
                row.glyphs
                    .iter()
                    .map(|glyph| glyph.logical_rect().max.x)
                    .fold(f32::NEG_INFINITY, f32::max),
            )
        }

        /// The measured row-relative geometry of an Arabic "ملف" label,
        /// in stored (left-to-right) order: a zero-advance placeholder, the
        /// ligature glyph drawn left of the pen (negative offset), the
        /// remaining glyphs, trailing zero-advance continuations.
        fn shaped_arabic() -> Vec<Glyph> {
            vec![
                glyph('ف', 0.0, 0.0),
                glyph('ف', -10.0, 15.37),
                glyph('ل', 5.0, 4.2),
                glyph('م', 9.0, 7.35),
                glyph('ل', 17.0, 0.0),
                glyph('ف', 17.0, 0.0),
            ]
        }

        let natural_width: f32 = shaped_arabic().iter().map(|g| g.advance_width).sum();
        // The first glyph starts at 0 and the last ends at 17, but the real
        // ink spans [-10, 17]: the vector is non-monotonic and the endpoints
        // do not bracket it. Sanity-check the premise:
        let (ink_min, ink_max) = ink_span(&Row {
            section_index_at_start: 0,
            base_direction: TextDirection::LeftToRight,
            leading_advance: 0.0,
            glyphs: shaped_arabic(),
            rtl_paragraph: false,
            visual_order: Vec::new(),
            size: vec2(natural_width, 16.0),
            visuals: RowVisuals::default(),
        });
        assert_close(ink_min, -10.0);
        assert_close(ink_max, 17.0);

        let point_scale = PointScale::new(1.0);

        // `Align::Max` is what a right-to-left label is laid out with. The
        // row must be anchored as if 27 wide (the true ink span), not 17:
        let mut placed = placed_row(
            shaped_arabic(),
            natural_width,
            TextDirection::LeftToRight,
            0.0,
        );
        halign_and_justify_row(
            point_scale,
            &mut placed,
            Align::Max,
            f32::INFINITY,
            false,
            false,
        );
        assert_close(placed.pos.x, -27.0);
        // The ink is repositioned to end at the galley origin (right
        // alignment) and to start exactly at the row's left edge — before
        // the fix it extended 10 px to the left of the allocated rect:
        let (ink_min, ink_max) = ink_span(&placed.row);
        assert_close(ink_min + placed.pos.x, placed.pos.x);
        assert_close(ink_max + placed.pos.x, 0.0);
        // The allocation keeps the natural width (trailing-whitespace
        // semantics), so inter-widget spacing is unchanged:
        assert_close(placed.row.size.x, natural_width);

        // Centered rows must center the true ink on the galley origin:
        let mut placed = placed_row(
            shaped_arabic(),
            natural_width,
            TextDirection::LeftToRight,
            0.0,
        );
        halign_and_justify_row(
            point_scale,
            &mut placed,
            Align::Center,
            f32::INFINITY,
            false,
            false,
        );
        assert_close(placed.pos.x, -14.0); // round_to_pixel(-13.5)
        let (ink_min, ink_max) = ink_span(&placed.row);
        // Centered on the galley origin, up to pixel rounding:
        assert!(((ink_min + ink_max) * 0.5 + placed.pos.x).abs() <= 0.5);

        // A monotonic row (endpoints bracket the ink) is laid out exactly as
        // before the fix:
        let mut placed = placed_row(
            vec![glyph('a', 0.0, 17.0)],
            17.0,
            TextDirection::LeftToRight,
            0.0,
        );
        halign_and_justify_row(
            point_scale,
            &mut placed,
            Align::Max,
            f32::INFINITY,
            false,
            false,
        );
        assert_close(placed.pos.x, -17.0);
        assert_close(placed.row.size.x, 17.0);
        assert_close(placed.row.glyphs[0].pos.x, 0.0);

        // Composition with the trailing-whitespace semantics: a trailing
        // space is trimmed from the ink range (the row stays anchored on the
        // ink, at -27) while the allocation keeps counting it:
        let mut with_trailing_space = shaped_arabic();
        with_trailing_space.push(glyph(' ', 17.0, 3.0));
        let natural_with_space = natural_width + 3.0;

        let mut placed = placed_row(
            with_trailing_space.clone(),
            natural_with_space,
            TextDirection::LeftToRight,
            0.0,
        );
        halign_and_justify_row(
            point_scale,
            &mut placed,
            Align::Max,
            f32::INFINITY,
            false,
            false,
        );
        assert_close(placed.pos.x, -27.0);
        assert_close(placed.row.size.x, natural_with_space);

        // With `keep_trailing_whitespace` the space is part of the aligned
        // span, so the row is anchored on [-10, 20] and sized to it:
        let mut placed = placed_row(
            with_trailing_space,
            natural_with_space,
            TextDirection::LeftToRight,
            0.0,
        );
        halign_and_justify_row(
            point_scale,
            &mut placed,
            Align::Max,
            f32::INFINITY,
            false,
            true,
        );
        assert_close(placed.pos.x, -30.0);
        assert_close(placed.row.size.x, 30.0);
    }

    // -------------------------------------------------------------------------
    // Leading-advance threading (the row field, the anchoring inset and the
    // wrap path), pinned with hand-built rows — no fonts needed:
    // `halign_and_justify_row` anchors from glyph-relative ink and
    // `line_break` is called directly on hand-built paragraphs.

    /// A self-consistent synthetic glyph: `logical_rect` spans
    /// `[pos.x, pos.x + advance_width]` around the baseline.
    fn advance_glyph(chr: char, x: f32, advance: f32, bidi_level: u8) -> Glyph {
        Glyph {
            chr,
            pos: pos2(x, 0.0),
            advance_width: advance,
            line_height: 16.0,
            font_ascent: 13.0,
            font_height: 16.0,
            font_face_ascent: 13.0,
            font_face_height: 16.0,
            font_face_align: FallbackAlign::default(),
            uv_rect: Default::default(),
            is_color: false,
            bidi_level,
            section_index: 0,
            first_vertex: 0,
        }
    }

    fn advance_assert_close(got: f32, want: f32) {
        assert!((got - want).abs() < 1e-3, "got {got:.4}, want {want:.4}");
    }

    fn advance_ink_span(row: &Row) -> (f32, f32) {
        (
            row.glyphs
                .iter()
                .map(|glyph| glyph.logical_rect().min.x)
                .fold(f32::INFINITY, f32::min),
            row.glyphs
                .iter()
                .map(|glyph| glyph.logical_rect().max.x)
                .fold(f32::NEG_INFINITY, f32::max),
        )
    }

    /// A row shaped as production leaves it: the glyph positions start at
    /// `leading_advance` (the `leading_space` cursor advance precedes
    /// shaping), the row box spans advance plus ink, and the advance gap is
    /// the box tail beyond the ink.
    fn advance_row(base_direction: TextDirection, leading_advance: f32) -> PlacedRow {
        let glyphs = vec![
            advance_glyph('ا', leading_advance, 13.0, 1),
            advance_glyph('ب', leading_advance + 13.0, 14.0, 1),
        ];
        PlacedRow {
            pos: pos2(0.0, 0.0),
            row: Arc::new(Row {
                section_index_at_start: 0,
                base_direction,
                leading_advance,
                glyphs,
                rtl_paragraph: base_direction == TextDirection::RightToLeft,
                visual_order: Vec::new(),
                size: vec2(leading_advance + 27.0, 16.0),
                visuals: RowVisuals::default(),
            }),
            ends_with_newline: false,
        }
    }

    /// A right-anchored right-to-left row keeps the leading advance inside
    /// the reading start: the anchored ink is inset by the advance (its
    /// galley-frame right edge lands at `-advance`), the row box still ends
    /// at the galley origin, and the allocation keeps the natural width.
    #[test]
    fn right_anchored_rtl_row_keeps_the_leading_advance_in_the_ink() {
        let mut placed = advance_row(TextDirection::RightToLeft, 40.0);
        halign_and_justify_row(
            PointScale::new(1.0),
            &mut placed,
            Align::RIGHT,
            f32::INFINITY,
            false,
            false,
        );

        let (ink_min, ink_max) = advance_ink_span(&placed.row);
        advance_assert_close(ink_max + placed.pos.x, -40.0);
        advance_assert_close(ink_min + placed.pos.x, -40.0 - 27.0);
        advance_assert_close(placed.pos.x + placed.row.size.x, 0.0);
        advance_assert_close(placed.row.size.x, 67.0);
    }

    /// The no-blast-radius condition: a zero leading advance leaves the
    /// anchoring exactly as before (ink right at the galley origin).
    #[test]
    fn zero_leading_advance_leaves_anchoring_unchanged() {
        let mut placed = advance_row(TextDirection::RightToLeft, 0.0);
        halign_and_justify_row(
            PointScale::new(1.0),
            &mut placed,
            Align::RIGHT,
            f32::INFINITY,
            false,
            false,
        );

        let (ink_min, ink_max) = advance_ink_span(&placed.row);
        advance_assert_close(ink_max + placed.pos.x, 0.0);
        advance_assert_close(ink_min + placed.pos.x, -27.0);
    }

    /// Left-to-right rows ignore the reading-start inset (the advance stays
    /// absorbed, exactly as before).
    #[test]
    fn ltr_row_ignores_the_reading_start_inset() {
        let mut placed = advance_row(TextDirection::LeftToRight, 40.0);
        halign_and_justify_row(
            PointScale::new(1.0),
            &mut placed,
            Align::RIGHT,
            f32::INFINITY,
            false,
            false,
        );

        let (ink_min, ink_max) = advance_ink_span(&placed.row);
        advance_assert_close(ink_max + placed.pos.x, 0.0);
        advance_assert_close(ink_min + placed.pos.x, -27.0);
    }

    /// Center-anchored rows still absorb the advance (which edge would a
    /// Center inset even use?).
    #[test]
    fn center_anchored_rows_still_absorb_the_advance() {
        let mut placed = advance_row(TextDirection::RightToLeft, 40.0);
        halign_and_justify_row(
            PointScale::new(1.0),
            &mut placed,
            Align::Center,
            f32::INFINITY,
            false,
            false,
        );

        let (ink_min, ink_max) = advance_ink_span(&placed.row);
        assert!(((ink_min + ink_max) * 0.5 + placed.pos.x).abs() <= 0.5);
    }

    /// The wrap path threads the paragraph's leading advance onto the row
    /// that carries the paragraph's first glyph — the advance stays in that
    /// row's glyphs (production leaves it in the first glyph's position) and
    /// in its `leading_advance` — and every later row is re-based without
    /// it. The empty-first-row escape consumes the advance: no row keeps it.
    #[test]
    fn line_break_threads_the_leading_advance_onto_the_first_glyph_row() {
        fn advance_paragraph() -> Paragraph {
            // "ab cd ef" with a 40 px indent in the first glyph's position,
            // as production leaves it (the `leading_space` cursor advance
            // precedes shaping).
            let mut paragraph = Paragraph::from_section_index(0);
            paragraph.rtl = true;
            paragraph.leading_advance = 40.0;
            paragraph.glyphs = vec![
                advance_glyph('a', 40.0, 10.0, 0),
                advance_glyph('b', 50.0, 10.0, 0),
                advance_glyph(' ', 60.0, 5.0, 0),
                advance_glyph('c', 65.0, 10.0, 0),
                advance_glyph('d', 75.0, 10.0, 0),
                advance_glyph(' ', 85.0, 5.0, 0),
                advance_glyph('e', 90.0, 10.0, 0),
                advance_glyph('f', 100.0, 10.0, 0),
            ];
            paragraph
        }

        fn fork_job(max_width: f32) -> LayoutJob {
            LayoutJob {
                text: "ab cd ef".to_owned(),
                sections: vec![LayoutSection {
                    leading_space: 0.0,
                    byte_range: index::ByteIndex(0)..index::ByteIndex(8),
                    format: TextFormat::default(),
                }],
                wrap: TextWrapping {
                    max_width,
                    ..Default::default()
                },
                direction: TextDirection::RightToLeft,
                round_output_to_gui: false,
                ..Default::default()
            }
        }

        // At width 65 the first break lands on the first space (after the
        // words "ab" fit beside the indent): the first row carries the
        // advance.
        let job = fork_job(65.0);
        let paragraph = advance_paragraph();
        let breaks = break_opportunities(&paragraph, &job, &SimpleLineBreaker);
        let mut rows = Vec::new();
        let mut elided = false;
        line_break(&paragraph, &breaks, &job, &mut rows, &mut elided);
        assert!(!elided);
        assert_eq!(
            rows.len(),
            2,
            "two rows: {:?}",
            rows.iter().map(|r| r.size.x).collect::<Vec<_>>()
        );
        advance_assert_close(rows[0].leading_advance, 40.0);
        advance_assert_close(rows[0].glyphs[0].pos.x, 40.0);
        advance_assert_close(rows[1].leading_advance, 0.0);
        advance_assert_close(rows[1].glyphs[0].pos.x, 0.0);

        // Below one word plus the indent, the empty-first-row escape fires
        // and consumes the advance: every row lands at 0.0 (mirroring the
        // LTR arm's pre-existing loss of the indent in this edge case).
        let narrow = fork_job(45.0);
        let paragraph = advance_paragraph();
        let breaks = break_opportunities(&paragraph, &narrow, &SimpleLineBreaker);
        let mut rows = Vec::new();
        let mut elided = false;
        line_break(&paragraph, &breaks, &narrow, &mut rows, &mut elided);
        assert!(!elided);
        assert!(rows.len() >= 2);
        assert!(
            rows.iter().all(|placed| placed.leading_advance == 0.0),
            "the escape consumes the advance: {:?}",
            rows.iter().map(|r| r.leading_advance).collect::<Vec<_>>()
        );
    }

    /// End to end with real fonts: a wrapping right-to-left paragraph whose
    /// first section carries a `leading_space` hangs the advance inside the
    /// reading-start edge. Every row's box right lands on the common
    /// reading edge (no overhang), the first row's text is inset by the
    /// advance from that edge, and later rows hug the edge.
    #[test]
    fn wrapped_rtl_leading_space_hangs_at_the_reading_start() {
        let mut job = LayoutJob::simple(
            "مرحبا بالعالم من اجل الاختبار الطويل".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            120.0,
        );
        job.halign = Align::Min;
        job.halign_min_is_start = true;
        job.sections[0].leading_space = 30.0;
        let mut fonts_base = Fonts::new(TextOptions::default(), test_font_definitions());
        let mut fonts = fonts_base.with_pixels_per_point(2.0);
        let galley = fonts.layout_job(job);
        assert!(galley.rows.len() >= 2, "the paragraph must wrap");

        let ink_right = |placed: &PlacedRow| -> f32 {
            placed
                .glyphs
                .iter()
                .map(|glyph| glyph.max_x())
                .fold(f32::NEG_INFINITY, f32::max)
                + placed.pos.x
        };
        let box_right = |placed: &PlacedRow| placed.pos.x + placed.size.x;
        let row0_ink_right = ink_right(&galley.rows[0]);
        for row in galley.rows.iter().skip(1) {
            // Later rows hug the reading-start edge (up to the row's
            // trailing wobble), exactly the advance further right than the
            // first row's text:
            assert!(
                (ink_right(row) - row0_ink_right - 30.0).abs() <= 1.0,
                "later rows hug the edge, the first row is inset by the \
                 advance: row0 {row0_ink_right:.2}, later {:.2}",
                ink_right(row)
            );
        }
        // All rows' boxes end on the common reading edge up to each row's
        // trailing break-space wobble (a wrapped row keeps its break space
        // in its box; ~3 px for the bundled font's space at 14 px — before
        // the fix the advance-carrying row's box overhung the edge by the
        // whole advance):
        let box_rights: Vec<f32> = galley.rows.iter().map(box_right).collect();
        let min_box_right = box_rights.iter().copied().fold(f32::INFINITY, f32::min);
        let max_box_right = box_rights.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        assert!(
            max_box_right - min_box_right <= 4.0,
            "every row hangs from the same reading edge (up to trailing \
             wobble): {box_rights:?}"
        );

        // The zero-advance twin keeps the pre-existing anchoring: every
        // row's text is flush at the common edge (up to wobble).
        let mut job = LayoutJob::simple(
            "مرحبا بالعالم من اجل الاختبار الطويل".to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            120.0,
        );
        job.halign = Align::Min;
        job.halign_min_is_start = true;
        let mut fonts_base = Fonts::new(TextOptions::default(), test_font_definitions());
        let mut fonts = fonts_base.with_pixels_per_point(2.0);
        let galley = fonts.layout_job(job);
        assert!(galley.rows.len() >= 2, "the paragraph must wrap");
        let row0_ink_right = ink_right(&galley.rows[0]);
        for row in galley.rows.iter().skip(1) {
            assert!(
                (ink_right(row) - row0_ink_right).abs() <= 1.0,
                "without an advance every row's text is flush: row0 \
                 {row0_ink_right:.2}, later {:.2}",
                ink_right(row)
            );
        }
    }

    // -------------------------------------------------------------------------
    // Per-paragraph directional-run pins: glyphs are stored in logical order
    // with a per-glyph `bidi_level`, wrapping already walks the logical
    // stream, and the visual placement per UAX #9 L1/L2 is derived from
    // those levels per row (`reset_whitespace_levels` +
    // `reorder_row_visually`). The directional runs are exactly recoverable
    // by grouping the row's glyphs on `bidi_level`, in logical order, for
    // any wrap width (there is nothing to gate on a finite one).

    /// The directional runs of a row's glyphs, recovered by grouping
    /// consecutive glyphs on their `bidi_level`: (level, chars).
    fn runs_by_level(row: &Row) -> Vec<(u8, String)> {
        let mut runs: Vec<(u8, String)> = Vec::new();
        for glyph in &row.glyphs {
            match runs.last_mut() {
                Some((level, text)) if *level == glyph.bidi_level => {
                    text.push(glyph.chr);
                }
                _ => runs.push((glyph.bidi_level, glyph.chr.to_string())),
            }
        }
        runs
    }

    /// A mixed-run shape decomposes into its four directional runs in
    /// logical order: the Arabic word plus the neutral
    /// space and dot at the odd (right-to-left) base level, the Arabic-Indic
    /// digits and the latin extension at even levels — one glyph slot per
    /// char, bytes contiguous. The visual order
    /// (`pdf.٢٠٢٦ تقرير`) is exactly this run list reversed, which the
    /// per-row L2 reversal then produces.
    #[test]
    fn bidi_runs_are_recoverable_from_logical_glyph_levels() {
        let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
        let mut job = LayoutJob::simple(
            "تقرير ٢٠٢٦.pdf".into(),
            FontId::proportional(14.0),
            Color32::WHITE,
            10_000.0, // big enough to never overflow these shapes
        );
        job.direction = TextDirection::RightToLeft;
        let galley = layout(&mut fonts, 1.0, job.into());

        assert_eq!(galley.rows.len(), 1);
        let row = &galley.rows[0].row;
        assert_eq!(
            row.glyphs.len(),
            "تقرير ٢٠٢٦.pdf".chars().count(),
            "one glyph slot per char"
        );
        let runs = runs_by_level(row);
        assert_eq!(
            runs,
            vec![
                (1, "تقرير ".to_owned()),
                (2, "٢٠٢٦".to_owned()),
                (1, ".".to_owned()),
                (2, "pdf".to_owned()),
            ],
            "runs: {runs:?}"
        );

        // A mixed RTL-base segment: the level-2 latin island and the
        // level-1 Arabic word, in logical order (the visual order is the
        // reverse: island first, Arabic second):
        let mut job = LayoutJob::simple(
            "عربي aaa bbb".into(),
            FontId::proportional(14.0),
            Color32::WHITE,
            10_000.0,
        );
        job.direction = TextDirection::RightToLeft;
        let galley = layout(&mut fonts, 1.0, job.into());
        let runs = runs_by_level(&galley.rows[0].row);
        assert_eq!(
            runs,
            vec![(1, "عربي ".to_owned()), (2, "aaa bbb".to_owned())],
            "runs: {runs:?}"
        );

        // Pure-LTR rows stamp level 0 on every glyph: one even-level run, no
        // reordering (the row keeps the stored order — `visual_order` empty).
        for text in ["Hello World 123", "Héllo wörld"] {
            let mut job = LayoutJob::simple(
                text.into(),
                FontId::proportional(14.0),
                Color32::WHITE,
                10_000.0,
            );
            job.direction = TextDirection::LeftToRight;
            let galley = layout(&mut fonts, 1.0, job.into());
            let row = &galley.rows[0].row;
            assert_eq!(
                runs_by_level(row),
                vec![(0, text.to_owned())],
                "{text:?} must be a single even-level run"
            );
            // No reordering happened: the stored (logical) order is the
            // visual order.
            assert!(
                row.visual_order()
                    .enumerate()
                    .all(|(position, index)| position == index),
                "{text:?} must not be reordered"
            );
            assert!(row.glyphs.iter().all(|glyph| !glyph.is_rtl()));
        }
    }

    // -------------------------------------------------------------------------
    // Wrapped rows keep UAX #9 L1/L2 per rendered line: `line_break` walks
    // the logical glyph vector and the visual placement happens per row
    // afterwards; these tests pin that end-state contract.
    //
    // All wrap-width measurements come from an LTR-pinned twin of the wrapped
    // job (same text, `round_output_to_gui = false`, infinite width): the
    // twin is never reordered, so its stored order is its visual order and
    // every measured quantity is a span or gap, never an absolute x. The
    // synthetic tofu fonts serve every glyph with the same whole-pixel
    // advance, so the twin's spans are bit-identical to the widths the wrap
    // compares. Each intended row's width is its content INCLUDING its
    // trailing break space; `W` is the maximum of those over all intended
    // rows (the final row contributes its bare content) plus 0.5 px, with
    // per-boundary asserts.
    //
    // A row's `glyphs` stay in LOGICAL order and the visual
    // placement lives in `pos.x` + `visual_order`, so all per-row visual
    // assertions below read the glyphs through `Row::visual_order`.

    /// The visible glyphs of every row of a galley, in visual (left-to-right)
    /// order, as `(chr, x, advance)`.
    fn visible_glyphs_per_row(galley: &Galley) -> Vec<Vec<(char, f32, f32)>> {
        galley
            .rows
            .iter()
            .map(|placed| {
                placed
                    .row
                    .visual_order()
                    .filter(|&i| placed.row.glyphs[i].advance_width > 0.0)
                    .map(|i| {
                        let glyph = &placed.row.glyphs[i];
                        (glyph.chr, glyph.pos.x, glyph.advance_width)
                    })
                    .collect()
            })
            .collect()
    }

    /// The visible non-whitespace glyphs of every row, in visual order.
    fn visible_chars_per_row(galley: &Galley) -> Vec<Vec<char>> {
        galley
            .rows
            .iter()
            .map(|placed| {
                placed
                    .row
                    .visual_order()
                    .filter(|&i| {
                        let glyph = &placed.row.glyphs[i];
                        glyph.advance_width > 0.0 && !glyph.chr.is_whitespace()
                    })
                    .map(|i| placed.row.glyphs[i].chr)
                    .collect()
            })
            .collect()
    }

    /// The stored (logical) text of every row.
    fn logical_text_per_row(galley: &Galley) -> Vec<String> {
        galley.rows.iter().map(|placed| placed.row.text()).collect()
    }

    /// The glyphs of `row` in visual order, filtered to advancing ones.
    fn visual_glyphs(row: &Row) -> Vec<(char, f32, f32)> {
        row.visual_order()
            .filter(|&i| row.glyphs[i].advance_width > 0.0)
            .map(|i| {
                let glyph = &row.glyphs[i];
                (glyph.chr, glyph.pos.x, glyph.advance_width)
            })
            .collect()
    }

    /// One row with no wrapping, from the synthetic tofu fonts at ppp 1.0
    /// (every advance a whole number of physical pixels).
    fn layout_unwrapped(text: &str, direction: TextDirection) -> Galley {
        let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
        let mut job = LayoutJob::simple(
            text.to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            f32::INFINITY,
        );
        job.direction = direction;
        job.round_output_to_gui = false;
        layout(&mut fonts, 1.0, job.into())
    }

    /// Lay out with a finite wrap width, from the synthetic tofu fonts.
    fn layout_wrapped(text: &str, direction: TextDirection, wrap_width: f32) -> Galley {
        let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
        let mut job = LayoutJob::simple(
            text.to_owned(),
            FontId::proportional(14.0),
            Color32::WHITE,
            wrap_width,
        );
        job.direction = direction;
        job.round_output_to_gui = false;
        layout(&mut fonts, 1.0, job.into())
    }

    /// The whitespace-separated visible runs of a single-row stream: each
    /// run's span (last `max_x` − first x) and the gaps between consecutive
    /// runs (next run's first x − previous run's last `max_x`). A run boundary
    /// is a gap in the stream (the filtered-out space's advance). Measured on
    /// the visual order (which for the LTR twins used here is the stored one).
    fn visible_runs(galley: &Galley) -> (Vec<f32>, Vec<f32>) {
        assert_eq!(galley.rows.len(), 1, "expected a single-row galley");
        let glyphs: Vec<(char, f32, f32)> = visual_glyphs(&galley.rows[0].row)
            .into_iter()
            .filter(|(chr, _, _)| !chr.is_whitespace())
            .collect();
        let mut spans = Vec::new();
        let mut gaps = Vec::new();
        let mut run_first = 0usize;
        for i in 1..=glyphs.len() {
            let boundary =
                i == glyphs.len() || glyphs[i].1 - (glyphs[i - 1].1 + glyphs[i - 1].2) > 0.5;
            if boundary {
                spans.push(glyphs[i - 1].1 + glyphs[i - 1].2 - glyphs[run_first].1);
                if i < glyphs.len() {
                    gaps.push(glyphs[i].1 - (glyphs[i - 1].1 + glyphs[i - 1].2));
                }
                run_first = i;
            }
        }
        (spans, gaps)
    }

    /// Assert two glyph vectors agree within half a pixel.
    #[track_caller]
    fn assert_glyphs_close(got: &[(char, f32, f32)], expected: &[(char, f32, f32)]) {
        assert_eq!(
            got.len(),
            expected.len(),
            "glyph counts: {got:?} vs {expected:?}"
        );
        for (i, ((gc, gx, ga), (ec, ex, ea))) in core::iter::zip(got, expected).enumerate() {
            assert_eq!(gc, ec, "glyph {i}: {got:?} vs {expected:?}");
            assert!(
                (gx - ex).abs() < 0.5 && (ga - ea).abs() < 0.5,
                "glyph {i} ({gc}): got x={gx} a={ga}, expected x={ex} a={ea}"
            );
        }
    }

    /// Assert the visual x positions of a row's glyphs are non-decreasing.
    #[track_caller]
    fn assert_visual_x_nondecreasing(glyphs: &[(char, f32, f32)]) {
        for pair in glyphs.windows(2) {
            assert!(
                pair[0].1 <= pair[1].1 + 0.5,
                "visual x must be non-decreasing: {glyphs:?}"
            );
        }
    }

    /// A wrapped pure-RTL paragraph reads top-down: one word per row, in
    /// logical order. (`line_break` walks the logical stream, so this pins
    /// the existing behavior; slicing an emitted visual stream from its
    /// visually-left, logically-last end would produce the rows bottom-up.)
    #[test]
    fn wrapped_pure_rtl_reads_top_down() {
        let text = "أول ثاني ثالث";

        // Wrap width measured from the LTR twin (a single run emits the same
        // stream in both bases): `أول ␣ ثاني ␣ ثالث`, so run 0 = أول,
        // run 1 = ثاني, run 2 = ثالث.
        let twin = layout_unwrapped(text, TextDirection::LeftToRight);
        let (spans, gaps) = visible_runs(&twin);
        assert_eq!(
            spans.len(),
            3,
            "the twin's stream is three words: {spans:?}"
        );
        assert_eq!(gaps.len(), 2, "spans {spans:?} gaps {gaps:?}");
        let (w_awwal, w_thani, w_thalith) = (spans[0], spans[1], spans[2]);
        let s = gaps[0];
        assert!(
            (gaps[1] - s).abs() < 1e-3,
            "the tofu space advance is uniform: {spans:?} {gaps:?}"
        );
        let wrap_width = (w_awwal + s).max(w_thani + s).max(w_thalith) + 0.5;
        // Per-boundary asserts: each intended row's content + break space
        // fits, the next word overflows, and the final row's bare content
        // fits:
        assert!(
            w_awwal + s <= wrap_width && w_awwal + s + w_thani > wrap_width,
            "spans {spans:?} gaps {gaps:?} W {wrap_width}"
        );
        assert!(
            w_thani + s <= wrap_width && w_thani + s + w_thalith > wrap_width,
            "spans {spans:?} gaps {gaps:?} W {wrap_width}"
        );
        assert!(w_thalith <= wrap_width, "W {wrap_width}");

        for direction in [TextDirection::RightToLeft, TextDirection::Auto] {
            let galley = layout_wrapped(text, direction, wrap_width);
            assert_eq!(galley.rows.len(), 3, "one word per row");
            let chars = visible_chars_per_row(&galley);
            assert_eq!(chars[0], ['ل', 'و', 'أ'], "row 0 renders أول: {chars:?}");
            assert_eq!(
                chars[1],
                ['ي', 'ن', 'ا', 'ث'],
                "row 1 renders ثاني: {chars:?}"
            );
            assert_eq!(
                chars[2],
                ['ث', 'ل', 'ا', 'ث'],
                "row 2 renders ثالث: {chars:?}"
            );
            // Rows are produced in logical (reading) order:
            assert_eq!(
                logical_text_per_row(&galley),
                ["أول ", "ثاني ", "ثالث"],
                "rows must follow the reading order (direction {direction:?})"
            );
            // Row invariant: visual x non-decreasing, and one glyph slot per
            // char of the row's logical slice (the word plus its trailing
            // break space):
            for glyphs in visible_glyphs_per_row(&galley) {
                assert_visual_x_nondecreasing(&glyphs);
            }
            let counts: Vec<usize> = galley
                .rows
                .iter()
                .map(|placed| placed.row.glyphs.len())
                .collect();
            assert_eq!(counts, vec![4, 5, 4], "glyph slots per row: {counts:?}");
        }

        // Two words then one: W = w(أول) + s + w(ثاني) + s + 0.5.
        let wrap_width2 = w_awwal + s + w_thani + s + 0.5;
        assert!(w_awwal + s + w_thani + s <= wrap_width2);
        assert!(w_awwal + s + w_thani + s + w_thalith > wrap_width2);
        assert!(w_thalith <= wrap_width2);
        let galley = layout_wrapped(text, TextDirection::RightToLeft, wrap_width2);
        assert_eq!(galley.rows.len(), 2);
        let chars = visible_chars_per_row(&galley);
        // Row 0 reads `أول ثاني` right-to-left: أول at the visual RIGHT, so
        // the visual order is ثاني's glyphs first:
        assert_eq!(chars[0], ['ي', 'ن', 'ا', 'ث', 'ل', 'و', 'أ'], "{chars:?}");
        assert_eq!(chars[1], ['ث', 'ل', 'ا', 'ث'], "{chars:?}");
        assert_eq!(logical_text_per_row(&galley), ["أول ثاني ", "ثالث"]);
        for glyphs in visible_glyphs_per_row(&galley) {
            assert_visual_x_nondecreasing(&glyphs);
        }
    }

    /// Single-row visual-layout pins for the wrapped-reference strings: the
    /// no-wrap path keeps the mirror layout — uniform
    /// advance, glyphs at exact multiples of it in visual order. Measured
    /// against the tofu advance rather than hard-coded.
    #[test]
    fn wrapped_oracle_strings_single_row_mirror_the_stream() {
        let w = visible_glyphs_per_row(&layout_unwrapped("a", TextDirection::Auto))[0][0].2;

        // An RTL-base paragraph (strong Arabic resolves Auto to RTL): the
        // visual order is the logically-last word first, positions the
        // exact multiples of the uniform advance:
        let glyphs =
            visual_glyphs(&layout_unwrapped("أول ثاني ثالث", TextDirection::Auto).rows[0].row);
        let expected: Vec<(char, f32, f32)> = [
            ('ث', 0.0),
            ('ل', 1.0),
            ('ا', 2.0),
            ('ث', 3.0),
            (' ', 4.0),
            ('ي', 5.0),
            ('ن', 6.0),
            ('ا', 7.0),
            ('ث', 8.0),
            (' ', 9.0),
            ('ل', 10.0),
            ('و', 11.0),
            ('أ', 12.0),
        ]
        .into_iter()
        .map(|(chr, k)| (chr, k * w, w))
        .collect();
        assert_glyphs_close(&glyphs, &expected);

        // An RTL-pinned mixed paragraph: the latin island renders at the
        // visual left, the Arabic word reversed at the visual right:
        let glyphs = visual_glyphs(
            &layout_unwrapped("عربي aaa bbb", TextDirection::RightToLeft).rows[0].row,
        );
        let expected: Vec<(char, f32, f32)> = [
            ('a', 0.0),
            ('a', 1.0),
            ('a', 2.0),
            (' ', 3.0),
            ('b', 4.0),
            ('b', 5.0),
            ('b', 6.0),
            (' ', 7.0),
            ('ي', 8.0),
            ('ب', 9.0),
            ('ر', 10.0),
            ('ع', 11.0),
        ]
        .into_iter()
        .map(|(chr, k)| (chr, k * w, w))
        .collect();
        assert_glyphs_close(&glyphs, &expected);
    }

    // -------------------------------------------------------------------------
    // Mixed-direction wrapped rows keep UAX #9 L1/L2 per rendered line: the
    // per-glyph bidi levels already express that per row, and the tests
    // below pin the per-row UAX #9 L1/L2 contract.

    /// Build a two-section job: `section_texts` are concatenated in order;
    /// the byte ranges are derived from the concatenation.
    fn two_section_job(
        section_texts: &[&str],
        formats: &[TextFormat; 2],
        wrap_width: f32,
        direction: TextDirection,
    ) -> LayoutJob {
        let mut text = String::new();
        let mut sections = Vec::new();
        for (i, section_text) in section_texts.iter().enumerate() {
            let start = ByteIndex(text.len());
            text.push_str(section_text);
            sections.push(LayoutSection {
                leading_space: 0.0,
                byte_range: start..ByteIndex(text.len()),
                format: formats[i].clone(),
            });
        }
        let mut job = LayoutJob {
            sections,
            text,
            wrap: TextWrapping {
                max_width: wrap_width,
                ..Default::default()
            },
            break_on_newline: true,
            ..Default::default()
        };
        job.direction = direction;
        job.round_output_to_gui = false;
        job
    }

    /// A wrapped mixed-direction RTL-base paragraph keeps UAX #9 L1/L2
    /// per rendered line: `عربي aaa bbb` (RTL-pinned) breaks at the logical
    /// space after `aaa`, and row 1 must render `aaa يبرع` (the latin island
    /// at the visual left, the Arabic word at its reading position), not a
    /// geometric slicing of the whole-segment visual stream (`aaa bbb يبرع`),
    /// which would strand the Arabic word alone on the last row.
    #[test]
    fn wrapped_mixed_rtl_base_keeps_uax9_per_row() {
        let text = "عربي aaa bbb";

        // Wrap width measured from the LTR twin: its visual stream is
        // `يبرع ␣ aaa ␣ bbb` (the RTL run reverses to the front), so
        // run 0 = يبرع, run 1 = aaa, run 2 = bbb. The intended row 1 is
        // `عربي aaa ␣` (content + trailing break space); the `aaa`→`bbb`
        // gap's space sits at an even level in both bases (level 0 in the
        // twin, level 2 inside the island in the RTL-pinned job), so its shaping
        // — and hence its advance — is identical in both.
        let twin = layout_unwrapped(text, TextDirection::LeftToRight);
        let (spans, gaps) = visible_runs(&twin);
        assert_eq!(spans.len(), 3, "spans {spans:?}");
        assert_eq!(gaps.len(), 2, "spans {spans:?} gaps {gaps:?}");
        let (w_arabi, w_aaa, w_bbb) = (spans[0], spans[1], spans[2]);
        let s = gaps[1];
        let wrap_width = w_arabi + s + w_aaa + s + 0.5;
        // Per-boundary asserts: row 1's content + break space fits W, the
        // next word (bbb) overflows, and the final row's bare content fits:
        assert!(
            w_arabi + s + w_aaa + s <= wrap_width && w_arabi + s + w_aaa + s + w_bbb > wrap_width,
            "spans {spans:?} gaps {gaps:?} W {wrap_width}"
        );
        assert!(w_bbb <= wrap_width, "W {wrap_width}");

        let galley = layout_wrapped(text, TextDirection::RightToLeft, wrap_width);
        assert_eq!(
            galley.rows.len(),
            2,
            "rows: {rows:?}",
            rows = visible_chars_per_row(&galley)
        );
        let chars = visible_chars_per_row(&galley);
        assert_eq!(chars[0], ['a', 'a', 'a', 'ي', 'ب', 'ر', 'ع'], "{chars:?}");
        assert_eq!(chars[1], ['b', 'b', 'b'], "{chars:?}");
        for glyphs in visible_glyphs_per_row(&galley) {
            assert_visual_x_nondecreasing(&glyphs);
        }
        // One glyph slot per char of each row's logical slice (row 1 =
        // `عربي aaa ␣`, row 2 = `bbb`):
        let counts: Vec<usize> = galley
            .rows
            .iter()
            .map(|placed| placed.row.glyphs.len())
            .collect();
        assert_eq!(counts, vec![9, 3], "glyph slots per row: {counts:?}");
        assert_eq!(logical_text_per_row(&galley), ["عربي aaa ", "bbb"]);

        // Trailing-space pin: the kept break-candidate space survives, is
        // L1-reset to the base level, and renders at the row's visual
        // reading end — the visually-LEFT slot of an RTL-base row (its
        // `pos.x` is at or before every non-space glyph's x). The row's
        // glyphs are stored logically, so the kept space is the row's LAST
        // stored glyph. At an under-sized wrap width
        // this pin fails by design (the kept space becomes the row-internal
        // one).
        let row0 = &galley.rows[0].row;
        assert_eq!(
            row0.glyphs.last().map(|glyph| glyph.chr),
            Some(' '),
            "the break space trails the row's logical vector"
        );
        let min_visible_x = row0
            .glyphs
            .iter()
            .filter(|glyph| glyph.advance_width > 0.0 && !glyph.chr.is_whitespace())
            .map(|glyph| glyph.pos.x)
            .fold(f32::INFINITY, f32::min);
        let space_x = row0.glyphs.last().map(|glyph| glyph.pos.x).unwrap_or(0.0);
        assert!(
            space_x <= min_visible_x,
            "the L1-reset break space must sit at the visual reading end: {space_x} vs {min_visible_x}"
        );
    }

    /// An LTR-base paragraph with embedded RTL wraps correctly: the
    /// Hebrew word renders visually reordered (`םולש`) inside whichever row
    /// it lands, the surrounding latin words keep their logical order, and
    /// every row keeps the visual-x invariant. The wrap width anchors on the
    /// job's own single-row galley (an LTR-pinned job's first `w` glyph sits
    /// at the exact logical offset of the intended break).
    #[test]
    fn wrapped_ltr_base_embedded_rtl_stays_uax9() {
        let text = "Hello שלום world wrapped again";

        // Break after `שלום `: the first `w` glyph of the stream (of
        // "world") starts after 12 chars (`Hello שלום `):
        let single = layout_unwrapped(text, TextDirection::LeftToRight);
        let first_world_x = single.rows[0]
            .row
            .visual_order()
            .find(|&i| single.rows[0].row.glyphs[i].chr == 'w')
            .map(|i| single.rows[0].row.glyphs[i].pos.x)
            .unwrap_or_default();
        let wrap_width = first_world_x + 0.5;
        // The intended row 1 (`Hello שלום `) fits, the next word overflows,
        // and the final row's content fits (uniform tofu advances):
        let w = first_world_x / 12.0;
        assert!(first_world_x <= wrap_width && first_world_x + 5.0 * w > wrap_width);

        let galley = layout_wrapped(text, TextDirection::LeftToRight, wrap_width);
        assert!(galley.rows.len() >= 2);
        let chars = visible_chars_per_row(&galley);
        // Row 0 carries the pinned boundary: `Hello שלום ` — the Hebrew word
        // visually reordered:
        assert_eq!(
            chars[0],
            ['H', 'e', 'l', 'l', 'o', 'ם', 'ו', 'ל', 'ש'],
            "{chars:?}"
        );
        // The remaining latin words keep their logical order, in whichever
        // rows they land (the remainder is wider than W, so the partition is
        // deliberately loose beyond row 0):
        let words: [&[char]; 3] = [
            &['w', 'o', 'r', 'l', 'd'],
            &['w', 'r', 'a', 'p', 'p', 'e', 'd'],
            &['a', 'g', 'a', 'i', 'n'],
        ];
        for word in words {
            assert!(
                chars
                    .iter()
                    .any(|row| row.windows(word.len()).any(|w| w == word)),
                "{word:?} must appear in logical order in some row: {chars:?}"
            );
        }
        for glyphs in visible_glyphs_per_row(&galley) {
            assert_visual_x_nondecreasing(&glyphs);
        }
    }

    /// D — a paragraph spanning several `LayoutSection`s wraps jointly: an
    /// RTL word SPLIT by a color change (two sections, `عر` + `بي`) reorders
    /// as ONE run on its row (the row's per-glyph levels are computed over
    /// the whole text, so the section boundary does not split the run), and
    /// the rows follow the reading order.
    #[test]
    fn wrapped_multi_section_rtl_reorders_jointly() {
        let text = "عربي aaa bbb ccc ddd eee";
        let formats = [
            TextFormat::simple(FontId::proportional(14.0), Color32::RED),
            TextFormat::simple(FontId::proportional(14.0), Color32::WHITE),
        ];
        // `عر` in the red section, the rest in the white one — the word
        // عربي is split across the boundary:
        assert_eq!(&text[..4], "عر");
        let w = visible_glyphs_per_row(&layout_unwrapped("a", TextDirection::Auto))[0][0].2;
        let job = two_section_job(
            &[&text[..4], &text[4..]],
            &formats,
            (4 + 1 + 3 + 1) as f32 * w + 0.5,
            TextDirection::RightToLeft,
        );
        let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
        let galley = layout(&mut fonts, 1.0, job.into());

        // Rows partition logically (عربي aaa / bbb ccc / ddd eee):
        assert_eq!(galley.rows.len(), 3);
        let chars = visible_chars_per_row(&galley);
        // Row 0 renders `عربي aaa`: the split word as ONE run in visual
        // order (يبرع at the right, jointly across the section boundary),
        // the latin island at the visual left:
        assert_eq!(chars[0], ['a', 'a', 'a', 'ي', 'ب', 'ر', 'ع'], "{chars:?}");
        assert_eq!(chars[1], ['b', 'b', 'b', 'c', 'c', 'c'], "{chars:?}");
        assert_eq!(chars[2], ['d', 'd', 'd', 'e', 'e', 'e'], "{chars:?}");
        assert_eq!(
            logical_text_per_row(&galley),
            ["عربي aaa ", "bbb ccc ", "ddd eee"]
        );
        // Both sections' glyphs appear (no drops):
        assert_eq!(
            galley
                .rows
                .iter()
                .map(|placed| placed.row.glyphs.len())
                .sum::<usize>(),
            text.chars().count(),
            "one glyph slot per char across both sections"
        );
        for glyphs in visible_glyphs_per_row(&galley) {
            assert_visual_x_nondecreasing(&glyphs);
        }
    }

    /// D (unrecorded edge) — a paragraph mixing a pure-LTR lead with a
    /// wrapped RTL section keeps every glyph and reads in order. LTR-first
    /// order (the LTR stretch is the paragraph's head).
    #[test]
    fn wrapped_unrecorded_edge_ltr_first_keeps_all_glyphs() {
        let formats = [
            TextFormat::simple(FontId::proportional(14.0), Color32::RED),
            TextFormat::simple(FontId::proportional(14.0), Color32::WHITE),
        ];
        let w = visible_glyphs_per_row(&layout_unwrapped("a", TextDirection::Auto))[0][0].2;
        let job = two_section_job(
            &["abcd", " عربي aaa bbb"],
            &formats,
            (4 + 1 + 4 + 1) as f32 * w + 0.5,
            TextDirection::RightToLeft,
        );
        let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
        let galley = layout(&mut fonts, 1.0, job.into());

        assert_eq!(galley.rows.len(), 2);
        let chars = visible_chars_per_row(&galley);
        // Row 0 reads `abcd عربي `: visually ␣ يبرع ␣ abcd — the LTR word
        // kept, the Arabic word reordered as one run:
        assert_eq!(
            chars[0],
            ['ي', 'ب', 'ر', 'ع', 'a', 'b', 'c', 'd'],
            "{chars:?}"
        );
        assert_eq!(chars[1], ['a', 'a', 'a', 'b', 'b', 'b'], "{chars:?}");
        assert_eq!(logical_text_per_row(&galley), ["abcd عربي ", "aaa bbb"]);
        // No dropped glyphs:
        assert_eq!(
            galley
                .rows
                .iter()
                .map(|placed| placed.row.glyphs.len())
                .sum::<usize>(),
            "abcd عربي aaa bbb".chars().count(),
        );
        for glyphs in visible_glyphs_per_row(&galley) {
            assert_visual_x_nondecreasing(&glyphs);
        }
    }

    /// D (unrecorded edge, mirrored) — the pure-LTR stretch is the
    /// paragraph's TAIL: the trailing section's glyphs still land in their
    /// logical row, in logical order.
    #[test]
    fn wrapped_unrecorded_edge_ltr_last_keeps_all_glyphs() {
        let formats = [
            TextFormat::simple(FontId::proportional(14.0), Color32::RED),
            TextFormat::simple(FontId::proportional(14.0), Color32::WHITE),
        ];
        let w = visible_glyphs_per_row(&layout_unwrapped("a", TextDirection::Auto))[0][0].2;
        let job = two_section_job(
            &["عربي aaa ", "abcd"],
            &formats,
            (4 + 1 + 3 + 1) as f32 * w + 0.5,
            TextDirection::RightToLeft,
        );
        let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
        let galley = layout(&mut fonts, 1.0, job.into());

        assert_eq!(galley.rows.len(), 2);
        let chars = visible_chars_per_row(&galley);
        // Row 0 reads `عربي aaa ` (visually ␣ aaa ␣ يبرع), row 1 the
        // unrecorded tail in logical order:
        assert_eq!(chars[0], ['a', 'a', 'a', 'ي', 'ب', 'ر', 'ع'], "{chars:?}");
        assert_eq!(chars[1], ['a', 'b', 'c', 'd'], "{chars:?}");
        assert_eq!(logical_text_per_row(&galley), ["عربي aaa ", "abcd"]);
        assert_eq!(
            galley
                .rows
                .iter()
                .map(|placed| placed.row.glyphs.len())
                .sum::<usize>(),
            "عربي aaa abcd".chars().count(),
        );
        for glyphs in visible_glyphs_per_row(&galley) {
            assert_visual_x_nondecreasing(&glyphs);
        }
    }

    /// Wrapped RTL elision is directed at the READING end: the `…` replaces
    /// the logically-last row's trailing glyphs — including the L1-reset
    /// break space, which must be dropped with them instead of stranding the
    /// `…` behind it. The LTR mirror keeps the historic trailing `…`. (On the
    /// fork the row's glyphs are stored in logical order, so the `…` is the
    /// last STORED glyph in both directions and the direction shows in its
    /// visual x: at the reading end — visual left — on a right-to-left row,
    /// trailing on the visual right of a left-to-right one.)
    #[test]
    fn wrapped_rtl_elision_lands_at_the_reading_end() {
        let text = "أول ثاني ثالث";
        // The one-word-per-row width from test A:
        let twin = layout_unwrapped(text, TextDirection::LeftToRight);
        let (spans, gaps) = visible_runs(&twin);
        let wrap_width = (spans[0] + gaps[0]).max(spans[1] + gaps[0]).max(spans[2]) + 0.5;

        for direction in [
            TextDirection::RightToLeft,
            TextDirection::Auto,
            TextDirection::LeftToRight,
        ] {
            let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
            let mut job = LayoutJob::simple(
                text.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
                wrap_width,
            );
            job.direction = direction;
            job.wrap.max_rows = 2;
            job.wrap.overflow_character = Some('…');
            let galley = layout(&mut fonts, 1.0, job.into());
            assert!(
                galley.elided,
                "max_rows 2 must elide the third row (direction {direction:?})"
            );
            assert_eq!(galley.rows.len(), 2);
            let last = &galley.rows[1].row;
            let is_rtl = direction != TextDirection::LeftToRight;

            // The `…` is appended after the row's surviving glyphs, so it is
            // the last STORED glyph either way:
            let ellipsis = last.glyphs.last().map(|glyph| glyph.chr);
            assert_eq!(
                ellipsis,
                Some('…'),
                "glyphs: {:?} (direction {direction:?})",
                last.glyphs.iter().map(|g| g.chr).collect::<Vec<_>>()
            );

            // Visual direction, measured against the kept text (whitespace
            // excluded): the `…` sits at the reading end of a right-to-left
            // row (visual left, at or before every kept glyph's x), and
            // trails the visual right edge of the LTR mirror.
            let ellipsis_x = last.glyphs.last().map(|glyph| glyph.pos.x).unwrap_or(0.0);
            let kept_x: Vec<f32> = last.glyphs[..last.glyphs.len() - 1]
                .iter()
                .filter(|glyph| glyph.advance_width > 0.0 && !glyph.chr.is_whitespace())
                .map(|glyph| glyph.pos.x)
                .collect();
            if is_rtl {
                assert!(
                    kept_x.iter().all(|x| ellipsis_x <= *x),
                    "the ellipsis must sit at the row's reading end (direction {direction:?}): \
                     {ellipsis_x} vs kept {kept_x:?}"
                );
            } else {
                assert!(
                    kept_x.iter().all(|x| *x <= ellipsis_x),
                    "the LTR mirror keeps the trailing ellipsis (direction {direction:?}): \
                     {ellipsis_x} vs kept {kept_x:?}"
                );
            }
        }

        // Known tofu residual (documented, not fixed here): the tofu
        // fonts serve the overflow character with a ZERO advance, so it fits
        // without popping and the L1-reset break space survives at the row's
        // visual reading end. With a real font the space is popped with the
        // replaced glyphs — pinned below.
        {
            let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
            let mut job = LayoutJob::simple(
                text.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
                wrap_width,
            );
            job.direction = TextDirection::RightToLeft;
            job.wrap.max_rows = 2;
            job.wrap.overflow_character = Some('…');
            let galley = layout(&mut fonts, 1.0, job.into());
            let last = &galley.rows[1].row;
            let ellipsis_advance = last
                .glyphs
                .last()
                .map(|glyph| glyph.advance_width)
                .unwrap_or(0.0);
            if ellipsis_advance <= 0.0 {
                assert_eq!(
                    last.glyphs.last().map(|glyph| glyph.chr),
                    Some('…'),
                    "the zero-advance tofu ellipsis still trails the row"
                );
            }
        }
    }

    /// The wrapped elision contract with a real font: the `…` has a positive
    /// advance, so popping reaches the L1-reset break space and drops it with
    /// the replaced glyphs; the `…` lands at the row's visual reading end
    /// (visual left) on a right-to-left row and trails the LTR mirror.
    #[test]
    fn wrapped_rtl_elision_drops_the_break_space_with_a_real_font() {
        let text = "אבגד הוזח טיכל";

        // Wrap width measured from the LTR-pinned twin of the same text (its
        // stored order is its visual order): rows read `אבגד הוזח ` and
        // `טיכל`.
        let twin = {
            let mut fonts = test_fonts();
            let mut job = LayoutJob::simple(
                text.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
                f32::INFINITY,
            );
            job.direction = TextDirection::LeftToRight;
            job.round_output_to_gui = false;
            layout(&mut fonts, 1.0, job.into())
        };
        assert_eq!(twin.rows.len(), 1);
        let glyphs: Vec<(char, f32, f32)> = twin.rows[0]
            .row
            .glyphs
            .iter()
            .filter(|g| g.advance_width > 0.0)
            .map(|g| (g.chr, g.pos.x, g.advance_width))
            .collect();
        // The intended first row is `אבגד הוזח ` (content + break space);
        // its width is the x of the first glyph of `טיכל`:
        let first_row_end = glyphs
            .iter()
            .find(|(chr, _, _)| *chr == 'ט')
            .map(|(_, x, _)| *x)
            .unwrap_or_default();
        let last_word_width = glyphs
            .iter()
            .filter(|(chr, _, _)| "טיכל".contains(*chr))
            .map(|(_, x, a)| x + a)
            .fold(0.0_f32, f32::max)
            - glyphs
                .iter()
                .find(|(chr, _, _)| *chr == 'ט')
                .map(|(_, x, _)| *x)
                .unwrap_or_default();
        let wrap_width = first_row_end.max(last_word_width) + 0.5;

        for direction in [TextDirection::RightToLeft, TextDirection::LeftToRight] {
            let mut fonts = test_fonts();
            let mut job = LayoutJob::simple(
                text.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
                wrap_width,
            );
            job.direction = direction;
            job.wrap.max_rows = 2;
            job.wrap.overflow_character = Some('…');
            let galley = layout(&mut fonts, 1.0, job.into());
            assert!(
                galley.elided,
                "max_rows 2 must elide (direction {direction:?})"
            );
            assert_eq!(galley.rows.len(), 2);
            let last = &galley.rows[1].row;

            // The `…` is the last stored glyph and no whitespace survives
            // next to it: the L1-reset break space was dropped with the
            // replaced glyphs:
            assert_eq!(
                last.glyphs.last().map(|glyph| glyph.chr),
                Some('…'),
                "glyphs: {:?} (direction {direction:?})",
                last.glyphs.iter().map(|g| g.chr).collect::<Vec<_>>()
            );
            assert!(
                !last.glyphs.iter().any(|glyph| glyph.chr.is_whitespace()),
                "no break space may survive next to the ellipsis (direction {direction:?}): {:?}",
                last.glyphs.iter().map(|g| g.chr).collect::<Vec<_>>()
            );

            // Visual direction against the kept text:
            let ellipsis_x = last.glyphs.last().map(|glyph| glyph.pos.x).unwrap_or(0.0);
            let kept_x: Vec<f32> = last.glyphs[..last.glyphs.len() - 1]
                .iter()
                .filter(|glyph| glyph.advance_width > 0.0 && !glyph.chr.is_whitespace())
                .map(|glyph| glyph.pos.x)
                .collect();
            if direction == TextDirection::RightToLeft {
                assert!(
                    kept_x.iter().all(|x| ellipsis_x <= *x),
                    "the ellipsis must sit at the row's reading end: {ellipsis_x} vs {kept_x:?}"
                );
            } else {
                assert!(
                    kept_x.iter().all(|x| *x <= ellipsis_x),
                    "the LTR mirror keeps the trailing ellipsis: {ellipsis_x} vs {kept_x:?}"
                );
            }
        }
    }

    /// Hostile wrapped shapes must not panic and must keep one glyph slot per
    /// char whenever the wrap produced all the rows (no truncation): the
    /// digit-mixed wrapped shape and an isolate-soup wrapped at width 100,
    /// plus two-section wrapped RTL jobs in both section orders through the
    /// same wrap + elision gauntlet; the cursor paths are smoked on every
    /// galley.
    #[test]
    fn bidi_wrapped_hostile_input_does_not_panic() {
        // Stepping the caret one visual position (the fork's arrow keys,
        // Galley::cursor_left/right_visually) must stay panic-free and
        // clamped to the galley's end on hostile input: walk both arrow
        // directions from both reading edges, max_steps + 2 presses. Row and
        // galley edges clamp; none may ever exceed the galley's end.
        fn assert_arrow_walk_stays_clamped(galley: &Galley, text: &str, direction: TextDirection) {
            let end = galley.end().index;
            let max_steps = text.chars().count().saturating_add(2).min(64);
            let mut left_from_begin = galley.begin();
            let mut left_from_end = galley.end();
            let mut right_from_begin = galley.begin();
            let mut right_from_end = galley.end();
            for _ in 0..max_steps {
                left_from_begin = galley.cursor_left_visually(&left_from_begin);
                left_from_end = galley.cursor_left_visually(&left_from_end);
                right_from_begin = galley.cursor_right_visually(&right_from_begin);
                right_from_end = galley.cursor_right_visually(&right_from_end);
                assert!(
                    left_from_begin.index <= end
                        && left_from_end.index <= end
                        && right_from_begin.index <= end
                        && right_from_end.index <= end,
                    "arrow stepping must stay within the galley for {text:?} (direction {direction:?})"
                );
            }
        }

        // The caret geometry paths (`char_at` / `x_offset`) must stay
        // panic-free and in-range on hostile input: probe `char_at` over a
        // spread of x values (on-ink, row edges, far off-ink, and every glyph
        // center) and `x_offset` over every column, on every row of the
        // elided and untruncated layouts. NaN/inf clicks make every scan
        // comparison false, so plain `f32` comparisons need no guard.
        fn assert_caret_geometry_stays_in_range(
            galley: &Galley,
            text: &str,
            direction: TextDirection,
        ) {
            for placed in &galley.rows {
                let row = &placed.row;
                let n = row.char_count_excluding_newline().0;
                let mut probes = vec![0.0, row.size.x, -1000.0, 1000.0];
                probes.extend(row.glyphs.iter().map(|g| g.pos.x + g.advance_width * 0.5));
                for probe_x in probes {
                    let column = row.char_at(probe_x).0;
                    assert!(
                        column <= n,
                        "char_at must stay in range for {text:?} \
                         (direction {direction:?}): {column} > {n}"
                    );
                }
                for column in 0..=n {
                    let x = row.x_offset(CharIndex(column));
                    assert!(
                        x.is_finite(),
                        "x_offset must stay finite for {text:?} (direction {direction:?})"
                    );
                }
            }
        }

        let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
        let directions = [
            TextDirection::Auto,
            TextDirection::LeftToRight,
            TextDirection::RightToLeft,
        ];
        let wrapped_shapes = [
            "أول ٢٠٢٦ ثالث".to_owned(),
            "\u{2066}أول\u{2069} \u{2067}ثاني\u{2069} ثالث \u{200F}\u{200E}".repeat(6),
        ];

        for text in &wrapped_shapes {
            for direction in directions {
                // Wrapped + elided at width 100 (exercises line_break and the
                // elision path):
                let mut job = LayoutJob::simple(
                    text.clone(),
                    FontId::proportional(14.0),
                    Color32::WHITE,
                    100.0,
                );
                job.direction = direction;
                job.wrap.max_rows = 5;
                job.wrap.overflow_character = Some('…');
                let galley = layout(&mut fonts, 1.0, job.into());

                // One glyph slot per char, preserved when the wrap produced
                // all the rows (no truncation). The bidi format characters
                // (LRI/PDI/RLM/LRE) are kept as zero-width glyphs on this
                // fork, so they count; there are no Cc controls in these
                // shapes:
                if !galley.elided {
                    let total: usize = galley
                        .rows
                        .iter()
                        .map(|placed| placed.row.glyphs.len())
                        .sum();
                    assert_eq!(
                        total,
                        text.chars().count(),
                        "glyph slots must equal the char count for {text:?} (direction {direction:?})"
                    );
                }

                // Smoke the cursor paths on the elided galley:
                let _ = galley.end();
                let _ = galley.cursor_from_pos(vec2(0.0, 0.0));
                let _ = galley.pos_from_cursor(galley.end());
                assert_arrow_walk_stays_clamped(&galley, text, direction);
                assert_caret_geometry_stays_in_range(&galley, text, direction);

                // The same shape wide enough to never elide: the totals
                // assert above then bites on the untruncated rows.
                let mut job = LayoutJob::simple(
                    text.clone(),
                    FontId::proportional(14.0),
                    Color32::WHITE,
                    10_000.0,
                );
                job.direction = direction;
                job.wrap.max_rows = 5;
                let galley = layout(&mut fonts, 1.0, job.into());
                assert!(!galley.elided, "10_000 px must not elide {text:?}");
                let total: usize = galley
                    .rows
                    .iter()
                    .map(|placed| placed.row.glyphs.len())
                    .sum();
                assert_eq!(
                    total,
                    text.chars().count(),
                    "glyph slots must equal the char count for {text:?} (direction {direction:?})"
                );
                let _ = galley.cursor_from_pos(vec2(0.0, 0.0));
                let _ = galley.pos_from_cursor(galley.end());
                assert_arrow_walk_stays_clamped(&galley, text, direction);
                assert_caret_geometry_stays_in_range(&galley, text, direction);
            }
        }

        // Two-section wrapped RTL jobs (both section orders, mixed with a
        // pure-LTR section) through the same wrap + elision gauntlet:
        let formats = [
            TextFormat::simple(FontId::proportional(14.0), Color32::RED),
            TextFormat::simple(FontId::proportional(14.0), Color32::WHITE),
        ];
        for section_texts in [
            ["تقرير ٢٠٢٦", " aaa bbb الأول ثاني"],
            ["abcd", " عربي aaa \u{2066}שלום\u{2069} pdf"],
        ] {
            let mut text = String::new();
            let mut sections = Vec::new();
            for (i, section_text) in section_texts.iter().enumerate() {
                let start = ByteIndex(text.len());
                text.push_str(section_text);
                sections.push(LayoutSection {
                    leading_space: 0.0,
                    byte_range: start..ByteIndex(text.len()),
                    format: formats[i].clone(),
                });
            }
            for direction in directions {
                let mut job = LayoutJob {
                    sections: sections.clone(),
                    text: text.clone(),
                    wrap: TextWrapping {
                        max_width: 100.0,
                        max_rows: 5,
                        break_anywhere: true,
                        ..Default::default()
                    },
                    break_on_newline: true,
                    ..Default::default()
                };
                job.direction = direction;
                job.round_output_to_gui = false;
                job.wrap.overflow_character = Some('…');
                let mut fonts = tofu_fonts(Arc::new(SimpleLineBreaker));
                let galley = layout(&mut fonts, 1.0, job.into());
                let _ = galley.end();
                let _ = galley.cursor_from_pos(vec2(0.0, 0.0));
                let _ = galley.pos_from_cursor(galley.end());
                assert_arrow_walk_stays_clamped(&galley, &text, direction);
                assert_caret_geometry_stays_in_range(&galley, &text, direction);
            }
        }
    }
}
