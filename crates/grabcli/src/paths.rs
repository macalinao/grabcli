//! Where grabcli keeps its files, and private/atomic file writes.
//!
//! - config dir: `$GRABCLI_CONFIG_DIR`, else `$XDG_CONFIG_HOME/grabcli`, else
//!   `~/.config/grabcli`. Holds `config.toml`, `session.json` (0600) and the
//!   optional `hydra-key.pem` (0600).
//! - state dir: `$GRABCLI_STATE_DIR`, else `$XDG_STATE_HOME/grabcli`, else
//!   `~/.local/state/grabcli`. Each service gets a subdirectory (`food/`).
//! - cache dir: `$GRABCLI_CACHE_DIR`, else `$XDG_CACHE_HOME/grabcli`, else
//!   `~/.cache/grabcli`. Disposable response cache (see `grabcli_core::cache`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use grabcli_core::error::{Error, Result};

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::invalid("HOME is not set"))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn config_dir() -> Result<PathBuf> {
    if let Some(p) = env_path("GRABCLI_CONFIG_DIR") {
        return Ok(p);
    }
    if let Some(p) = env_path("XDG_CONFIG_HOME") {
        return Ok(p.join("grabcli"));
    }
    Ok(home()?.join(".config").join("grabcli"))
}

pub fn state_dir() -> Result<PathBuf> {
    if let Some(p) = env_path("GRABCLI_STATE_DIR") {
        return Ok(p);
    }
    if let Some(p) = env_path("XDG_STATE_HOME") {
        return Ok(p.join("grabcli"));
    }
    Ok(home()?.join(".local").join("state").join("grabcli"))
}

pub fn cache_dir() -> Result<PathBuf> {
    if let Some(p) = env_path("GRABCLI_CACHE_DIR") {
        return Ok(p);
    }
    if let Some(p) = env_path("XDG_CACHE_HOME") {
        return Ok(p.join("grabcli"));
    }
    Ok(home()?.join(".cache").join("grabcli"))
}

/// `$GRABCLI_SESSION` or `<config dir>/session.json`.
pub fn session_path() -> Result<PathBuf> {
    if let Some(p) = env_path("GRABCLI_SESSION") {
        return Ok(p);
    }
    Ok(config_dir()?.join("session.json"))
}

/// `$GRABCLI_CONFIG` or `<config dir>/config.toml`.
pub fn config_path() -> Result<PathBuf> {
    if let Some(p) = env_path("GRABCLI_CONFIG") {
        return Ok(p);
    }
    Ok(config_dir()?.join("config.toml"))
}

/// Create `dir` (and parents) with mode 0700 on the leaf.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.tmp.{}", std::process::id()))
}

/// Atomically write a secret file: parent dir 0700, file 0600 from the moment
/// it is created, then renamed over `path`.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        ensure_private_dir(parent)?;
    }
    let tmp = tmp_path(path);
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Atomically write a non-secret file (tmp + rename, default permissions).
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn private_write_is_0600() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let p = dir.path().join("sub").join("s.json");
        write_private(&p, b"{}")?;
        assert_eq!(fs::metadata(&p)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(p.parent().ok_or_else(|| Error::invalid("no parent"))?)?
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(fs::read(&p)?, b"{}");
        Ok(())
    }
}
