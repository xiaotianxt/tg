//! Remote-mode configuration and private state files under `~/.tg/mcp/`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const DEFAULT_MAX_LOOKBACK_DAYS: i64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Public HTTPS origin, without a trailing slash; the OAuth issuer and the
    /// base of the protected resource URL.
    pub public_url: String,
    /// Must be a loopback or tailnet address.
    pub listen: SocketAddr,
    /// Peer whose `X-Forwarded-For` is trusted for rate limiting and audit.
    #[serde(default)]
    pub trusted_proxy: Option<IpAddr>,
    #[serde(default)]
    pub remote: RemotePolicy,
    /// Redirect URIs accepted at registration in addition to ChatGPT's.
    #[serde(default)]
    pub extra_redirect_uris: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct RemotePolicy {
    /// Empty means every session not in `deny_sessions`.
    pub allow_sessions: Vec<String>,
    pub deny_sessions: Vec<String>,
    pub max_lookback_days: i64,
    pub show_contact_remarks: bool,
    pub max_result_bytes: usize,
    /// Per-message character cap; unset keeps every message whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_text_chars: Option<usize>,
}

impl Default for RemotePolicy {
    fn default() -> Self {
        Self {
            allow_sessions: Vec::new(),
            deny_sessions: Vec::new(),
            max_lookback_days: DEFAULT_MAX_LOOKBACK_DAYS,
            show_contact_remarks: false,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            max_text_chars: None,
        }
    }
}

impl Config {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let host = self
            .public_url
            .strip_prefix("https://")
            .ok_or("public_url must start with https://")?;
        if host.is_empty() || host.contains('/') || host.contains('?') || host.contains('#') {
            return Err("public_url must be an origin such as https://mcp.example.com".into());
        }
        if !is_private_listen_address(self.listen.ip()) {
            return Err(format!(
                "listen address {} must be loopback or a Tailscale address",
                self.listen.ip()
            ));
        }
        let policy = &self.remote;
        if policy.max_lookback_days <= 0 {
            return Err("remote.max_lookback_days must be positive".into());
        }
        if policy.max_result_bytes < 1024 || policy.max_text_chars == Some(0) {
            return Err("remote.max_result_bytes must be >= 1024 and max_text_chars > 0".into());
        }
        Ok(())
    }

    /// Host header value the public proxy forwards.
    pub(crate) fn public_host(&self) -> &str {
        self.public_url.trim_start_matches("https://")
    }

    pub(crate) fn resource_url(&self) -> String {
        format!("{}/mcp", self.public_url)
    }
}

/// Loopback or Tailscale (CGNAT 100.64.0.0/10, ULA fd7a:115c:a1e0::/48) only:
/// the HTTP server must never listen on LAN or wildcard interfaces.
pub(crate) fn is_private_listen_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback() || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            v6.is_loopback() || segments[..3] == [0xfd7a, 0x115c, 0xa1e0]
        }
    }
}

pub(crate) fn state_dir() -> PathBuf {
    crate::paths::default_state_dir().join("mcp")
}

pub(crate) fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

pub(crate) fn load_config(dir: &Path) -> Result<Config, String> {
    let path = config_path(dir);
    let config: Config = read_private_json(&path)?
        .ok_or_else(|| format!("{} not found; run `tg mcp init` first", path.display()))?;
    config.validate()?;
    Ok(config)
}

/// Read a JSON state file, refusing files other users could read or write.
pub(crate) fn read_private_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<Option<T>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot read {}: {error}", path.display())),
    };
    check_private(path)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("Invalid {}: {error}", path.display()))
}

#[cfg(unix)]
fn check_private(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let check = |path: &Path, max: u32| -> Result<(), String> {
        let mode = fs::metadata(path)
            .map_err(|error| format!("Cannot stat {}: {error}", path.display()))?
            .permissions()
            .mode()
            & 0o777;
        if mode & !max != 0 {
            return Err(format!(
                "{} has mode {mode:o}; expected at most {max:o}",
                path.display()
            ));
        }
        Ok(())
    };
    check(path, 0o600)?;
    if let Some(parent) = path.parent() {
        check(parent, 0o700)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Atomically replace `path` with `value`, mode 0600 inside a 0700 directory.
pub(crate) fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let dir = path.parent().ok_or("state file has no parent directory")?;
    crate::paths::ensure_private_dir(dir)
        .map_err(|error| format!("Cannot create {}: {error}", dir.display()))?;
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(|error| format!("Random source failed: {error}"))?;
    let temp = dir.join(format!(".{}.tmp", hex::encode(nonce)));
    let result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|error| format!("Cannot create {}: {error}", temp.display()))?;
        let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("Cannot write {}: {error}", temp.display()))?;
        fs::rename(&temp, path)
            .map_err(|error| format!("Cannot install {}: {error}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_addresses_are_limited_to_loopback_and_tailnet() {
        for ok in [
            "127.0.0.1",
            "100.87.128.22",
            "100.64.0.1",
            "::1",
            "fd7a:115c:a1e0::5",
        ] {
            assert!(is_private_listen_address(ok.parse().unwrap()), "{ok}");
        }
        for bad in ["0.0.0.0", "192.168.1.5", "100.128.0.1", "::", "fd00::1"] {
            assert!(!is_private_listen_address(bad.parse().unwrap()), "{bad}");
        }
    }

    #[test]
    fn public_url_must_be_an_https_origin() {
        let mut config = Config {
            public_url: "https://mcp.example.com".into(),
            listen: "127.0.0.1:8765".parse().unwrap(),
            trusted_proxy: None,
            remote: RemotePolicy::default(),
            extra_redirect_uris: Vec::new(),
        };
        assert!(config.validate().is_ok());
        for bad in [
            "http://mcp.example.com",
            "https://mcp.example.com/mcp",
            "https://",
        ] {
            config.public_url = bad.into();
            assert!(config.validate().is_err(), "{bad}");
        }
    }
}
