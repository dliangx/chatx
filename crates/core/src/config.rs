//! User-writable runtime configuration.
//!
//! Values live in a single `config.toml` in the per-profile home directory
//! (e.g. `~/.chatx/default/config.toml`). On first launch the file is seeded
//! from the crate default (`crates/core/config.toml`) so every value is
//! editable in one place — the same mechanism the signal-server default uses.
//!
//! Reads always *merge* the crate defaults under the user's file (user wins),
//! so an older user config that predates a new key still gets a sane default
//! instead of silently losing it.

use crate::identity::DeviceIdentity;
use std::path::PathBuf;
use toml::Value;

/// A single AI-assistant model in the built-in download catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub name: String,
    pub url: String,
}

/// Hosts the model download URL is allowed to point at. The catalog is
/// user-editable, so we constrain it: models are multi-gigabyte binaries that
/// later run on-device, and an arbitrary host is a man-in-the-middle /
/// supply-chain vector. Restrict to the Hugging Face CDN + its official
/// mirror over TLS.
const ALLOWED_MODEL_HOSTS: &[&str] = &["huggingface.co", "hf-mirror.com"];

/// True when `url` is a well-formed, allowed model download URL:
///   * `https` only
///   * host in [`ALLOWED_MODEL_HOSTS`]
///   * path ends in `.gguf`
///
/// Parsed by hand (no extra dependency) so the check is cheap and explicit.
pub fn is_allowed_model_url(url: &str) -> bool {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        None => return false,
    };
    if scheme != "https" {
        return false;
    }
    let host = match rest.split('/').next() {
        Some(h) if !h.is_empty() => h,
        _ => return false,
    };
    let (host, port) = match host.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, true),
        Some(_) => return false,
        None => (host, false),
    };
    if !port
        && !host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
    {
        return false;
    }
    if !ALLOWED_MODEL_HOSTS
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed))
    {
        return false;
    }
    url.trim_end_matches('/').ends_with(".gguf")
}

/// The per-profile `config.toml` path.
pub fn path() -> PathBuf {
    let profile = DeviceIdentity::default_profile();
    DeviceIdentity::profile_dir(&profile).join("config.toml")
}

/// Read and return the effective configuration: crate defaults with the user's
/// `config.toml` overlaid (user values win). Seeds the user file from the crate
/// default on first launch. Never fails — on any read/parse error the effective
/// config is simply the crate defaults.
pub fn load() -> Value {
    let p = path();
    if !p.exists() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&p, include_str!("../config.toml"));
    }
    let user = std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| toml::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| Value::Table(toml::map::Map::new()));
    let defaults: Value = include_str!("../config.toml")
        .parse()
        .unwrap_or_else(|_| Value::Table(toml::map::Map::new()));
    merge(defaults, user)
}

/// `over` wins over `base`; nested tables are merged recursively.
fn merge(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Table(mut a), Value::Table(b)) => {
            for (k, v) in b {
                let merged = merge(a.remove(&k).unwrap_or(Value::String(String::new())), v);
                a.insert(k, merged);
            }
            Value::Table(a)
        }
        (_, over) => over,
    }
}

/// The built-in model catalog for `platform` (`"desktop"` / `"ios"` /
/// `"android"`), read from the `[models]` table of the effective config.
/// Returns an empty list when the platform section is absent or malformed.
pub fn model_catalog(platform: &str) -> Vec<ModelEntry> {
    let cfg = load();
    catalog_from_value(&cfg, platform)
}

/// Extract and validate the model catalog for `platform` from an already-parsed
/// config value. The catalog is a nested table —
/// `models.<platform>.<model-name> = "<url>"` — so the model name is the table
/// key and the value is the download URL. Split out from [`model_catalog`] so
/// it can be tested against a synthetic TOML without touching the on-disk user
/// config. Returns entries in (stable) key order; malformed entries are skipped.
pub fn catalog_from_value(cfg: &Value, platform: &str) -> Vec<ModelEntry> {
    let Some(t) = cfg
        .get("models")
        .and_then(|m| m.get(platform))
        .and_then(|v| v.as_table())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, urlv) in t {
        let Some(url) = urlv.as_str() else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        if !is_allowed_model_url(url) {
            tracing::warn!(
                name = %name,
                url = %url,
                "model catalog entry dropped: URL not in the allowed"
            );
            continue;
        }
        out.push(ModelEntry {
            name: name.trim().to_string(),
            url: url.trim().to_string(),
        });
    }
    out
}
