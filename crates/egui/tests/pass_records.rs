use std::sync::Arc;

use egui::{
    Context, Id, PassRecords, PassRecordsDiff, Rect, Role, Sense, Ui, WidgetInfo,
    epaint::PaintCallback, pos2, vec2,
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

/// A part with text, a widget and its node, and a paint callback (a new one each run) compares equal to itself in
/// the next pass.
#[test]
fn identical_parts_compare_equal() {
    let ctx = context();
    let part = |ui: &mut Ui| {
        ui.label("text");
        let _ = ui.button("button");
        ui.painter().add(PaintCallback {
            rect: Rect::from_min_size(pos2(10.0, 10.0), vec2(20.0, 20.0)),
            callback: Arc::new(7_u32),
        });
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
