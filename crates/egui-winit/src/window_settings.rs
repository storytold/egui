use egui::ViewportBuilder;

/// On Mac and Windows, [`WindowSettings::initialize_window`] moves the window to its saved position
/// after it is created, and only then maximizes it or makes it fullscreen.
const PLACE_AFTER_CREATION: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// Position and size of a native window.
///
/// Windows and Linux position windows in physical pixels, which are unambiguous
/// across monitors with different scale factors. Mac positions them in logical points.
/// We store the position in pixels together with the scale factor it was saved with,
/// so it can be converted to the logical units winit and Mac expect at restore time.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct WindowGeometry {
    /// Position of window frame/titlebar in physical pixels.
    outer_position_pixels: Option<egui::Pos2>,

    /// Scale factor of the window when it was saved, without egui zoom.
    ///
    /// Needed to convert `outer_position_pixels` to logical units, which
    /// `ViewportBuilder::with_position` and Mac's `set_outer_position` take.
    /// Stored because at restore time there is no window to ask yet, and the
    /// monitor winit creates it on may not be the one it was saved on.
    scale_factor: f32,

    /// Inner size of window in points.
    inner_size_points: egui::Vec2,

    /// Interface zoom used for `inner_size_points`. Older settings did not store it and
    /// relied on egui memory restoring the same zoom before the window was created.
    #[cfg_attr(feature = "serde", serde(default))]
    zoom_factor: Option<f32>,
}

impl WindowGeometry {
    /// Capture a normal, non-minimized window, including a hidden window's startup geometry.
    /// On Windows the native show state can change before winit's cached maximized flag is
    /// updated by the subsequent `WM_SIZE` message.
    pub fn from_normal_window(
        egui_zoom_factor: f32,
        window: &winit::window::Window,
    ) -> Option<Self> {
        (window.is_minimized() != Some(true)
            && !is_maximized(window)
            && window.fullscreen().is_none())
        .then(|| Self::from_window(egui_zoom_factor, window))
    }

    pub fn from_window(egui_zoom_factor: f32, window: &winit::window::Window) -> Self {
        let scale_factor = window.scale_factor() as f32;
        let inner_size = window.inner_size();
        Self {
            outer_position_pixels: window
                .outer_position()
                .ok()
                .map(|p| egui::pos2(p.x as f32, p.y as f32)),
            scale_factor,
            inner_size_points: egui::vec2(inner_size.width as f32, inner_size.height as f32)
                / (egui_zoom_factor * scale_factor),
            zoom_factor: Some(egui_zoom_factor),
        }
    }
}

/// winit emits Moved from `WM_WINDOWPOSCHANGED` before `DefWindowProc` sends `WM_SIZE`,
/// where its cached maximized flag is updated. Read the native state during that interval
/// so a maximized rectangle cannot overwrite the last normal one.
fn is_maximized(window: &winit::window::Window) -> bool {
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
        use windows_sys::Win32::Foundation::{POINT, RECT};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowPlacement, SW_SHOWMAXIMIZED, WINDOWPLACEMENT,
        };

        if let Ok(RawWindowHandle::Win32(handle)) = window.window_handle().map(|h| h.as_raw()) {
            let mut placement = WINDOWPLACEMENT {
                length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                flags: 0,
                showCmd: 0,
                ptMinPosition: POINT { x: 0, y: 0 },
                ptMaxPosition: POINT { x: 0, y: 0 },
                rcNormalPosition: RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
            };
            // SAFETY: the borrowed window owns this HWND and placement is a correctly sized,
            // initialized output buffer, valid for the duration of the synchronous call.
            #[expect(unsafe_code)]
            if unsafe { GetWindowPlacement(handle.hwnd.get() as _, &raw mut placement) } != 0 {
                return placement.showCmd == SW_SHOWMAXIMIZED as u32;
            }
        }
    }
    window.is_maximized()
}

/// Can be used to store native window settings (position and size).
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct WindowSettings {
    /// Position and size when neither maximized nor fullscreen.
    normal_geometry: WindowGeometry,

    fullscreen: bool,

    maximized: bool,
}

impl WindowSettings {
    /// The state explicitly requested for a first launch, before anything has been persisted.
    /// X11 needs maximize/fullscreen requests repeated after mapping even in this case.
    pub fn requested_at_start(
        egui_zoom_factor: f32,
        window: &winit::window::Window,
        viewport: &ViewportBuilder,
    ) -> Option<Self> {
        let maximized = viewport.maximized.unwrap_or(false);
        let fullscreen = viewport.fullscreen.unwrap_or(false);
        if !maximized && !fullscreen {
            return None;
        }
        let mut normal_geometry = WindowGeometry::from_window(egui_zoom_factor, window);
        if let Some(size) = viewport.inner_size {
            normal_geometry.inner_size_points = size;
        }
        if let Some(position) = viewport.position {
            normal_geometry.outer_position_pixels =
                Some(position * (egui_zoom_factor * normal_geometry.scale_factor));
        }
        Some(Self {
            normal_geometry,
            fullscreen,
            maximized,
        })
    }

    /// Zoom in which the saved inner size is expressed, with compatibility for old settings.
    pub fn zoom_factor(&self, fallback: f32) -> f32 {
        self.normal_geometry
            .zoom_factor
            .filter(|z| z.is_finite() && (0.01..=100.0).contains(z))
            .unwrap_or(fallback)
    }

    /// While maximized or fullscreen, keeps `last_normal_geometry`, so the window restores to it.
    pub fn from_window(
        egui_zoom_factor: f32,
        window: &winit::window::Window,
        last_normal_geometry: Option<WindowGeometry>,
    ) -> Self {
        let fullscreen = window.fullscreen().is_some();
        let maximized = is_maximized(window);
        let normal_geometry = last_normal_geometry
            .filter(|_| fullscreen || maximized || window.is_minimized() == Some(true))
            .unwrap_or_else(|| WindowGeometry::from_window(egui_zoom_factor, window));
        Self {
            normal_geometry,
            fullscreen,
            maximized,
        }
    }

    pub fn inner_size_points(&self) -> egui::Vec2 {
        self.normal_geometry.inner_size_points
    }

    /// Position and size when neither maximized nor fullscreen.
    pub fn normal_geometry(&self) -> WindowGeometry {
        self.normal_geometry
    }

    pub fn initialize_viewport_builder(
        &self,
        egui_zoom_factor: f32,
        mut viewport_builder: ViewportBuilder,
    ) -> ViewportBuilder {
        profiling::function_scope!();

        let geometry = &self.normal_geometry;

        if let Some(pos) = geometry.outer_position_pixels {
            // On Windows, winit converts this back to pixels with the scale factor of the monitor
            // the window is created on, which can differ from `geometry.scale_factor`.
            // So on Mac and Windows, `initialize_window` sets the saved position again.
            viewport_builder =
                viewport_builder.with_position(pos / (egui_zoom_factor * geometry.scale_factor));
        }

        viewport_builder = viewport_builder.with_inner_size(
            geometry.inner_size_points * (self.zoom_factor(egui_zoom_factor) / egui_zoom_factor),
        );
        if PLACE_AFTER_CREATION {
            // Moving a maximized or fullscreen window resets it,
            // so `initialize_window` maximizes or enters fullscreen after moving.
            viewport_builder
                .with_fullscreen(false)
                .with_maximized(false)
        } else {
            viewport_builder
                .with_fullscreen(self.fullscreen)
                .with_maximized(self.maximized)
        }
    }

    pub fn initialize_window(&self, window: &winit::window::Window) {
        if !PLACE_AFTER_CREATION {
            return;
        }

        // The position from `initialize_viewport_builder` can be off:
        // on Windows, winit converts it to pixels with the scale factor of the monitor the window
        // was created on, not the one we saved it on.
        // On Mac, windows were sometimes not restored onto secondary monitors from it alone.
        // So set the saved position directly, in the unit each platform positions windows in:
        let geometry = &self.normal_geometry;
        if let Some(pos) = geometry.outer_position_pixels {
            if cfg!(target_os = "macos") {
                // Mac places windows in points. Pixels are ambiguous across monitors with different scale factors.
                let pos = pos / geometry.scale_factor;
                #[expect(
                    clippy::disallowed_types,
                    reason = "these are OS points, unaffected by egui zoom"
                )]
                window.set_outer_position(winit::dpi::LogicalPosition::new(pos.x, pos.y));
            } else {
                window.set_outer_position(winit::dpi::PhysicalPosition::new(pos.x, pos.y));
            }
        }

        if self.fullscreen && cfg!(target_os = "macos") {
            // Mac only enters fullscreen once the window is visible, animating from its current frame.
            // Maximize so the first frame fills the screen,
            // and enter fullscreen in `before_first_show`.
            window.set_maximized(true);
        } else if self.fullscreen {
            window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        } else if self.maximized {
            crate::set_maximized(window, true);
        }
    }

    /// Call once the first frame is painted, right before showing the window.
    pub fn before_first_show(&self, window: &winit::window::Window) {
        if cfg!(target_os = "macos") && self.fullscreen {
            // Enter the fullscreen deferred by `initialize_window`.
            // Un-maximize first, so the window leaves fullscreen to its normal frame:
            window.set_maximized(false);
            window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        }
    }

    /// Call right after showing the window for the first time.
    pub fn after_first_show(&self, window: &winit::window::Window) {
        // X11 window managers ignore maximize and fullscreen requests for hidden windows,
        // so the ones from `initialize_viewport_builder` are lost:
        if is_x11(window) {
            if self.fullscreen {
                window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            } else if self.maximized {
                window.set_maximized(true);
            }
        }
    }

    pub fn clamp_size_to_sane_values(&mut self, largest_monitor_size_points: egui::Vec2) {
        use egui::NumExt as _;

        let size = &mut self.normal_geometry.inner_size_points;

        // Prevent ridiculously small windows:
        let min_size = egui::Vec2::splat(64.0);
        *size = size.at_least(min_size);

        // Make sure we don't try to create a window larger than the largest monitor
        // because on Linux that can lead to a crash.
        *size = size.at_most(largest_monitor_size_points);
    }

    pub fn clamp_position_to_monitors(
        &mut self,
        egui_zoom_factor: f32,
        event_loop: &winit::event_loop::ActiveEventLoop,
    ) {
        let egui_zoom_factor = self.zoom_factor(egui_zoom_factor);
        // If the app last ran on two monitors and only one is now connected, then
        // the given position is invalid.
        // If this happens on Mac, a maximized window gets zero size, so we drop the position.
        // If this happens on Windows, the window becomes invisible to the user 🤦‍♂️
        // So on Windows we clamp the position to the monitor it is on.
        let geometry = &mut self.normal_geometry;
        let Some(pos_px) = geometry.outer_position_pixels else {
            return;
        };

        if cfg!(target_os = "macos") {
            // Pixels mean different points on each Mac monitor, so compare in points:
            let pos = pos_px / geometry.scale_factor;
            let is_on_a_monitor = event_loop.available_monitors().any(|monitor| {
                (monitor_rect_px(&monitor) / monitor.scale_factor() as f32).contains(pos)
            });
            if !is_on_a_monitor {
                geometry.outer_position_pixels = None;
            }
        } else if cfg!(target_os = "windows") {
            let mut pos_px = pos_px;
            clamp_pos_to_monitors(
                egui_zoom_factor,
                event_loop,
                geometry.inner_size_points,
                &mut pos_px,
            );
            geometry.outer_position_pixels = Some(pos_px);
        }
    }
}

#[cfg(feature = "x11")]
fn is_x11(window: &winit::window::Window) -> bool {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    matches!(
        window.window_handle().map(|h| h.as_raw()),
        Ok(RawWindowHandle::Xlib(_) | RawWindowHandle::Xcb(_))
    )
}

#[cfg(not(feature = "x11"))]
fn is_x11(_window: &winit::window::Window) -> bool {
    false
}

fn find_active_monitor(
    egui_zoom_factor: f32,
    event_loop: &winit::event_loop::ActiveEventLoop,
    window_size_pts: egui::Vec2,
    position_px: &egui::Pos2,
) -> Option<winit::monitor::MonitorHandle> {
    profiling::function_scope!();
    let monitors = event_loop.available_monitors();

    // default to primary monitor, in case the correct monitor was disconnected.
    let Some(mut active_monitor) = event_loop
        .primary_monitor()
        .or_else(|| event_loop.available_monitors().next())
    else {
        return None; // no monitors 🤷
    };

    let mut active_monitor_overlap = 0.0;
    for monitor in monitors {
        let window_size_px = window_size_pts * (egui_zoom_factor * monitor.scale_factor() as f32);
        let window_rect = egui::Rect::from_min_size(*position_px, window_size_px);
        let overlap = window_rect.intersect(monitor_rect_px(&monitor)).area();

        if active_monitor_overlap < overlap {
            active_monitor = monitor;
            active_monitor_overlap = overlap;
        }
    }

    Some(active_monitor)
}

fn monitor_rect_px(monitor: &winit::monitor::MonitorHandle) -> egui::Rect {
    let pos = monitor.position();
    let size = monitor.size();
    egui::Rect::from_min_size(
        egui::pos2(pos.x as f32, pos.y as f32),
        egui::vec2(size.width as f32, size.height as f32),
    )
}

fn clamp_pos_to_monitors(
    egui_zoom_factor: f32,
    event_loop: &winit::event_loop::ActiveEventLoop,
    window_size_pts: egui::Vec2,
    position_px: &mut egui::Pos2,
) {
    profiling::function_scope!();

    let Some(active_monitor) =
        find_active_monitor(egui_zoom_factor, event_loop, window_size_pts, position_px)
    else {
        return; // no monitors 🤷
    };

    let mut window_size_px =
        window_size_pts * (egui_zoom_factor * active_monitor.scale_factor() as f32);
    // Add size of title bar. This is 32 px by default in Win 10/11.
    if cfg!(target_os = "windows") {
        window_size_px += egui::Vec2::new(
            0.0,
            32.0 * egui_zoom_factor * active_monitor.scale_factor() as f32,
        );
    }
    let monitor_rect = monitor_rect_px(&active_monitor);

    // Window size cannot be negative or the subsequent `clamp` will panic.
    let window_size = (monitor_rect.size() - window_size_px).max(egui::Vec2::ZERO);
    // To get the maximum position, we get the rightmost corner of the display, then
    // subtract the size of the window to get the bottom right most value window.position
    // can have.
    *position_px = position_px.clamp(monitor_rect.min, monitor_rect.min + window_size);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(zoom: Option<f32>) -> WindowSettings {
        WindowSettings {
            normal_geometry: WindowGeometry {
                outer_position_pixels: Some(egui::pos2(120.0, 90.0)),
                scale_factor: 1.5,
                inner_size_points: egui::vec2(800.0, 600.0),
                zoom_factor: zoom,
            },
            maximized: false,
            fullscreen: false,
        }
    }

    /// Exercise the native title-bar maximize message, including Moved before `WM_SIZE`.
    /// Programmatic maximization inside a winit callback buffers those events and hides the race.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_title_bar_maximize_preserves_normal_geometry() {
        use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            PostMessageW, SC_MAXIMIZE, SC_RESTORE, WM_SYSCOMMAND,
        };
        use winit::{
            application::ApplicationHandler,
            event::WindowEvent,
            event_loop::{ActiveEventLoop, EventLoop},
            platform::{
                pump_events::EventLoopExtPumpEvents as _, windows::EventLoopBuilderExtWindows as _,
            },
            window::{Window, WindowId},
        };

        #[derive(Default)]
        struct Probe {
            window: Option<Window>,
            geometry: Option<WindowGeometry>,
            expected: Option<egui::Vec2>,
            phase: u8,
            saw_stale_flag: bool,
            complete: bool,
        }

        impl ApplicationHandler for Probe {
            fn resumed(&mut self, event_loop: &ActiveEventLoop) {
                let window = event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title("egui normal geometry regression")
                            .with_visible(false)
                            .with_inner_size(winit::dpi::LogicalSize::new(640.0, 480.0)),
                    )
                    .unwrap();
                self.geometry = WindowGeometry::from_normal_window(1.5, &window);
                assert!(
                    self.geometry.is_some(),
                    "a hidden normal window must seed startup geometry"
                );
                window.set_visible(true);
                window.request_redraw();
                self.window = Some(window);
            }

            fn window_event(
                &mut self,
                event_loop: &ActiveEventLoop,
                _: WindowId,
                event: WindowEvent,
            ) {
                let window = self.window.as_ref().unwrap();
                if self.phase == 1 && is_maximized(window) && !window.is_maximized() {
                    self.saw_stale_flag = true;
                }
                if let Some(geometry) = WindowGeometry::from_normal_window(1.5, window) {
                    self.geometry = Some(geometry);
                }
                if event != WindowEvent::RedrawRequested {
                    return;
                }
                let command = match self.phase {
                    0 => {
                        self.expected = self.geometry.map(|g| g.inner_size_points);
                        assert!(self.expected.is_some());
                        self.phase = 1;
                        Some(SC_MAXIMIZE)
                    }
                    1 if is_maximized(window) => {
                        assert_eq!(
                            self.geometry.unwrap().inner_size_points,
                            self.expected.unwrap()
                        );
                        let saved = WindowSettings::from_window(1.5, window, self.geometry);
                        assert!(saved.maximized);
                        let reopened =
                            saved.initialize_viewport_builder(1.0, ViewportBuilder::default());
                        assert_eq!(reopened.inner_size, self.expected.map(|size| size * 1.5));
                        self.phase = 2;
                        Some(SC_RESTORE)
                    }
                    2 if !is_maximized(window) => {
                        let restored = WindowGeometry::from_window(1.5, window);
                        assert!(
                            (restored.inner_size_points - self.expected.unwrap()).length() < 1.0
                        );
                        self.complete = true;
                        event_loop.exit();
                        None
                    }
                    _ => None,
                };
                if let Some(command) = command {
                    let RawWindowHandle::Win32(handle) = window.window_handle().unwrap().as_raw()
                    else {
                        panic!("expected a Windows window");
                    };
                    // SAFETY: the live window owns this HWND; WM_SYSCOMMAND has integer arguments
                    // and the queued title-bar command borrows no memory from this stack.
                    #[expect(unsafe_code)]
                    let posted = unsafe {
                        PostMessageW(handle.hwnd.get() as _, WM_SYSCOMMAND, command as usize, 0)
                    };
                    assert_ne!(posted, 0);
                }
            }
        }

        let mut event_loop = EventLoop::builder().with_any_thread(true).build().unwrap();
        let mut probe = Probe::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !probe.complete && std::time::Instant::now() < deadline {
            event_loop.pump_app_events(Some(std::time::Duration::from_millis(20)), &mut probe);
        }
        assert!(probe.complete, "native maximize/restore did not finish");
        assert!(
            probe.saw_stale_flag,
            "did not exercise the Moved-before-WM_SIZE interval"
        );
    }

    #[test]
    fn saved_size_survives_a_different_startup_zoom() {
        for saved_zoom in [0.75, 1.0, 1.25, 1.5, 2.0, 3.0] {
            for startup_zoom in [0.75, 1.0, 1.5, 2.0] {
                let builder = settings(Some(saved_zoom))
                    .initialize_viewport_builder(startup_zoom, ViewportBuilder::default());
                let logical = builder.inner_size.unwrap() * startup_zoom;
                assert!((logical - egui::vec2(800.0, 600.0) * saved_zoom).length() < 0.001);
                assert!(
                    (builder.position.unwrap() * startup_zoom - egui::pos2(80.0, 60.0)).length()
                        < 0.001
                );
            }
        }
    }

    #[test]
    fn old_or_invalid_zoom_keeps_the_previous_restore_semantics() {
        for zoom in [
            None,
            Some(0.0),
            Some(-1.0),
            Some(f32::NAN),
            Some(f32::INFINITY),
            Some(f32::MAX),
        ] {
            let builder =
                settings(zoom).initialize_viewport_builder(1.5, ViewportBuilder::default());
            assert_eq!(builder.inner_size, Some(egui::vec2(800.0, 600.0)));
        }
    }
}
