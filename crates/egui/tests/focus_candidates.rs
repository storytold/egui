//! The widgets interested in keyboard focus in a pass, in egui's Tab order, and the focused widget's event filter.

use egui::{Button, Context, DragValue, FocusCandidate, Id, RawInput, TextEdit, Ui};

/// One pass of `ctx` showing `ui`, its texture delta dropped: no renderer here.
fn run(ctx: &Context, ui: impl FnMut(&mut Ui)) {
    ctx.run_ui(RawInput::default(), ui).textures_delta.clear();
}

#[test]
fn a_pass_lists_its_focus_candidates_in_registration_order() {
    let ctx = Context::default();
    let (button, drag, field) = (Id::new("button"), Id::new("drag"), Id::new("field"));
    let mut value = 1.0_f32;
    let mut text = String::new();
    let mut candidates = Vec::new();
    run(&ctx, |ui| {
        ui.push_id(button, |ui| ui.add(Button::new("Button")));
        ui.add_enabled(false, Button::new("Disabled"));
        ui.push_id(drag, |ui| ui.add(DragValue::new(&mut value)));
        ui.add(TextEdit::singleline(&mut text).id(field));
        candidates = ui.memory(|mem| mem.focus_candidates().to_vec());
    });
    let text_flags: Vec<bool> = candidates.iter().map(|c| c.text).collect();
    assert_eq!(
        text_flags,
        [false, false, true],
        "the button, the drag value once and the text edit, which alone is text; the disabled button is none: {candidates:?}"
    );
    assert_eq!(
        candidates.last().map(|c| c.id),
        Some(field),
        "the text edit comes last"
    );

    run(&ctx, |ui| {
        candidates = ui.memory(|mem| mem.focus_candidates().to_vec());
    });
    assert_eq!(
        candidates,
        Vec::<FocusCandidate>::new(),
        "the next pass starts with none"
    );
}

#[test]
fn a_focused_multiline_text_edit_that_locks_focus_keeps_tab_in_its_filter() {
    let ctx = Context::default();
    let field = Id::new("code");
    let mut text = String::new();
    let mut show = |ctx: &Context| {
        run(&ctx, |ui| {
            ui.add(TextEdit::multiline(&mut text).id(field).lock_focus(true));
        });
    };
    show(&ctx);
    assert!(ctx.memory(|mem| mem.focus_lock_filter()).is_none());
    ctx.memory_mut(|mem| mem.request_focus(field));
    // The text edit sets its filter once it has held focus for a pass.
    show(&ctx);
    show(&ctx);
    assert_eq!(
        ctx.memory(|mem| mem.focus_lock_filter())
            .map(|filter| filter.tab),
        Some(true)
    );
}
