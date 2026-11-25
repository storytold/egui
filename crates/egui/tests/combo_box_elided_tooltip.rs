use egui::{ComboBox, Context, Event, RawInput, Rect, Shape, TextWrapMode, pos2, vec2};

const LONG: &str = "A selected text much too long for the button of a narrow combo box";

/// Hovers a combo box showing [`LONG`] in room `width` wide, and returns whether the full text
/// is painted outside of its button.
fn full_text_shown_on_hover(
    width: f32,
    labeled: bool,
    build: impl Fn(ComboBox<'static>) -> ComboBox<'static>,
) -> bool {
    let ctx = Context::default();
    ctx.all_styles_mut(|style| {
        style.interaction.tooltip_delay = 0.0;
        style.wrap_mode = Some(TextWrapMode::Truncate);
    });
    let frame = |events| {
        let mut button = Rect::NOTHING;
        let mut output = ctx.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 400.0))),
                events,
                ..Default::default()
            },
            |ui| {
                ui.set_max_width(width);
                let combo = if labeled {
                    ComboBox::new("combo", "Label")
                } else {
                    ComboBox::from_id_salt("combo")
                };
                button = build(combo.selected_text(LONG).width(100.0))
                    .show_ui(ui, |_ui| ())
                    .response
                    .rect;
            },
        );
        output.textures_delta.clear();
        (button, output.shapes)
    };
    let (button, _) = frame(Vec::new());
    let over = pos2(button.left() + 10.0, button.center().y);
    frame(vec![Event::PointerMoved(over)]);
    for _ in 0..3 {
        frame(Vec::new());
    }
    let (_, shapes) = frame(Vec::new());
    shapes.iter().any(|clipped| match &clipped.shape {
        Shape::Text(text) => {
            text.galley.text() == LONG
                && !text.galley.elided
                && !button.contains(text.visual_bounding_rect().center())
        }
        _ => false,
    })
}

#[test]
fn selected_text_cut_short_shows_in_full_on_hover() {
    for labeled in [false, true] {
        assert!(
            full_text_shown_on_hover(200.0, labeled, |combo| combo),
            "cut short, labeled: {labeled}"
        );
        assert!(
            !full_text_shown_on_hover(800.0, labeled, |combo| combo),
            "not cut short, labeled: {labeled}"
        );
    }
}

#[test]
fn tooltip_for_a_cut_selected_text_can_be_turned_off() {
    assert!(!full_text_shown_on_hover(200.0, false, |combo| combo.show_tooltip_when_elided(false)));
}
