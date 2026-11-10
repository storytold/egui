//! Native regression for a hidden parent's resize followed by an immediate child's partial update.

use core::time::Duration;
use std::{sync::Arc, time::Instant};

use eframe::egui::{
    self, Color32, ColorImage, TextureHandle, ViewportCommand, ViewportId, mutex::Mutex,
};

pub fn run() -> eframe::Result {
    let pixels = Arc::new(Mutex::new(None));
    let result = Arc::clone(&pixels);
    eframe::run_native(
        "Texture update ordering",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Wgpu,
            viewport: egui::ViewportBuilder::default().with_inner_size([250.0, 180.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            cc.egui_ctx.set_pixels_per_point(2.0);
            let texture = cc.egui_ctx.load_texture(
                "shared texture",
                ColorImage::filled([32, 32], Color32::RED),
                egui::TextureOptions::NEAREST,
            );
            Ok(Box::new(App {
                texture,
                phase: 0,
                started: Instant::now(),
                pixels,
            }))
        }),
    )?;
    let pixels = result.lock();
    let (red, green) =
        pixels.expect("the native regression must receive a screenshot before its deadline");
    assert!(
        red >= 256,
        "the untouched texture region must remain red: {red}"
    );
    assert!(
        green >= 256,
        "the partial update must be visible in green: {green}"
    );
    println!(
        "Texture ordering verified: preserved {red} red pixels and rendered {green} green pixels."
    );
    Ok(())
}

struct App {
    texture: TextureHandle,
    phase: u8,
    started: Instant,
    pixels: Arc<Mutex<Option<(usize, usize)>>>,
}

impl App {
    fn child() -> ViewportId {
        ViewportId::from_hash_of("texture-order-child")
    }

    fn finish_if_ready(&self, ctx: &egui::Context) {
        if self.pixels.lock().is_some() || self.started.elapsed() > Duration::from_secs(15) {
            ctx.send_viewport_cmd_to(Self::child(), ViewportCommand::Close);
            ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
        } else {
            ctx.request_repaint_after(Duration::from_millis(20));
            ctx.request_repaint_of(Self::child());
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.finish_if_ready(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let phase = self.phase;
        let mut texture = self.texture.clone();
        ui.ctx().show_viewport_immediate(
            Self::child(),
            egui::ViewportBuilder::default()
                .with_title("Shared texture")
                .with_inner_size([180.0, 180.0]),
            move |ui, _class| {
                if phase == 2 {
                    texture.set_partial(
                        [0, 0],
                        ColorImage::filled([32, 54], Color32::GREEN),
                        egui::TextureOptions::NEAREST,
                    );
                }
                ui.image((texture.id(), egui::vec2(64.0, 64.0)));
                ui.ctx().request_repaint();
            },
        );
        if self.phase >= 3 {
            // Screenshot requests are captured on a later pass; retain the image until delivery.
            ui.image((self.texture.id(), egui::vec2(64.0, 64.0)));
        }
        match self.phase {
            0 => {
                ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
                self.phase = 1;
            }
            1 if ui.input(|i| i.viewport().minimized == Some(true)) => {
                // This full resize is queued after the child paints, while the parent is hidden.
                self.texture.set(
                    ColorImage::filled([64, 64], Color32::RED),
                    egui::TextureOptions::NEAREST,
                );
                self.phase = 2;
            }
            2 => {
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::Minimized(false));
                self.phase = 3;
            }
            3 if ui.input(|i| i.viewport().minimized != Some(true)) => {
                // Capture the restored parent: immediate viewports do not collect screenshots.
                let pixels = Arc::clone(&self.pixels);
                ui.ctx().request_screenshot(move |image| {
                    let red = image.pixels.iter().filter(|&&p| p == Color32::RED).count();
                    let green = image
                        .pixels
                        .iter()
                        .filter(|&&p| p == Color32::GREEN)
                        .count();
                    *pixels.lock() = Some((red, green));
                });
                self.phase = 4;
            }
            _ => {}
        }
        self.finish_if_ready(ui.ctx());
    }
}
