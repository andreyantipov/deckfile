//! Dynamic Slint-backed screens for individual Stream Deck buttons.
//!
//! Each button with a `screen:` field in the deckfile gets its own
//! [`SlintScreen`]. We compile the `.slint` source at startup via the
//! interpreter, hand the resulting `ComponentInstance` a fresh
//! `MinimalSoftwareWindow` sized to the LCD (96×96 for Stream Deck
//! Plus keys), and render frame-by-frame into an `image::RgbImage`
//! that flows straight into `StreamDeck::set_button_image`.
//!
//! ## Why the thread-local stash
//!
//! Slint requires exactly ONE `Platform` per process, set via
//! `slint::platform::set_platform`. That platform's
//! `create_window_adapter` is the only hook through which a freshly
//! instantiated component receives its window. To bind a specific
//! `MinimalSoftwareWindow` to a specific component, we stash the
//! window in a thread-local before calling `definition.create()` —
//! the platform pops it out, hands it back, and the binding is set.
//! Without the stash, the platform falls back to a default-sized
//! window so `create()` never panics in surprising contexts (e.g.
//! parallel test threads or accidental component spawn).
//!
//! ## Thread model
//!
//! All Slint state (windows, instances, the platform's clock) is
//! `!Send`. The daemon parks all SlintScreen work on one dedicated
//! UI thread and forwards device events into it via channels.

use anyhow::{anyhow, Result};
use image::{Rgb, RgbImage};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::SharedPixelBuffer;
use slint_interpreter::{ComponentInstance, Compiler, DiagnosticLevel, Value};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

thread_local! {
    /// Window the next `definition.create()` should adopt. Populated
    /// by `SlintScreen::from_result` and drained by
    /// `DeckPlatform::create_window_adapter`. Falls back to a fresh
    /// window when empty so we never panic on stray spawns.
    static NEXT_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> =
        const { RefCell::new(None) };

    /// Per-thread guard for platform installation. Slint stores the
    /// platform in thread-local storage, so each thread that wants
    /// to drive Slint must call `set_platform` itself. The cell
    /// avoids the cost of re-allocating the platform box on repeat
    /// calls from the same thread (e.g. each SlintScreen::load).
    static PLATFORM_INSTALLED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

struct DeckPlatform {
    start: Instant,
}

impl Platform for DeckPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let win = NEXT_WINDOW
            .with(|w| w.borrow_mut().take())
            .unwrap_or_else(|| MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer));
        Ok(win)
    }

    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }
}

/// Install the deckfile platform on the current thread. Idempotent
/// per thread. Slint's platform slot is thread-local — every thread
/// that touches Slint needs its own platform installed, even though
/// the renderer itself is shared. Subsequent calls on the same
/// thread are cheap (one `Cell::get`) and never reallocate.
pub fn init_platform() {
    PLATFORM_INSTALLED.with(|installed| {
        if installed.get() {
            return;
        }
        // The `set_platform` Result is intentionally ignored: if some
        // upstream code (e.g. another integration in the same process)
        // already installed a platform on this thread, our DeckPlatform
        // just doesn't take effect and we proceed with theirs. Marking
        // the thread "installed" prevents repeat allocations either way.
        let _ = slint::platform::set_platform(Box::new(DeckPlatform {
            start: Instant::now(),
        }));
        installed.set(true);
    });
}

/// Advance Slint timers and active animations. Call this before each
/// frame; without it `animate { ... }` blocks stay frozen at t=0.
pub fn tick_animations() {
    slint::platform::update_timers_and_animations();
}

pub struct SlintScreen {
    instance: ComponentInstance,
    window: Rc<MinimalSoftwareWindow>,
    width: u32,
    height: u32,
}

impl SlintScreen {
    /// Compile `path` and bind the first matching component to a
    /// fresh `width × height` window. When `component_name` is None
    /// the first exported component in the file is used.
    pub fn load_path(
        path: &Path,
        component_name: Option<&str>,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        init_platform();
        let compiler = Compiler::default();
        let result = pollster::block_on(compiler.build_from_path(path));
        Self::from_result(result, component_name, width, height)
            .map_err(|e| anyhow!("{}: {}", path.display(), e))
    }

    /// Compile from an in-memory `.slint` source string. Used in tests
    /// and short-lived snippets where touching disk is overkill.
    pub fn load_source(
        source: &str,
        component_name: Option<&str>,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        init_platform();
        let compiler = Compiler::default();
        let result = pollster::block_on(
            compiler.build_from_source(source.to_string(), PathBuf::from("inline.slint")),
        );
        Self::from_result(result, component_name, width, height)
    }

    fn from_result(
        result: slint_interpreter::CompilationResult,
        component_name: Option<&str>,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        if result.has_errors() {
            let msgs: Vec<String> = result
                .diagnostics()
                .filter(|d| d.level() == DiagnosticLevel::Error)
                .map(|d| {
                    let (line, col) = d.line_column();
                    format!("L{line}:{col}: {}", d.message())
                })
                .collect();
            return Err(anyhow!("slint compile errors:\n  {}", msgs.join("\n  ")));
        }

        let definition = match component_name {
            Some(name) => result.component(name).ok_or_else(|| {
                let available: Vec<&str> = result.component_names().collect();
                anyhow!(
                    "component {:?} not found; exported: [{}]",
                    name,
                    available.join(", ")
                )
            })?,
            None => result
                .components()
                .next()
                .ok_or_else(|| anyhow!("no exported components in slint source"))?,
        };

        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        window.set_size(slint::PhysicalSize::new(width, height));
        NEXT_WINDOW.with(|w| *w.borrow_mut() = Some(window.clone()));

        let instance = definition
            .create()
            .map_err(|e| anyhow!("create instance: {e}"))?;

        // Defensive: if create() somehow didn't claim the stash (e.g.
        // the component reused an existing window from another adapter
        // path), clear it so a future load_* doesn't see stale state.
        NEXT_WINDOW.with(|w| w.borrow_mut().take());

        Ok(Self {
            instance,
            window,
            width,
            height,
        })
    }

    pub fn set_bool(&self, name: &str, value: bool) -> Result<()> {
        self.instance
            .set_property(name, Value::Bool(value))
            .map_err(|e| anyhow!("set_property({name}={value}): {e}"))
    }

    pub fn set_number(&self, name: &str, value: f64) -> Result<()> {
        self.instance
            .set_property(name, Value::Number(value))
            .map_err(|e| anyhow!("set_property({name}={value}): {e}"))
    }

    pub fn set_string(&self, name: &str, value: &str) -> Result<()> {
        self.instance
            .set_property(name, Value::String(value.into()))
            .map_err(|e| anyhow!("set_property({name}={value:?}): {e}"))
    }

    pub fn set_callback(
        &self,
        name: &str,
        cb: impl Fn(&[Value]) -> Value + 'static,
    ) -> Result<()> {
        self.instance
            .set_callback(name, cb)
            .map_err(|e| anyhow!("set_callback({name}): {e}"))
    }

    /// Synchronously invoke a public callback by name. Used to feed
    /// hardware events ("tap", "release", etc.) into the component.
    pub fn invoke(&self, name: &str) -> Result<()> {
        self.instance
            .invoke(name, &[])
            .map_err(|e| anyhow!("invoke({name}): {e}"))?;
        Ok(())
    }

    pub fn has_active_animations(&self) -> bool {
        self.window.has_active_animations()
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    /// Take a fresh RGB snapshot of the current component render.
    /// Slint's `take_snapshot` always renders into a new buffer (no
    /// dependency on the dirty-tracking we get from `draw_if_needed`),
    /// which keeps the API trivially predictable for callers that
    /// don't want to think about repaint state.
    pub fn render(&self) -> Result<RgbImage> {
        let buf: SharedPixelBuffer<slint::Rgba8Pixel> = self
            .window
            .take_snapshot()
            .map_err(|e| anyhow!("take_snapshot: {e}"))?;

        let w = buf.width();
        let h = buf.height();
        if w != self.width || h != self.height {
            return Err(anyhow!(
                "snapshot {w}x{h} != requested {}x{}",
                self.width,
                self.height
            ));
        }

        let mut img = RgbImage::new(w, h);
        for (i, px) in buf.as_slice().iter().enumerate() {
            let x = (i as u32) % w;
            let y = (i as u32) / w;
            img.put_pixel(x, y, Rgb([px.r, px.g, px.b]));
        }
        Ok(img)
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}
