use std::path::PathBuf;

use nostr_sdk::{Keys, ToBech32};
use tokio::sync::RwLock;

use crate::error::AppError;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KeyPair {
    pub npub: String,
    pub nsec: String,
    pub cwd: String,
    #[serde(rename = "created_at")]
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct KeysFile {
    version: u32,
    keys: Vec<KeyPair>,
}

impl Default for KeysFile {
    fn default() -> Self {
        Self { version: 1, keys: Vec::new() }
    }
}

pub struct KeyStore {
    path: PathBuf,
    inner: RwLock<KeysFile>,
}

impl KeyStore {
    /// Open or create a key store at the given path.
    pub fn new(path: PathBuf) -> Result<Self, AppError> {
        let inner = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            serde_json::from_str(&raw).unwrap_or_default()
        } else {
            KeysFile::default()
        };
        Ok(Self { path, inner: RwLock::new(inner) })
    }

    /// Return existing key for cwd, or generate and persist a new one.
    pub async fn get_or_create_key(&self, cwd: &str) -> Result<KeyPair, AppError> {
        // Fast path: check without write lock
        {
            let inner = self.inner.read().await;
            if let Some(existing) = inner.keys.iter().find(|k| k.cwd == cwd) {
                return Ok(existing.clone());
            }
        }

        let mut inner = self.inner.write().await;
        // Double-check after acquiring write lock
        if let Some(existing) = inner.keys.iter().find(|k| k.cwd == cwd) {
            return Ok(existing.clone());
        }

        let keys = Keys::generate();
        let pair = KeyPair {
            npub: keys.public_key().to_bech32()
                .map_err(|e| AppError::Nostr(format!("bech32 encode failed: {e}")))?,
            nsec: keys.secret_key().to_bech32()
                .map_err(|e| AppError::Nostr(format!("bech32 encode failed: {e}")))?,
            cwd: cwd.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };

        inner.keys.push(pair.clone());
        self.save_locked(&inner).await?;
        Ok(pair)
    }

    /// Lookup nsec by npub. Returns None if not found.
    pub async fn get_nsec_by_npub(&self, npub: &str) -> Option<String> {
        let inner = self.inner.read().await;
        inner.keys.iter().find(|k| k.npub == npub).map(|k| k.nsec.clone())
    }

    /// Return all key pairs.
    pub async fn all_keys(&self) -> Vec<KeyPair> {
        self.inner.read().await.keys.clone()
    }

    /// Return all npubs.
    #[allow(dead_code)]
    pub async fn all_npubs(&self) -> Vec<String> {
        self.inner.read().await.keys.iter().map(|k| k.npub.clone()).collect()
    }

    /// Return number of keys.
    pub async fn len(&self) -> usize {
        self.inner.read().await.keys.len()
    }

    async fn save_locked(&self, inner: &KeysFile) -> Result<(), AppError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(inner)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }
}
