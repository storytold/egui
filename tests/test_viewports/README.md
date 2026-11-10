This is a test of the viewports feature of eframe and egui, where we show off using multiple windows.

For a simple example, see [`multiple_viewports`](../../examples/multiple_viewports).

## Shared texture-update ordering regression

Run on an X11 desktop with a window manager and Vulkan support:

```sh
WAYLAND_DISPLAY= WINIT_X11_SCALE_FACTOR=2 WGPU_BACKEND=vulkan \
  cargo run -p test_viewports --features wgpu -- --texture-order-regression
```

The regression opens two windows, minimizes its parent, queues a full texture resize from
32×32 to 64×64 after the immediate child paints, then uploads a 32×54 partial update from
the child. Separate viewport queues can upload that partial update into the old 32-pixel-high
GPU texture. The parent is restored and its rendered pixels must contain both the green
partial update and the untouched red region. Both windows close automatically; failure to
receive a screenshot within 15 seconds fails the regression.
