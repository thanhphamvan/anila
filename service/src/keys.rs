//! The service's ed25519 identity. The worker holds only the public key.
//!
//! Key file format: the 32-byte seed in unpadded base64url, then a newline.
//! The file must be mode 0600.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anila_proto::sig::{HEADER_HOST, HEADER_SIG, HEADER_TS, http_sign_input, ws_sign_input};
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;
use sha2::{Digest, Sha256};

/// The private signing key identifying this service to the worker.
pub struct ServiceKey {
    key: SigningKey,
}

impl ServiceKey {
    /// Generates a fresh signing key using the operating system's randomness.
    pub fn generate() -> Self {
        Self {
            key: SigningKey::generate(&mut OsRng),
        }
    }

    /// Constructs a signing key from its 32-byte seed.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// Fails unless the file is mode 0600 and holds a valid seed. The error
    /// for a wrong mode mentions "0600".
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut file =
            File::open(path).with_context(|| format!("opening key file {}", path.display()))?;
        if file.metadata()?.permissions().mode() & 0o077 != 0 {
            bail!(
                "key file {} must have mode 0600; run chmod 600 {}",
                path.display(),
                path.display()
            );
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .with_context(|| format!("reading key file {}", path.display()))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(text.trim())
            .context("key seed must be unpadded base64url")?;
        let seed = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("key seed must be exactly 32 bytes"))?;
        Ok(Self::from_seed(seed))
    }

    /// Generates a key and writes it with mode 0600, creating parent
    /// directories. Refuses to overwrite an existing file.
    pub fn save_new(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating key directory {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| {
                format!(
                    "creating new key file {} (must not already exist)",
                    path.display()
                )
            })?;
        let key = Self::generate();
        writeln!(file, "{}", URL_SAFE_NO_PAD.encode(key.key.to_bytes()))
            .with_context(|| format!("writing key file {}", path.display()))?;
        Ok(key)
    }

    /// Unpadded base64url of the 32-byte public key, as `HOST_KEYS` holds it.
    pub fn public_key_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key.verifying_key().to_bytes())
    }

    /// Unpadded base64url signature over `anila_proto::sig::ws_sign_input`.
    pub fn sign_ws(&self, host: &str, ts: i64, nonce: &str) -> String {
        URL_SAFE_NO_PAD.encode(
            self.key
                .sign(ws_sign_input(host, ts, nonce).as_bytes())
                .to_bytes(),
        )
    }

    /// `Term-Host`, `Term-Ts` and `Term-Sig` for a signed `POST /api/agent/*`
    /// call, over `anila_proto::sig::http_sign_input` with the lowercase hex
    /// SHA-256 of `body`.
    pub fn signed_headers(
        &self,
        host: &str,
        method: &str,
        path: &str,
        ts: i64,
        body: &[u8],
    ) -> [(&'static str, String); 3] {
        let hash = format!("{:x}", Sha256::digest(body));
        let input = http_sign_input(method, path, ts, &hash);
        let sig = URL_SAFE_NO_PAD.encode(self.key.sign(input.as_bytes()).to_bytes());
        [
            (HEADER_HOST, host.to_owned()),
            (HEADER_TS, ts.to_string()),
            (HEADER_SIG, sig),
        ]
    }
}
