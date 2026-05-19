//! Button image rendering. Two fonts in play:
//!   - LUCIDE_FONT_BYTES (embedded by the lucide-icons crate) renders
//!     glyphs for the Icon enum (`icon: microphone` in YAML).
//!   - A regular text font (via $DECKFILE_FONT or `device.font`) renders
//!     plain alphabetic labels when no icon is set.
//!
//! Variant selection follows ButtonState (Processing > Active > Idle)
//! and falls through for icon/label/bg/fg independently.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{anyhow, Context, Result};
use image::{Rgb, RgbImage};
use imageproc::drawing::draw_text_mut;
use lucide_icons::{Icon, LUCIDE_FONT_BYTES};
use std::path::Path;

use crate::config::{Button, ButtonState};

pub struct Renderer {
    text_font: Vec<u8>,
    size: (u32, u32),
}

impl Renderer {
    pub fn new(font_path: Option<&Path>, size: (u32, u32)) -> Result<Self> {
        let path = match font_path {
            Some(p) => p.to_path_buf(),
            None => std::env::var("DECKFILE_FONT")
                .map(std::path::PathBuf::from)
                .map_err(|_| anyhow!(
                    "no text font: set device.font in deckfile.yaml or DECKFILE_FONT env"
                ))?,
        };
        let text_font = std::fs::read(&path)
            .with_context(|| format!("read font {}", path.display()))?;
        FontRef::try_from_slice(&text_font).context("text font parse")?;
        // Sanity-check the embedded Lucide font as well — it's a compile-
        // time constant but worth catching corruption early.
        FontRef::try_from_slice(LUCIDE_FONT_BYTES).context("lucide font parse")?;
        Ok(Self { text_font, size })
    }

    pub fn render(&self, btn: &Button, state: ButtonState) -> Result<RgbImage> {
        let bg = parse_color(pick_bg(btn, state))?;
        let fg = parse_color(pick_fg(btn, state))?;
        let mut img = RgbImage::from_pixel(self.size.0, self.size.1, bg);

        let font_size = btn.font_size.unwrap_or(48) as f32;

        if let Some(icon) = pick_icon(btn, state) {
            // Render the Lucide glyph.
            let font = FontRef::try_from_slice(LUCIDE_FONT_BYTES)?;
            draw_centered(&mut img, &font, icon.unicode(), font_size, fg, self.size);
        } else {
            let label = pick_label(btn, state);
            if !label.is_empty() {
                let font = FontRef::try_from_slice(&self.text_font)?;
                draw_text_centered(&mut img, &font, label, font_size, fg, self.size);
            }
        }

        Ok(img)
    }
}

fn pick_icon(btn: &Button, state: ButtonState) -> Option<Icon> {
    match state {
        ButtonState::Processing => btn.icon_processing.or(btn.icon_active).or(btn.icon),
        ButtonState::Active => btn.icon_active.or(btn.icon),
        ButtonState::Idle => btn.icon,
    }
}

fn pick_label(btn: &Button, state: ButtonState) -> &str {
    match state {
        ButtonState::Processing => btn.label_processing.as_deref()
            .or(btn.label_active.as_deref())
            .or(btn.label.as_deref())
            .unwrap_or(""),
        ButtonState::Active => btn.label_active.as_deref()
            .or(btn.label.as_deref())
            .unwrap_or(""),
        ButtonState::Idle => btn.label.as_deref().unwrap_or(""),
    }
}

fn pick_bg(btn: &Button, state: ButtonState) -> &str {
    match state {
        ButtonState::Processing => btn.bg_processing.as_deref()
            .or(btn.bg_active.as_deref())
            .or(btn.bg.as_deref())
            .unwrap_or("#000000"),
        ButtonState::Active => btn.bg_active.as_deref()
            .or(btn.bg.as_deref())
            .unwrap_or("#000000"),
        ButtonState::Idle => btn.bg.as_deref().unwrap_or("#000000"),
    }
}

fn pick_fg(btn: &Button, state: ButtonState) -> &str {
    match state {
        ButtonState::Processing => btn.fg_processing.as_deref()
            .or(btn.fg_active.as_deref())
            .or(btn.fg.as_deref())
            .unwrap_or("#FFFFFF"),
        ButtonState::Active => btn.fg_active.as_deref()
            .or(btn.fg.as_deref())
            .unwrap_or("#FFFFFF"),
        ButtonState::Idle => btn.fg.as_deref().unwrap_or("#FFFFFF"),
    }
}

fn draw_centered<F: Font>(
    img: &mut RgbImage, font: &F, ch: char, size: f32, color: Rgb<u8>, canvas: (u32, u32),
) {
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let gid = font.glyph_id(ch);
    let width = scaled.h_advance(gid);
    let height = scaled.ascent() - scaled.descent();
    let x = ((canvas.0 as f32 - width) / 2.0).max(0.0) as i32;
    let y = ((canvas.1 as f32 - height) / 2.0).max(0.0) as i32;
    let s = ch.to_string();
    draw_text_mut(img, color, x, y, scale, font, &s);
}

fn draw_text_centered<F: Font>(
    img: &mut RgbImage, font: &F, text: &str, size: f32, color: Rgb<u8>, canvas: (u32, u32),
) {
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let mut width = 0.0f32;
    let mut last: Option<ab_glyph::GlyphId> = None;
    for c in text.chars() {
        let g = font.glyph_id(c);
        width += scaled.h_advance(g);
        if let Some(prev) = last {
            width += scaled.kern(prev, g);
        }
        last = Some(g);
    }
    let height = scaled.ascent() - scaled.descent();
    let x = ((canvas.0 as f32 - width) / 2.0).max(0.0) as i32;
    let y = ((canvas.1 as f32 - height) / 2.0).max(0.0) as i32;
    draw_text_mut(img, color, x, y, scale, font, text);
}

fn parse_color(s: &str) -> Result<Rgb<u8>> {
    let h = s.trim_start_matches('#');
    let hex = match h.len() {
        6 => h.to_string(),
        _ => match h {
            "black" => "000000".into(),
            "white" => "FFFFFF".into(),
            "red"   => "FF0000".into(),
            "green" => "00FF00".into(),
            "blue"  => "0000FF".into(),
            _ => return Err(anyhow!("invalid color: {}", s)),
        },
    };
    let r = u8::from_str_radix(&hex[0..2], 16)?;
    let g = u8::from_str_radix(&hex[2..4], 16)?;
    let b = u8::from_str_radix(&hex[4..6], 16)?;
    Ok(Rgb([r, g, b]))
}
