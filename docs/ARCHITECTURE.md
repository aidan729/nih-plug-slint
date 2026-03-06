# nih-plug-slint Architecture

This document explains how the Slint/baseview bridge works. Most users won't need to read this, but it's useful if you're debugging something weird or want to contribute.

## Overview

NIH-plug's `Editor` trait requires implementing `spawn()`, which is called by the host to open the plugin window. We use baseview for the actual OS window, and Slint's FemtoVG renderer (OpenGL) for drawing the UI.

The tricky parts are:
1. Slint needs to be told which platform/renderer to use, and it can only be set once per process
2. The OpenGL context doesn't exist until baseview creates the window, but Slint may try to create the renderer earlier
3. Window close/reopen cycles need to work correctly

## Components

### `SlintEditor<T>`

The public-facing struct that implements `Editor`. It holds the component factory, default size, optional persisted state, and the event loop handler. Nothing interesting happens here until `spawn()` is called.

### `WindowHandler<T>`

Created inside `spawn()` and passed to baseview. This is where the actual work happens - it implements baseview's `WindowHandler` trait and receives `on_frame()` and `on_event()` calls.

### `BaseviewSlintAdapter`

Implements `slint::platform::WindowAdapter`. Slint calls this to get the window size and renderer. The renderer is lazily initialized (stored in a `OnceCell`) because it can only be created once the GL context is current.

### `BaseviewSlintPlatform`

Implements `slint::platform::Platform`. Slint calls `create_window_adapter()` on this when a new component is created. We set this once globally and use a thread-local (`CURRENT_ADAPTER`) to return the right adapter for the current window.

### `BaseviewOpenGLInterface`

Implements `slint::platform::femtovg_renderer::OpenGLInterface`. Mostly no-ops since baseview handles context management - the only real implementation is `get_proc_address`, which delegates to `baseview::gl::GlContext::get_proc_address`.

## How window open/close/reopen works

When `spawn()` is called:
1. We create a `BaseviewSlintAdapter` and store it in `CURRENT_ADAPTER`
2. We call `slint::platform::set_platform()` - this only takes effect the first time; subsequent calls are silently ignored by Slint
3. When Slint creates the component and calls `create_window_adapter()`, it gets the adapter we just stored

When the window is closed, the `WindowHandler` is dropped. When it's reopened, `spawn()` is called again and we store a new adapter in `CURRENT_ADAPTER`. Since the platform is already set, `create_window_adapter()` just picks up the new adapter from thread-local storage.

## GL context and renderer initialization

We can't create the FemtoVG renderer until the GL context is active, but Slint wants to call `renderer()` during component initialization. The fix is:

1. Make the GL context current before creating the component (done in `spawn()`)
2. Store the `GlContext`'s proc-address function in the adapter via `set_gl_context()`
3. The renderer is created lazily in `WindowAdapter::renderer()` using that proc-address function

We also explicitly re-initialize the renderer at the start of the first `on_frame()` call (before calling `component.show()`), so FemtoVG can query `GL_VERSION` with a definitely-current context.

## State persistence

`SlintEditorState` is stored in the plugin's params struct under `#[persist]`, which means NIH-plug/the host handles serialization. We update the `width` and `height` fields directly through the `Arc` when the window is resized - this is technically a data race if the audio thread reads those fields at the same time, but in practice NIH-plug only reads them from the GUI thread.

## Resize handling

There are two resize paths:

- **Programmatic** (from Slint callbacks): Use `handler.pending_resizes()` to push to a queue, which gets processed in `on_frame()`. You can't call `resize()` directly from a Slint callback because you don't have access to `&mut Window`.
- **User-dragged**: baseview sends a `WindowEvent::Resized` which we handle in `on_event()`, also going through the resize queue.

`resize()` updates the internal size, notifies Slint, tells the host via `context.request_resize()`, and then actually resizes the baseview window.

## Data flow

**Plugin → UI:** Read parameter values in the `with_event_loop` handler and push them to Slint component properties each frame.

**UI → Plugin:** Register Slint callbacks (e.g. `component.on_gain_changed(...)`) using `with_setup`. This runs once when the window first opens, before the event loop starts. Prefer this over registering callbacks in `with_event_loop`, since re-registering every frame is wasteful even if harmless. 