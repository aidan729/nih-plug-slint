# nih-plug-slint

An adapter for using [Slint](https://slint.dev/) GUIs with [NIH-plug](https://github.com/robbert-vdh/nih-plug) audio plugins. It uses baseview for windowing and FemtoVG (OpenGL) for rendering, so you get native plugin windows without a webview.

## Example

I took the liberty of creating a simple gain knob VST example project using NIH-Plug and NIH-Plug-Slint.

please see that here: [Gain Knob](https://github.com/aidan729/Gain-Knob)

## Usage

Add the dependency:

```toml
[dependencies]
nih_plug_slint = { git = "https://github.com/aidan729/nih-plug-slint" }
```

In your plugin:

```rust
use nih_plug_slint::{SlintEditor, SlintEditorState};
use std::sync::Arc;

#[derive(Params)]
struct MyParams {
    #[persist = "editor-state"]
    editor_state: Arc<SlintEditorState>,
}

fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
    let params = self.params.clone();
    Some(Box::new(
        SlintEditor::with_factory(|| gui::AppWindow::new(), (400, 300))
            .with_state(self.params.editor_state.clone())
            .with_event_loop(move |handler, _setter, _window| {
                let component = handler.component();

                // Push parameter values to the UI each frame
                component.set_gain(params.gain.unmodulated_normalized_value());

                // Wire up UI callbacks (safe to re-register every frame)
                let context = handler.context().clone();
                let params = params.clone();
                component.on_gain_changed(move |value| {
                    let setter = ParamSetter::new(&*context);
                    setter.begin_set_parameter(&params.gain);
                    setter.set_parameter_normalized(&params.gain, value);
                    setter.end_set_parameter(&params.gain);
                });
            }),
    ))
}
```

## API

### `SlintEditor`

Created with `SlintEditor::with_factory(factory, (width, height))`. The factory closure is called each time the window is opened.

- `.with_state(Arc<SlintEditorState>)` - load the initial size from persisted state and write back to it on resize. The state needs to be stored in your params struct under `#[persist]`.
- `.with_event_loop(handler)` - called every frame. Use this to push parameter values to the UI and register UI callbacks.

### `WindowHandler`

Passed to the event loop handler. Gives you access to:

- `.component()` - the Slint component
- `.window()` - the Slint window
- `.context()` - NIH-plug's `GuiContext` for parameter operations
- `.resize(window, width, height)` - resize the window programmatically
- `.queue_resize(width, height)` - use this from inside Slint callbacks instead of calling `resize` directly, since you won't have the `&mut Window` handy

```rust
// Resizing from a Slint callback
let pending = handler.pending_resizes().clone();
component.on_resize(move || {
    pending.borrow_mut().push((800, 600));
});
```

### `SlintEditorState`

Holds `width`, `height`, and `scale_factor`. Construct with `SlintEditorState::new(w, h)` or `SlintEditorState::with_scale(w, h, scale)`.

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for more detail on how the Slint/baseview bridge works internally.

## License

ISC

## Credits

- [NIH-plug](https://github.com/robbert-vdh/nih-plug) by Robbert van der Helm
- [Slint](https://slint.dev/) UI toolkit
- [baseview](https://github.com/RustAudio/baseview) for windowing
