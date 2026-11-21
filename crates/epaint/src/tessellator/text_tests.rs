//! Font-independent regression fixtures for text vertex transforms and color glyphs.

use alloc::sync::Arc;

use super::*;
use crate::text::{FallbackAlign, Galley, Glyph, LayoutJob, PlacedRow, Row, RowVisuals};

const BACKGROUND: Color32 = Color32::from_rgba_premultiplied(60, 20, 10, 128);
const DECORATION: Color32 = Color32::from_rgba_premultiplied(20, 100, 40, 150);
const FALLBACK: Color32 = Color32::from_rgba_premultiplied(30, 60, 90, 100);
const OVERRIDE: Color32 = Color32::from_rgba_premultiplied(20, 40, 60, 80);

fn glyph(first_vertex: u32, color: bool, visible: bool) -> Glyph {
    let mut glyph = Glyph {
        chr: if visible { 'a' } else { ' ' },
        pos: Pos2::ZERO,
        advance_width: 4.0,
        line_height: 8.0,
        font_ascent: 6.0,
        font_height: 8.0,
        font_face_ascent: 6.0,
        font_face_height: 8.0,
        font_face_align: FallbackAlign::default(),
        uv_rect: Default::default(),
        is_color: color,
        bidi_level: 0,
        section_index: 0,
        first_vertex,
    };
    if visible {
        glyph.uv_rect.max = [4, 8];
        glyph.uv_rect.size = vec2(4.0, 8.0);
    }
    glyph
}

fn fixture(color: bool, invisible_color: bool) -> TextShape {
    let mut mesh = Mesh::default();
    for (quad, tint) in [
        BACKGROUND,
        Color32::PLACEHOLDER,
        Color32::PLACEHOLDER,
        DECORATION,
    ]
    .into_iter()
    .enumerate()
    {
        let x = quad as f32 * 4.0;
        mesh.add_rect_with_uv(
            Rect::from_min_size(pos2(x, 0.0), vec2(4.0, 8.0)),
            Rect::from_min_max(pos2(2.0, 4.0), pos2(6.0, 12.0)),
            tint,
        );
    }
    let bounds = mesh.calc_bounds();
    let row = Row {
        section_index_at_start: 0,
        // The invisible glyph shares its first vertex with the next visible glyph.
        // The trailing invisible glyph shares the decoration's first vertex.
        glyphs: vec![
            glyph(4, false, true),
            glyph(8, invisible_color, false),
            glyph(8, color, true),
            glyph(12, invisible_color, false),
        ],
        rtl_paragraph: false,
        visual_order: vec![],
        size: vec2(16.0, 8.0),
        visuals: RowVisuals {
            mesh,
            mesh_bounds: bounds,
            glyph_index_start: 6,
            glyph_vertex_range: 4..12,
        },
    };
    let galley = Galley {
        job: Arc::new(LayoutJob::simple(
            "a a ".to_owned(),
            Default::default(),
            Color32::PLACEHOLDER,
            f32::INFINITY,
        )),
        rows: vec![PlacedRow {
            pos: pos2(2.0, 3.0),
            row: Arc::new(row),
            ends_with_newline: false,
        }],
        elided: false,
        rect: bounds,
        mesh_bounds: bounds.translate(vec2(2.0, 3.0)),
        num_vertices: 16,
        num_indices: 24,
        pixels_per_point: 2.0,
        intrinsic_size: bounds.size(),
    };
    TextShape::new(pos2(10.26, 20.26), Arc::new(galley), FALLBACK)
}

fn tessellator() -> Tessellator {
    Tessellator::new(2.0, TessellationOptions::default(), [16, 32], vec![])
}

#[test]
fn text_colors_preserve_background_decoration_and_color_glyph_alpha() {
    for color in [false, true] {
        for invisible_color in [false, true] {
            for override_color in [None, Some(OVERRIDE)] {
                for opacity in [1.0, 0.5] {
                    let mut shape = fixture(color, invisible_color);
                    shape.override_text_color = override_color;
                    shape.opacity_factor = opacity;
                    let mut output = Mesh::default();
                    tessellator().tessellate_text(&shape, &mut output);
                    let text = override_color.unwrap_or(FALLBACK);
                    let colored = if color {
                        Color32::from_white_alpha(text.a())
                    } else {
                        text
                    };
                    let expected = [BACKGROUND, text, colored, DECORATION];
                    assert_eq!(output.vertices.len(), 16);
                    assert_eq!(output.indices.len(), 24);
                    for (quad, expected_color) in
                        core::iter::zip(output.vertices.chunks_exact(4), expected)
                    {
                        for vertex in quad {
                            assert_eq!(
                                vertex.color,
                                expected_color.gamma_multiply(opacity),
                                "color={color}, invisible_color={invisible_color}, override={override_color:?}, opacity={opacity}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn final_color_glyph_does_not_tint_following_decoration_vertices() {
    let mut shape = fixture(true, false);
    let galley = Arc::make_mut(&mut shape.galley);
    // A final visible color glyph has no following glyph to terminate its vertex span.
    // The row's glyph range must prevent its color treatment spilling into decoration.
    Arc::make_mut(&mut galley.rows[0].row).glyphs.pop();
    let mut output = Mesh::default();
    tessellator().tessellate_text(&shape, &mut output);
    assert_eq!(output.vertices.len(), 16);
    for vertex in &output.vertices[8..12] {
        assert_eq!(vertex.color, Color32::from_white_alpha(FALLBACK.a()));
    }
    for vertex in &output.vertices[12..16] {
        assert_eq!(vertex.color, DECORATION);
    }
}

#[test]
fn text_vertices_keep_position_uv_and_index_offsets_on_both_paths() {
    for color in [false, true] {
        for angle in [0.0, core::f32::consts::FRAC_PI_2] {
            let mut shape = fixture(color, false);
            shape.angle = angle;
            let mut output = Mesh::default();
            output.add_colored_rect(
                Rect::from_min_size(Pos2::ZERO, Vec2::splat(1.0)),
                Color32::GREEN,
            );
            let prefix = output.clone();
            tessellator().tessellate_text(&shape, &mut output);
            assert_eq!(&output.vertices[..4], prefix.vertices.as_slice());
            assert_eq!(&output.indices[..6], prefix.indices.as_slice());
            let input = &shape.galley.rows[0].visuals.mesh;
            assert_eq!(output.vertices.len(), 4 + input.vertices.len());
            let expected_indices: Vec<_> = input.indices.iter().map(|index| index + 4).collect();
            assert_eq!(&output.indices[6..], expected_indices.as_slice());
            for (actual, original) in core::iter::zip(output.vertices[4..].iter(), &input.vertices)
            {
                let offset = original.pos + vec2(2.0, 3.0);
                let expected_position = if angle == 0.0 {
                    pos2(10.5 + offset.x, 20.5 + offset.y)
                } else {
                    pos2(10.5 - offset.y, 20.5 + offset.x)
                };
                assert!(actual.pos.distance(expected_position) < 0.00001);
                assert_eq!(actual.uv, pos2(original.uv.x / 16.0, original.uv.y / 32.0));
            }
        }
    }
}

#[test]
fn invisible_or_culled_text_does_not_append_vertices() {
    for color in [false, true] {
        let mut shape = fixture(color, false);
        let mut output = Mesh::default();
        shape.opacity_factor = 0.0;
        tessellator().tessellate_text(&shape, &mut output);
        assert!(output.is_empty());
        shape.opacity_factor = 1.0;
        let mut tess = tessellator();
        tess.set_clip_rect(Rect::from_min_size(pos2(1000.0, 1000.0), Vec2::splat(2.0)));
        tess.tessellate_text(&shape, &mut output);
        assert!(output.is_empty());
    }
}
