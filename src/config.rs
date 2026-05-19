//! deckfile.yaml schema + loader.
//!
//! Lookup order: $DECKFILE env → ./deckfile.yaml → $XDG_CONFIG_HOME/deckfile/deckfile.yaml.
//! Structs are serde-annotated; new fields stay backwards-compatible (all Option<>).
//!
//! Schema supports two forms:
//!   1. Single-page (implicit): top-level `buttons:` / `dials:`.
//!   2. Multi-page (explicit): `pages: { name: { buttons:, dials: } }`.
//! Loader normalizes both into a `pages` map (single-page → "main").

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default)]
pub struct Deckfile {
    #[serde(default)]
    pub device: Device,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// Implicit single-page form. Mutually exclusive with `pages`.
    #[serde(default)]
    pub buttons: BTreeMap<u8, Button>,
    #[serde(default)]
    pub dials: BTreeMap<u8, Dial>,
    /// Explicit multi-page form. When set, `buttons`/`dials` at top level
    /// are ignored.
    #[serde(default)]
    pub pages: BTreeMap<String, Page>,
}

#[derive(Debug, Deserialize)]
pub struct Device {
    #[serde(default = "default_brightness")]
    pub brightness: u8,
    pub font: Option<PathBuf>,
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
}

impl Default for Device {
    fn default() -> Self {
        Self {
            brightness: default_brightness(),
            font: None,
            poll_ms: default_poll_ms(),
        }
    }
}

fn default_brightness() -> u8 { 60 }
fn default_poll_ms() -> u64 { 500 }

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Page {
    #[serde(default)]
    pub buttons: BTreeMap<u8, Button>,
    #[serde(default)]
    pub dials: BTreeMap<u8, Dial>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Button {
    pub label: Option<String>,
    pub label_active: Option<String>,
    /// Image file rendered onto the key (alternative to `label`).
    pub icon: Option<PathBuf>,
    pub icon_active: Option<PathBuf>,
    pub bg: Option<String>,
    pub bg_active: Option<String>,
    pub fg: Option<String>,
    pub fg_active: Option<String>,
    pub font_size: Option<u32>,

    pub on_press: Option<String>,
    pub on_release: Option<String>,
    /// Command fired when the button is held longer than `hold_ms`
    /// (default 800ms). Hold is detected purely client-side by the daemon.
    pub on_hold: Option<String>,

    /// If this path exists → state=active → renderer picks the *_active
    /// variants. Polled every `device.poll_ms`.
    pub state_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Dial {
    /// Optional label shown on the LCD touchscreen strip above this dial.
    /// Only visible on Stream Deck Plus.
    pub label: Option<String>,
    pub on_press: Option<String>,
    pub on_release: Option<String>,
    pub on_turn_up: Option<String>,
    pub on_turn_down: Option<String>,
}

impl Deckfile {
    pub fn load() -> Result<Self> {
        let path = Self::find_path()?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let mut cfg: Deckfile = serde_yaml::from_str(&content)
            .with_context(|| format!("parse {}", path.display()))?;

        // Normalize the implicit single-page form into the `pages` map.
        if cfg.pages.is_empty() && (!cfg.buttons.is_empty() || !cfg.dials.is_empty()) {
            let page = Page {
                buttons: std::mem::take(&mut cfg.buttons),
                dials: std::mem::take(&mut cfg.dials),
            };
            cfg.pages.insert("main".into(), page);
        }

        // Apply ${var} substitutions to label/command/path fields.
        cfg.substitute_vars();

        tracing::info!(path = %path.display(), pages = cfg.pages.len(), "loaded deckfile.yaml");
        Ok(cfg)
    }

    fn find_path() -> Result<PathBuf> {
        if let Ok(p) = std::env::var("DECKFILE") {
            return Ok(PathBuf::from(p));
        }
        let cwd_path = Path::new("deckfile.yaml");
        if cwd_path.exists() {
            return Ok(cwd_path.to_path_buf());
        }
        let xdg = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|_| {
                std::env::var("HOME")
                    .map(|h| PathBuf::from(h).join(".config"))
            })
            .map_err(|_| anyhow!("neither XDG_CONFIG_HOME nor HOME set"))?;
        let xdg_path = xdg.join("deckfile/deckfile.yaml");
        if xdg_path.exists() {
            return Ok(xdg_path);
        }
        Err(anyhow!(
            "deckfile.yaml not found. tried: $DECKFILE, ./deckfile.yaml, {}",
            xdg_path.display()
        ))
    }

    /// Replace ${var} occurrences in label/command strings using `self.vars`.
    /// Unknown vars are left as-is (so users see them in error messages).
    fn substitute_vars(&mut self) {
        if self.vars.is_empty() {
            return;
        }
        let vars = self.vars.clone();
        let subst = |s: &mut Option<String>| {
            if let Some(v) = s {
                *v = interp(v, &vars);
            }
        };
        for page in self.pages.values_mut() {
            for btn in page.buttons.values_mut() {
                subst(&mut btn.label);
                subst(&mut btn.label_active);
                subst(&mut btn.on_press);
                subst(&mut btn.on_release);
                subst(&mut btn.on_hold);
            }
            for d in page.dials.values_mut() {
                subst(&mut d.label);
                subst(&mut d.on_press);
                subst(&mut d.on_release);
                subst(&mut d.on_turn_up);
                subst(&mut d.on_turn_down);
            }
        }
    }
}

/// Minimal `${name}` interpolator. Unknown names are left untouched to
/// make typos visible to the user instead of silently expanding to empty.
fn interp(src: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(src.len());
    let bytes = src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
            if let Some(end) = src[i + 2..].find('}') {
                let name = &src[i + 2..i + 2 + end];
                if let Some(v) = vars.get(name) {
                    out.push_str(v);
                    i += 2 + end + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
