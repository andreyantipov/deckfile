//! Main daemon loop. hidapi is fundamentally synchronous and not Sync,
//! so we use plain `std::thread` rather than tokio: one thread reads input
//! from the device, another polls state_files; the main thread renders the
//! initial layout and joins.
//!
//! Multi-page navigation (`@switch_page`) is a follow-up; v0 only renders
//! the first page in the deckfile (or the implicit "main" page).

use anyhow::{anyhow, Context, Result};
use elgato_streamdeck::{new_hidapi, DeviceStateUpdate, StreamDeck};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{Deckfile, Page};
use crate::render::Renderer;

pub fn run(config_path: Option<PathBuf>) -> Result<()> {
    if let Some(p) = config_path {
        std::env::set_var("DECKFILE", p);
    }
    let cfg = Arc::new(Deckfile::load()?);

    // v0 renders the first page only. The implicit single-page form
    // normalizes to "main" in the loader.
    let (page_name, page) = cfg.pages.iter().next()
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
    let renderer = Arc::new(Renderer::new(
        cfg.device.font.as_deref(),
        (img_fmt.size.0 as u32, img_fmt.size.1 as u32),
    )?);

    render_all(&deck, &cfg, &page, &renderer)?;

    // Mutex wraps StreamDeck because hidapi's HidDevice isn't Sync.
    // Threads coordinate through the mutex; contention is negligible
    // (reader sleeps in hidraw poll, state-poller wakes every poll_ms).
    let deck = Arc::new(Mutex::new(deck));

    // State polling thread: re-render buttons whose state_file existence flips.
    {
        let cfg = cfg.clone();
        let page = page.clone();
        let renderer = renderer.clone();
        let deck = deck.clone();
        std::thread::spawn(move || {
            let mut prev: HashMap<u8, bool> = HashMap::new();
            let interval = Duration::from_millis(cfg.device.poll_ms);
            loop {
                std::thread::sleep(interval);
                for (idx, btn) in &page.buttons {
                    let Some(sf) = &btn.state_file else { continue };
                    let cur = sf.exists();
                    if prev.get(idx) != Some(&cur) {
                        prev.insert(*idx, cur);
                        let Ok(img) = renderer.render(btn, cur) else { continue };
                        let dyn_img = image::DynamicImage::ImageRgb8(img);
                        let d = deck.lock().unwrap();
                        let _ = d.set_button_image(*idx, dyn_img);
                        let _ = d.flush();
                    }
                }
            }
        });
    }

    // Event-reader loop. read_input blocks until either a real event lands
    // or the timeout elapses; on each pass we dispatch ButtonDown/Up,
    // EncoderTwist, etc. to the configured shell commands.
    loop {
        let updates = {
            let d = deck.lock().unwrap();
            // Use the StreamDeck's own DeviceStateReader via Arc<Mutex> —
            // but DeviceStateReader needs Arc<StreamDeck>, not Arc<Mutex>.
            // Simplest: call read_input directly (returns StreamDeckInput,
            // a snapshot) and track previous state ourselves.
            d.read_input(Some(Duration::from_secs(60)))?
        };
        for ev in to_updates(updates) {
            handle_event(ev, &page);
        }
    }
}

/// Convert a single `StreamDeckInput` snapshot into a sequence of
/// `DeviceStateUpdate` events by diffing against the previous snapshot.
/// We keep prev state inside a static once-init thread-local since this
/// fn is called only from the reader thread.
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

fn diff(prev: &elgato_streamdeck::StreamDeckInput, cur: &elgato_streamdeck::StreamDeckInput) -> Vec<DeviceStateUpdate> {
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
            // First read after NoData — emit downs for any pressed keys.
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

fn render_all(deck: &StreamDeck, cfg: &Deckfile, page: &Page, r: &Renderer) -> Result<()> {
    deck.reset()?;
    deck.set_brightness(cfg.device.brightness)?;
    for (idx, btn) in &page.buttons {
        if btn.label.is_none() && btn.label_active.is_none() {
            continue;
        }
        let active = btn.state_file.as_ref().is_some_and(|p| p.exists());
        let img = r.render(btn, active)?;
        let dyn_img = image::DynamicImage::ImageRgb8(img);
        deck.set_button_image(*idx, dyn_img)?;
    }
    deck.flush()?;
    Ok(())
}

fn handle_event(ev: DeviceStateUpdate, page: &Page) {
    match ev {
        DeviceStateUpdate::ButtonDown(idx) => {
            if let Some(btn) = page.buttons.get(&idx) {
                if let Some(cmd) = &btn.on_press {
                    tracing::info!(idx, %cmd, "btn press");
                    spawn_shell(cmd);
                }
            }
        }
        DeviceStateUpdate::ButtonUp(idx) => {
            if let Some(btn) = page.buttons.get(&idx) {
                if let Some(cmd) = &btn.on_release {
                    spawn_shell(cmd);
                }
            }
        }
        DeviceStateUpdate::EncoderDown(idx) => {
            if let Some(d) = page.dials.get(&idx) {
                if let Some(c) = &d.on_press { spawn_shell(c); }
            }
        }
        DeviceStateUpdate::EncoderUp(idx) => {
            if let Some(d) = page.dials.get(&idx) {
                if let Some(c) = &d.on_release { spawn_shell(c); }
            }
        }
        DeviceStateUpdate::EncoderTwist(idx, delta) => {
            if let Some(d) = page.dials.get(&idx) {
                let cmd = if delta > 0 { &d.on_turn_up } else { &d.on_turn_down };
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
