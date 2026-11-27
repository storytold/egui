#![cfg(feature = "wgpu")]

//! A paint callback that wants the backdrop is given what the frame holds before it, and not what comes after.

use egui::{Color32, Pos2, Rect, Vec2};
use egui_kittest::Harness;
use egui_wgpu::{Backdrop, CallbackResources, CallbackTrait, ScreenDescriptor, wgpu};

/// Inverts the colours under its rect: a fragment reads the backdrop pixel at its own position.
struct Invert;

struct InvertPipeline(wgpu::RenderPipeline);

const SHADER: &str = r"
@group(0) @binding(0) var backdrop: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    // One triangle covering the viewport (the callback's rect).
    let x = f32(i32(index & 1u) * 4 - 1);
    let y = f32(i32(index >> 1u) * 4 - 1);
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let under = textureLoad(backdrop, vec2<i32>(position.xy), 0);
    return vec4<f32>(1.0 - under.rgb, under.a);
}
";

impl CallbackTrait for Invert {
    fn wants_backdrop(&self) -> bool {
        true
    }

    fn prepare(
        &self,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _screen_descriptor: &ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if callback_resources.get::<InvertPipeline>().is_none() {
            let backdrop = callback_resources
                .get::<Backdrop>()
                .expect("a callback that wants the backdrop is given one before `prepare`");
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("invert"),
                source: wgpu::ShaderSource::Wgsl(SHADER.into()),
            });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("invert"),
                bind_group_layouts: &[Some(&backdrop.bind_group_layout)],
                immediate_size: 0,
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("invert"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: backdrop.format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
            callback_resources.insert(InvertPipeline(pipeline));
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &CallbackResources,
    ) {
        let (Some(pipeline), Some(backdrop)) = (
            callback_resources.get::<InvertPipeline>(),
            callback_resources.get::<Backdrop>(),
        ) else {
            return;
        };
        render_pass.set_pipeline(&pipeline.0);
        render_pass.set_bind_group(0, &backdrop.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

#[test]
fn a_backdrop_callback_sees_what_was_drawn_before_it_and_not_after() {
    let mut harness = Harness::builder()
        .with_size(Vec2::new(100.0, 80.0))
        .with_pixels_per_point(1.0)
        .build_ui(|ui| {
            let painter = ui.painter();
            // Before the callback: the left half red, the right half green.
            painter.rect_filled(Rect::from_min_max(Pos2::ZERO, Pos2::new(50.0, 80.0)), 0.0, Color32::RED);
            painter.rect_filled(Rect::from_min_max(Pos2::new(50.0, 0.0), Pos2::new(100.0, 80.0)), 0.0, Color32::GREEN);
            // The callback inverts the left half.
            painter.add(egui_wgpu::Callback::new_paint_callback(
                Rect::from_min_max(Pos2::ZERO, Pos2::new(50.0, 80.0)),
                Invert,
            ));
            // After the callback: a blue band over the bottom of the left half.
            painter.rect_filled(Rect::from_min_max(Pos2::new(0.0, 60.0), Pos2::new(50.0, 80.0)), 0.0, Color32::BLUE);
        });

    let image = harness.render().expect("the harness renders with wgpu");
    let pixel = |x: u32, y: u32| {
        let p = image.get_pixel(x, y).0;
        Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3])
    };
    assert_eq!(pixel(25, 20), Color32::from_rgb(0, 255, 255), "the red under the callback is inverted");
    assert_eq!(pixel(75, 40), Color32::GREEN, "outside its rect the callback changes nothing");
    assert_eq!(pixel(25, 70), Color32::BLUE, "what is drawn after the callback covers it, not inverted");
}
