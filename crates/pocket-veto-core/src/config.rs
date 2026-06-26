//! Project and global configuration loading/parsing.
//!
//! The config file lives at `~/.pocket-veto/config.toml` (mode 0600 on unix)
//! and is the single source of truth for the server URL, bearer token,
//! database path, approval timeout, and Bluetooth backend selection.
//!
//! # Path resolution
//!
//! The home directory is resolved via [`dirs::home_dir`]. If that returns
//! `None` (rare, but possible in stripped-down environments), the loader
//! falls back to [`dirs::config_dir`] joined with `pocket-veto/config.toml`
//! — i.e. the XDG-style `~/.config/pocket-veto/config.toml` on Linux and the
//! equivalent on macOS/Windows. This keeps the loader functional even when
//! `$HOME` is unset, while preferring the canonical `~/.pocket-veto/`
//! location.
//!
//! # Defaults
//!
//! [`Config::default`] produces the canonical default values. Each field
//! carries a `#[serde(default = "default_<field>")]` (or `#[serde(default)]`
//! when the type's [`Default`] already matches), so a partial TOML file fills
//! missing fields with those same defaults rather than failing — and an empty
//! TOML deserializes to a value equal to [`Config::default`] (modulo the
//! freshly-randomized [`Token`]).

use std::path::{Path, PathBuf};

use rand;
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, NormalizeError, Result};
use crate::protocol::Token;

/// Subdirectory inside the user's home directory that holds the config and
/// the `SQLite` database.
const CONFIG_DIR: &str = ".pocket-veto";

/// File name within [`CONFIG_DIR`].
const CONFIG_FILE: &str = "config.toml";

/// Default server / bind URL. The hook subcommand talks to this URL; the
/// server binds this address.
const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:38475";
const DEFAULT_BIND_ADDR: &str = "127.0.0.1:38475";

/// Default approval timeout (5 minutes), in seconds.
const DEFAULT_APPROVAL_TIMEOUT_SECONDS: u64 = 300;

/// Which Bluetooth backend the server should drive. Selected via `init` and
/// stored in config; the binary uses it to pick the `pocket-veto-bt` impl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BtBackend {
    /// Linux: `BlueZ` via the `bluer` crate (RFCOMM).
    Bluer,
    /// Windows: serial port profile via the `serialport` crate.
    Serialport,
}

impl BtBackend {
    /// The lowercase wire/config string for this backend (`"bluer"` /
    /// `"serialport"`), matching the `#[serde(rename_all = "snake_case")]`
    /// form written to `config.toml`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bluer => "bluer",
            Self::Serialport => "serialport",
        }
    }
}

impl Default for BtBackend {
    /// Defaults to the Linux/`BlueZ` backend; Windows callers override via
    /// `init` or by setting `bt_com_port` (which forces [`BtBackend::Serialport`]).
    fn default() -> Self {
        Self::Bluer
    }
}

/// The on-disk config shape (`~/.pocket-veto/config.toml`).
///
/// Every field is `#[serde(default = ...)]` so a partial TOML file is
/// accepted and missing fields are filled from the same defaults used by
/// [`Config::default`]. The bearer token is the [`Token`] newtype, which
/// serializes as a plain string (`#[serde(transparent)]`), so the config
/// file format is identical to a bare `String`-token shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// URL the hook subcommand uses to reach the server (e.g.
    /// `http://127.0.0.1:38475` or `http://host.docker.internal:38475`).
    #[serde(default = "default_server_url")]
    pub server_url: String,
    /// Address the server binds (`127.0.0.1:38475`, or `0.0.0.0:38475` when
    /// devcontainer support is enabled).
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    /// Bearer token shared between hook subcommand and server.
    #[serde(default = "default_token")]
    pub token: Token,
    /// Filesystem path to the `SQLite` database.
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// How long the hook waits for a decision before timing out, in seconds.
    #[serde(default = "default_approval_timeout_seconds")]
    pub approval_timeout_seconds: u64,
    /// Which Bluetooth backend to use.
    #[serde(default)]
    pub bt_backend: BtBackend,
    /// Windows-only: the COM port of the paired SPP device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bt_com_port: Option<String>,
    /// Linux-only: the adapter address to bind the RFCOMM listener to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bt_adapter_addr: Option<String>,
    /// Linux-only: the RFCOMM channel number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bt_channel: Option<u8>,
    /// When true, the server binds `0.0.0.0` for devcontainer access.
    #[serde(default)]
    pub devcontainer: bool,
}

// Per-field default functions. These are the single source of truth for the
// non-trivial defaults; both `#[serde(default = "...")]` and `Config::default`
// call them, so an empty/partial TOML deserializes to the same value as
// `Config::default` (no duplicated default literals).

fn default_server_url() -> String {
    DEFAULT_SERVER_URL.to_string()
}

fn default_bind_addr() -> String {
    DEFAULT_BIND_ADDR.to_string()
}

/// Default token: a freshly generated 32-byte random token, hex-encoded.
/// Random by construction, so it intentionally differs between calls.
fn default_token() -> Token {
    Config::generate_token()
}

fn default_db_path() -> String {
    dirs::home_dir().map_or_else(
        || "pocket-veto.sqlite".to_string(),
        |h| {
            h.join(CONFIG_DIR)
                .join("pocket-veto.sqlite")
                .to_string_lossy()
                .into_owned()
        },
    )
}

fn default_approval_timeout_seconds() -> u64 {
    DEFAULT_APPROVAL_TIMEOUT_SECONDS
}

impl Default for Config {
    /// Build a [`Config`] with sensible defaults (used by `init`).
    ///
    /// The token is freshly generated; every other field matches the
    /// corresponding `default_<field>` helper, so this is also what serde
    /// produces for an empty TOML file.
    fn default() -> Self {
        Self {
            server_url: default_server_url(),
            bind_addr: default_bind_addr(),
            token: default_token(),
            db_path: default_db_path(),
            approval_timeout_seconds: default_approval_timeout_seconds(),
            bt_backend: BtBackend::default(),
            bt_com_port: None,
            bt_adapter_addr: None,
            bt_channel: None,
            devcontainer: false,
        }
    }
}

impl Config {
    /// Resolve the config file path.
    ///
    /// Prefers `$HOME/.pocket-veto/config.toml`. If `dirs::home_dir()`
    /// returns `None`, falls back to `dirs::config_dir()/pocket-veto/config.toml`.
    /// If both are unavailable, returns a [`CoreError::Normalize`]-style error
    /// (reused here as a generic "missing environment" error).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Normalize`] when neither a home directory nor a
    /// config directory can be located.
    pub fn config_path() -> Result<PathBuf> {
        if let Some(home) = dirs::home_dir() {
            return Ok(home.join(CONFIG_DIR).join(CONFIG_FILE));
        }
        if let Some(cfg) = dirs::config_dir() {
            return Ok(cfg.join("pocket-veto").join(CONFIG_FILE));
        }
        Err(CoreError::Normalize {
            kind: NormalizeError::NoConfigDir,
            field: "config_path",
        })
    }

    /// Load the config from `path`.
    ///
    /// Errors clearly if the file is missing (a distinct, actionable problem)
    /// versus unreadable or unparseable. Missing fields are filled from
    /// [`Config::default`] via the per-field `#[serde(default = "...")]` attrs.
    ///
    /// # Errors
    ///
    /// - [`CoreError::Normalize`] if the config file does not exist.
    /// - [`CoreError::ConfigIo`] if the file exists but cannot be read.
    /// - [`CoreError::ConfigParse`] if the file contents are not valid TOML
    ///   for [`Config`].
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(CoreError::Normalize {
                kind: NormalizeError::ConfigNotFound {
                    path: path.display().to_string(),
                },
                field: "config_path",
            });
        }
        let contents = std::fs::read_to_string(path).map_err(|e| CoreError::ConfigIo {
            path: path.to_path_buf(),
            source: e,
        })?;
        let cfg: Config = toml::from_str(&contents)?;
        Ok(cfg)
    }

    /// Serialize `self` to TOML and write it to `path`, creating the parent
    /// directory and setting the file mode to `0600` on unix.
    ///
    /// On non-unix platforms the mode step is a no-op.
    ///
    /// # Errors
    ///
    /// - [`CoreError::ConfigIo`] if the parent directory cannot be created,
    ///   the file cannot be written, or (on unix) permissions cannot be
    ///   read/set.
    /// - [`CoreError::ConfigSerialize`] if `self` cannot be serialized to TOML.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().ok_or_else(|| CoreError::Normalize {
            kind: NormalizeError::NoParentDir,
            field: "config_path",
        })?;
        std::fs::create_dir_all(dir).map_err(|e| CoreError::ConfigIo {
            path: dir.to_path_buf(),
            source: e,
        })?;

        let toml_str = toml::to_string_pretty(self)?;

        std::fs::write(path, toml_str.as_bytes()).map_err(|e| CoreError::ConfigIo {
            path: path.to_path_buf(),
            source: e,
        })?;

        set_owner_only(path)?;
        Ok(())
    }

    /// Generate a fresh 32-byte random token, hex-encoded (64 chars).
    #[must_use]
    pub fn generate_token() -> Token {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        Token::from(hex::encode(bytes))
    }
}

/// Set the file mode to `0600` on unix; no-op elsewhere.
#[cfg(target_family = "unix")]
fn set_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| CoreError::ConfigIo {
            path: path.to_path_buf(),
            source: e,
        })?
        .permissions();
    // `PermissionsExt::set_mode` is a safe setter that writes a `mode_t`
    // bitfield of permission bits. 0o600 = owner read+write only, which is
    // the desired confidentiality for the bearer-token-bearing config file.
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms).map_err(|e| CoreError::ConfigIo {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

#[cfg(not(target_family = "unix"))]
#[allow(clippy::unnecessary_wraps)]
fn set_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unreachable,
    clippy::unwrap_in_result,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.server_url, DEFAULT_SERVER_URL);
        assert_eq!(cfg.bind_addr, DEFAULT_BIND_ADDR);
        assert_eq!(
            cfg.approval_timeout_seconds,
            DEFAULT_APPROVAL_TIMEOUT_SECONDS
        );
        assert_eq!(cfg.bt_backend, BtBackend::Bluer);
        assert!(!cfg.devcontainer);
        // 32 bytes -> 64 hex chars.
        assert_eq!(cfg.token.as_ref().len(), 64);
        assert!(cfg.token.as_ref().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_is_64_hex_chars() {
        let t = Config::generate_token();
        assert_eq!(t.as_ref().len(), 64);
        assert!(t.as_ref().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_is_random() {
        let a = Config::generate_token();
        let b = Config::generate_token();
        assert_ne!(a, b, "tokens should not collide on two draws");
    }

    #[test]
    fn bt_backend_serializes_snake_case() {
        // TOML's root must be a table, so a bare enum cannot be serialized
        // directly with `toml::to_string`. Use `serde_json` to verify the
        // tag spelling (the wire/JSON path), and rely on
        // `config_roundtrips_through_toml` for the TOML path.
        let s = serde_json::to_string(&BtBackend::Serialport).expect("ser");
        assert_eq!(s, "\"serialport\"");
        let s = serde_json::to_string(&BtBackend::Bluer).expect("ser");
        assert_eq!(s, "\"bluer\"");
    }

    #[test]
    fn bt_backend_toml_roundtrips_inside_config() {
        let cfg = Config {
            bt_backend: BtBackend::Serialport,
            ..Config::default()
        };
        let toml_str = toml::to_string_pretty(&cfg).expect("ser");
        assert!(toml_str.contains("bt_backend = \"serialport\""));
        let back: Config = toml::from_str(&toml_str).expect("de");
        assert_eq!(back.bt_backend, BtBackend::Serialport);
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let cfg = Config::default();
        let toml_str = toml::to_string_pretty(&cfg).expect("ser");
        let back: Config = toml::from_str(&toml_str).expect("de");
        assert_eq!(back, cfg);
    }

    #[test]
    fn empty_toml_deserializes_to_config_default() {
        // An empty TOML file must fill every field from the same defaults
        // `Config::default` uses (the token is freshly random, so only its
        // shape is asserted, not equality with the default's token).
        let from_empty: Config = toml::from_str("").expect("empty de");
        let default = Config::default();
        assert_eq!(from_empty.server_url, default.server_url);
        assert_eq!(from_empty.bind_addr, default.bind_addr);
        assert_eq!(from_empty.db_path, default.db_path);
        assert_eq!(
            from_empty.approval_timeout_seconds,
            default.approval_timeout_seconds
        );
        assert_eq!(from_empty.bt_backend, default.bt_backend);
        assert_eq!(from_empty.devcontainer, default.devcontainer);
        assert_eq!(from_empty.bt_com_port, None);
        assert_eq!(from_empty.bt_adapter_addr, None);
        assert_eq!(from_empty.bt_channel, None);
        assert_eq!(from_empty.token.as_ref().len(), 64);
        assert!(
            from_empty
                .token
                .as_ref()
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "default token is hex"
        );
    }

    #[test]
    fn partial_toml_fills_missing_fields_with_defaults() {
        // Only override `devcontainer` and `bt_backend`; everything else must
        // come from the per-field defaults.
        let toml_str = "devcontainer = true\nbt_backend = \"serialport\"\n";
        let cfg: Config = toml::from_str(toml_str).expect("partial de");
        assert!(cfg.devcontainer);
        assert_eq!(cfg.bt_backend, BtBackend::Serialport);
        assert_eq!(cfg.server_url, DEFAULT_SERVER_URL);
        assert_eq!(cfg.bind_addr, DEFAULT_BIND_ADDR);
        assert_eq!(
            cfg.approval_timeout_seconds,
            DEFAULT_APPROVAL_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn hex_encode_via_crate_matches_known_values() {
        assert_eq!(hex::encode([]), "");
        assert_eq!(hex::encode([0x00u8]), "00");
        assert_eq!(hex::encode([0xab, 0xcd, 0xef]), "abcdef");
        assert_eq!(hex::encode([0xff]), "ff");
    }
}
