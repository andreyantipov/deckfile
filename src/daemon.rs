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

use crate::config::{ButtonState, Deckfile, Page};
use crate::render::Renderer;

pub fn run(config_path: Option<PathBuf>) -> Result<()> {
    if let Some(p) = config_path {
        std::env::set_var("DECKFILE", p);
    }
    let cfg = Arc::new(Deckfile::load()?);

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

    let deck = Arc::new(Mutex::new(deck));

    // State polling. For each button with state_file OR processing_file,
    // recheck on every device.poll_ms tick and re-render when the resolved
    // ButtonState (Processing > Active > Idle) changes.
    {
        let cfg = cfg.clone();
        let page = page.clone();
        let renderer = renderer.clone();
        let deck = deck.clone();
        std::thread::spawn(move || {
            let mut prev: HashMap<u8, ButtonState> = HashMap::new();
            let interval = Duration::from_millis(cfg.device.poll_ms);
            loop {
                std::thread::sleep(interval);
                for (idx, btn) in &page.buttons {
                    if btn.state_file.is_none() && btn.processing_file.is_none() {
                        continue;
                    }
                    let cur = btn.state();
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

    // Event-reader loop.
    loop {
        let updates = {
            let d = deck.lock().unwrap();
            d.read_input(Some(Duration::from_secs(60)))?
        };
        for ev in to_updates(updates) {
            handle_event(ev, &page);
        }
    }
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
        // Skip overwriting `prev` when the device just returned NoData
        // (an idle poll cycle). Without this, holding a key while the
        // device's HID layer cycles between ButtonStateChange and
        // NoData would re-fire ButtonDown on every subsequent state
        // report, producing phantom double-presses ~1-2s apart.
        if !matches!(input, I::NoData) {
            *prev = input;
        }
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
        (I::NoData, I::ButtonStateChange(cur_b)) => {
            // First real button report after device init (NoData →
            // ButtonStateChange). Emit Down only for keys that are
            // currently pressed at this moment — but with the prev-
            // NoData-skip guard above, we should only hit this branch
            // ONCE per session, on the very first frame.
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
        // Skip only when the button has neither an icon nor a label
        // configured in any state — those are decorations the user
        // intentionally left blank.
        let has_content =
            btn.icon.is_some() || btn.icon_active.is_some() || btn.icon_processing.is_some()
            || btn.label.is_some() || btn.label_active.is_some() || btn.label_processing.is_some()
            || btn.bg.is_some() || btn.bg_active.is_some() || btn.bg_processing.is_some();
        if !has_content {
            continue;
        }
        let img = r.render(btn, btn.state())?;
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
