//! Main daemon loop. hidapi is fundamentally synchronous and not Sync,
//! so we use plain `std::thread` rather than tokio: one thread reads input
//! from the device, another polls state_files; a third (when any button
//! has a `.slint` screen) drives Slint rendering. The main thread renders
//! the initial static layout and then enters the event-reader loop.
//!
//! Multi-page navigation (`@switch_page`) is a follow-up; v0 only renders
//! the first page in the deckfile (or the implicit "main" page).

use anyhow::{anyhow, Context, Result};
use elgato_streamdeck::{new_hidapi, DeviceStateUpdate, StreamDeck};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{Button, ButtonState, Deckfile, Page};
use crate::render::Renderer;
use crate::slint_screen::{tick_animations, SlintScreen};

/// Cross-thread message into the Slint UI thread. The thread owns
/// all SlintScreen instances (they're `!Send`) so every state change
/// or hardware event must funnel through this channel.
enum SlintCmd {
    /// Hardware button press — fires the component's `tap` callback.
    Tap(u8),
    /// State-file change — sets the `active` and `processing` boolean
    /// properties on the screen for the given button index.
    SetState {
        idx: u8,
        active: bool,
        processing: bool,
    },
}

pub fn run(config_path: Option<PathBuf>) -> Result<()> {
    if let Some(p) = config_path {
        std::env::set_var("DECKFILE", p);
    }
    let cfg = Arc::new(Deckfile::load()?);

    let (page_name, page) = cfg
        .pages
        .iter()
        .next()
        .ok_or_else(|| anyhow!("deckfile has no pages and no top-level buttons/dials"))?;
    let page = Arc::new(page.clone());
    tracing::info!(page = %page_name, "active page");

    let hid = new_hidapi().context("hidapi init")?;
    let devs = elgato_streamdeck::list_devices(&hid);
    let (kind, serial) = devs
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no Stream Deck found"))?;

    tracing::info!(?kind, %serial, "opening deck");
    let deck = StreamDeck::connect(&hid, kind, &serial).context("connect")?;
    deck.set_brightness(cfg.device.brightness).context("brightness")?;

    let img_fmt = kind.key_image_format();
    let key_size = (img_fmt.size.0 as u32, img_fmt.size.1 as u32);
    let renderer = Arc::new(Renderer::new(cfg.device.font.as_deref(), key_size)?);

    render_static_buttons(&deck, &cfg, &page, &renderer)?;

    let deck = Arc::new(Mutex::new(deck));

    // If any button has `screen:`, spin up the Slint thread and keep
    // a sender we can hand to the state-poll and input loops.
    let slint_tx: Option<Sender<SlintCmd>> = spawn_slint_thread_if_needed(
        page.clone(),
        deck.clone(),
        key_size,
    )?;

    // State-polling thread. Slint-backed buttons forward their state
    // changes to the UI thread via the channel; static buttons paint
    // directly through `Renderer`.
    spawn_state_poll_thread(
        cfg.clone(),
        page.clone(),
        deck.clone(),
        renderer,
        slint_tx.clone(),
    );

    // Event-reader loop. Runs on the main thread and dispatches every
    // device update through `handle_event`.
    loop {
        let updates = {
            let d = deck.lock().unwrap();
            d.read_input(Some(Duration::from_secs(60)))?
        };
        for ev in to_updates(updates) {
            handle_event(&ev, &page, slint_tx.as_ref());
        }
    }
}

fn spawn_state_poll_thread(
    cfg: Arc<Deckfile>,
    page: Arc<Page>,
    deck: Arc<Mutex<StreamDeck>>,
    renderer: Arc<Renderer>,
    slint_tx: Option<Sender<SlintCmd>>,
) {
    std::thread::Builder::new()
        .name("deckfile-state-poll".into())
        .spawn(move || {
            let mut prev: HashMap<u8, ButtonState> = HashMap::new();
            let interval = Duration::from_millis(cfg.device.poll_ms);
            loop {
                std::thread::sleep(interval);
                for (idx, btn) in &page.buttons {
                    if btn.state_file.is_none() && btn.processing_file.is_none() {
                        continue;
                    }
                    let cur = btn.state();
                    if prev.get(idx) == Some(&cur) {
                        continue;
                    }
                    prev.insert(*idx, cur);

                    if btn.screen.is_some() {
                        if let Some(tx) = &slint_tx {
                            let _ = tx.send(SlintCmd::SetState {
                                idx: *idx,
                                active: matches!(cur, ButtonState::Active),
                                processing: matches!(cur, ButtonState::Processing),
                            });
                        }
                    } else if let Ok(img) = renderer.render(btn, cur) {
                        let dyn_img = image::DynamicImage::ImageRgb8(img);
                        let d = deck.lock().unwrap();
                        let _ = d.set_button_image(*idx, dyn_img);
                        let _ = d.flush();
                    }
                }
            }
        })
        .expect("spawn state-poll thread");
}

/// If any button has a `screen:` field, spin up the Slint UI thread
/// and return its command sender. Returning `None` means no Slint
/// work is happening and callers should not bother building messages.
fn spawn_slint_thread_if_needed(
    page: Arc<Page>,
    deck: Arc<Mutex<StreamDeck>>,
    key_size: (u32, u32),
) -> Result<Option<Sender<SlintCmd>>> {
    // Collect (idx, path, component, initial_state) snapshots so the
    // spawned thread can build screens without re-locking the page.
    let mut specs: Vec<SlintSpec> = Vec::new();
    for (idx, btn) in &page.buttons {
        let Some(path) = &btn.screen else { continue };
        specs.push(SlintSpec {
            idx: *idx,
            path: path.clone(),
            component: btn.screen_component.clone(),
            initial_state: btn.state(),
        });
    }
    if specs.is_empty() {
        return Ok(None);
    }

    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("deckfile-slint-ui".into())
        .spawn(move || {
            if let Err(e) = run_slint_loop(rx, specs, deck, key_size) {
                tracing::error!(error = %e, "slint UI thread terminated");
            }
        })
        .context("spawn slint UI thread")?;
    Ok(Some(tx))
}

struct SlintSpec {
    idx: u8,
    path: PathBuf,
    component: Option<String>,
    initial_state: ButtonState,
}

fn run_slint_loop(
    rx: mpsc::Receiver<SlintCmd>,
    specs: Vec<SlintSpec>,
    deck: Arc<Mutex<StreamDeck>>,
    key_size: (u32, u32),
) -> Result<()> {
    let mut screens: HashMap<u8, SlintScreen> = HashMap::new();
    for spec in specs {
        let screen = SlintScreen::load_path(
            &spec.path,
            spec.component.as_deref(),
            key_size.0,
            key_size.1,
        )
        .with_context(|| {
            format!(
                "load slint screen for button {} ({})",
                spec.idx,
                spec.path.display()
            )
        })?;
        // Apply initial state so the first frame matches the live
        // state-file situation rather than the .slint file defaults.
        let _ = screen.set_bool("active", matches!(spec.initial_state, ButtonState::Active));
        let _ = screen.set_bool(
            "processing",
            matches!(spec.initial_state, ButtonState::Processing),
        );
        tracing::info!(idx = spec.idx, path = %spec.path.display(), "slint screen loaded");
        screens.insert(spec.idx, screen);
    }

    // First frame for every Slint button.
    push_renders(&deck, &screens, screens.keys().copied().collect());

    let frame = Duration::from_millis(33);
    loop {
        // Drain pending commands without blocking — the channel is a
        // hint, not a tick source. Each command flags its button dirty
        // so the next render pass picks it up alongside any animations.
        let mut dirty: HashSet<u8> = HashSet::new();
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                SlintCmd::Tap(idx) => {
                    if let Some(s) = screens.get(&idx) {
                        let _ = s.invoke("tap");
                    }
                    dirty.insert(idx);
                }
                SlintCmd::SetState {
                    idx,
                    active,
                    processing,
                } => {
                    if let Some(s) = screens.get(&idx) {
                        let _ = s.set_bool("active", active);
                        let _ = s.set_bool("processing", processing);
                    }
                    dirty.insert(idx);
                }
            }
        }

        tick_animations();

        for (idx, s) in &screens {
            if s.has_active_animations() {
                dirty.insert(*idx);
            }
        }

        if !dirty.is_empty() {
            push_renders(&deck, &screens, dirty);
        }

        std::thread::sleep(frame);
    }
}

fn push_renders(
    deck: &Arc<Mutex<StreamDeck>>,
    screens: &HashMap<u8, SlintScreen>,
    indices: HashSet<u8>,
) {
    let mut imgs: Vec<(u8, image::RgbImage)> = Vec::with_capacity(indices.len());
    for idx in indices {
        let Some(screen) = screens.get(&idx) else { continue };
        match screen.render() {
            Ok(img) => imgs.push((idx, img)),
            Err(e) => tracing::warn!(idx, error = %e, "slint render failed"),
        }
    }
    if imgs.is_empty() {
        return;
    }
    let d = deck.lock().unwrap();
    for (idx, img) in imgs {
        let dyn_img = image::DynamicImage::ImageRgb8(img);
        if let Err(e) = d.set_button_image(idx, dyn_img) {
            tracing::warn!(idx, error = %e, "set_button_image failed");
        }
    }
    let _ = d.flush();
}

fn to_updates(input: elgato_streamdeck::StreamDeckInput) -> Vec<DeviceStateUpdate> {
    use elgato_streamdeck::StreamDeckInput as I;
    use std::cell::RefCell;
    thread_local! {
        static PREV: RefCell<I> = const { RefCell::new(I::NoData) };
    }
    PREV.with(|p| {
        let mut prev = p.borrow_mut();
        let events = diff(&prev, &input);
        *prev = input;
        events
    })
}

fn diff(
    prev: &elgato_streamdeck::StreamDeckInput,
    cur: &elgato_streamdeck::StreamDeckInput,
) -> Vec<DeviceStateUpdate> {
    use elgato_streamdeck::StreamDeckInput as I;
    let mut out = Vec::new();
    match (prev, cur) {
        (I::ButtonStateChange(prev_b), I::ButtonStateChange(cur_b)) => {
            for (i, (was, now)) in prev_b.iter().zip(cur_b.iter()).enumerate() {
                if !was && *now {
                    out.push(DeviceStateUpdate::ButtonDown(i as u8));
                } else if *was && !now {
                    out.push(DeviceStateUpdate::ButtonUp(i as u8));
                }
            }
        }
        (_, I::ButtonStateChange(cur_b)) => {
            for (i, now) in cur_b.iter().enumerate() {
                if *now {
                    out.push(DeviceStateUpdate::ButtonDown(i as u8));
                }
            }
        }
        (I::EncoderStateChange(prev_e), I::EncoderStateChange(cur_e)) => {
            for (i, (was, now)) in prev_e.iter().zip(cur_e.iter()).enumerate() {
                if !was && *now {
                    out.push(DeviceStateUpdate::EncoderDown(i as u8));
                } else if *was && !now {
                    out.push(DeviceStateUpdate::EncoderUp(i as u8));
                }
            }
        }
        (_, I::EncoderTwist(deltas)) => {
            for (i, d) in deltas.iter().enumerate() {
                if *d != 0 {
                    out.push(DeviceStateUpdate::EncoderTwist(i as u8, *d));
                }
            }
        }
        _ => {}
    }
    out
}

fn render_static_buttons(
    deck: &StreamDeck,
    cfg: &Deckfile,
    page: &Page,
    r: &Renderer,
) -> Result<()> {
    deck.reset()?;
    deck.set_brightness(cfg.device.brightness)?;
    for (idx, btn) in &page.buttons {
        if btn.screen.is_some() {
            // Slint thread paints these buttons.
            continue;
        }
        if !has_static_content(btn) {
            continue;
        }
        let img = r.render(btn, btn.state())?;
        let dyn_img = image::DynamicImage::ImageRgb8(img);
        deck.set_button_image(*idx, dyn_img)?;
    }
    deck.flush()?;
    Ok(())
}

fn has_static_content(btn: &Button) -> bool {
    btn.icon.is_some()
        || btn.icon_active.is_some()
        || btn.icon_processing.is_some()
        || btn.label.is_some()
        || btn.label_active.is_some()
        || btn.label_processing.is_some()
        || btn.bg.is_some()
        || btn.bg_active.is_some()
        || btn.bg_processing.is_some()
}

fn handle_event(ev: &DeviceStateUpdate, page: &Page, slint_tx: Option<&Sender<SlintCmd>>) {
    match ev {
        DeviceStateUpdate::ButtonDown(idx) => {
            if let Some(btn) = page.buttons.get(idx) {
                if let Some(cmd) = &btn.on_press {
                    tracing::info!(idx, %cmd, "btn press");
                    spawn_shell(cmd);
                }
                if btn.screen.is_some() {
                    if let Some(tx) = slint_tx {
                        let _ = tx.send(SlintCmd::Tap(*idx));
                    }
                }
            }
        }
        DeviceStateUpdate::ButtonUp(idx) => {
            if let Some(btn) = page.buttons.get(idx) {
                if let Some(cmd) = &btn.on_release {
                    tracing::info!(idx, %cmd, "btn release");
                    spawn_shell(cmd);
                } else {
                    tracing::debug!(idx, "btn release (no binding)");
                }
            } else {
                tracing::debug!(idx, "btn release (no config)");
            }
        }
        DeviceStateUpdate::EncoderDown(idx) => {
            if let Some(d) = page.dials.get(idx) {
                if let Some(c) = &d.on_press {
                    spawn_shell(c);
                }
            }
        }
        DeviceStateUpdate::EncoderUp(idx) => {
            if let Some(d) = page.dials.get(idx) {
                if let Some(c) = &d.on_release {
                    spawn_shell(c);
                }
            }
        }
        DeviceStateUpdate::EncoderTwist(idx, delta) => {
            if let Some(d) = page.dials.get(idx) {
                let cmd = if *delta > 0 {
                    &d.on_turn_up
                } else {
                    &d.on_turn_down
                };
                if let Some(c) = cmd {
                    tracing::debug!(idx, delta, %c, "dial turn");
                    spawn_shell(c);
                }
            }
        }
        _ => {}
    }
}

fn spawn_shell(cmd: &str) {
    let cmd = cmd.to_string();
    std::thread::spawn(move || {
        let out = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output();
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                tracing::warn!(%cmd, status = ?o.status.code(), %err, "shell failed");
            }
            Err(e) => tracing::error!(%cmd, ?e, "spawn"),
        }
    });
}
