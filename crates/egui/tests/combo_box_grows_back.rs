use egui::{ComboBox, Context, Event, Modifiers, PointerButton, RawInput, Rect, pos2, vec2};

/// Runs one frame with a combo box `top` points down a 400×400 screen whose menu lists `rows`
/// rows. Returns the button's rect and the open menu's rect.
fn frame(ctx: &Context, top: f32, rows: usize, events: Vec<Event>) -> (Rect, Option<Rect>) {
    let mut button = Rect::NOTHING;
    let mut menu_layer = None;
    let mut output = ctx.run_ui(
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 400.0))),
            events,
            ..Default::default()
        },
        |ui| {
            ui.add_space(top);
            let response = ComboBox::from_id_salt("combo")
                .selected_text("Row")
                .show_ui(ui, |ui| {
                    for row in 0..rows {
                        ui.label(format!("Row {row}"));
                    }
                    ui.layer_id()
                });
            button = response.response.rect;
            menu_layer = response.inner;
        },
    );
    output.textures_delta.clear();
    let menu = menu_layer.and_then(|layer| ctx.memory(|m| m.area_rect(layer.id)));
    (button, menu)
}

/// Opens the combo box with a click and returns the frame function's results after `rows`
/// change to each of `steps`, checked on every frame by `each`.
fn open_and_resize(top: f32, steps: &[usize], each: impl Fn(Rect, Rect)) -> Vec<Rect> {
    let ctx = Context::default();
    let (button, _) = frame(&ctx, top, steps[0], Vec::new());
    let click = |pressed| Event::PointerButton {
        pos: button.center(),
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    frame(
        &ctx,
        top,
        steps[0],
        vec![Event::PointerMoved(button.center())],
    );
    frame(&ctx, top, steps[0], vec![click(true)]);
    frame(&ctx, top, steps[0], vec![click(false)]);
    // Opening runs an invisible sizing pass first.
    frame(&ctx, top, steps[0], Vec::new());
    steps
        .iter()
        .map(|&rows| {
            let mut menu = Rect::NOTHING;
            for _ in 0..3 {
                let (button, shown) = frame(&ctx, top, rows, Vec::new());
                menu = shown.expect("the menu is open");
                each(button, menu);
            }
            menu
        })
        .collect()
}

#[test]
fn open_menu_grows_back_after_its_contents_shrank() {
    let menus = open_and_resize(10.0, &[40, 2, 40], |button, menu| {
        assert!(menu.top() >= button.bottom(), "menu opens below its button");
    });
    let (tall, short, regrown) = (menus[0].height(), menus[1].height(), menus[2].height());
    assert!(
        short < tall / 2.0,
        "fewer rows shrink the menu: {short} vs {tall}"
    );
    assert!(
        (regrown - tall).abs() < 0.5,
        "more rows grow it back: {regrown} vs {tall}"
    );
}

/// Room for the long menu only above the button, for the short one on both sides: the menu
/// stays above on every frame while it shrinks and grows.
#[test]
fn open_menu_keeps_its_side_while_it_shrinks_and_grows() {
    let menus = open_and_resize(230.0, &[40, 2, 40], |button, menu| {
        assert!(
            menu.bottom() <= button.top(),
            "menu stays above its button: {menu:?} vs {button:?}"
        );
    });
    assert!(menus[1].height() < menus[0].height() / 2.0);
    assert!((menus[2].height() - menus[0].height()).abs() < 0.5);
}
