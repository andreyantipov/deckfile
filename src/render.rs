//! Button image rendering: text/icon centered on the device's LCD key
//! (96x96 on Plus). Builds an RGB image, draws the background and a
//! centered glyph via imageproc + ab_glyph TTF. The font path comes
//! from `device.font` in deckfile.yaml or the $DECKFILE_FONT env var
//! (set by the Nix flake to a system font by default).

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use anyhow::{anyhow, Context, Result};
use image::{Rgb, RgbImage};
use imageproc::drawing::draw_text_mut;
use std::path::Path;

use crate::config::Button;

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

    pub fn render(&self, btn: &Button, active: bool) -> Result<RgbImage> {
        let label = if active {
            btn.label_active.as_deref().or(btn.label.as_deref()).unwrap_or("")
        } else {
            btn.label.as_deref().unwrap_or("")
        };
        let bg = parse_color(if active {
            btn.bg_active.as_deref().or(btn.bg.as_deref())
        } else {
            btn.bg.as_deref()
        }.unwrap_or("#000000"))?;
        let fg = parse_color(if active {
            btn.fg_active.as_deref().or(btn.fg.as_deref())
        } else {
            btn.fg.as_deref()
        }.unwrap_or("#FFFFFF"))?;

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
