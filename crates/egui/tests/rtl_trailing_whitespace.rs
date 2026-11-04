//! A label ending in a space keeps the gap before the next widget at zero item
//! spacing in both reading directions: the trailing whitespace is part of the
//! label's allocated width under right-to-left layout just like under
//! left-to-right, so `label + link` rows don't collapse the separator.

use egui::epaint::Shape;
use egui::{Context, FullOutput, Label, Pos2, RawInput, Rect, Ui, Vec2, vec2};

fn screen() -> Vec2 {
    vec2(800.0, 600.0)
}

fn input() -> RawInput {
    RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen())),
        ..Default::default()
    }
}

/// The bounding rect of the painted non-whitespace glyphs of a text shape.
fn shape_ink_rect(text_shape: &egui::epaint::TextShape) -> Rect {
    let mut ink = Rect::NOTHING;
    for row in &text_shape.galley.rows {
        let row_pos = text_shape.pos.to_vec2() + row.pos.to_vec2();
        for glyph in &row.glyphs {
            if glyph.chr.is_whitespace() {
                continue;
            }
            ink = ink.union(glyph.logical_rect().translate(row_pos));
        }
    }
    ink
}

/// The bounding rect of the painted non-whitespace glyphs of the text shape
/// with the given job text.
fn text_ink_rect(output: &FullOutput, text: &str) -> Option<Rect> {
    fn collect(shape: &Shape, text: &str, out: &mut Option<Rect>) {
        match shape {
            Shape::Text(text_shape) if text_shape.galley.job.text == text => {
                *out = Some(shape_ink_rect(text_shape));
            }
            Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, text, out);
                }
            }
            _ => {}
        }
    }
    let mut ink = None;
    for clipped in &output.shapes {
        collect(&clipped.shape, text, &mut ink);
        if ink.is_some() {
            break;
        }
    }
    ink
}

/// Two labels side by side at zero item spacing, the first ending in a space.
/// Returns (first label's painted ink rect, second label's widget rect).
fn adjacent_rects(rtl: bool) -> (Rect, Rect) {
    let ctx = Context::default();
    if rtl {
        ctx.set_right_to_left(true);
    }
    let first_text = if rtl {
        "يعمل egui داخل "
    } else {
        "egui works "
    };
    let mut ink = Rect::NOTHING;
    let mut second = Rect::NOTHING;
    // Two passes: the first sizes, the second settles the geometry.
    for pass in 0..2 {
        let mut output = ctx.run_ui(input(), |ui: &mut Ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add(Label::new(first_text));
                second = ui.add(Label::new("second")).rect;
            });
        });
        output.textures_delta.clear();
        if pass == 1 {
            ink = text_ink_rect(&output, first_text).expect("the first label must paint text");
        }
    }
    (ink, second)
}

#[test]
fn trailing_space_keeps_the_gap_before_the_next_widget_in_both_directions() {
    let (ltr_ink, ltr_second) = adjacent_rects(false);
    let (rtl_ink, rtl_second) = adjacent_rects(true);

    // Zero item spacing: the second widget touches the first label's rect, but
    // its ink must keep a whitespace-wide gap in both directions.
    let ltr_gap = ltr_second.left() - ltr_ink.right();
    let rtl_gap = rtl_ink.left() - rtl_second.right();

    assert!(ltr_gap > 1.0, "LTR gap {ltr_gap}");
    assert!(
        (rtl_gap - ltr_gap).abs() <= 1.0,
        "RTL gap {rtl_gap} vs LTR gap {ltr_gap}"
    );
}
