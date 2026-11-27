//! Texture deltas of a multi-pass run reach the renderer in the order they were made, also when an immediate viewport
//! shown in a later pass is painted before the run ends.

use std::collections::HashSet;
use std::sync::Arc;

use egui::epaint::mutex::Mutex;
use egui::epaint::textures::TexturesDelta;
use egui::{Context, RawInput, TextureId, ViewportBuilder, ViewportId, pos2, vec2};

/// The one texture store every viewport's painter shares, as eframe's renderer: the deltas it was given, in order.
#[derive(Default)]
struct Store {
    allocated: HashSet<TextureId>,

    /// Partial updates of a texture the store had not allocated: egui-wgpu panics on these.
    unallocated: Vec<TextureId>,
}

impl Store {
    fn apply(&mut self, delta: &mut TexturesDelta) {
        // Each id's deltas keep their order; the order between ids changes nothing here.
        #[expect(clippy::iter_over_hash_type)]
        for (id, deltas) in delta.set.drain() {
            for image in deltas {
                if image.is_whole() {
                    self.allocated.insert(id);
                } else if !self.allocated.contains(&id) {
                    self.unallocated.push(id);
                }
            }
        }
        #[expect(clippy::iter_over_hash_type)]
        for id in delta.free.drain() {
            self.allocated.remove(&id);
        }
    }
}

fn input(viewport_id: ViewportId) -> RawInput {
    RawInput {
        viewport_id,
        screen_rect: Some(egui::Rect::from_min_size(
            pos2(0.0, 0.0),
            vec2(400.0, 300.0),
        )),
        viewports: core::iter::once((viewport_id, egui::ViewportInfo::default())).collect(),
        ..Default::default()
    }
}

#[test]
fn a_viewport_shown_in_a_later_pass_paints_with_the_textures_the_first_pass_made() {
    let store = Arc::new(Mutex::new(Store::default()));
    Context::set_immediate_viewport_renderer({
        let store = Arc::clone(&store);
        move |ctx, immediate| {
            let mut show = immediate.viewport_ui_cb;
            let mut output = ctx.run_ui(input(immediate.ids.this), |ui| show(ui));
            // eframe paints an immediate viewport as soon as its pass ends.
            store.lock().apply(&mut output.textures_delta);
        }
    });

    let ctx = Context::default();
    ctx.set_embed_viewports(false);
    let child = ViewportId::from_hash_of("floating window");
    let mut pass = 0;
    let mut output = ctx.run_ui(input(ViewportId::ROOT), |ui| {
        pass += 1;
        if pass == 1 {
            // The first pass lays out text, which allocates the font atlas, and asks for another pass.
            ui.label("a");
            ui.ctx().request_discard("sizing");
        } else {
            ui.ctx()
                .show_viewport_immediate(child, ViewportBuilder::default(), |ui, _| {
                    // Glyphs at a size the atlas has none of yet: a partial update of the atlas.
                    ui.label(egui::RichText::new("Floating panel").size(31.0));
                });
        }
    });
    assert_eq!(pass, 2, "the run takes two passes");
    store.lock().apply(&mut output.textures_delta);

    let store = store.lock();
    assert!(
        store.allocated.contains(&TextureId::default()),
        "the font atlas is allocated"
    );
    assert!(
        store.unallocated.is_empty(),
        "no partial update reaches the store before its texture: {:?}",
        store.unallocated
    );
}
