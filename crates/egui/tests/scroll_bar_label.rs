use egui::{Context, Role, ScrollArea, containers::scroll_area::ScrollBarVisibility};

/// Scroll bars get the accessible names the app gives them, per axis.
#[test]
fn scroll_bar_nodes_carry_their_labels() {
    let ctx = Context::default();
    ctx.enable_accesskit();

    let mut update = None;
    for _ in 0..2 {
        let output = ctx.run_ui(Default::default(), |ui| {
            ScrollArea::both()
                .max_height(50.0)
                .max_width(50.0)
                .scroll_bar_visibility(ScrollBarVisibility::AlwaysVisible)
                .horizontal_scroll_bar_label("Horizontal bar")
                .vertical_scroll_bar_label("Vertical bar")
                .show(ui, |ui| ui.allocate_space(egui::vec2(500.0, 500.0)));
        });
        update = output.platform_output.accesskit_update.clone();
        output.drop_without_applying_deltas();
    }

    let mut labels: Vec<String> = update
        .expect("accesskit update")
        .nodes
        .iter()
        .filter(|(_, node)| node.role() == Role::ScrollBar)
        .filter_map(|(_, node)| node.label().map(str::to_owned))
        .collect();
    labels.sort();
    assert_eq!(labels, ["Horizontal bar", "Vertical bar"]);
}
