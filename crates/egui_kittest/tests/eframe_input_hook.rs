#![cfg(feature = "eframe")]

use egui_kittest::Harness;

/// Counts its `raw_input_hook` calls and adds a key press to each pass's input.
#[derive(Default)]
struct HookApp {
    hooked: usize,
    seen: usize,
}

impl eframe::App for HookApp {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.hooked += 1;
        raw_input.events.push(egui::Event::Key {
            key: egui::Key::F9,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if ui.input(|input| input.key_pressed(egui::Key::F9)) {
            self.seen += 1;
        }
    }
}

#[test]
fn the_harness_runs_raw_input_hook_before_every_pass() {
    let mut harness = Harness::builder().build_eframe(|_| HookApp::default());
    let built = harness.state().hooked;
    assert!(
        built > 0,
        "the passes the harness runs at build call the hook"
    );
    harness.step();
    harness.step();
    assert_eq!(
        harness.state().hooked,
        built + 2,
        "one call per step, as eframe makes one per frame"
    );
    assert_eq!(
        harness.state().seen,
        harness.state().hooked,
        "each pass gets the event its hook added"
    );
}
