//! `~/.config/anila/service.toml`. See `service.example.toml`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anila_proto::{HostId, Preset, limits};
use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The machine id from `anila-service pair` (or a legacy name from the worker's `HOST_KEYS`).
    pub host: HostId,
    /// Base URL of the worker, e.g. `https://term.example.com`.
    pub worker: String,
    /// Private signing key file. Unset uses the default config directory.
    pub key_file: Option<String>,
    #[serde(default = "default_grace_secs")]
    pub grace_secs: u64,
    #[serde(default = "default_buffer_bytes")]
    pub buffer_bytes: usize,
    /// tmux server socket name (`tmux -L`). Unset means your normal server.
    #[serde(default)]
    pub tmux_socket: Option<String>,
    /// Cloudflare Access service token, when the worker's domain is behind
    /// Access: sent as `CF-Access-Client-Id`/`CF-Access-Client-Secret` on
    /// `/ws/agent` and `/api/agent/*`. Set both or neither. The environment
    /// variables `ANILA_ACCESS_CLIENT_ID`/`ANILA_ACCESS_CLIENT_SECRET` win.
    #[serde(default)]
    pub access_client_id: Option<String>,
    #[serde(default)]
    pub access_client_secret: Option<Secret>,
    #[serde(default, rename = "preset")]
    pub presets: Vec<PresetConfig>,
}

/// A value that never appears in `Debug` output (`check-config`, logs).
#[derive(Deserialize, Clone, PartialEq)]
#[serde(transparent)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("\"<redacted>\"")
    }
}

/// The Cloudflare Access service token headers for an id and secret: both
/// headers when both are set, none when neither is, an error otherwise.
pub fn access_headers(
    id: Option<&str>,
    secret: Option<&str>,
) -> anyhow::Result<Vec<(&'static str, String)>> {
    fn set(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|v| !v.is_empty())
    }
    match (set(id), set(secret)) {
        (Some(id), Some(secret)) => Ok(vec![
            ("cf-access-client-id", id.to_owned()),
            ("cf-access-client-secret", secret.to_owned()),
        ]),
        (None, None) => Ok(Vec::new()),
        _ => bail!("set both access_client_id and access_client_secret, or neither"),
    }
}

/// `access_headers` with `ANILA_ACCESS_CLIENT_ID`/`ANILA_ACCESS_CLIENT_SECRET`
/// taking precedence over the given values.
pub fn access_headers_env(
    id: Option<&str>,
    secret: Option<&str>,
) -> anyhow::Result<Vec<(&'static str, String)>> {
    let env_id = std::env::var("ANILA_ACCESS_CLIENT_ID").ok();
    let env_secret = std::env::var("ANILA_ACCESS_CLIENT_SECRET").ok();
    access_headers(env_id.as_deref().or(id), env_secret.as_deref().or(secret))
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PresetConfig {
    pub id: String,
    pub label: String,
    #[serde(default = "default_dir")]
    pub dir: String,
    /// Command for the first window. Unset means your login shell.
    #[serde(default)]
    pub cmd: Option<String>,
}

fn default_grace_secs() -> u64 {
    limits::GRACE_SECS
}

fn default_buffer_bytes() -> usize {
    limits::REPLAY_BUFFER_BYTES
}

fn default_dir() -> String {
    "~".into()
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("anila").join("service.toml")
}

/// Default private key location in the config directory.
pub fn default_key_path() -> PathBuf {
    default_path().with_file_name("service.key")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Expands a leading `~` to `$HOME`.
pub fn expand_home(dir: &str) -> PathBuf {
    match dir.strip_prefix('~') {
        Some(rest) => home().join(rest.trim_start_matches('/')),
        None => PathBuf::from(dir),
    }
}

impl Config {
    /// The configured private key path, with a leading `~` expanded.
    pub fn key_path(&self) -> PathBuf {
        self.key_file
            .as_deref()
            .map(expand_home)
            .unwrap_or_else(default_key_path)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let config: Config = toml::from_str(text).context("invalid service config")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let local = self.worker.starts_with("http://localhost")
            || self.worker.starts_with("http://127.0.0.1");
        if !self.worker.starts_with("https://") && !local {
            bail!("worker must be an https:// URL (plain http is allowed only for localhost)");
        }
        access_headers(
            self.access_client_id.as_deref(),
            self.access_client_secret.as_ref().map(|s| s.0.as_str()),
        )?;
        let mut ids = HashSet::new();
        for p in &self.presets {
            if !ids.insert(p.id.as_str()) {
                bail!("preset id {:?} appears twice", p.id);
            }
        }
        Ok(())
    }

    /// The Access headers to send, environment variables first.
    pub fn access_headers(&self) -> anyhow::Result<Vec<(&'static str, String)>> {
        access_headers_env(
            self.access_client_id.as_deref(),
            self.access_client_secret.as_ref().map(|s| s.0.as_str()),
        )
    }

    pub fn preset(&self, id: &str) -> Option<&PresetConfig> {
        self.presets.iter().find(|p| p.id == id)
    }

    /// The presets as the browser sees them.
    pub fn public_presets(&self) -> Vec<Preset> {
        self.presets
            .iter()
            .map(|p| Preset {
                id: p.id.clone(),
                label: p.label.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../service.example.toml");

    #[test]
    fn example_parses() {
        let c = Config::parse(EXAMPLE).unwrap();
        assert_eq!(c.host.as_str(), "my-mac");
        assert_eq!(c.grace_secs, 600);
        assert_eq!(c.preset("claude").unwrap().cmd.as_deref(), Some("claude"));
        assert_eq!(c.public_presets().len(), 2);
    }

    #[test]
    fn rejects_bad_configs() {
        assert!(Config::parse("host = \"My Mac\"\nworker = \"https://x\"").is_err());
        assert!(Config::parse("host = \"m\"\nworker = \"http://x.com\"").is_err());
        assert!(Config::parse("host = \"m\"\nworker = \"https://x\"\ntypo = 1").is_err());
        let dup = "host = \"m\"\nworker = \"https://x\"\n[[preset]]\nid=\"a\"\nlabel=\"A\"\n[[preset]]\nid=\"a\"\nlabel=\"B\"";
        assert!(Config::parse(dup).is_err());
        assert!(Config::parse("host = \"m\"\nworker = \"http://localhost:8787\"").is_ok());
    }

    #[test]
    fn access_token_settings() {
        let base = "host = \"m\"\nworker = \"https://x\"\n";
        let both = Config::parse(&format!(
            "{base}access_client_id = \"abc.access\"\naccess_client_secret = \"s3cr3t\"\n"
        ))
        .unwrap();
        assert_eq!(
            access_headers(Some("abc.access"), Some("s3cr3t")).unwrap(),
            vec![
                ("cf-access-client-id", "abc.access".to_string()),
                ("cf-access-client-secret", "s3cr3t".to_string())
            ]
        );
        let debug = format!("{both:?}");
        assert!(!debug.contains("s3cr3t"), "the secret is redacted: {debug}");
        assert!(debug.contains("abc.access"));
        assert!(Config::parse(&format!("{base}access_client_id = \"abc\"\n")).is_err());
        assert!(Config::parse(&format!("{base}access_client_secret = \"s\"\n")).is_err());
        assert!(access_headers(None, None).unwrap().is_empty());
        assert!(
            access_headers(Some(" "), Some("")).unwrap().is_empty(),
            "blank means unset"
        );
    }

    #[test]
    fn expands_home() {
        assert_eq!(expand_home("~/code"), home().join("code"));
        assert_eq!(expand_home("~"), home());
        assert_eq!(expand_home("/tmp"), PathBuf::from("/tmp"));
    }
}
