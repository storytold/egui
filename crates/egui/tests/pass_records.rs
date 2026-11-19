use std::sync::Arc;

use egui::{
    Color32, Context, Id, LayerId, Order, PassRecords, PassRecordsDiff, Rect, Role, Sense, Ui,
    WidgetInfo, epaint::PaintCallback, pos2, vec2,
};

/// The records of `part`, run in one pass of `ctx` after a label it does not include.
fn records(ctx: &Context, mut part: impl FnMut(&mut Ui)) -> PassRecords {
    let mut records = None;
    let output = ctx.run_ui(Default::default(), |ui| {
        ui.label("before");
        let mark = ui.ctx().pass_mark();
        part(ui);
        records = Some(ui.ctx().pass_records_since(&mark));
    });
    output.drop_without_applying_deltas();
    records.expect("the pass ran")
}

fn context() -> Context {
    let ctx = Context::default();
    ctx.enable_accesskit();
    ctx
}

/// A part with text, a widget and its node compares equal to itself in the next pass.
#[test]
fn identical_parts_compare_equal() {
    let ctx = context();
    let part = |ui: &mut Ui| {
        ui.label("text");
        let _ = ui.button("button");
    };
    let first = records(&ctx, part);
    let second = records(&ctx, part);
    assert!(
        !first.widgets().is_empty(),
        "the part placed widgets, so the comparison covers them"
    );
    let diff = first.first_difference(&second);
    assert!(diff.is_none(), "{}", diff.expect("a difference"));
}

/// Two callbacks of one type (a backend wraps every callback in its own type) and one rect: the records cannot tell
/// whether they paint the same, so they are a difference.
#[test]
fn a_paint_callback_cannot_be_compared() {
    struct Wrapper {
        _content: u32,
    }
    let ctx = context();
    let callback = |content: u32| {
        move |ui: &mut Ui| {
            ui.label("text");
            ui.painter().add(PaintCallback {
                rect: Rect::from_min_size(pos2(10.0, 10.0), vec2(20.0, 20.0)),
                callback: Arc::new(Wrapper { _content: content }),
            });
        }
    };
    let first = records(&ctx, callback(1));
    let second = records(&ctx, callback(2));
    let diff = first.first_difference(&second);
    assert!(
        matches!(diff, Some(PassRecordsDiff::Callback { index: 1, .. })),
        "{diff:?}"
    );
}

/// Shapes on two layers of one order compare equal when a third layer of that order shows up between the passes.
#[test]
fn records_on_several_layers_keep_their_order() {
    let ctx = context();
    let layer = |name: &str| LayerId::new(Order::Foreground, Id::unique(name));
    let [one, two, third] = [layer("one"), layer("two"), layer("third")];
    let pass = |with_third: bool| {
        let mut records = None;
        let output = ctx.run_ui(Default::default(), |ui| {
            if with_third {
                ui.ctx().layer_painter(third).rect_filled(
                    Rect::from_min_size(pos2(0.0, 0.0), vec2(5.0, 5.0)),
                    0.0,
                    Color32::RED,
                );
            }
            let mark = ui.ctx().pass_mark();
            for (n, layer) in [one, two].into_iter().enumerate() {
                ui.ctx().layer_painter(layer).rect_filled(
                    Rect::from_min_size(pos2(n as f32 * 10.0, 0.0), vec2(5.0, 5.0)),
                    0.0,
                    Color32::BLUE,
                );
            }
            records = Some(ui.ctx().pass_records_since(&mark));
        });
        output.drop_without_applying_deltas();
        records.expect("the pass ran")
    };
    let first = pass(false);
    let second = pass(true);
    assert_eq!(first.layers().count(), 2, "the part painted on two layers");
    let diff = first.first_difference(&second);
    assert!(diff.is_none(), "{}", diff.expect("a difference"));
}

#[test]
fn a_changed_shape_is_the_first_difference() {
    let ctx = context();
    let first = records(&ctx, |ui| {
        ui.label("left");
    });
    let second = records(&ctx, |ui| {
        ui.label("right");
    });
    let diff = first.first_difference(&second);
    assert!(
        matches!(
            diff,
            Some(PassRecordsDiff::Shape {
                index: 0,
                left: Some(_),
                right: Some(_),
                ..
            })
        ),
        "{diff:?}"
    );
}

/// A widget that paints nothing and places the same rect, with another AccessKit label.
#[test]
fn a_changed_node_is_the_first_difference() {
    let ctx = context();
    let widget = |label: &'static str| {
        move |ui: &mut Ui| {
            let response = ui.allocate_response(vec2(20.0, 20.0), Sense::click());
            response.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
        }
    };
    let first = records(&ctx, widget("left"));
    let second = records(&ctx, widget("right"));
    let Some(PassRecordsDiff::Node {
        left: Some((left_id, left)),
        right: Some((right_id, right)),
        ..
    }) = first.first_difference(&second)
    else {
        panic!("{:?}", first.first_difference(&second));
    };
    assert_eq!(left_id, right_id, "the same widget's node");
    assert_ne!(left.label(), right.label());
}

/// A member node whose registered parent, created before the part, is another.
#[test]
fn a_changed_parent_is_the_first_difference() {
    let ctx = context();
    let member = Id::unique("member");
    let [one, other] = [Id::unique("one"), Id::unique("other")];
    let under = |parent: Id| {
        move |ui: &mut Ui| {
            ui.ctx().register_accesskit_parent(member, parent);
            ui.ctx()
                .accesskit_node_builder(member, |node| node.set_role(Role::RadioButton));
        }
    };
    let parents = |ctx: &Context, parent: Id| {
        let mut records = None;
        let output = ctx.run_ui(Default::default(), |ui| {
            for id in [one, other] {
                ui.ctx()
                    .accesskit_node_builder(id, |node| node.set_role(Role::Group));
            }
            let mark = ui.ctx().pass_mark();
            under(parent)(ui);
            records = Some(ui.ctx().pass_records_since(&mark));
        });
        output.drop_without_applying_deltas();
        records.expect("the pass ran")
    };
    let first = parents(&ctx, one);
    let second = parents(&ctx, other);
    let diff = first.first_difference(&second);
    assert!(
        matches!(
            diff,
            Some(PassRecordsDiff::Parent {
                left: Some((left_member, left_parent)),
                right: Some((right_member, right_parent)),
                ..
            }) if left_member == member && right_member == member && left_parent == one && right_parent == other
        ),
        "{diff:?}"
    );
}
