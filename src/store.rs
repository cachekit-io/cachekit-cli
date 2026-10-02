//! Encrypted reads and writes against a cachekit-rs `Backend`.

use std::path::Path;
use std::time::Duration;

use cachekit::backend::file::FileBackend;
use cachekit::backend::Backend;
use cachekit::{BackendErrorKind, CachekitError, EncryptionLayer};

use crate::entry::{Envelope, Marker};
use crate::keys::TENANT;
use crate::Fatal;

/// Why a read produced no answer.
pub enum ReadError {
    /// The backend failed. The call runs uncached.
    Backend(String),
    /// A local configuration fault, not an answer from the backend: exit 125.
    Local(Fatal),
}

pub struct Store {
    runtime: tokio::runtime::Runtime,
    backend: Box<dyn Backend>,
    layer: EncryptionLayer,
}

impl Store {
    pub fn open_file(data_dir: &Path, master_key: &[u8]) -> Result<Self, Fatal> {
        let backend = FileBackend::builder().cache_dir(data_dir).build().map_err(|e| {
            Fatal(format!(
                "cannot use the cache directory: {e}. Fix it with `chmod 700 {0}` if you own it, or remove {0}",
                data_dir.display()
            ))
        })?;
        Self::new(Box::new(backend), master_key)
    }

    fn new(backend: Box<dyn Backend>, master_key: &[u8]) -> Result<Self, Fatal> {
        let layer = EncryptionLayer::new(master_key, TENANT)
            .map_err(|e| Fatal(format!("cannot set up encryption: {e}")))?;
        // The file backend runs its I/O on tokio's blocking pool; a
        // current-thread runtime is the cheapest one that provides it.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .map_err(|e| Fatal(format!("cannot start the async runtime: {e}")))?;
        Ok(Self {
            runtime,
            backend,
            layer,
        })
    }

    pub fn value(&self, key: &str) -> Result<Option<Envelope>, ReadError> {
        Ok(self.get(key)?.and_then(Envelope::decode))
    }

    pub fn marker(&self, key: &str) -> Result<Option<Marker>, ReadError> {
        Ok(self.get(key)?.as_deref().and_then(Marker::decode))
    }

    pub fn set(&self, key: &str, plaintext: &[u8], ttl: Duration) -> Result<(), String> {
        let ciphertext = self
            .layer
            .encrypt(plaintext, key)
            .map_err(|e| e.to_string())?;
        self.runtime
            .block_on(self.backend.set(key, ciphertext, Some(ttl)))
            .map_err(|e| e.to_string())
    }

    pub fn delete(&self, key: &str) -> Result<(), String> {
        self.runtime
            .block_on(self.backend.delete(key))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, ReadError> {
        let ciphertext = match self.runtime.block_on(self.backend.get(key)) {
            Ok(Some(c)) => c,
            Ok(None) => return Ok(None),
            // ck's own traffic cannot produce lock contention on the file
            // backend (reads share the lock, writes take none), so a timeout
            // is a miss rather than a fault.
            Err(e) if e.kind == BackendErrorKind::Timeout => return Ok(None),
            Err(e) => return Err(ReadError::Backend(e.to_string())),
        };
        match self.layer.decrypt(&ciphertext, key) {
            Ok(plaintext) => Ok(Some(plaintext)),
            Err(CachekitError::Config(e)) => Err(ReadError::Local(Fatal(format!(
                "encryption is misconfigured: {e}"
            )))),
            // A decrypt or AAD failure is a miss: it is never served, not even
            // as stale, and a marker that fails to decrypt is absent.
            Err(_) => Ok(None),
        }
    }
}
