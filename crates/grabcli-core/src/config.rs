//! `config.toml`: user settings. Every key is optional.
//!
//! ```toml
//! # Placing real orders stays impossible until the user sets this to true
//! # themselves. grabcli never writes this key.
//! allow_ordering = false
//!
//! # Default payment method for quotes and orders (Grab's paymentTypeID).
//! payment_type_id = "123456789"
//!
//! # Chrome DevTools Protocol endpoint of a logged-in browser, used by
//! # `grab auth import` and as hydra/cookie refresh fallback.
//! cdp = "http://127.0.0.1:9222"
//!
//! # Refresh hydra / cookies automatically when they expire (default true).
//! auto_refresh = true
//!
//! # How to mint the hydra device JWT: "auto" (native if a key is present,
//! # else CDP), "native" or "cdp".
//! hydra_mode = "auto"
//!
//! # Phone number `grab auth login` uses when --phone is not given
//! # (international form). Only ever printed masked.
//! phone = "+63XXXXXXXXXX"
//!
//! # Local cache for read-only browse data (see cache/mod.rs). TTLs in seconds.
//! [cache]
//! enabled = true
//! image_max_mb = 500
//! index_max_age_days = 30
//! [cache.ttl]
//! list = 600
//! menu = 1800
//! details = 21600
//! categories = 86400
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::cache::CacheConfig;
use crate::error::{Error, Result};

pub const DEFAULT_CDP: &str = "http://127.0.0.1:9222";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub allow_ordering: bool,
    pub payment_type_id: Option<String>,
    pub cdp: Option<String>,
    pub auto_refresh: Option<bool>,
    pub hydra_mode: Option<HydraMode>,
    /// Path to the hydra signing key (PEM). Default `<config dir>/hydra-key.pem`.
    pub hydra_key: Option<PathBuf>,
    /// Page the CDP hydra minter opens. Must be a food.grab.com page that
    /// makes a hydra-protected API call.
    pub hydra_page: Option<String>,
    /// Override the User-Agent sent to Grab (default: the browser's, saved at
    /// import time).
    pub user_agent: Option<String>,
    /// Default phone for `grab auth login` (international form, `+63…`).
    /// Validated on load; only ever printed masked.
    pub phone: Option<String>,
    /// `[cache]`: response cache switch and per-kind TTLs.
    pub cache: CacheConfig,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HydraMode {
    #[default]
    Auto,
    Native,
    Cdp,
}

impl std::str::FromStr for HydraMode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "native" => Ok(Self::Native),
            "cdp" => Ok(Self::Cdp),
            other => Err(format!("unknown hydra mode `{other}` (auto|native|cdp)")),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::parse(&s)
                .map_err(|e| Error::new(e.kind, format!("{}: {}", path.display(), e.message))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Parse and validate `config.toml` text.
    pub fn parse(s: &str) -> Result<Self> {
        let c: Self = toml::from_str(s).map_err(|e| Error::invalid(e.to_string()))?;
        if let Some(p) = &c.phone {
            // The error never echoes the number.
            crate::login::split_phone(p)
                .map_err(|e| Error::invalid(format!("`phone`: {}", e.message)))?;
        }
        Ok(c)
    }

    pub fn auto_refresh(&self) -> bool {
        self.auto_refresh.unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_ordering_off() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = Config::load(&dir.path().join("missing.toml"))?;
        assert!(!c.allow_ordering);
        assert!(c.auto_refresh());
        Ok(())
    }

    #[test]
    fn parses_keys() -> Result<()> {
        let c: Config = toml::from_str(
            "allow_ordering = true\npayment_type_id = \"42\"\nhydra_mode = \"cdp\"\n",
        )
        .map_err(|e| Error::invalid(e.to_string()))?;
        assert!(c.allow_ordering);
        assert_eq!(c.payment_type_id.as_deref(), Some("42"));
        assert_eq!(c.hydra_mode, Some(HydraMode::Cdp));
        Ok(())
    }

    #[test]
    fn parses_cache_section() -> Result<()> {
        use crate::cache::CacheKind;
        let c: Config = toml::from_str("[cache]\nenabled = false\n[cache.ttl]\nmenu = 60\n")
            .map_err(|e| Error::invalid(e.to_string()))?;
        assert!(!c.cache.enabled());
        assert_eq!(c.cache.ttl(CacheKind::Menu), 60);
        assert_eq!(c.cache.ttl(CacheKind::List), CacheKind::List.default_ttl());
        assert!(toml::from_str::<Config>("[cache.ttl]\nquote = 60\n").is_err());
        Ok(())
    }

    #[test]
    fn phone_is_optional() -> Result<()> {
        assert_eq!(Config::parse("allow_ordering = false\n")?.phone, None);
        let c = Config::parse("phone = \"+63 900 000 0012\"\n")?;
        assert_eq!(c.phone.as_deref(), Some("+63 900 000 0012"));
        Ok(())
    }

    #[test]
    fn rejects_malformed_phone() -> Result<()> {
        for bad in ["09000000012", "+1 555 000 0000", "+63 12", "+63abc", ""] {
            let e = Config::parse(&format!("phone = \"{bad}\"\n")).err();
            let e = e.ok_or_else(|| Error::invalid(format!("accepted {bad:?}")))?;
            assert!(e.message.starts_with("`phone`: "), "{}", e.message);
        }
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "phone = \"0900 000 0012\"\n")?;
        let e = Config::load(&path)
            .err()
            .ok_or_else(|| Error::invalid("loaded"))?;
        assert!(
            e.message
                .contains("config.toml: `phone`: phone must be in international form")
        );
        assert!(!e.message.contains("0012"));
        Ok(())
    }

    #[test]
    fn rejects_unknown_keys() {
        let r: std::result::Result<Config, _> = toml::from_str("allow_orderin = true");
        assert!(r.is_err());
    }
}
