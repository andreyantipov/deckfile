//! Button image rendering: text/icon centered on the device's LCD key
//! (96x96 on Plus). The renderer picks the variant based on ButtonState
//! (Idle / Active / Processing) and falls back through label_active →
//! label → "" the same way for bg/fg/icon.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{anyhow, Context, Result};
use image::{Rgb, RgbImage};
use imageproc::drawing::draw_text_mut;
use std::path::Path;

use crate::config::{Button, ButtonState};

pub struct Renderer {
    font_data: Vec<u8>,
    size: (u32, u32),
}

impl Renderer {
    pub fn new(font_path: Option<&Path>, size: (u32, u32)) -> Result<Self> {
        let path = match font_path {
            Some(p) => p.to_path_buf(),
            None => std::env::var("DECKFILE_FONT")
                .map(std::path::PathBuf::from)
                .map_err(|_| anyhow!(
                    "no font: set device.font in deckfile.yaml or DECKFILE_FONT env"
                ))?,
        };
        let font_data = std::fs::read(&path)
            .with_context(|| format!("read font {}", path.display()))?;
        FontRef::try_from_slice(&font_data).context("font parse")?;
        Ok(Self { font_data, size })
    }

    pub fn render(&self, btn: &Button, state: ButtonState) -> Result<RgbImage> {
        let label = pick_label(btn, state);
        let bg = parse_color(pick_bg(btn, state))?;
        let fg = parse_color(pick_fg(btn, state))?;

        let mut img = RgbImage::from_pixel(self.size.0, self.size.1, bg);
        let font = FontRef::try_from_slice(&self.font_data)?;

        if !label.is_empty() {
            let font_size = btn.font_size.unwrap_or(36) as f32;
            let scale = PxScale::from(font_size);
            let scaled = font.as_scaled(scale);
            let mut width = 0.0f32;
            let mut last_glyph: Option<ab_glyph::GlyphId> = None;
            for c in label.chars() {
                let g = font.glyph_id(c);
                width += scaled.h_advance(g);
                if let Some(prev) = last_glyph {
                    width += scaled.kern(prev, g);
                }
                last_glyph = Some(g);
            }
            let height = scaled.ascent() - scaled.descent();
            let x = ((self.size.0 as f32 - width) / 2.0).max(0.0) as i32;
            let y = ((self.size.1 as f32 - height) / 2.0).max(0.0) as i32;
            draw_text_mut(&mut img, fg, x, y, scale, &font, label);
        }

        Ok(img)
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
