use egui::{Context, Sense};

/// The description and name `describe_for_accessibility` leaves on a button that was given
/// `name` (if any) before the call.
fn describe(name: Option<&str>, text: &str) -> Option<(Option<String>, Option<String>)> {
    let ctx = Context::default();
    ctx.enable_accesskit();

    let mut node = None;
    let output = ctx.run_ui(Default::default(), |ui| {
        let (_, response) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), Sense::click());
        // The widget has a node once it has been given widget info.
        response.widget_info(|| match name {
            Some(name) => egui::WidgetInfo::labeled(egui::Role::Button, true, name),
            None => egui::WidgetInfo::new(egui::Role::Button),
        });
        response.describe_for_accessibility(text);
        node = ctx.accesskit_node_builder(response.id, |n| {
            (
                n.description().map(str::to_owned),
                n.label().map(str::to_owned),
            )
        });
    });
    output.drop_without_applying_deltas();
    node
}

/// An unnamed widget (an icon-only button) is named by the text, which is not repeated as its
/// description, so a screen reader says "Save the file, button" once.
#[test]
fn describe_for_accessibility_names_an_unnamed_widget_once() {
    assert_eq!(
        describe(None, "Save the file"),
        Some((None, Some("Save the file".to_owned())))
    );
}

/// A named widget is described by a text that says more than its name.
#[test]
fn describe_for_accessibility_describes_a_named_widget() {
    assert_eq!(
        describe(Some("Save"), "Save the file"),
        Some((Some("Save the file".to_owned()), Some("Save".to_owned())))
    );
}

/// A text that is already the widget's name is not repeated as its description.
#[test]
fn describe_for_accessibility_does_not_repeat_the_name() {
    assert_eq!(
        describe(Some("Rewind"), "Rewind"),
        Some((None, Some("Rewind".to_owned())))
    );
}
