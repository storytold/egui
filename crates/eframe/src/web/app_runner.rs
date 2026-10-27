use std::sync::Arc;

use egui::{ScreenshotCallback, TexturesDelta, ViewportCommand};

use crate::{App, epi, web::web_painter::WebPainter};

use super::{NeedRepaint, now_sec, text_agent::TextAgent};

pub struct AppRunner {
    #[allow(clippy::allow_attributes, dead_code)]
    pub(crate) web_options: crate::WebOptions,
    pub(crate) frame: epi::Frame,
    egui_ctx: egui::Context,
    painter: Box<dyn WebPainter>,
    pub(crate) input: super::WebInput,
    app: Box<dyn epi::App>,
    pub(crate) needs_repaint: Arc<NeedRepaint>,
    last_save_time: f64,
    pub(crate) text_agent: TextAgent,

    // If not empty, the painter should capture n frames from now.
    // zero means capture the exact next frame.
    screenshot_commands_with_frame_delay: Vec<(ScreenshotCallback, usize)>,

    // Output for the last run:
    textures_delta: TexturesDelta,
    clipped_primitives: Option<Vec<egui::ClippedPrimitive>>,
}

impl Drop for AppRunner {
    fn drop(&mut self) {
        log::debug!("AppRunner has fully dropped");
    }
}

impl AppRunner {
    /// # Errors
    /// Failure to initialize WebGL renderer, or failure to create app.
    #[cfg_attr(
        not(feature = "wgpu_no_default_features"),
        expect(clippy::unused_async)
    )]
    pub async fn new(
        canvas: web_sys::HtmlCanvasElement,
        web_options: crate::WebOptions,
        app_creator: epi::AppCreator<'static>,
        text_agent: TextAgent,
    ) -> Result<Self, String> {
        let egui_ctx = egui::Context::default();
        egui_ctx.add_glyph_rasterizer(super::canvas_glyphs::glyph_rasterizer());

        #[allow(clippy::allow_attributes, unused_assignments)]
        #[cfg(feature = "glow")]
        let mut gl = None;

        #[allow(clippy::allow_attributes, unused_assignments)]
        #[cfg(feature = "wgpu_no_default_features")]
        let mut wgpu_render_state = None;

        let painter = match web_options.renderer {
            #[cfg(feature = "glow")]
            epi::Renderer::Glow => {
                log::debug!("Using the glow renderer");
                let painter = super::web_painter_glow::WebPainterGlow::new(
                    egui_ctx.clone(),
                    canvas,
                    &web_options,
                )?;
                gl = Some(Arc::clone(painter.gl()));
                Box::new(painter) as Box<dyn WebPainter>
            }

            #[cfg(feature = "wgpu_no_default_features")]
            epi::Renderer::Wgpu => {
                log::debug!("Using the wgpu renderer");
                let painter = super::web_painter_wgpu::WebPainterWgpu::new(
                    egui_ctx.clone(),
                    canvas,
                    &web_options,
                )
                .await?;
                wgpu_render_state = painter.render_state();
                Box::new(painter) as Box<dyn WebPainter>
            }
        };

        let info = epi::IntegrationInfo {
            web_info: epi::WebInfo {
                user_agent: super::user_agent().unwrap_or_default(),
                location: super::web_location(),
            },
            cpu_usage: None,
        };
        let storage = LocalStorage::default();

        egui_ctx.set_os(egui::os::OperatingSystem::from_user_agent(
            &super::user_agent().unwrap_or_default(),
        ));
        super::storage::load_memory(&egui_ctx);

        egui_ctx.options_mut(|o| {
            // On web by default egui follows the zoom factor of the browser,
            // and lets the browser handle the zoom shortcuts.
            // A user can still zoom egui separately by calling [`egui::Context::set_zoom_factor`].
            o.zoom_with_keyboard = false;
            o.zoom_factor = 1.0;
        });

        // Tell egui right away about native_pixels_per_point
        // so that the app knows about it during app creation:
        egui_ctx.input_mut(|i| {
            let viewport_info = i.raw.viewports.entry(egui::ViewportId::ROOT).or_default();
            viewport_info.native_pixels_per_point = Some(super::native_pixels_per_point());
            i.pixels_per_point = super::native_pixels_per_point();
        });

        let cc = epi::CreationContext {
            egui_ctx: egui_ctx.clone(),
            integration_info: info.clone(),
            storage: Some(&storage),

            #[cfg(feature = "glow")]
            gl: gl.clone(),

            #[cfg(feature = "glow")]
            get_proc_address: None,

            #[cfg(feature = "wgpu_no_default_features")]
            wgpu_render_state: wgpu_render_state.clone(),
        };
        let app = app_creator(&cc).map_err(|err| err.to_string())?;

        let frame = epi::Frame {
            info,
            storage: Some(Box::new(storage)),

            #[cfg(feature = "glow")]
            gl,

            #[cfg(feature = "wgpu_no_default_features")]
            // We only care about the surface config if we are using wgpu
            wgpu_surface_config: wgpu_render_state
                .is_some()
                .then_some(web_options.wgpu_options.surface),

            #[cfg(feature = "wgpu_no_default_features")]
            wgpu_render_state,

            egui_ctx: Some(egui_ctx.clone()),
            text_input_language: None,
        };

        let needs_repaint: Arc<NeedRepaint> = Arc::new(NeedRepaint::new(web_options.max_fps));
        {
            let needs_repaint = Arc::clone(&needs_repaint);
            egui_ctx.set_request_repaint_callback(move |info| {
                needs_repaint.repaint_after(info.delay.as_secs_f64());
            });
        }

        let mut runner = Self {
            web_options,
            frame,
            egui_ctx,
            painter,
            input: Default::default(),
            app,
            needs_repaint,
            last_save_time: now_sec(),
            text_agent,
            screenshot_commands_with_frame_delay: vec![],
            textures_delta: Default::default(),
            clipped_primitives: None,
        };

        runner.input.raw.max_texture_side = Some(runner.painter.max_texture_side());
        runner
            .input
            .raw
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(super::native_pixels_per_point());
        runner.input.raw.system_theme = super::system_theme();

        Ok(runner)
    }

    pub fn egui_ctx(&self) -> &egui::Context {
        &self.egui_ctx
    }

    /// Get mutable access to the concrete [`App`] we enclose.
    ///
    /// This will panic if your app does not implement [`App::as_any_mut`].
    pub fn app_mut<ConcreteApp: 'static + App>(&mut self) -> &mut ConcreteApp {
        self.app
            .as_any_mut()
            .expect("Your app must implement `as_any_mut`, but it doesn't")
            .downcast_mut::<ConcreteApp>()
            .expect("app_mut got the wrong type of App")
    }

    pub fn auto_save_if_needed(&mut self) {
        let time_since_last_save = now_sec() - self.last_save_time;
        if time_since_last_save > self.app.auto_save_interval().as_secs_f64() {
            self.save();
        }
    }

    pub fn save(&mut self) {
        if self.app.persist_egui_memory() {
            super::storage::save_memory(&self.egui_ctx);
        }
        if let Some(storage) = self.frame.storage_mut() {
            self.app.save(storage);
        }
        self.last_save_time = now_sec();
    }

    pub fn canvas(&self) -> &web_sys::HtmlCanvasElement {
        self.painter.canvas()
    }

    pub fn destroy(mut self) {
        log::debug!("Destroying AppRunner");
        self.egui_ctx.on_exit();
        // Shutdown discards output that will never be painted. Backend destruction frees
        // textures; explicitly consume the remaining CPU delta rather than dropping it.
        self.textures_delta.clear();
        self.painter.destroy();
    }

    pub fn has_outstanding_paint_data(&self) -> bool {
        self.clipped_primitives.is_some()
    }

    /// Does the eframe app have focus?
    ///
    /// Technically: does either the canvas or the [`TextAgent`] have focus?
    pub fn has_focus(&self) -> bool {
        let window = web_sys::window().unwrap();
        let document = window.document().unwrap();
        if document.hidden() {
            return false;
        }

        super::has_focus(self.canvas()) || self.text_agent.has_focus()
    }

    pub fn update_focus(&mut self) {
        self.text_agent.restore_pending_focus();
        let has_focus = self.has_focus();
        if self.input.raw.focused != has_focus {
            log::trace!("{} Focus changed to {has_focus}", self.canvas().id());
            self.input.set_focus(has_focus);

            if !has_focus {
                // We lost focus - good idea to save
                self.save();
            }
            self.egui_ctx().request_repaint();
        }
    }

    /// Runs the logic, but doesn't paint the result.
    ///
    /// The result can be painted later with a call to [`Self::run_and_paint`] or [`Self::paint`].
    pub fn logic(&mut self) {
        // We sometimes miss blur/focus events due to the text agent, so let's just poll each frame:
        self.update_focus();

        let canvas_size = super::canvas_size_in_points(self.canvas(), self.egui_ctx());
        let mut raw_input = self.input.new_frame(canvas_size);

        if super::DEBUG_RESIZE {
            log::info!(
                "egui running at canvas size: {}x{}, DPR: {}, zoom_factor: {}. egui size: {}x{} points",
                self.canvas().width(),
                self.canvas().height(),
                super::native_pixels_per_point(),
                self.egui_ctx.zoom_factor(),
                canvas_size.x,
                canvas_size.y,
            );
        }

        self.app.raw_input_hook(&self.egui_ctx, &mut raw_input);

        let is_visible = raw_input
            .viewports
            .get(&egui::ViewportId::ROOT)
            .and_then(|v| v.visible())
            .unwrap_or(true);

        if is_visible {
            let full_output = self.egui_ctx.run_ui(raw_input, |ui| {
                self.app.logic(ui.ctx(), &mut self.frame);
                self.app.ui(ui, &mut self.frame);
            });
            let egui::FullOutput {
                platform_output,
                textures_delta,
                shapes,
                pixels_per_point,
                viewport_output,
            } = full_output;

            if viewport_output.len() > 1 {
                log::warn!("Multiple viewports not yet supported on the web");
            }
            self.handle_viewport_commands(
                viewport_output
                    .into_values()
                    .flat_map(|viewport_output| viewport_output.commands),
            );

            self.handle_platform_output(platform_output);
            self.textures_delta.append(textures_delta);
            self.clipped_primitives = Some(self.egui_ctx.tessellate(shapes, pixels_per_point));
        } else {
            // The tab is hidden, so we run no egui pass at all.
            // That way all ui state is left untouched, and is still there
            // when the tab is shown again.

            let egui::LogicOutput {
                platform_output,
                viewport_commands,
            } = self.egui_ctx.run_logic(&raw_input, |ctx| {
                self.app.logic(ctx, &mut self.frame);
            });

            // No pass consumed the input, so save it for the next one:
            self.input.raw.append(raw_input);

            self.handle_viewport_commands(viewport_commands.into_values().flatten());
            self.handle_platform_output(platform_output);
        }
    }

    fn handle_viewport_commands(&mut self, commands: impl Iterator<Item = ViewportCommand>) {
        for command in commands {
            match command {
                ViewportCommand::Screenshot(callback) => {
                    self.screenshot_commands_with_frame_delay
                        .push((callback, 1));
                }
                ViewportCommand::SetTheme(_) => {
                    // Web has no window decorations to theme, egui visuals follow the configured theme.
                }
                _ => {
                    // TODO(emilk): handle some of the commands
                    log::warn!(
                        "Unhandled egui viewport command: {command:?} - not implemented in web backend"
                    );
                }
            }
        }
    }

    /// Paint the results of the last call to [`Self::logic`].
    pub fn paint(&mut self) {
        let clipped_primitives = core::mem::take(&mut self.clipped_primitives);

        if let Some(clipped_primitives) = clipped_primitives {
            let mut screenshot_commands = vec![];
            self.screenshot_commands_with_frame_delay
                .retain_mut(|(callback, frame_delay)| {
                    if *frame_delay == 0 {
                        screenshot_commands.push(callback.clone());
                        false
                    } else {
                        *frame_delay -= 1;
                        true
                    }
                });
            if !self.screenshot_commands_with_frame_delay.is_empty() {
                self.egui_ctx().request_repaint();
            }

            if let Err(err) = self.painter.paint_and_update_textures(
                self.app.clear_color(&self.egui_ctx.global_style().visuals),
                &clipped_primitives,
                self.egui_ctx.pixels_per_point(),
                &mut self.textures_delta,
                screenshot_commands,
            ) {
                log::error!("Failed to paint: {}", super::string_from_js_value(&err));
            }
        }
    }

    pub fn report_frame_time(&mut self, cpu_usage_seconds: f32) {
        self.frame.info.cpu_usage = Some(cpu_usage_seconds);
    }

    fn handle_platform_output(&mut self, platform_output: egui::PlatformOutput) {
        #[cfg(feature = "web_screen_reader")]
        if self.egui_ctx.options(|o| o.screen_reader) {
            super::screen_reader::speak(
                &platform_output.events_description(),
                self.egui_ctx.language().as_deref(),
            );
        }

        let egui::PlatformOutput {
            commands,
            cursor_icon,
            cursor_image: _, // TODO(alextournai): support custom bitmap cursors on the web (via CSS `url(...)`)
            events: _,       // already handled
            mutable_text_under_cursor: _, // TODO(#4569): https://github.com/emilk/egui/issues/4569
            ime,
            accesskit_update: _,        // not currently implemented
            num_completed_passes: _,    // handled by `Context::run`
            request_discard_reasons: _, // handled by `Context::run`
        } = platform_output;

        for command in commands {
            match command {
                egui::OutputCommand::CopyText(text) => {
                    super::set_clipboard_text(&text);
                }
                egui::OutputCommand::TextSelectionSettled(_) => {} // No PRIMARY selection on web
                egui::OutputCommand::CopyImage(image) => {
                    super::set_clipboard_image(&image);
                }
                egui::OutputCommand::OpenUrl(open_url) => {
                    super::open_url(&open_url.url, open_url.new_tab);
                }
            }
        }

        super::set_cursor_icon(self.canvas(), cursor_icon);

        let ui_language = self.egui_ctx.language();
        let language = self.frame.text_input_language.clone().or(ui_language);
        let canvas = self.canvas().clone();
        let zoom_factor = self.egui_ctx.zoom_factor();
        let request = self.text_agent.admit_reset(ime.as_ref());
        if let Some(request_id) = request {
            // Snapshot before removing a focused agent; afterwards has_focus() alone is false.
            // Retire even while hidden/unfocused, but never steal another element's focus.
            self.text_agent.capture_reset_focus(ime.as_ref().unwrap());
            let forwarded_keys = self.text_agent.take_forwarded_keys();
            let descriptor = ime.as_ref().unwrap(); // admission requires an actual descriptor
            let replacement = self.text_agent.replace_for_reset(
                descriptor.clone(),
                &canvas,
                zoom_factor,
                language.as_deref(),
            );
            // Only this retired source's forwarded presses need an ordered release. Old-node
            // keyup callbacks are now inactive; do not blanket-reset canvas keys/modifiers.
            for forwarded in forwarded_keys {
                let (key, physical_key) = self
                    .input
                    .keys
                    .key_up_if_current(&forwarded.event, forwarded.press);
                if let Some(key) = key {
                    self.input.raw.events.push(egui::Event::Key {
                        key,
                        physical_key,
                        pressed: false,
                        repeat: false,
                        modifiers: self.input.modifiers,
                    });
                }
            }
            match replacement {
                Ok(())
                    if self.text_agent.complete_reset(
                        request_id,
                        descriptor,
                        &mut self.input.raw.events,
                        &self.needs_repaint,
                    ) =>
                {
                    // Genuine next raw batch: accepted old prefix is already queued (including
                    // hidden replay), then fence, then only fresh-source callbacks. No reentrant
                    // AppRunner borrow and no fabricated composition/lifecycle event.
                    self.text_agent.restore_pending_focus();
                }
                Ok(()) => {
                    self.text_agent.fail_reset();
                    log::warn!("text-agent reset descriptor no longer matches its request");
                }
                Err(err) => {
                    self.text_agent.fail_reset();
                    log::error!(
                        "text-agent local reset failed; no completion: {}",
                        super::string_from_js_value(&err)
                    );
                }
            }
        } else {
            // Preserve the ordinary untokened interrupt/focus path. Replayed tokened output
            // never degrades into an untokened clear or an acknowledgement.
            self.text_agent.set_lang(language.as_deref());
            if self.has_focus() {
                if let Some(ime) = &ime {
                    if ime.should_interrupt_composition && ime.reset_request.is_none() {
                        self.text_agent.interrupt_ime_composition();
                    }
                    self.text_agent.focus();
                } else {
                    self.text_agent.blur();
                    super::focus_without_scroll(&canvas).ok();
                }
            }
        }

        if let Err(err) = self.text_agent.update(ime, &canvas, zoom_factor) {
            log::error!(
                "failed to update text agent position: {}",
                super::string_from_js_value(&err)
            );
        }
    }
}

// ----------------------------------------------------------------------------

#[derive(Default)]
struct LocalStorage {}

impl epi::Storage for LocalStorage {
    fn get_string(&self, key: &str) -> Option<String> {
        super::storage::local_storage_get(key)
    }

    fn set_string(&mut self, key: &str, value: String) {
        super::storage::local_storage_set(key, &value);
    }

    fn remove_string(&mut self, key: &str) {
        super::storage::local_storage_remove(key);
    }

    fn flush(&mut self) {}
}

#[cfg(test)]
mod ime_reset_browser_tests {
    use super::*;
    use core::cell::{Cell, RefCell};
    use std::rc::Rc;
    use wasm_bindgen::prelude::*;

    // Drives the actual document.hidden branch without pretending this fixture changes OS tab
    // visibility. All DOM focus, TextAgent listeners and AppRunner/keyboard handlers are real.
    #[wasm_bindgen(
        inline_js = "let eguiHiddenDescriptor; let eguiHiddenInstalled = false; export function egui_probe_hidden(mode) { if (!eguiHiddenInstalled && mode >= 0) { eguiHiddenDescriptor = Object.getOwnPropertyDescriptor(document, 'hidden'); eguiHiddenInstalled = true; } if (mode < 0) { if (eguiHiddenInstalled) { if (eguiHiddenDescriptor) Object.defineProperty(document, 'hidden', eguiHiddenDescriptor); else delete document.hidden; eguiHiddenInstalled = false; } } else Object.defineProperty(document, 'hidden', { configurable: true, get: () => !!mode }); }\nexport function egui_probe_key(type, key, code) { return new KeyboardEvent(type, {key, code, keyCode: key.toUpperCase().charCodeAt(0), bubbles: true, cancelable: true}); }\nexport function egui_probe_shortcut(type, key, code, meta) { return new KeyboardEvent(type, {key, code, keyCode: key === 'Control' ? 17 : key === 'Meta' ? 91 : key.toUpperCase().charCodeAt(0), ctrlKey: type === 'keydown' && !meta, metaKey: type === 'keydown' && meta, bubbles: true, cancelable: true}); }\nexport function egui_probe_open_shadow(host) { return host.attachShadow({mode: 'open'}); }\nexport function egui_probe_focus_body() { const body = document.body, previous = body.getAttribute('tabindex'); body.setAttribute('tabindex', '-1'); body.focus({preventScroll: true}); if (previous === null) body.removeAttribute('tabindex'); else body.setAttribute('tabindex', previous); }"
    )]
    extern "C" {
        fn egui_probe_hidden(mode: i32);

        fn egui_probe_open_shadow(host: &web_sys::Element) -> web_sys::ShadowRoot;

        fn egui_probe_focus_body();

        fn egui_probe_key(kind: &str, key: &str, code: &str) -> web_sys::KeyboardEvent;

        fn egui_probe_shortcut(
            kind: &str,
            key: &str,
            code: &str,
            meta: bool,
        ) -> web_sys::KeyboardEvent;
    }

    #[derive(Default)]
    struct PaintLifecycle {
        paints: Cell<usize>,
        destroys: Cell<usize>,
    }

    struct NoopPainter(web_sys::HtmlCanvasElement, Rc<PaintLifecycle>);

    impl WebPainter for NoopPainter {
        fn canvas(&self) -> &web_sys::HtmlCanvasElement {
            &self.0
        }

        fn max_texture_side(&self) -> usize {
            4096
        }

        fn paint_and_update_textures(
            &mut self,
            _: [f32; 4],
            _: &[egui::ClippedPrimitive],
            _: f32,
            textures_delta: &mut egui::TexturesDelta,
            _: Vec<ScreenshotCallback>,
        ) -> Result<(), JsValue> {
            // This test painter intentionally renders no pixels, but owns the same texture
            // consumption contract as the real browser painters.
            self.1.paints.set(self.1.paints.get() + 1);
            textures_delta.clear();
            Ok(())
        }

        fn destroy(&mut self) {
            self.1.destroys.set(self.1.destroys.get() + 1);
        }
    }

    struct Model {
        ime: Option<egui::output::IMEOutput>,
        request: bool,
        ids: Vec<egui::ImeResetId>,
        logic_calls: usize,
        ui_calls: usize,
        batches: Vec<Vec<egui::Event>>,
    }

    struct ProbeApp(Rc<RefCell<Model>>);

    impl epi::App for ProbeApp {
        fn raw_input_hook(&mut self, _: &egui::Context, raw: &mut egui::RawInput) {
            self.0.borrow_mut().batches.push(raw.events.clone());
        }

        fn logic(&mut self, ctx: &egui::Context, _: &mut epi::Frame) {
            let mut model = self.0.borrow_mut();
            model.logic_calls += 1;
            if model.request {
                model.request = false;
                model
                    .ids
                    .push(ctx.request_ime_reset(egui::ViewportId::ROOT).unwrap());
            }
            ctx.output_mut(|out| out.ime = model.ime.clone());
        }

        fn ui(&mut self, _: &mut egui::Ui, _: &mut epi::Frame) {
            self.0.borrow_mut().ui_calls += 1;
        }

        fn persist_egui_memory(&self) -> bool {
            false
        }
    }

    struct Fixture {
        runner: super::super::WebRunner,
        canvas: web_sys::HtmlCanvasElement,
        model: Rc<RefCell<Model>>,
        paint_lifecycle: Rc<PaintLifecycle>,
    }

    impl Fixture {
        fn new() -> Self {
            let body = web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .body()
                .unwrap();
            Self::new_in_parent(body.unchecked_ref())
        }

        fn new_in_parent(parent: &web_sys::Node) -> Self {
            egui_probe_hidden(0);
            let document = web_sys::window().unwrap().document().unwrap();
            let canvas = document
                .create_element("canvas")
                .unwrap()
                .dyn_into::<web_sys::HtmlCanvasElement>()
                .unwrap();
            canvas.set_width(320);
            canvas.set_height(180);
            canvas.set_tab_index(0);
            parent.append_child(&canvas).unwrap();
            let runner = super::super::WebRunner::new();
            let text_agent = TextAgent::attach(&runner, &canvas).unwrap();
            let egui_ctx = egui::Context::default();
            let model = Rc::new(RefCell::new(Model {
                ime: Some(egui::output::IMEOutput::new(
                    egui::Id::unique("AppRunner editor"),
                    1,
                    egui::IMEPurpose::Normal,
                    egui::Rect::ZERO,
                    egui::Rect::ZERO,
                )),
                request: false,
                ids: Vec::new(),
                logic_calls: 0,
                ui_calls: 0,
                batches: Vec::new(),
            }));
            let paint_lifecycle = Rc::new(PaintLifecycle::default());
            let app_runner = AppRunner {
                web_options: crate::WebOptions::default(),
                frame: epi::Frame {
                    info: epi::IntegrationInfo {
                        web_info: epi::WebInfo {
                            user_agent: String::new(),
                            location: super::super::web_location(),
                        },
                        cpu_usage: None,
                    },
                    storage: None,
                    #[cfg(feature = "glow")]
                    gl: None,
                    #[cfg(feature = "wgpu_no_default_features")]
                    wgpu_surface_config: None,
                    #[cfg(feature = "wgpu_no_default_features")]
                    wgpu_render_state: None,
                    egui_ctx: Some(egui_ctx.clone()),
                    text_input_language: None,
                },
                egui_ctx,
                painter: Box::new(NoopPainter(canvas.clone(), Rc::clone(&paint_lifecycle))),
                input: Default::default(),
                app: Box::new(ProbeApp(Rc::clone(&model))),
                needs_repaint: Arc::new(NeedRepaint::new(None)),
                last_save_time: super::super::now_sec(),
                text_agent,
                screenshot_commands_with_frame_delay: Vec::new(),
                textures_delta: Default::default(),
                clipped_primitives: None,
            };
            runner.install_test_app_runner(app_runner).unwrap();
            runner.try_lock().unwrap().logic();
            assert!(runner.try_lock().unwrap().text_agent.has_focus());
            Self {
                runner,
                canvas,
                model,
                paint_lifecycle,
            }
        }

        fn input(&self) -> web_sys::HtmlInputElement {
            self.runner.try_lock().unwrap().text_agent.test_input()
        }

        fn visibility(hidden: bool) {
            egui_probe_hidden(i32::from(hidden));
            web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .dispatch_event(&web_sys::Event::new("visibilitychange").unwrap())
                .unwrap();
        }

        fn reset(&self) {
            self.model.borrow_mut().request = true;
            self.runner.try_lock().unwrap().logic();
        }

        fn completions(&self) -> usize {
            self.runner
                .try_lock()
                .unwrap()
                .input
                .raw
                .events
                .iter()
                .filter(|event| matches!(event, egui::Event::ImeResetCompleted { .. }))
                .count()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(mut runner) = self.runner.try_lock() {
                // Keep actual logic/input paths unchanged; finish their pending paint only
                // at fixture teardown before releasing the application.
                runner.paint();
                assert!(runner.textures_delta.is_empty());
                assert!(!runner.has_outstanding_paint_data());
            }
            self.runner.destroy();
            self.canvas.remove();
            egui_probe_hidden(-1);
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn browser_repeat_survives_focus_regain_and_a_fresh_press_after_a_missing_release() {
        fn keyboard_event(
            kind: &str,
            key: &str,
            code: &str,
            repeat: bool,
        ) -> web_sys::KeyboardEvent {
            // Use the real DOM constructor, in Rust, rather than replacing its readonly getter.
            let options = js_sys::Object::new();
            for (name, value) in [
                ("key", JsValue::from_str(key)),
                ("code", JsValue::from_str(code)),
                ("repeat", JsValue::from_bool(repeat)),
                ("bubbles", JsValue::TRUE),
            ] {
                js_sys::Reflect::set(&options, &JsValue::from_str(name), &value).unwrap();
            }
            let constructor = js_sys::Reflect::get(
                &web_sys::window().unwrap(),
                &JsValue::from_str("KeyboardEvent"),
            )
            .unwrap()
            .dyn_into::<js_sys::Function>()
            .unwrap();
            js_sys::Reflect::construct(
                &constructor,
                &js_sys::Array::of2(&JsValue::from_str(kind), &options),
            )
            .unwrap()
            .unchecked_into()
        }

        for (key, code, egui_key) in [("n", "KeyN", egui::Key::N), ("y", "KeyY", egui::Key::Y)] {
            let fixture = Fixture::new();
            let input = fixture.input();
            let document = web_sys::window().unwrap().document().unwrap();
            let outside = document
                .create_element("input")
                .unwrap()
                .unchecked_into::<web_sys::HtmlInputElement>();
            document.body().unwrap().append_child(&outside).unwrap();

            for repeat in [true, false] {
                input
                    .dispatch_event(&keyboard_event("keydown", key, code, false))
                    .unwrap();
                fixture.runner.try_lock().unwrap().logic();
                outside.focus().unwrap();
                fixture.runner.try_lock().unwrap().logic();
                input.focus().unwrap();
                fixture.runner.try_lock().unwrap().logic();
                // true: same physical hold continues; false: release went elsewhere, then a fresh press.
                let event = keyboard_event("keydown", key, code, repeat);
                assert_eq!(event.repeat(), repeat);
                input.dispatch_event(&event).unwrap();
                let mut runner = fixture.runner.try_lock().unwrap();
                assert!(runner.input.raw.events.iter().any(|e| matches!(e,
                    egui::Event::Key { key, pressed: true, repeat: actual, .. } if *key == egui_key && *actual == repeat
                )), "the actual DOM keydown adapter must preserve browser repeat");
                runner.logic();
                assert!(runner.egui_ctx().input(|i| i.events.iter().any(|e| matches!(e,
                    egui::Event::Key { key, pressed: true, repeat: actual, .. } if *key == egui_key && *actual == repeat
                ))));
                drop(runner);
                input
                    .dispatch_event(&keyboard_event("keyup", key, code, false))
                    .unwrap();
                fixture.runner.try_lock().unwrap().logic();
                assert!(
                    !fixture
                        .runner
                        .try_lock()
                        .unwrap()
                        .egui_ctx()
                        .input(|i| i.key_down(egui_key))
                );
            }
            outside.remove();
        }
    }

    // Invoke the public lifecycle before Fixture::drop can consume any pending output.
    fn destroy_with_pending_output(fixture: Fixture) {
        let input = fixture.input();
        let callback = fixture
            .runner
            .try_lock()
            .unwrap()
            .text_agent
            .test_keyup_callback();
        let listeners = fixture.runner.text_agent_context().listeners;
        {
            let lock = fixture.runner.try_lock().unwrap();
            assert!(
                !lock.textures_delta.is_empty(),
                "a real logic pass queued font upload"
            );
            assert!(lock.has_outstanding_paint_data());
            assert_eq!(fixture.paint_lifecycle.paints.get(), 0);
            assert_eq!(listeners.borrow().test_listener_counts().0, 5);
        }
        fixture.runner.destroy();
        assert!(fixture.runner.try_lock().is_none());
        assert_eq!(
            fixture.paint_lifecycle.paints.get(),
            0,
            "shutdown must not force paint"
        );
        assert_eq!(fixture.paint_lifecycle.destroys.get(), 1);
        assert!(input.parent_node().is_none());
        assert_eq!(listeners.borrow().test_listener_counts(), (0, 0));
        callback
            .call1(
                &JsValue::NULL,
                egui_probe_key("keyup", "a", "KeyA").as_ref(),
            )
            .unwrap();
        fixture.runner.destroy();
        assert_eq!(fixture.paint_lifecycle.destroys.get(), 1);
        drop(fixture);
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn public_destroy_after_logic_before_paint_disposes_pending_output() {
        destroy_with_pending_output(Fixture::new());
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn public_destroy_after_hidden_logic_disposes_pending_visible_output() {
        let fixture = Fixture::new();
        Fixture::visibility(true);
        fixture.runner.try_lock().unwrap().logic();
        destroy_with_pending_output(fixture);
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn public_destroy_after_consumed_paint_is_idempotent() {
        let fixture = Fixture::new();
        fixture.runner.try_lock().unwrap().paint();
        assert_eq!(fixture.paint_lifecycle.paints.get(), 1);
        assert!(fixture.runner.try_lock().unwrap().textures_delta.is_empty());
        fixture.runner.destroy();
        fixture.runner.destroy();
        assert!(fixture.runner.try_lock().is_none());
        assert_eq!(fixture.paint_lifecycle.paints.get(), 1);
        assert_eq!(fixture.paint_lifecycle.destroys.get(), 1);
    }

    fn shadow_fixture(depth: usize) -> (Fixture, web_sys::Element, web_sys::ShadowRoot) {
        assert!(depth == 1 || depth == 2);
        let document = web_sys::window().unwrap().document().unwrap();
        let host = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&host).unwrap();
        let mut root = egui_probe_open_shadow(&host);
        if depth == 2 {
            let nested = document.create_element("div").unwrap();
            root.append_child(&nested).unwrap();
            root = egui_probe_open_shadow(&nested);
        }
        let fixture = Fixture::new_in_parent(root.unchecked_ref());
        (fixture, host, root)
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn shadow_hidden_reset_restores_the_same_owned_root_without_click() {
        for depth in [1, 2] {
            let (fixture, host, root) = shadow_fixture(depth);
            let old = fixture.input();
            assert_eq!(root.active_element().as_ref(), Some(old.unchecked_ref()));
            Fixture::visibility(true);
            fixture.reset();
            let replacement = fixture.input();
            assert_ne!(old, replacement);
            assert!(old.parent_node().is_none());
            assert_eq!(replacement.get_root_node(), root.clone().unchecked_into());
            assert!(
                fixture
                    .runner
                    .try_lock()
                    .unwrap()
                    .text_agent
                    .test_has_restore_intent()
            );
            assert!(
                !super::super::has_focus(&replacement),
                "hidden replacement must not focus early"
            );
            Fixture::visibility(false);
            assert_eq!(
                root.active_element().as_ref(),
                Some(replacement.unchecked_ref())
            );
            assert!(fixture.runner.try_lock().unwrap().has_focus());
            assert_eq!(fixture.completions(), 1);
            drop(fixture);
            host.remove();
        }
    }

    fn shadow_external_focus_then_body(depth: usize, ancestor: bool, document_target: bool) {
        let (fixture, host, root) = shadow_fixture(depth);
        Fixture::visibility(true);
        fixture.reset();
        let document = web_sys::window().unwrap().document().unwrap();
        let outside = document
            .create_element("input")
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        let parent: web_sys::Node = if document_target {
            document.body().unwrap().unchecked_into()
        } else if ancestor {
            assert_eq!(depth, 2);
            root.host().get_root_node()
        } else {
            root.clone().unchecked_into()
        };
        parent.append_child(&outside).unwrap();
        {
            let lock = fixture.runner.try_lock().unwrap();
            assert!(lock.text_agent.test_has_restore_intent());
            outside.focus().unwrap();
            assert_eq!(
                super::super::focused_element(&outside.get_root_node()).as_ref(),
                Some(outside.unchecked_ref())
            );
            assert!(
                !lock.text_agent.test_has_restore_intent(),
                "external focus itself must cancel restoration"
            );
            egui_probe_focus_body(); // external choice then neutral BODY must remain cancelled
            assert_eq!(
                document.active_element(),
                document.body().map(|body| body.unchecked_into())
            );
            assert!(!lock.text_agent.test_has_restore_intent());
        }
        Fixture::visibility(false);
        assert!(!fixture.runner.try_lock().unwrap().has_focus());
        assert_eq!(
            document.active_element(),
            document.body().map(|body| body.unchecked_into())
        );
        assert_eq!(fixture.completions(), 1);
        outside.remove();
        drop(fixture);
        host.remove();
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn shadow_external_document_focus_then_body_cancels_restore_while_borrowed() {
        for depth in [1, 2] {
            shadow_external_focus_then_body(depth, false, true);
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn shadow_external_leaf_or_ancestor_focus_then_body_never_revives_restore() {
        for (depth, ancestor) in [(1, false), (2, false), (2, true)] {
            shadow_external_focus_then_body(depth, ancestor, false);
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn actual_hidden_app_runner_retains_focus_intent_and_delivers_one_completion_after_show() {
        let fixture = Fixture::new();
        let old = fixture.input();
        Fixture::visibility(true);
        assert!(
            super::super::has_focus(&old),
            "hidden fixture retains actual input DOM focus"
        );
        fixture.reset();
        let replacement = fixture.input();
        assert_ne!(old, replacement);
        assert!(old.parent_node().is_none());
        assert!(!super::super::has_focus(&replacement));
        assert!(
            fixture
                .runner
                .try_lock()
                .unwrap()
                .text_agent
                .test_has_restore_intent()
        );
        assert_eq!(fixture.completions(), 1);
        assert_eq!(
            fixture
                .runner
                .try_lock()
                .unwrap()
                .needs_repaint
                .when_to_repaint(),
            f64::NEG_INFINITY
        );
        fixture.runner.try_lock().unwrap().logic(); // actual second hidden run_logic/replay
        assert_eq!(fixture.completions(), 1);
        assert_eq!(
            fixture.model.borrow().ui_calls,
            1,
            "hidden logic must not execute UI"
        );
        Fixture::visibility(false); // production visibility handler calls update_focus
        assert!(
            super::super::has_focus(&replacement),
            "no click or additional text input required"
        );
        assert!(fixture.runner.try_lock().unwrap().input.raw.focused);
        fixture.runner.try_lock().unwrap().logic();
        let model = fixture.model.borrow();
        assert_eq!(model.logic_calls, 4);
        assert_eq!(model.ui_calls, 2);
        assert_eq!(model.ids.len(), 1);
        let batch = model.batches.last().unwrap();
        assert_eq!(batch.iter().filter(|event| matches!(event, egui::Event::ImeResetCompleted { request_id } if *request_id == model.ids[0])).count(), 1);
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn actual_external_focus_then_body_while_runner_borrowed_cancels_hidden_restore() {
        let fixture = Fixture::new();
        Fixture::visibility(true);
        fixture.reset();
        let document = web_sys::window().unwrap().document().unwrap();
        let external = document
            .create_element("input")
            .unwrap()
            .dyn_into::<web_sys::HtmlInputElement>()
            .unwrap();
        document.body().unwrap().append_child(&external).unwrap();
        {
            let lock = fixture.runner.try_lock().unwrap();
            assert!(lock.text_agent.test_has_restore_intent());
            external.focus().unwrap(); // observer must work without another AppRunner borrow
            external.blur().unwrap();
            assert!(!lock.text_agent.test_has_restore_intent());
        }
        assert!(
            document
                .active_element()
                .is_some_and(|element| element.tag_name() == "BODY")
        );
        Fixture::visibility(false);
        assert!(!fixture.runner.try_lock().unwrap().has_focus());
        assert!(!super::super::has_focus(&fixture.input()));
        assert_eq!(
            fixture.completions(),
            1,
            "local completion does not authorize focus theft"
        );
        external.remove();
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn hidden_repeat_reset_preserves_intent_but_context_loss_cancels_it() {
        for lose_context in [false, true] {
            let fixture = Fixture::new();
            Fixture::visibility(true);
            fixture.reset();
            if lose_context {
                fixture.model.borrow_mut().ime = None;
                fixture.runner.try_lock().unwrap().logic();
                assert!(
                    !fixture
                        .runner
                        .try_lock()
                        .unwrap()
                        .text_agent
                        .test_has_restore_intent()
                );
            } else {
                fixture.reset();
                assert_eq!(fixture.completions(), 2);
                assert!(
                    fixture
                        .runner
                        .try_lock()
                        .unwrap()
                        .text_agent
                        .test_has_restore_intent()
                );
            }
            Fixture::visibility(false);
            assert_eq!(
                fixture.runner.try_lock().unwrap().has_focus(),
                !lose_context
            );
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn actual_agent_down_canvas_up_new_canvas_down_survives_retirement() {
        for focus_clear in [false, true] {
            let fixture = Fixture::new();
            let old_input = fixture.input();
            old_input
                .dispatch_event(&egui_probe_key("keydown", "a", "KeyA"))
                .unwrap();
            let old_receipt = fixture
                .runner
                .try_lock()
                .unwrap()
                .input
                .keys
                .press_id(&egui_probe_key("keydown", "a", "KeyA"))
                .unwrap();
            super::super::focus_without_scroll(&fixture.canvas).unwrap();
            if focus_clear {
                fixture.runner.try_lock().unwrap().input.set_focus(false);
                fixture.runner.try_lock().unwrap().update_focus();
            } else {
                fixture
                    .canvas
                    .dispatch_event(&egui_probe_key("keyup", "a", "KeyA"))
                    .unwrap();
            }
            fixture
                .canvas
                .dispatch_event(&egui_probe_key("keydown", "a", "KeyA"))
                .unwrap();
            let new_receipt = fixture
                .runner
                .try_lock()
                .unwrap()
                .input
                .keys
                .press_id(&egui_probe_key("keydown", "a", "KeyA"))
                .unwrap();
            assert_ne!(old_receipt, new_receipt);
            fixture.reset(); // actual raw hook/run_ui/platform-output path
            let lock = fixture.runner.try_lock().unwrap();
            assert_eq!(
                lock.input
                    .keys
                    .press_id(&egui_probe_key("keydown", "a", "KeyA")),
                Some(new_receipt)
            );
            assert!(
                !lock.input.raw.events.iter().any(|event| matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::A,
                        pressed: false,
                        ..
                    }
                )),
                "retiring stale agent must not release the newer canvas press"
            );
            assert_eq!(
                lock.input
                    .raw
                    .events
                    .iter()
                    .filter(|event| matches!(event, egui::Event::ImeResetCompleted { .. }))
                    .count(),
                1
            );
            drop(lock);
            fixture
                .canvas
                .dispatch_event(&egui_probe_key("keyup", "a", "KeyA"))
                .unwrap();
            assert!(
                fixture
                    .runner
                    .try_lock()
                    .unwrap()
                    .input
                    .keys
                    .press_id(&egui_probe_key("keydown", "a", "KeyA"))
                    .is_none()
            );
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn actual_current_agent_press_releases_once_before_fence_and_old_keyup_cannot_repeat_it() {
        let fixture = Fixture::new();
        let old = fixture.input();
        old.dispatch_event(&egui_probe_key("keydown", "a", "KeyA"))
            .unwrap();
        fixture.runner.try_lock().unwrap().logic();
        super::super::focus_without_scroll(&fixture.canvas).unwrap();
        fixture
            .canvas
            .dispatch_event(&egui_probe_key("keydown", "b", "KeyB"))
            .unwrap();
        let unrelated = fixture
            .runner
            .try_lock()
            .unwrap()
            .input
            .keys
            .press_id(&egui_probe_key("keydown", "b", "KeyB"))
            .unwrap();
        fixture.reset();
        let before = fixture.runner.try_lock().unwrap().input.raw.events.clone();
        let release = before
            .iter()
            .position(|event| {
                matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::A,
                        pressed: false,
                        ..
                    }
                )
            })
            .unwrap();
        let fence = before
            .iter()
            .position(|event| matches!(event, egui::Event::ImeResetCompleted { .. }))
            .unwrap();
        assert!(release < fence);
        assert_eq!(
            fixture
                .runner
                .try_lock()
                .unwrap()
                .input
                .keys
                .press_id(&egui_probe_key("keydown", "b", "KeyB")),
            Some(unrelated)
        );
        assert!(!before.iter().any(|event| matches!(
            event,
            egui::Event::Key {
                key: egui::Key::B,
                pressed: false,
                ..
            }
        )));
        assert_eq!(
            before
                .iter()
                .filter(|event| matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::A,
                        pressed: false,
                        ..
                    }
                ))
                .count(),
            1
        );
        old.dispatch_event(&egui_probe_key("keyup", "a", "KeyA"))
            .unwrap();
        assert_eq!(fixture.runner.try_lock().unwrap().input.raw.events, before);
        assert!(
            fixture
                .runner
                .try_lock()
                .unwrap()
                .input
                .keys
                .press_id(&egui_probe_key("keydown", "a", "KeyA"))
                .is_none()
        );
    }

    fn modifier_up(meta: bool) -> web_sys::KeyboardEvent {
        egui_probe_shortcut(
            "keyup",
            if meta { "Meta" } else { "Control" },
            if meta { "MetaLeft" } else { "ControlLeft" },
            meta,
        )
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn queued_shortcut_modifier_up_keeps_authority_to_release_retired_agent_key() {
        for meta in [false, true] {
            let fixture = Fixture::new();
            let old = fixture.input();
            let old_up_callback = fixture
                .runner
                .try_lock()
                .unwrap()
                .text_agent
                .test_keyup_callback();
            assert!(
                !fixture
                    .runner
                    .try_lock()
                    .unwrap()
                    .egui_ctx()
                    .input(|input| input.keys_down.contains(&egui::Key::A))
            );
            old.dispatch_event(&egui_probe_shortcut("keydown", "a", "KeyA", meta))
                .unwrap();
            assert!(fixture.runner.try_lock().unwrap().input.raw.events.iter().any(|event| matches!(event, egui::Event::Key { key: egui::Key::A, pressed: true, modifiers, .. } if modifiers.command)));
            old.dispatch_event(&modifier_up(meta)).unwrap();
            // Still no logic boundary: the modifier workaround cannot find queued A in ctx.
            assert!(
                !fixture
                    .runner
                    .try_lock()
                    .unwrap()
                    .egui_ctx()
                    .input(|input| input.keys_down.contains(&egui::Key::A))
            );
            fixture.reset();
            let queued = fixture.runner.try_lock().unwrap().input.raw.events.clone();
            let release = queued.iter().position(|event| {
                matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::A,
                        pressed: false,
                        ..
                    }
                )
            });
            assert!(
                release.is_some(),
                "queued Ctrl/Meta+A was not previously consumed; retirement must release it before fencing its old keyup"
            );
            let fence = queued
                .iter()
                .position(|event| matches!(event, egui::Event::ImeResetCompleted { .. }))
                .unwrap();
            assert!(release.unwrap() < fence);
            assert_eq!(
                queued
                    .iter()
                    .filter(|event| matches!(
                        event,
                        egui::Event::Key {
                            key: egui::Key::A,
                            pressed: false,
                            ..
                        }
                    ))
                    .count(),
                1
            );
            old_up_callback
                .call1(
                    &JsValue::NULL,
                    egui_probe_key("keyup", "a", "KeyA").as_ref(),
                )
                .unwrap();
            assert_eq!(
                fixture.runner.try_lock().unwrap().input.raw.events,
                queued,
                "captured retired callback remains a no-op"
            );
            fixture.runner.try_lock().unwrap().logic();
            assert!(
                !fixture
                    .runner
                    .try_lock()
                    .unwrap()
                    .egui_ctx()
                    .input(|input| input.keys_down.contains(&egui::Key::A)),
                "the next actual AppRunner batch must not leave A stuck"
            );
            assert!(
                fixture
                    .runner
                    .try_lock()
                    .unwrap()
                    .input
                    .keys
                    .press_id(&egui_probe_key("keydown", "a", "KeyA"))
                    .is_none()
            );
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn consumed_shortcut_modifier_up_releases_all_aliases_without_duplicate_retirement_up() {
        for meta in [false, true] {
            for aliases in [false, true] {
                let fixture = Fixture::new();
                let old = fixture.input();
                old.dispatch_event(&egui_probe_shortcut("keydown", "a", "KeyA", meta))
                    .unwrap();
                if aliases {
                    // A second physical alias for the same logical key (e.g. a changed layout).
                    old.dispatch_event(&egui_probe_shortcut("keydown", "a", "KeyQ", meta))
                        .unwrap();
                }
                fixture.runner.try_lock().unwrap().logic();
                assert!(
                    fixture
                        .runner
                        .try_lock()
                        .unwrap()
                        .egui_ctx()
                        .input(|input| input.keys_down.contains(&egui::Key::A))
                );
                old.dispatch_event(&modifier_up(meta)).unwrap();
                assert_eq!(
                    fixture
                        .runner
                        .try_lock()
                        .unwrap()
                        .input
                        .raw
                        .events
                        .iter()
                        .filter(|event| matches!(
                            event,
                            egui::Event::Key {
                                key: egui::Key::A,
                                pressed: false,
                                ..
                            }
                        ))
                        .count(),
                    1
                );
                fixture.reset();
                let lock = fixture.runner.try_lock().unwrap();
                assert!(
                    !lock.input.raw.events.iter().any(|event| matches!(
                        event,
                        egui::Event::Key {
                            key: egui::Key::A,
                            pressed: false,
                            ..
                        }
                    )),
                    "already ordered synthetic release must not be repeated by retirement"
                );
                assert!(
                    lock.input
                        .keys
                        .press_id(&egui_probe_key("keydown", "a", "KeyA"))
                        .is_none()
                );
                assert!(
                    lock.input
                        .keys
                        .press_id(&egui_probe_key("keydown", "a", "KeyQ"))
                        .is_none()
                );
                assert!(
                    !lock
                        .egui_ctx()
                        .input(|input| input.keys_down.contains(&egui::Key::A))
                );
                assert_eq!(
                    lock.input
                        .raw
                        .events
                        .iter()
                        .filter(|event| matches!(event, egui::Event::ImeResetCompleted { .. }))
                        .count(),
                    1
                );
            }
        }
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn modifier_up_then_new_canvas_presses_keep_their_new_receipts_through_reset() {
        for meta in [false, true] {
            let fixture = Fixture::new();
            let old = fixture.input();
            old.dispatch_event(&egui_probe_shortcut("keydown", "a", "KeyA", meta))
                .unwrap();
            old.dispatch_event(&modifier_up(meta)).unwrap();
            super::super::focus_without_scroll(&fixture.canvas).unwrap();
            fixture
                .canvas
                .dispatch_event(&egui_probe_key("keydown", "a", "KeyA"))
                .unwrap();
            fixture
                .canvas
                .dispatch_event(&egui_probe_key("keydown", "b", "KeyB"))
                .unwrap();
            let a = fixture
                .runner
                .try_lock()
                .unwrap()
                .input
                .keys
                .press_id(&egui_probe_key("keydown", "a", "KeyA"))
                .unwrap();
            let b = fixture
                .runner
                .try_lock()
                .unwrap()
                .input
                .keys
                .press_id(&egui_probe_key("keydown", "b", "KeyB"))
                .unwrap();
            fixture.reset();
            let lock = fixture.runner.try_lock().unwrap();
            assert_eq!(
                lock.input
                    .keys
                    .press_id(&egui_probe_key("keydown", "a", "KeyA")),
                Some(a)
            );
            assert_eq!(
                lock.input
                    .keys
                    .press_id(&egui_probe_key("keydown", "b", "KeyB")),
                Some(b)
            );
            assert!(!lock.input.raw.events.iter().any(|event| matches!(
                event,
                egui::Event::Key {
                    key: egui::Key::A | egui::Key::B,
                    pressed: false,
                    ..
                }
            )));
            assert!(
                lock.egui_ctx()
                    .input(|input| input.keys_down.contains(&egui::Key::A)
                        && input.keys_down.contains(&egui::Key::B))
            );
        }
    }
}
