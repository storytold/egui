use egui::{
    Context, Event, Id, Modifiers, PointerButton, RawInput, Rect, Sense, SurrenderFocusOn, pos2,
    vec2,
};

const FIELD: Rect = Rect::from_min_max(pos2(10.0, 10.0), pos2(110.0, 40.0));

/// Runs one pass with `events`, showing a focusable widget `id` at `FIELD`; `before` runs first.
fn pass(ctx: &Context, id: Id, events: Vec<Event>, before: impl Fn(&Context)) {
    let mut output = ctx.run_ui(
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0))),
            events,
            ..Default::default()
        },
        |ui| {
            before(ui.ctx());
            ui.interact(FIELD, id, Sense::click());
        },
    );
    output.textures_delta.clear();
}

/// Whether `id`, focused (with its own rule `own`, if any), keeps focus through a click
/// outside it.
fn keeps_focus_through_a_click_elsewhere(own: Option<SurrenderFocusOn>) -> bool {
    let ctx = Context::default();
    let id = Id::unique("field");
    pass(&ctx, id, Vec::new(), |_| {});
    pass(&ctx, id, Vec::new(), |ctx| {
        ctx.memory_mut(|memory| {
            memory.request_focus(id);
            if let Some(own) = own {
                memory.set_focus_surrender(id, own);
            }
        });
    });
    let outside = pos2(300.0, 200.0);
    let button = |pressed| Event::PointerButton {
        pos: outside,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    pass(&ctx, id, vec![Event::PointerMoved(outside)], |_| {});
    pass(&ctx, id, vec![button(true)], |_| {});
    pass(&ctx, id, vec![button(false)], |_| {});
    pass(&ctx, id, Vec::new(), |_| {});
    ctx.memory(|memory| memory.has_focus(id))
}

#[test]
fn a_focused_widget_s_own_surrender_rule_wins_over_the_option() {
    assert_eq!(
        Context::default().options(|options| options.input_options.surrender_focus_on),
        SurrenderFocusOn::Clicks
    );
    assert!(
        !keeps_focus_through_a_click_elsewhere(None),
        "the option surrenders on a click elsewhere"
    );
    assert!(
        keeps_focus_through_a_click_elsewhere(Some(SurrenderFocusOn::Never)),
        "the widget's own rule keeps it"
    );
}
