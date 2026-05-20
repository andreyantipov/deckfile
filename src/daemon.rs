//! Single-threaded daemon loop.
//!
//! Everything that touches the device — input reads, button rendering,
//! the Slint screens, state-file polling — happens on one thread. The
//! original v0 sharded these across an input thread, a state-poll
//! thread, and a Slint UI thread, all serialised through an
//! `Arc<Mutex<StreamDeck>>`. That sounded clean but in practice the
//! input thread held the mutex inside `read_input` for up to its
//! timeout (we had it set to 60 seconds), so the Slint thread could
//! only acquire the lock during the microsecond gap between input
//! iterations and almost never managed to push an updated frame —
//! a tap visibly registered only after five-or-so presses.
//!
//! The cure is to stop sharing the device at all. One tick of the
//! main loop now does, in order:
//!
//!   1. poll device input with a short timeout (10ms) and dispatch
//!      ButtonDown / ButtonUp / EncoderTwist events
//!   2. re-check state files every `device.poll_ms`, push the
//!      truth values into the corresponding Slint properties
//!   3. tick Slint animations
//!   4. render any dirty Slint screens, blit them to the deck
//!   5. flush, sleep until the next frame
//!
//! Slint and the StreamDeck handle both live on this thread — no
//! `Send` requirement, no mutex, no contention.

use anyhow::{anyhow, Context, Result};
use elgato_streamdeck::{new_hidapi, DeviceStateUpdate, StreamDeck};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{Button, ButtonState, Deckfile, Page};
use crate::render::Renderer;
use crate::slint_screen::{tick_animations, SlintScreen};

/// USB input poll window. Short enough that we wake quickly when a
/// key is pressed but long enough that we don't burn CPU spinning.
/// Picked so the loop wakes ~100 times per second.
const INPUT_POLL: Duration = Duration::from_millis(10);

/// Target frame interval for Slint redraws. 30 FPS is plenty for the
/// 96/120px LCDs and keeps CPU use negligible.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);

pub fn run(config_path: Option<PathBuf>) -> Result<()> {
    if let Some(p) = config_path {
        std::env::set_var("DECKFILE", p);
    }
    let cfg = Deckfile::load()?;

    let (page_name, page) = cfg
        .pages
        .iter()
        .next()
        .ok_or_else(|| anyhow!("deckfile has no pages and no top-level buttons/dials"))?;
    let page = page.clone();
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
    tracing::info!(
        width = key_size.0,
        height = key_size.1,
        mode = ?img_fmt.mode,
        "key image format",
    );

    let renderer = Renderer::new(cfg.device.font.as_deref(), key_size)?;

    // Pre-build the Slint screens. They're !Send so they live forever
    // on this thread. Initial state is whatever the state-files report
    // right now (so first frame matches reality, not the .slint default).
    let mut screens: HashMap<u8, SlintScreen> = HashMap::new();
    for (idx, btn) in &page.buttons {
        let Some(path) = &btn.screen else { continue };
        let screen = SlintScreen::load_path(
            path,
            btn.screen_component.as_deref(),
            key_size.0,
            key_size.1,
        )
        .with_context(|| {
            format!("load slint screen for button {} ({})", idx, path.display())
        })?;
        let state = btn.state();
        let _ = screen.set_bool("active", matches!(state, ButtonState::Active));
        let _ = screen.set_bool(
            "processing",
            matches!(state, ButtonState::Processing),
        );
        tracing::info!(idx, path = %path.display(), "slint screen loaded");
        screens.insert(*idx, screen);
    }

    // Paint the initial layout — static buttons through `Renderer`,
    // Slint buttons via their first snapshot.
    render_static_buttons(&deck, &cfg, &page, &renderer)?;
    for (idx, screen) in &screens {
        if let Ok(img) = screen.render() {
            let _ = deck.set_button_image(*idx, image::DynamicImage::ImageRgb8(img));
        }
    }
    deck.flush().ok();

    main_loop(deck, page, cfg, renderer, screens)
}

fn main_loop(
    deck: StreamDeck,
    page: Page,
    cfg: Deckfile,
    renderer: Renderer,
    mut screens: HashMap<u8, SlintScreen>,
) -> Result<()> {
    // Track button and encoder pressed states *separately* and *only*
    // update when we actually receive that kind of report. Using the
    // whole `StreamDeckInput` enum as prev (the old approach) lost
    // ButtonUp edges whenever a `NoData` poll fell between the press
    // and release — prev got rewritten to `NoData` and the next
    // ButtonStateChange([false, ...]) compared against nothing.
    let mut prev_buttons: Option<Vec<bool>> = None;
    let mut prev_encoders: Option<Vec<bool>> = None;
    let mut prev_states: HashMap<u8, ButtonState> = HashMap::new();
    let state_poll = Duration::from_millis(cfg.device.poll_ms);
    let mut last_state_check = Instant::now() - state_poll;
    let mut dirty: HashSet<u8> = HashSet::new();

    loop {
        let frame_start = Instant::now();

        // 1. Input — only mutate prev_* state when the read returns the
        //    matching kind. NoData is treated as "nothing to do".
        match deck.read_input(Some(INPUT_POLL)) {
            Ok(input) => {
                for ev in diff_input(&input, &mut prev_buttons, &mut prev_encoders) {
                    handle_event(&ev, &page, &mut screens, &mut dirty);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "read_input failed");
            }
        }

        // 2. State files. Slint buttons → property updates; static
        //    buttons → render now (rare, doesn't share the frame budget).
        if last_state_check.elapsed() >= state_poll {
            last_state_check = Instant::now();
            for (idx, btn) in &page.buttons {
                if btn.state_file.is_none() && btn.processing_file.is_none() {
                    continue;
                }
                let cur = btn.state();
                if prev_states.get(idx) == Some(&cur) {
                    continue;
                }
                let from = prev_states.insert(*idx, cur);
                tracing::info!(idx, from = ?from, to = ?cur, "state change");

                if let Some(screen) = screens.get(idx) {
                    let _ = screen.set_bool("active", matches!(cur, ButtonState::Active));
                    let _ = screen.set_bool(
                        "processing",
                        matches!(cur, ButtonState::Processing),
                    );
                    dirty.insert(*idx);
                } else if let Ok(img) = renderer.render(btn, cur) {
                    let _ = deck.set_button_image(*idx, image::DynamicImage::ImageRgb8(img));
                }
            }
        }

        // 3. Tick Slint animations and pick up any in-progress ones.
        tick_animations();
        for (idx, screen) in &screens {
            if screen.has_active_animations() {
                dirty.insert(*idx);
            }
        }

        // 4. Render every dirty Slint screen and blit.
        if !dirty.is_empty() {
            let drained: Vec<u8> = dirty.drain().collect();
            for idx in &drained {
                let Some(screen) = screens.get(idx) else { continue };
                match screen.render() {
                    Ok(img) => {
                        if let Err(e) = deck
                            .set_button_image(*idx, image::DynamicImage::ImageRgb8(img))
                        {
                            tracing::warn!(idx, error = %e, "set_button_image failed");
                        }
                    }
                    Err(e) => tracing::warn!(idx, error = %e, "slint render failed"),
                }
            }
            deck.flush().ok();
        }

        // 5. Pace the loop. The input poll already costs INPUT_POLL,
        //    so we only need to top up to FRAME_INTERVAL.
        let elapsed = frame_start.elapsed();
        if elapsed < FRAME_INTERVAL {
            std::thread::sleep(FRAME_INTERVAL - elapsed);
        }
    }
}

/// Translate a single `read_input` result into edge events,
/// mutating `prev_buttons` / `prev_encoders` only when the report
/// concerns that kind of input. Returning the events as a Vec keeps
/// the call site loop-free.
fn diff_input(
    input: &elgato_streamdeck::StreamDeckInput,
    prev_buttons: &mut Option<Vec<bool>>,
    prev_encoders: &mut Option<Vec<bool>>,
) -> Vec<DeviceStateUpdate> {
    use elgato_streamdeck::StreamDeckInput as I;
    let mut out = Vec::new();
    match input {
        I::ButtonStateChange(cur) => {
            match prev_buttons {
                Some(prev) => {
                    for (i, (was, now)) in prev.iter().zip(cur.iter()).enumerate() {
                        if !was && *now {
                            out.push(DeviceStateUpdate::ButtonDown(i as u8));
                        } else if *was && !now {
                            out.push(DeviceStateUpdate::ButtonUp(i as u8));
                        }
                    }
                }
                None => {
                    // First button report after boot. Synthesise downs
                    // for anything already pressed so a held-at-boot key
                    // still gets its release fired later.
                    for (i, now) in cur.iter().enumerate() {
                        if *now {
                            out.push(DeviceStateUpdate::ButtonDown(i as u8));
                        }
                    }
                }
            }
            *prev_buttons = Some(cur.clone());
        }
        I::EncoderStateChange(cur) => {
            match prev_encoders {
                Some(prev) => {
                    for (i, (was, now)) in prev.iter().zip(cur.iter()).enumerate() {
                        if !was && *now {
                            out.push(DeviceStateUpdate::EncoderDown(i as u8));
                        } else if *was && !now {
                            out.push(DeviceStateUpdate::EncoderUp(i as u8));
                        }
                    }
                }
                None => {
                    for (i, now) in cur.iter().enumerate() {
                        if *now {
                            out.push(DeviceStateUpdate::EncoderDown(i as u8));
                        }
                    }
                }
            }
            *prev_encoders = Some(cur.clone());
        }
        I::EncoderTwist(deltas) => {
            for (i, d) in deltas.iter().enumerate() {
                if *d != 0 {
                    out.push(DeviceStateUpdate::EncoderTwist(i as u8, *d));
                }
            }
        }
        // NoData / TouchScreenPress / etc. — don't touch prev state.
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

fn handle_event(
    ev: &DeviceStateUpdate,
    page: &Page,
    screens: &mut HashMap<u8, SlintScreen>,
    dirty: &mut HashSet<u8>,
) {
    match ev {
        DeviceStateUpdate::ButtonDown(idx) => {
            if let Some(btn) = page.buttons.get(idx) {
                if let Some(cmd) = &btn.on_press {
                    tracing::info!(idx, %cmd, "btn press");
                    spawn_shell(cmd);
                }
                if let Some(screen) = screens.get(idx) {
                    let _ = screen.invoke("tap");
                    dirty.insert(*idx);
                }
            }
        }
        DeviceStateUpdate::ButtonUp(idx) => {
            if let Some(btn) = page.buttons.get(idx) {
                if let Some(cmd) = &btn.on_release {
                    tracing::info!(idx, %cmd, "btn release");
                    spawn_shell(cmd);
                }
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
