//! Debug renderer — dumps a .slint file to a PNG at the device's
//! actual key resolution, so we can SEE what the daemon is shipping
//! to the LCD instead of guessing from a 2cm screen.
//!
//! Usage:
//!   render-slint <file.slint> [Component] [--active] [--processing]
//!                              [--count=N] [--size=W,H] [--out=path.png]
//!
//! Without a component name the first exported one wins. State flags
//! flip the corresponding boolean property; `--count=N` invokes the
//! `tap` callback N times before rendering (lets you inspect the
//! tap-test's color cycle).

use anyhow::{anyhow, Context, Result};
use deckfile::slint_screen::{tick_animations, SlintScreen};
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: render-slint <file.slint> [Component] [--active] [--processing] [--count=N] [--size=WxH] [--out=path.png]");
        std::process::exit(2);
    }

    let path = PathBuf::from(&args[1]);
    let component = args.iter().skip(2).find(|a| !a.starts_with("--")).cloned();

    let active = args.iter().any(|a| a == "--active");
    let processing = args.iter().any(|a| a == "--processing");
    let count: u32 = args
        .iter()
        .find_map(|a| a.strip_prefix("--count="))
        .map(|n| n.parse().unwrap_or(0))
        .unwrap_or(0);
    let (w, h) = args
        .iter()
        .find_map(|a| a.strip_prefix("--size="))
        .map(|s| {
            let (a, b) = s
                .split_once('x')
                .ok_or_else(|| anyhow!("--size must be WxH"))?;
            Ok::<_, anyhow::Error>((a.parse::<u32>()?, b.parse::<u32>()?))
        })
        .transpose()?
        .unwrap_or((120, 120));
    let out = args
        .iter()
        .find_map(|a| a.strip_prefix("--out="))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rendered.png"));

    let screen = SlintScreen::load_path(&path, component.as_deref(), w, h)
        .with_context(|| format!("load {}", path.display()))?;

    let _ = screen.set_bool("active", active);
    let _ = screen.set_bool("processing", processing);
    for _ in 0..count {
        let _ = screen.invoke("tap");
    }

    // Let animations tick to their resting state so the snapshot
    // shows what a *settled* frame looks like, not the first 1ms.
    for _ in 0..40 {
        tick_animations();
        std::thread::sleep(std::time::Duration::from_millis(15));
    }

    let img = screen.render()?;
    img.save(&out).with_context(|| format!("save {}", out.display()))?;

    println!(
        "wrote {} ({}x{}) — active={active} processing={processing} count={count}",
        out.display(),
        w,
        h,
    );
    Ok(())
}
