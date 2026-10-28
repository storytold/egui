use egui::{Context, Sense};

/// A tooltip with other contents can give its widget the description and name
/// that `on_hover_text` sets, from outside the crate.
#[test]
fn describe_for_accessibility_sets_description_and_name() {
    let ctx = Context::default();
    ctx.enable_accesskit();

    let mut node = None;
    let output = ctx.run_ui(Default::default(), |ui| {
        let (_, response) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), Sense::click());
        // The widget has a node once it has been given widget info.
        response.widget_info(|| egui::WidgetInfo::new(egui::Role::Button));
        response.describe_for_accessibility("Save the file");
        node = ctx.accesskit_node_builder(response.id, |n| {
            (n.description().map(str::to_owned), n.label().map(str::to_owned))
        });
    });
    output.drop_without_applying_deltas();

    assert_eq!(
        node,
        Some((Some("Save the file".to_owned()), Some("Save the file".to_owned())))
    );
}
