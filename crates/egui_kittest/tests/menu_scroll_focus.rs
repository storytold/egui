//! Scrollable menus reveal newly focused entries without pinning the scroll position to focus.
//! Exercise keyboard navigation, nested menus and custom controls with actual paint clipping.

use egui::containers::menu::{MenuButton, MenuConfig};
use egui::{Key, Rect, ScrollArea, Shape, Ui, vec2};
use egui_kittest::Harness;
use kittest::Queryable as _;

#[derive(Default)]
struct State {
    picked: Option<String>,
    checked: bool,
    parent_center: egui::Pos2,
    parent_offset: f32,
    child_offset: f32,
}

fn entries(ui: &mut Ui, state: &mut State, prefix: &str) {
    for index in 0..20 {
        let label = format!("{prefix} {index:02}");
        // All kinds must participate, not just Button's paint implementation.
        let response = match index % 3 {
            0 => ui.button(&label),
            1 => ui.checkbox(&mut state.checked, &label),
            _ => {
                let (rect, response) =
                    ui.allocate_exact_size(vec2(120.0, 20.0), egui::Sense::click());
                response
                    .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &label));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    &label,
                    egui::FontId::proportional(14.0),
                    ui.visuals().text_color(),
                );
                response
            }
        };
        if response.clicked() {
            state.picked = Some(label);
            ui.close();
        }
    }
}

fn menu(context_menu: bool, nested: bool, scale: f32) -> Harness<'static, State> {
    Harness::builder()
        .with_size(vec2(600.0, 400.0))
        .with_pixels_per_point(scale)
        .build_ui_state(
            move |ui, state: &mut State| {
                let content = |ui: &mut Ui| {
                    let output = ScrollArea::vertical().max_height(100.0).show(ui, |ui| {
                        entries(ui, state, "Item");
                        if nested {
                            ui.menu_button("Load", |ui| {
                                let output =
                                    ScrollArea::vertical().max_height(100.0).show(ui, |ui| {
                                        entries(ui, state, "Child");
                                    });
                                state.child_offset = output.state.clamped_offset().y;
                            });
                        }
                    });
                    state.parent_center = output.inner_rect.center();
                    state.parent_offset = output.state.clamped_offset().y;
                };
                if context_menu {
                    ui.button("Menu").context_menu(content);
                } else {
                    MenuButton::new("Menu")
                        .config(
                            MenuConfig::new()
                                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                        )
                        .ui(ui, content);
                }
            },
            State::default(),
        )
}

fn press(h: &mut Harness<'_, State>, key: Key) {
    h.key_press(key);
    h.run();
}

fn open(h: &mut Harness<'_, State>, context_menu: bool) {
    h.get_by_label("Menu").focus();
    h.run();
    if context_menu {
        h.get_by_label("Menu").click_secondary();
        h.run();
    } else {
        press(h, Key::Enter);
    }
}

fn painted(shape: &Shape, clip: Rect, label: &str) -> bool {
    match shape {
        Shape::Text(text) => {
            text.galley.text() == label && clip.contains_rect(text.visual_bounding_rect())
        }
        Shape::Vec(shapes) => shapes.iter().any(|shape| painted(shape, clip, label)),
        _ => false,
    }
}

fn visible(h: &Harness<'_, State>, label: &str) -> bool {
    h.output()
        .shapes
        .iter()
        .any(|shape| painted(&shape.shape, shape.clip_rect, label))
}

fn assert_visible_focus(h: &Harness<'_, State>, label: &str) {
    assert!(
        h.get_by_label(label).is_focused(),
        "{label} must have focus"
    );
    assert!(
        visible(h, label),
        "{label} must be painted inside its clip, not just present in AccessKit"
    );
}

#[test]
fn arrows_reveal_entries_and_wrap_in_both_directions() {
    for context_menu in [false, true] {
        for scale in [1.0, 2.0] {
            let mut h = menu(context_menu, false, scale);
            open(&mut h, context_menu);
            for index in 0..20 {
                press(&mut h, Key::ArrowDown);
                assert_visible_focus(&h, &format!("Item {index:02}"));
            }
            press(&mut h, Key::ArrowDown);
            assert_visible_focus(&h, "Item 00");
            press(&mut h, Key::ArrowUp);
            assert_visible_focus(&h, "Item 19");
            for index in (0..19).rev() {
                press(&mut h, Key::ArrowUp);
                assert_visible_focus(&h, &format!("Item {index:02}"));
            }
            press(&mut h, Key::ArrowUp);
            assert_visible_focus(&h, "Item 19");
            press(&mut h, Key::Enter);
            assert_eq!(h.state().picked.as_deref(), Some("Item 19"));
            assert!(
                h.query_by_label("Item 00").is_none(),
                "activation closes the menu"
            );
        }
    }
}

#[test]
fn tab_navigation_reveals_entries_without_opening_submenus() {
    let mut h = menu(false, true, 1.0);
    open(&mut h, false);
    press(&mut h, Key::ArrowDown);
    for index in 1..20 {
        press(&mut h, Key::Tab);
        assert_visible_focus(&h, &format!("Item {index:02}"));
    }
    press(&mut h, Key::Tab);
    assert_visible_focus(&h, "Load");
    assert!(
        h.query_by_label("Child 00").is_none(),
        "Tab does not open a submenu"
    );
    for index in (0..20).rev() {
        h.key_press_modifiers(egui::Modifiers::SHIFT, Key::Tab);
        h.run();
        assert_visible_focus(&h, &format!("Item {index:02}"));
    }
}

#[test]
fn focus_outside_menus_does_not_implicitly_scroll() {
    let mut h = Harness::new_ui_state(
        |ui, state: &mut State| {
            ScrollArea::vertical()
                .max_height(100.0)
                .show(ui, |ui| entries(ui, state, "Item"));
        },
        State::default(),
    );
    h.get_by_label("Item 19").focus();
    h.run();
    assert!(h.get_by_label("Item 19").is_focused());
    assert!(
        !visible(&h, "Item 19"),
        "ordinary scroll areas retain their existing focus policy"
    );
}

#[test]
fn manual_scroll_is_not_overridden_by_unchanged_focus() {
    let mut h = menu(false, false, 1.0);
    open(&mut h, false);
    press(&mut h, Key::ArrowUp);
    assert_visible_focus(&h, "Item 19");
    h.hover_at(h.state().parent_center);
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: vec2(0.0, 2000.0),
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Unknown,
        modifiers: egui::Modifiers::NONE,
    });
    h.run_steps(30);
    assert!(
        h.get_by_label("Item 19").is_focused(),
        "scrolling does not change focus"
    );
    assert!(
        visible(&h, "Item 00"),
        "the wheel can reveal the top while focus stays at the bottom"
    );
    assert!(
        !visible(&h, "Item 19"),
        "focus must not pull the wheel position back"
    );
    h.run_steps(10);
    assert!(
        visible(&h, "Item 00"),
        "idle frames must not pull back either"
    );
    press(&mut h, Key::ArrowUp);
    assert_visible_focus(&h, "Item 18");
}

#[test]
fn nested_navigation_scrolls_only_the_owning_menu() {
    let mut h = menu(false, true, 1.0);
    open(&mut h, false);
    press(&mut h, Key::ArrowUp);
    assert_visible_focus(&h, "Load");
    let parent_offset = h.state().parent_offset;
    press(&mut h, Key::ArrowRight);
    assert_visible_focus(&h, "Child 00");
    press(&mut h, Key::ArrowUp);
    assert_visible_focus(&h, "Child 19");
    assert!(h.state().child_offset > 0.0, "the child scrolls");
    assert_eq!(
        h.state().parent_offset,
        parent_offset,
        "the parent does not scroll with the child"
    );
    press(&mut h, Key::ArrowLeft);
    assert_visible_focus(&h, "Load");
    assert!(
        h.query_by_label("Child 00").is_none(),
        "Left closes only the child"
    );
    press(&mut h, Key::ArrowRight);
    assert_visible_focus(&h, "Child 00");
    press(&mut h, Key::ArrowUp);
    assert_visible_focus(&h, "Child 19");
    press(&mut h, Key::Enter);
    assert_eq!(h.state().picked.as_deref(), Some("Child 19"));
    assert!(
        h.query_by_label("Item 00").is_none(),
        "selection closes both menus"
    );
}

#[test]
fn an_unscrolled_menu_does_not_scroll_its_background() {
    let mut h = Harness::builder()
        .with_size(vec2(600.0, 600.0))
        .build_ui_state(
            |ui, state: &mut State| {
                let output = ScrollArea::vertical().max_height(100.0).show(ui, |ui| {
                    ui.menu_button("Menu", |ui| entries(ui, state, "Item"));
                    for index in 0..50 {
                        ui.label(format!("Background {index}"));
                    }
                });
                state.parent_offset = output.state.clamped_offset().y;
            },
            State::default(),
        );
    open(&mut h, false);
    press(&mut h, Key::ArrowUp);
    assert_visible_focus(&h, "Item 19");
    assert_eq!(
        h.state().parent_offset,
        0.0,
        "a popup without its own scroller must not target the background"
    );
}
