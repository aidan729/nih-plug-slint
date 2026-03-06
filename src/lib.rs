use baseview::{
    gl::GlConfig, Event, Size, Window, WindowEvent as BaseviewWindowEvent, WindowHandle,
    WindowInfo, WindowOpenOptions, WindowScalePolicy,
};
use nih_plug::params::Param;
use nih_plug::prelude::{Editor, GuiContext, ParamSetter};
use once_cell::unsync::OnceCell;
use slint::platform::femtovg_renderer::FemtoVGRenderer;
use slint::platform::WindowAdapter;
use slint::{LogicalPosition, PhysicalSize};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

pub use baseview::{DropData, DropEffect, EventStatus, MouseEvent};

// Re export slint so users can access it
pub use slint;

use serde::{Deserialize, Serialize};

type EventLoopHandler<T> = dyn Fn(&WindowHandler<T>, ParamSetter, &mut Window) + Send + Sync;
type SetupHandler<T> = dyn Fn(&WindowHandler<T>, &mut Window) + Send + Sync;

/// Window size/state that gets persisted via NIH-plug's `#[persist]` mechanism.
///
/// Put this in your params struct so the host can save and restore the window size:
///
/// ```rust,ignore
/// #[derive(Params)]
/// struct MyParams {
///     #[persist = "editor-state"]
///     editor_state: Arc<SlintEditorState>,
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlintEditorState {
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_scale")]
    pub scale_factor: f32,
}

fn default_width() -> u32 {
    400
}
fn default_height() -> u32 {
    300
}
fn default_scale() -> f32 {
    1.0
}

impl Default for SlintEditorState {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            scale_factor: default_scale(),
        }
    }
}

impl SlintEditorState {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            scale_factor: 1.0,
        }
    }

    pub fn with_scale(width: u32, height: u32, scale_factor: f32) -> Self {
        Self {
            width,
            height,
            scale_factor,
        }
    }
}

/// The NIH-plug [`Editor`] implementation for Slint UIs.
///
/// Build one with [`SlintEditor::with_factory`], optionally chaining
/// [`with_state`][Self::with_state] for window-size persistence and
/// [`with_event_loop`][Self::with_event_loop] to sync parameters each frame.
///
/// ```rust,ignore
/// fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
///     Some(Box::new(
///         SlintEditor::with_factory(|| gui::AppWindow::new(), (400, 300))
///             .with_state(self.params.editor_state.clone())
///             .with_event_loop({
///                 let params = self.params.clone();
///                 move |handler, _setter, _window| {
///                     handler.component().set_gain(params.gain.value());
///                 }
///             }),
///     ))
/// }
/// ```
pub struct SlintEditor<T: slint::ComponentHandle> {
    component_factory: Arc<dyn Fn() -> Result<T, slint::PlatformError> + Send + Sync>,
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
    state: Option<Arc<SlintEditorState>>,
    event_loop_handler: Arc<EventLoopHandler<T>>,
    setup_handler: Arc<SetupHandler<T>>,
}

impl<T: slint::ComponentHandle + 'static> SlintEditor<T> {
    /// Create an editor from a factory closure.  `size` is the default window size in
    /// logical pixels; it's overridden by the persisted state if you call [`with_state`][Self::with_state].
    pub fn with_factory<F>(factory: F, size: (u32, u32)) -> Self
    where
        F: Fn() -> Result<T, slint::PlatformError> + 'static + Send + Sync,
    {
        Self {
            component_factory: Arc::new(factory),
            width: Arc::new(AtomicU32::new(size.0)),
            height: Arc::new(AtomicU32::new(size.1)),
            state: None,
            event_loop_handler: Arc::new(|_, _, _| {}),
            setup_handler: Arc::new(|_, _| {}),
        }
    }

    /// Load the initial window size from `state` and write back to it on resize.
    /// Pass an `Arc<SlintEditorState>` stored under `#[persist]` in your params.
    pub fn with_state(mut self, state: Arc<SlintEditorState>) -> Self {
        self.width = Arc::new(AtomicU32::new(state.width));
        self.height = Arc::new(AtomicU32::new(state.height));
        self.state = Some(state);
        self
    }

    pub fn with_setup<F>(mut self, handler: F) -> Self
    where
        F: Fn(&WindowHandler<T>, &mut Window) + 'static + Send + Sync,
    {
        self.setup_handler = Arc::new(handler);
        self
    }

    /// Set the handler called every frame. Use it to push parameter values to the UI
    /// and register Slint callbacks for UI → plugin communication.
    pub fn with_event_loop<F>(mut self, handler: F) -> Self
    where
        F: Fn(&WindowHandler<T>, ParamSetter, &mut Window) + 'static + Send + Sync,
    {
        self.event_loop_handler = Arc::new(handler);
        self
    }
}

/// OpenGL interface implementation for baseview.
///
/// Delegates all GL symbol resolution to baseview's `GlContext::get_proc_address`,
/// which handles the platform-specific details (WGL on Windows, dlsym on Unix).
///
/// The inner function is stored in an `Arc` so this type can be cheaply cloned
/// when the `FemtoVGRenderer` is created from inside `WindowAdapter::renderer()`.
#[derive(Clone)]
struct BaseviewOpenGLInterface {
    get_proc_address: Arc<dyn Fn(&str) -> *const core::ffi::c_void + Send + Sync>,
}

impl BaseviewOpenGLInterface {
    fn new(window: &baseview::Window) -> Self {
        // Store the GlContext address as a plain usize so that the closure is Send + Sync.
        //
        // SAFETY: The `GlContext` is owned by the `Window` and lives as long as the window is
        // open.  The `FemtoVGRenderer` (and therefore this interface) is dropped before the
        // window closes, so the pointer is valid for the entire lifetime of the renderer.
        // We only dereference it on the GUI thread (inside FemtoVG's GL loader callback).
        let ctx_addr = window
            .gl_context()
            .expect("window must have an OpenGL context")
            as *const baseview::gl::GlContext as usize;

        Self {
            get_proc_address: Arc::new(move |name: &str| {
                let ctx = ctx_addr as *const baseview::gl::GlContext;
                // SAFETY: see constructor comment above.
                unsafe { &*ctx }.get_proc_address(name)
            }),
        }
    }
}

unsafe impl slint::platform::femtovg_renderer::OpenGLInterface for BaseviewOpenGLInterface {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // baseview makes the context current before calling on_frame
        Ok(())
    }

    fn swap_buffers(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // baseview handles buffer swapping (we call swap_buffers manually at end of frame)
        Ok(())
    }

    fn resize(
        &self,
        _width: core::num::NonZeroU32,
        _height: core::num::NonZeroU32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Resize is handled via WindowAdapter::size()
        Ok(())
    }

    fn get_proc_address(&self, name: &core::ffi::CStr) -> *const core::ffi::c_void {
        let name = name.to_str().unwrap_or("");
        (self.get_proc_address)(name)
    }
}

// Thread-local storage for the current adapter
// Holds the active adapter so that BaseviewSlintPlatform::create_window_adapter can
// return it.  Updated each time a window is opened.
thread_local! {
    static CURRENT_ADAPTER: RefCell<Option<Rc<BaseviewSlintAdapter>>> = RefCell::new(None);
}

/// Platform implementation for Slint
struct BaseviewSlintPlatform;

impl slint::platform::Platform for BaseviewSlintPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        CURRENT_ADAPTER.with(|adapter| {
            adapter
                .borrow()
                .clone()
                .map(|a| a as Rc<dyn WindowAdapter>)
                .ok_or_else(|| slint::PlatformError::Other("No adapter set".into()))
        })
    }
}

/// Custom WindowAdapter that bridges baseview and Slint
struct BaseviewSlintAdapter {
    window: slint::Window,
    renderer: OnceCell<FemtoVGRenderer>,
    /// Physical size in actual pixels (for the OpenGL framebuffer)
    physical_size: RefCell<PhysicalSize>,
    /// Scale factor (e.g., 2.0 on Retina displays)
    scale_factor: RefCell<f32>,
    /// Stored proc-address loader, set once the GL context is available
    gl_interface: OnceCell<BaseviewOpenGLInterface>,
}

impl BaseviewSlintAdapter {
    fn new(physical_width: u32, physical_height: u32, scale_factor: f32) -> Rc<Self> {
        Rc::new_cyclic(|weak_self| {
            let window = slint::Window::new(weak_self.clone() as _);
            Self {
                window,
                renderer: OnceCell::new(),
                physical_size: RefCell::new(PhysicalSize::new(physical_width, physical_height)),
                scale_factor: RefCell::new(scale_factor),
                gl_interface: OnceCell::new(),
            }
        })
    }

    /// Call once after the GL context is live to wire up the proc-address loader.
    fn set_gl_context(&self, window: &baseview::Window) {
        let _ = self.gl_interface.set(BaseviewOpenGLInterface::new(window));
    }

    /// Update the size and scale factor (called when window is resized or scale changes)
    fn update_size(&self, physical_width: u32, physical_height: u32, scale_factor: f32) {
        *self.physical_size.borrow_mut() = PhysicalSize::new(physical_width, physical_height);
        *self.scale_factor.borrow_mut() = scale_factor;
    }
}

impl WindowAdapter for BaseviewSlintAdapter {
    fn window(&self) -> &slint::Window {
        &self.window
    }

    fn size(&self) -> PhysicalSize {
        *self.physical_size.borrow()
    }

    fn renderer(&self) -> &dyn slint::platform::Renderer {
        self.renderer.get_or_init(|| {
            let interface = self
                .gl_interface
                .get()
                .expect("GL context must be set via set_gl_context() before renderer() is called");
            FemtoVGRenderer::new(interface.clone()).expect("Failed to create FemtoVG renderer")
        })
    }

    fn request_redraw(&self) {
        // baseview handles redraws in on_frame
    }
}

/// Per-window state, passed to the event loop handler each frame.
pub struct WindowHandler<T: slint::ComponentHandle> {
    context: Arc<dyn GuiContext>,
    event_loop_handler: Arc<EventLoopHandler<T>>,
    setup_handler: Arc<SetupHandler<T>>,
    pub width: Arc<AtomicU32>,
    pub height: Arc<AtomicU32>,
    scale_factor: RefCell<f32>,
    state: Option<Arc<SlintEditorState>>,
    // Rc so it can be shared with Slint callbacks without needing &mut self
    pending_resizes: Rc<RefCell<Vec<(u32, u32)>>>,
    last_cursor_pos: RefCell<LogicalPosition>,
    window_shown: RefCell<bool>,
    component: T,
    adapter: Rc<BaseviewSlintAdapter>,
}

impl<T: slint::ComponentHandle> WindowHandler<T> {
    /// Resize the window. `width` and `height` are in logical pixels.
    pub fn resize(&self, window: &mut baseview::Window, width: u32, height: u32) {
        let scale = *self.scale_factor.borrow();
        let physical_width = (width as f32 * scale) as u32;
        let physical_height = (height as f32 * scale) as u32;

        self.width.store(width, Ordering::Relaxed);
        self.height.store(height, Ordering::Relaxed);

        // Update adapter with physical size and scale factor
        self.adapter
            .update_size(physical_width, physical_height, scale);

        // Notify Slint window of new size to trigger re-layout
        // Slint expects logical size here
        let slint_window = self.window();
        slint_window.dispatch_event(slint::platform::WindowEvent::Resized {
            size: slint::LogicalSize::new(width as f32, height as f32),
        });

        // Request redraw to show changes immediately
        slint_window.request_redraw();

        // Notify host
        self.context.request_resize();

        // Resize baseview window (uses logical size, baseview handles physical conversion)
        window.resize(Size {
            width: width as f64,
            height: height as f64,
        });

        // Persist the new size if state is available.
        //
        // SAFETY: We write through an Arc that is shared with the audio thread.
        // This is technically a data race if the audio thread reads width/height concurrently.
        // In practice NIH-plug only reads these fields from the GUI thread (in `spawn`),
        // so there is no concurrent access.  A future improvement would be to store these
        // as `AtomicU32` fields inside `SlintEditorState`.
        if let Some(state) = &self.state {
            let state_ptr = Arc::as_ptr(state) as *mut SlintEditorState;
            unsafe {
                (*state_ptr).width = width;
                (*state_ptr).height = height;
            }
        }
    }

    /// Handle a window info update from baseview (scale factor or size change)
    fn handle_window_info(&self, info: &WindowInfo) {
        let scale = info.scale() as f32;
        let physical_size = info.physical_size();

        *self.scale_factor.borrow_mut() = scale;

        // Update adapter with physical size
        self.adapter
            .update_size(physical_size.width, physical_size.height, scale);

        // Update our logical size tracking
        let logical_size = info.logical_size();
        self.width
            .store(logical_size.width as u32, Ordering::Relaxed);
        self.height
            .store(logical_size.height as u32, Ordering::Relaxed);

        // Notify Slint of the new size (logical)
        self.adapter
            .window
            .dispatch_event(slint::platform::WindowEvent::Resized {
                size: slint::LogicalSize::new(
                    logical_size.width as f32,
                    logical_size.height as f32,
                ),
            });

        // Also set the scale factor on the Slint window
        self.adapter
            .window
            .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
    }

    /// Queue a resize to be applied next frame. Use this from Slint callbacks where
    /// you don't have access to `&mut Window`.
    pub fn queue_resize(&self, width: u32, height: u32) {
        self.pending_resizes.borrow_mut().push((width, height));
    }

    /// Returns the resize queue so you can clone the `Rc` and push to it from callbacks.
    pub fn pending_resizes(&self) -> &Rc<RefCell<Vec<(u32, u32)>>> {
        &self.pending_resizes
    }

    pub fn process_pending_resizes(&self, window: &mut baseview::Window) -> Option<(u32, u32)> {
        let mut queue = self.pending_resizes.borrow_mut();
        if let Some((width, height)) = queue.pop() {
            // Only process the most recent resize request to avoid lag
            queue.clear();
            drop(queue); // Release the borrow before calling resize

            self.resize(window, width, height);
            Some((width, height))
        } else {
            None
        }
    }

    pub fn component(&self) -> &T {
        &self.component
    }

    pub fn window(&self) -> &slint::Window {
        &self.adapter.window
    }

    pub fn context(&self) -> &Arc<dyn GuiContext> {
        &self.context
    }

    pub fn set_parameter_normalized(&self, param: &impl nih_plug::prelude::Param, normalized: f32) {
        let setter = ParamSetter::new(&*self.context);
        setter.set_parameter_normalized(param, normalized);
    }

    pub fn begin_set_parameter(&self, param: &impl nih_plug::prelude::Param) {
        let setter = ParamSetter::new(&*self.context);
        setter.begin_set_parameter(param);
    }

    pub fn end_set_parameter(&self, param: &impl nih_plug::prelude::Param) {
        let setter = ParamSetter::new(&*self.context);
        setter.end_set_parameter(param);
    }
}

impl<T: slint::ComponentHandle> baseview::WindowHandler for WindowHandler<T> {
    fn on_frame(&mut self, window: &mut baseview::Window) {
        // Make the GL context current for this frame.
        unsafe { window.gl_context().unwrap().make_current() };

        // On first frame: initialize the renderer and show the component.
        // We defer this until on_frame (rather than doing it in spawn's closure)
        // so the GL context is guaranteed to be current when FemtoVG queries GL_VERSION.
        if !*self.window_shown.borrow() {
            self.adapter.set_gl_context(window);
            let _ = self.adapter.renderer.get_or_init(|| {
                self.adapter
                    .gl_interface
                    .get()
                    .map(|iface| {
                        FemtoVGRenderer::new(iface.clone())
                            .expect("Failed to create FemtoVG renderer")
                    })
                    .expect("gl_interface must be set before renderer init")
            });
            self.component.show().expect("Failed to show component");
            *self.window_shown.borrow_mut() = true;

            // This fires once, allowing users to register parameter update callbacks for UI -> plugin one time before the event loop starts
            (self.setup_handler)(self, window);
        }

        // Call custom event loop handler first
        let setter = ParamSetter::new(&*self.context);
        (self.event_loop_handler)(&self, setter, window);

        // Update Slint timers and animations
        slint::platform::update_timers_and_animations();

        // Render the component - Slint handles the rendering internally
        // It will call our WindowAdapter's renderer() method when needed
        self.component.window().request_redraw();

        // Process Slint's internal rendering queue
        // This is where Slint actually renders using our FemtoVG renderer
        slint::platform::duration_until_next_timer_update();

        // CRITICAL: Actually trigger the render by accessing the renderer
        // Slint's FemtoVG renderer needs to be explicitly told to render
        if let Some(renderer) = self.adapter.renderer.get() {
            let _ = renderer.render();
        }

        // Process pending resizes
        self.process_pending_resizes(window);

        // Swap buffers after rendering
        window.gl_context().unwrap().swap_buffers();
    }

    fn on_event(&mut self, _window: &mut baseview::Window, event: Event) -> EventStatus {
        use slint::platform::WindowEvent;

        match event {
            Event::Mouse(mouse_event) => {
                // Convert baseview mouse event to Slint event
                let slint_event = match mouse_event {
                    baseview::MouseEvent::CursorMoved { position, .. } => {
                        let pos = LogicalPosition::new(position.x as f32, position.y as f32);
                        *self.last_cursor_pos.borrow_mut() = pos;
                        WindowEvent::PointerMoved { position: pos }
                    }
                    baseview::MouseEvent::ButtonPressed { button, .. } => {
                        let slint_button = match button {
                            baseview::MouseButton::Left => {
                                slint::platform::PointerEventButton::Left
                            }
                            baseview::MouseButton::Right => {
                                slint::platform::PointerEventButton::Right
                            }
                            baseview::MouseButton::Middle => {
                                slint::platform::PointerEventButton::Middle
                            }
                            _ => return EventStatus::Ignored,
                        };
                        WindowEvent::PointerPressed {
                            button: slint_button,
                            position: *self.last_cursor_pos.borrow(),
                        }
                    }
                    baseview::MouseEvent::ButtonReleased { button, .. } => {
                        let slint_button = match button {
                            baseview::MouseButton::Left => {
                                slint::platform::PointerEventButton::Left
                            }
                            baseview::MouseButton::Right => {
                                slint::platform::PointerEventButton::Right
                            }
                            baseview::MouseButton::Middle => {
                                slint::platform::PointerEventButton::Middle
                            }
                            _ => return EventStatus::Ignored,
                        };
                        WindowEvent::PointerReleased {
                            button: slint_button,
                            position: *self.last_cursor_pos.borrow(),
                        }
                    }
                    baseview::MouseEvent::WheelScrolled { delta, .. } => {
                        let (delta_x, delta_y) = match delta {
                            baseview::ScrollDelta::Lines { x, y } => (x * 20.0, y * 20.0),
                            baseview::ScrollDelta::Pixels { x, y } => (x, y),
                        };
                        WindowEvent::PointerScrolled {
                            position: LogicalPosition::new(0.0, 0.0),
                            delta_x: delta_x as f32,
                            delta_y: delta_y as f32,
                        }
                    }
                    _ => return EventStatus::Ignored,
                };

                self.adapter.window.dispatch_event(slint_event);
                EventStatus::Captured
            }
            Event::Keyboard(_key_event) => {
                // TODO: Implement keyboard event forwarding
                // This requires mapping baseview's keyboard types to Slint's
                EventStatus::Ignored
            }
            Event::Window(window_event) => {
                match window_event {
                    BaseviewWindowEvent::Resized(info) => {
                        // Handle scale factor and size changes from baseview
                        self.handle_window_info(&info);
                        EventStatus::Captured
                    }
                    BaseviewWindowEvent::Focused => EventStatus::Ignored,
                    BaseviewWindowEvent::Unfocused => EventStatus::Ignored,
                    BaseviewWindowEvent::WillClose => EventStatus::Ignored,
                }
            }
        }
    }
}

struct Instance {
    window_handle: WindowHandle,
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.window_handle.close();
    }
}

// SAFETY: `Instance` only contains a `WindowHandle`, which is not `Send` because it holds
// a raw pointer to the platform window.  However, we only ever close the window from the
// audio thread (via `Drop`), and baseview guarantees that `WindowHandle::close` is safe to
// call from any thread.
unsafe impl Send for Instance {}

impl<T: slint::ComponentHandle + 'static> Editor for SlintEditor<T> {
    fn spawn(
        &self,
        parent: nih_plug::prelude::ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn std::any::Any + Send> {
        let options = WindowOpenOptions {
            scale: WindowScalePolicy::SystemScaleFactor,
            size: Size {
                width: self.width.load(Ordering::Relaxed) as f64,
                height: self.height.load(Ordering::Relaxed) as f64,
            },
            title: "Plug-in".to_owned(),
            // Request OpenGL context for FemtoVG rendering
            gl_config: Some(GlConfig {
                version: (3, 2),
                red_bits: 8,
                blue_bits: 8,
                green_bits: 8,
                alpha_bits: 8,
                depth_bits: 24,
                stencil_bits: 8,
                samples: None,
                srgb: true,
                double_buffer: true,
                vsync: false,
                ..Default::default()
            }),
        };

        let width = self.width.clone();
        let height = self.height.clone();
        let state = self.state.clone();
        let event_loop_handler = self.event_loop_handler.clone();
        let setup_handler = self.setup_handler.clone();
        let component_factory = self.component_factory.clone();

        let window_handle =
            baseview::Window::open_parented(&parent, options, move |baseview_window| {
                // Make the GL context current so that any renderer creation during component
                // initialization (Slint may call renderer() eagerly) has a valid context.
                unsafe { baseview_window.gl_context().unwrap().make_current() };

                // Create the Slint window adapter.
                // Start with scale 1.0; the actual system scale arrives via the first
                // WindowEvent::Resized from baseview.
                let initial_scale = 1.0f32;
                let logical_width = width.load(Ordering::Relaxed);
                let logical_height = height.load(Ordering::Relaxed);
                let adapter =
                    BaseviewSlintAdapter::new(logical_width, logical_height, initial_scale);

                // Wire up the GL proc-address loader now that the context is live.
                adapter.set_gl_context(baseview_window);

                // Register this adapter so BaseviewSlintPlatform::create_window_adapter returns it.
                CURRENT_ADAPTER.with(|current| {
                    *current.borrow_mut() = Some(adapter.clone());
                });

                // Install our platform on first open; ignored (returns Err) on subsequent opens
                // since Slint only allows setting the platform once per process.
                let _ = slint::platform::set_platform(Box::new(BaseviewSlintPlatform));

                let component = component_factory()
                    .unwrap_or_else(|e| panic!("Failed to create Slint component: {}", e));

                // Defer show() until on_frame so the GL context is current when FemtoVG
                // queries GL_VERSION during its first render.

                WindowHandler {
                    context,
                    event_loop_handler,
                    setup_handler,
                    width,
                    height,
                    scale_factor: RefCell::new(initial_scale),
                    state,
                    pending_resizes: Rc::new(RefCell::new(Vec::new())),
                    last_cursor_pos: RefCell::new(LogicalPosition::new(0.0, 0.0)),
                    window_shown: RefCell::new(false),
                    component,
                    adapter,
                }
            });

        Box::new(Instance { window_handle })
    }

    fn size(&self) -> (u32, u32) {
        (
            self.width.load(Ordering::Relaxed),
            self.height.load(Ordering::Relaxed),
        )
    }

    fn set_scale_factor(&self, _factor: f32) -> bool {
        // TODO: Implement scale factor handling for Slint
        false
    }

    fn param_values_changed(&self) {}

    fn param_value_changed(&self, _id: &str, _normalized_value: f32) {}

    fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {}
}
