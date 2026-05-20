//! Deckfile config — YAML or Rhai.
//!
//! Lookup order: `$DECKFILE` env → `./deckfile.rhai` → `./deckfile.yaml`
//! → `$XDG_CONFIG_HOME/deckfile/deckfile.rhai`
//! → `$XDG_CONFIG_HOME/deckfile/deckfile.yaml`.
//!
//! YAML is the simple declarative form. Rhai gives you config-as-code:
//! factory functions, shell-outs via `sh()`, env reads via `env()`, and
//! conditional logic. Both deserialize into the same `Deckfile` struct.
//!
//! Schema supports two forms (in YAML AND Rhai):
//!   1. Single-page (implicit): top-level `buttons:` / `dials:`.
//!   2. Multi-page (explicit): `pages: { name: { buttons:, dials: } }`.
//! Loader normalizes both into a `pages` map (single-page → "main").

use anyhow::{anyhow, Context, Result};
use lucide_icons::Icon;
use rhai::{Engine, EvalAltResult, Scope};
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Accept maps keyed by either int (YAML `0:`) or string (Rhai `"0":`).
/// Rhai's object maps only support string keys, so without this helper
/// `buttons: { "0": ... }` from a Rhai script fails to deserialize into
/// `BTreeMap<u8, Button>`. YAML's integer keys also fit — serde_yaml
/// stringifies them for us when going through Visitor::visit_str.
fn de_u8_keyed_map<'de, D, T>(d: D) -> Result<BTreeMap<u8, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    use serde::de::Error;
    let raw: BTreeMap<String, T> = BTreeMap::deserialize(d)?;
    raw.into_iter()
        .map(|(k, v)| {
            k.parse::<u8>()
                .map(|i| (i, v))
                .map_err(|e| Error::custom(format!("map key {k:?}: {e}")))
        })
        .collect()
}

#[derive(Debug, Deserialize, Default)]
pub struct Deckfile {
    #[serde(default)]
    pub device: Device,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// Implicit single-page form. Mutually exclusive with `pages`.
    #[serde(default, deserialize_with = "de_u8_keyed_map")]
    pub buttons: BTreeMap<u8, Button>,
    #[serde(default, deserialize_with = "de_u8_keyed_map")]
    pub dials: BTreeMap<u8, Dial>,
    /// Explicit multi-page form. When set, top-level `buttons`/`dials`
    /// are ignored.
    #[serde(default)]
    pub pages: BTreeMap<String, Page>,
}

#[derive(Debug, Deserialize)]
pub struct Device {
    #[serde(default = "default_brightness")]
    pub brightness: u8,
    /// Path to a TTF for fallback text labels. Icons use the embedded
    /// Lucide font regardless of this setting.
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
    #[serde(default, deserialize_with = "de_u8_keyed_map")]
    pub buttons: BTreeMap<u8, Button>,
    #[serde(default, deserialize_with = "de_u8_keyed_map")]
    pub dials: BTreeMap<u8, Dial>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Button {
    /// Lucide icon name (e.g. `microphone`, `globe`, `settings`).
    /// Takes precedence over `label` when set.
    pub icon: Option<Icon>,
    pub icon_active: Option<Icon>,
    pub icon_processing: Option<Icon>,

    /// Plain text label, rendered with the fallback font. Used only when
    /// no icon is set for the current state.
    pub label: Option<String>,
    pub label_active: Option<String>,
    pub label_processing: Option<String>,

    pub bg: Option<String>,
    pub bg_active: Option<String>,
    pub bg_processing: Option<String>,
    pub fg: Option<String>,
    pub fg_active: Option<String>,
    pub fg_processing: Option<String>,
    pub font_size: Option<u32>,

    pub on_press: Option<String>,
    pub on_release: Option<String>,
    pub on_hold: Option<String>,

    /// File whose existence signals "active" (e.g. session pid). Renderer
    /// picks the *_active variants while present.
    pub state_file: Option<PathBuf>,

    /// File whose existence signals "processing" — overrides active so
    /// a transient STT/LLM round-trip can overlay the listening indicator.
    pub processing_file: Option<PathBuf>,

    /// Path to a `.slint` file driving the visual for this button.
    /// When set, the icon/label/bg/fg fields are ignored and rendering
    /// is delegated to the Slint screen — properties `active` and
    /// `processing` (booleans) are bound to the button's state files,
    /// and the `tap` callback is invoked on hardware press.
    pub screen: Option<PathBuf>,

    /// Optional component name inside `screen`. When omitted, the
    /// first exported component is used. Required only when a `.slint`
    /// file declares more than one `export component`.
    pub screen_component: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ButtonState {
    Idle,
    Active,
    Processing,
}

impl Button {
    pub fn state(&self) -> ButtonState {
        if self.processing_file.as_ref().is_some_and(|p| p.exists()) {
            ButtonState::Processing
        } else if self.state_file.as_ref().is_some_and(|p| p.exists()) {
            ButtonState::Active
        } else {
            ButtonState::Idle
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Dial {
    pub icon: Option<Icon>,
    pub label: Option<String>,
    pub on_press: Option<String>,
    pub on_release: Option<String>,
    pub on_turn_up: Option<String>,
    pub on_turn_down: Option<String>,
}

impl Deckfile {
    pub fn load() -> Result<Self> {
        let path = Self::find_path()?;
        let mut cfg: Deckfile = match path.extension().and_then(|s| s.to_str()) {
            Some("rhai") => Self::load_rhai(&path)?,
            _ => Self::load_yaml(&path)?,
        };

        // Normalize implicit single-page form into the `pages` map.
        if cfg.pages.is_empty() && (!cfg.buttons.is_empty() || !cfg.dials.is_empty()) {
            let page = Page {
                buttons: std::mem::take(&mut cfg.buttons),
                dials: std::mem::take(&mut cfg.dials),
            };
            cfg.pages.insert("main".into(), page);
        }

        cfg.substitute_vars();

        tracing::info!(
            path = %path.display(),
            pages = cfg.pages.len(),
            "loaded deckfile",
        );
        Ok(cfg)
    }

    fn load_yaml(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        serde_yaml::from_str(&content)
            .with_context(|| format!("parse YAML {}", path.display()))
    }

    /// Run the Rhai script, take its final expression value, and
    /// deserialize that into our Deckfile struct. The script can use
    /// `sh()`, `env()`, factory functions, conditionals — anything Rhai
    /// supports — to build the configuration dynamically.
    fn load_rhai(path: &Path) -> Result<Self> {
        let mut engine = Engine::new();
        register_helpers(&mut engine);
        let mut scope = Scope::new();
        let result: rhai::Dynamic = engine
            .eval_file_with_scope(&mut scope, path.to_path_buf())
            .map_err(|e: Box<EvalAltResult>| anyhow!("Rhai eval {}: {e}", path.display()))?;
        rhai::serde::from_dynamic::<Deckfile>(&result)
            .map_err(|e| anyhow!("Rhai → Deckfile deserialize: {e}"))
    }

    fn find_path() -> Result<PathBuf> {
        if let Ok(p) = std::env::var("DECKFILE") {
            return Ok(PathBuf::from(p));
        }
        // Prefer Rhai over YAML when both are present — config-as-code
        // is treated as the higher-fidelity source.
        for name in ["deckfile.rhai", "deckfile.yaml"] {
            let p = Path::new(name);
            if p.exists() {
                return Ok(p.to_path_buf());
            }
        }
        let xdg = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|_| {
                std::env::var("HOME")
                    .map(|h| PathBuf::from(h).join(".config"))
            })
            .map_err(|_| anyhow!("neither XDG_CONFIG_HOME nor HOME set"))?;
        for name in ["deckfile/deckfile.rhai", "deckfile/deckfile.yaml"] {
            let p = xdg.join(name);
            if p.exists() {
                return Ok(p);
            }
        }
        Err(anyhow!(
            "no deckfile.{{rhai,yaml}} found. tried: $DECKFILE, ./deckfile.{{rhai,yaml}}, {}/deckfile/deckfile.{{rhai,yaml}}",
            xdg.display(),
        ))
    }

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
                subst(&mut btn.label_processing);
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

/// Minimal `${name}` interpolator. Unknown names are left untouched so
/// typos remain visible instead of silently expanding to empty.
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

/// Register helper functions usable from deckfile.rhai. Keep this list
/// short and dependency-free; complex side-effects belong in shell
/// commands, not config-time.
///
///   sh(cmd)        — run cmd through `sh -c`, return trimmed stdout
///   env(name)      — read env var, "" if missing
///   env(name, dft) — same with explicit default
///   file_exists(p) — true if path exists
fn register_helpers(engine: &mut Engine) {
    use std::process::Command;

    engine.register_fn("sh", |cmd: &str| -> String {
        Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    });

    engine.register_fn("env", |name: &str| -> String {
        std::env::var(name).unwrap_or_default()
    });

    engine.register_fn("env", |name: &str, default: &str| -> String {
        std::env::var(name).unwrap_or_else(|_| default.to_string())
    });

    engine.register_fn("file_exists", |path: &str| -> bool {
        std::path::Path::new(path).exists()
    });
}
