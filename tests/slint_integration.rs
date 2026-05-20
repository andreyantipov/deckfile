//! Integration tests for the Slint screen pipeline.
//!
//! These run end-to-end: real slint-interpreter, real software
//! renderer, real `image::RgbImage` output. We avoid `Text` elements
//! in the inline test components so the suite works on hosts without
//! fontconfig fully configured — the rendering pipeline still
//! exercises animations, property updates, and callbacks via
//! Rectangle shapes and color transitions.
//!
//! Tests run in parallel. That's safe because every Slint piece of
//! state (NEXT_WINDOW stash, component instance, window) is
//! thread-local; the only piece of global state is the installed
//! `Platform`, which `init_platform()` guards behind a `Once`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// Re-export the crate's screen helpers. We can't depend on private
// modules of a binary crate from an integration test, so the crate
// exposes them through `lib.rs`-style re-exports below this test
// file in `src/lib.rs`.
use deckfile::slint_screen::{tick_animations, SlintScreen};

const SIZE: u32 = 96;

/// Minimal idle/active toggle: red when inactive, green when active.
/// Used by half the suite because it gives unambiguous pixel
/// answers — every center pixel is exactly one of the two colors.
const TOGGLE_SLINT: &str = r#"
export component Toggle inherits Rectangle {
    in property <bool> on: false;
    callback tap;
    width: 96px;
    height: 96px;
    background: on ? #00ff00 : #ff0000;
    TouchArea { clicked => { root.tap(); } }
}
"#;

fn center_pixel(img: &image::RgbImage) -> [u8; 3] {
    let p = img.get_pixel(img.width() / 2, img.height() / 2);
    [p[0], p[1], p[2]]
}

fn approx_eq(actual: [u8; 3], expected: [u8; 3], tol: u8) -> bool {
    actual
        .iter()
        .zip(expected.iter())
        .all(|(a, e)| (*a as i16 - *e as i16).unsigned_abs() <= tol as u16)
}

#[test]
fn loads_inline_component_and_renders() {
    let screen = SlintScreen::load_source(TOGGLE_SLINT, Some("Toggle"), SIZE, SIZE).unwrap();
    let img = screen.render().unwrap();
    assert_eq!(img.dimensions(), (SIZE, SIZE), "rendered image size");
    let pixel = center_pixel(&img);
    assert!(
        approx_eq(pixel, [255, 0, 0], 4),
        "default state is red, got {pixel:?}"
    );
}

#[test]
fn property_change_alters_render() {
    let screen = SlintScreen::load_source(TOGGLE_SLINT, Some("Toggle"), SIZE, SIZE).unwrap();
    let before = center_pixel(&screen.render().unwrap());

    screen.set_bool("on", true).unwrap();
    let after = center_pixel(&screen.render().unwrap());

    assert_ne!(before, after, "color must change after property update");
    assert!(
        approx_eq(after, [0, 255, 0], 4),
        "on=true should turn green, got {after:?}"
    );
}

#[test]
fn picks_first_component_when_name_omitted() {
    let screen = SlintScreen::load_source(TOGGLE_SLINT, None, SIZE, SIZE).unwrap();
    assert_eq!(screen.size(), (SIZE, SIZE));
}

#[test]
fn missing_component_returns_clear_error() {
    let err = SlintScreen::load_source(TOGGLE_SLINT, Some("DoesNotExist"), SIZE, SIZE)
        .err()
        .expect("should fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("DoesNotExist"),
        "error mentions the missing name: {msg}"
    );
    assert!(
        msg.contains("Toggle"),
        "error lists the available components: {msg}"
    );
}

#[test]
fn invalid_slint_source_returns_compile_error() {
    let bad = "this is not slint syntax {{}";
    let err = SlintScreen::load_source(bad, None, SIZE, SIZE)
        .err()
        .expect("should fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("compile errors"),
        "error labelled as compile failure: {msg}"
    );
}

#[test]
fn callback_invoke_runs_registered_handler() {
    let screen = SlintScreen::load_source(TOGGLE_SLINT, Some("Toggle"), SIZE, SIZE).unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter2 = counter.clone();

    screen
        .set_callback("tap", move |_| {
            counter2.fetch_add(1, Ordering::SeqCst);
            slint_interpreter::Value::Void
        })
        .unwrap();

    screen.invoke("tap").unwrap();
    screen.invoke("tap").unwrap();

    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn tap_test_example_compiles_and_responds_to_invoke() {
    // Diagnostic harness for the daemon's invoke("tap") pipeline.
    // The .slint declares a default handler for `tap` that bumps the
    // `count` property; invoking the callback from Rust should shift
    // the background color, which we sample at the safe corner pixel.
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("tap-test.slint");
    let screen = SlintScreen::load_path(&path, Some("TapTest"), SIZE, SIZE)
        .expect("tap-test.slint should compile");
    let idle = center_pixel_excluding_white(&screen.render().unwrap());

    screen.invoke("tap").unwrap();
    for _ in 0..30 {
        tick_animations();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let tapped = center_pixel_excluding_white(&screen.render().unwrap());

    assert_ne!(idle, tapped, "tap should change visuals: {idle:?} vs {tapped:?}");
}

#[test]
fn voice_example_loads_from_disk() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("voice.slint");
    let screen = SlintScreen::load_path(&path, Some("VoiceScreen"), SIZE, SIZE)
        .expect("voice.slint should compile");

    // Idle: dark gray background (#3d3d3d).
    let idle = center_pixel_excluding_white(&screen.render().unwrap());
    assert!(
        approx_eq(idle, [0x3d, 0x3d, 0x3d], 8),
        "idle bg ≈ #3d3d3d, got {idle:?}"
    );

    // Activate: bg fades toward green (#1e4d2b). Animation crossfades
    // over 250ms, so we tick a few times to advance past it.
    screen.set_bool("active", true).unwrap();
    for _ in 0..30 {
        tick_animations();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let active = center_pixel_excluding_white(&screen.render().unwrap());
    assert!(
        approx_eq(active, [0x1e, 0x4d, 0x2b], 12),
        "active bg ≈ #1e4d2b, got {active:?}"
    );
}

/// Voice screen has a white circle in the middle and 16-pixel rounded
/// corners. Sampling near any corner picks up antialiasing from the
/// rounded mask; sampling the exact center picks up the white dot.
/// Read the left edge at the vertical midpoint — far from both the
/// dot (x ∈ [30..66]) and the rounded corner band (y ∈ 16..80).
fn center_pixel_excluding_white(img: &image::RgbImage) -> [u8; 3] {
    let p = img.get_pixel(8, img.height() / 2);
    [p[0], p[1], p[2]]
}

#[test]
fn animation_advances_property_over_time() {
    // A property bound with `animate { duration: 200ms }` should
    // interpolate between two values across calls to
    // tick_animations(). We verify by sampling pixel intensity at
    // the start vs. partway through.
    const ANIM_SRC: &str = r#"
        export component Fader inherits Rectangle {
            in property <bool> bright: false;
            width: 96px;
            height: 96px;
            background: bright ? #ffffff : #000000;
            animate background { duration: 400ms; easing: linear; }
        }
    "#;

    let screen = SlintScreen::load_source(ANIM_SRC, Some("Fader"), SIZE, SIZE).unwrap();
    let dark = center_pixel(&screen.render().unwrap());
    assert!(dark[0] < 30, "initial frame is black, got {dark:?}");

    screen.set_bool("bright", true).unwrap();

    // Right after toggling, before time advances, the rendered frame
    // should still be near-black (animation hasn't started ticking).
    // We tick a bit to advance roughly half the duration.
    let start = std::time::Instant::now();
    let mut mid: [u8; 3] = [0, 0, 0];
    while start.elapsed() < std::time::Duration::from_millis(180) {
        tick_animations();
        mid = center_pixel(&screen.render().unwrap());
        if mid[0] > 60 && mid[0] < 220 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        mid[0] > dark[0] + 30,
        "mid-animation pixel brighter than start: dark={dark:?}, mid={mid:?}"
    );

    // Now tick well past the duration; we should land at full white.
    let end_deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
    let mut bright = mid;
    while std::time::Instant::now() < end_deadline {
        tick_animations();
        bright = center_pixel(&screen.render().unwrap());
        if bright[0] > 240 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        bright[0] > 240,
        "animation reaches full white, got {bright:?}"
    );
}
