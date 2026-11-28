use egui::{Context, Id, Role};

/// A member with no widget gets its node under its parent `Ui`'s node.
#[test]
fn registered_accesskit_parent_holds_the_member_node() {
    let ctx = Context::default();
    ctx.enable_accesskit();

    let member = Id::unique("member");
    let mut parent = None;
    let output = ctx.run_ui(Default::default(), |ui| {
        ui.scope(|ui| {
            let parent_id = ui.unique_id();
            parent = Some(parent_id);
            ui.ctx().accesskit_node_builder(parent_id, |node| {
                node.set_role(Role::Group);
            });
            ui.ctx().register_accesskit_parent(member, parent_id);
            ui.ctx().accesskit_node_builder(member, |node| {
                node.set_role(Role::RadioButton);
            });
        });
    });
    let update = output
        .platform_output
        .accesskit_update
        .clone()
        .expect("accesskit update");
    output.drop_without_applying_deltas();

    let parent_node = update
        .nodes
        .iter()
        .find(|(id, _)| *id == parent.expect("parent").accesskit_id())
        .map(|(_, node)| node)
        .expect("parent node");
    assert!(
        parent_node.children().contains(&member.accesskit_id()),
        "member node is a child of the parent node"
    );
}
