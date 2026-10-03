//! Encrypted reads and writes against a cachekit-rs `Backend`.

use std::cell::Cell;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use cachekit::backend::file::FileBackend;
use cachekit::backend::Backend;
use cachekit::{BackendError, BackendErrorKind, CachekitError, EncryptionLayer};

use crate::entry::{Envelope, Marker};
use crate::keys::TENANT;
use crate::{warn, Fatal};

/// Each saas round trip gets this long. The cachekit-rs client's own
/// timeouts (30 s per request, 10 s to connect) cannot be changed.
pub const SAAS_DEADLINE: Duration = Duration::from_secs(1);

/// Why a read produced no answer.
pub enum ReadError {
    /// The backend failed. The call runs uncached.
    Backend(String),
    /// A local configuration fault, not an answer from the backend: exit 125.
    Local(Fatal),
}

pub struct Store {
    runtime: Runtime,
    backend: Box<dyn Backend>,
    layer: EncryptionLayer,
    /// The per-round-trip timeout, or `None` for the file backend, whose
    /// calls are local.
    deadline: Option<Duration>,
    decrypt_warned: Cell<bool>,
}

impl Store {
    pub fn open_file(data_dir: &Path, master_key: &[u8]) -> Result<Self, Fatal> {
        let backend = FileBackend::builder().cache_dir(data_dir).build().map_err(|e| {
            Fatal(format!(
                "cannot use the cache directory: {e}. Fix it with `chmod 700 {0}` if you own it, or remove {0}",
                data_dir.display()
            ))
        })?;
        // The file backend runs its I/O on tokio's blocking pool; a
        // current-thread runtime with no drivers is the cheapest one that
        // provides it, and keeps the hit path off the network stack.
        let runtime = runtime(&mut tokio::runtime::Builder::new_current_thread())?;
        Self::new(runtime, Box::new(backend), master_key, None)
    }

    pub fn open_saas(backend: Box<dyn Backend>, master_key: &[u8]) -> Result<Self, Fatal> {
        let runtime = runtime(tokio::runtime::Builder::new_current_thread().enable_all())?;
        Self::new(runtime, backend, master_key, Some(SAAS_DEADLINE))
    }

    fn new(
        runtime: Runtime,
        backend: Box<dyn Backend>,
        master_key: &[u8],
        deadline: Option<Duration>,
    ) -> Result<Self, Fatal> {
        let layer = EncryptionLayer::new(master_key, TENANT)
            .map_err(|e| Fatal(format!("cannot set up encryption: {e}")))?;
        Ok(Self {
            runtime,
            backend,
            layer,
            deadline,
            decrypt_warned: Cell::new(false),
        })
    }

    pub fn value(&self, key: &str) -> Result<Option<Envelope>, ReadError> {
        Ok(self
            .runtime
            .block_on(self.get(key))?
            .and_then(Envelope::decode))
    }

    pub fn marker(&self, key: &str) -> Result<Option<Marker>, ReadError> {
        Ok(self
            .runtime
            .block_on(self.get(key))?
            .as_deref()
            .and_then(Marker::decode))
    }

    /// Both reads at once: one round trip of latency on saas.
    pub fn value_and_marker(
        &self,
        key: &str,
        marker_key: &str,
    ) -> Result<(Option<Envelope>, Option<Marker>), ReadError> {
        let (value, marker) = self
            .runtime
            .block_on(async { tokio::join!(self.get(key), self.get(marker_key)) });
        Ok((
            value?.and_then(Envelope::decode),
            marker?.as_deref().and_then(Marker::decode),
        ))
    }

    pub fn set(&self, key: &str, plaintext: &[u8], ttl: Duration) -> Result<(), String> {
        self.runtime.block_on(self.put(key, plaintext, ttl))
    }

    /// After a successful run: store `value` (when there is output to store)
    /// and clear the marker, at once. Returns the outcome of each.
    pub fn record_success(
        &self,
        key: &str,
        value: Option<(&[u8], Duration)>,
        marker_key: &str,
    ) -> (Result<(), String>, Result<(), String>) {
        let stored = async {
            match value {
                Some((plaintext, ttl)) => self.put(key, plaintext, ttl).await,
                None => Ok(()),
            }
        };
        let cleared = async {
            self.bounded(self.backend.delete(marker_key))
                .await
                .map(|_| ())
                .map_err(|e| describe(&e))
        };
        self.runtime
            .block_on(async { tokio::join!(stored, cleared) })
    }

    async fn put(&self, key: &str, plaintext: &[u8], ttl: Duration) -> Result<(), String> {
        let ciphertext = self
            .layer
            .encrypt(plaintext, key)
            .map_err(|e| e.to_string())?;
        self.bounded(self.backend.set(key, ciphertext, Some(ttl)))
            .await
            .map_err(|e| describe(&e))
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, ReadError> {
        let ciphertext = match self.bounded(self.backend.get(key)).await {
            Ok(Some(c)) => c,
            Ok(None) => return Ok(None),
            // ck's own traffic cannot produce lock contention on the file
            // backend (reads share the lock, writes take none), so there a
            // timeout is a miss rather than a fault. The file backend is the
            // one with no deadline; on saas a timeout is an outage.
            Err(e) if e.kind == BackendErrorKind::Timeout && self.deadline.is_none() => {
                return Ok(None)
            }
            Err(e) => return Err(ReadError::Backend(describe(&e))),
        };
        match self.layer.decrypt(&ciphertext, key) {
            Ok(plaintext) => Ok(Some(plaintext)),
            Err(CachekitError::Config(e)) => Err(ReadError::Local(Fatal(format!(
                "encryption is misconfigured: {e}"
            )))),
            // A decrypt or AAD failure is a miss: it is never served, not even
            // as stale, and a marker that fails to decrypt is absent. A new
            // master key changes the keyed name too, so a rotation never
            // lands here: this means a corrupted or tampered entry, which is
            // worth one line. The error names no key material.
            Err(e) => {
                // A call reads an entry twice (before and under the fill
                // lock), so report it once.
                if !self.decrypt_warned.replace(true) {
                    warn(&format!(
                        "a stored entry failed to decrypt ({e}); treating it as a miss"
                    ));
                }
                Ok(None)
            }
        }
    }

    /// One round trip, cut off at the deadline when there is one.
    async fn bounded<T>(
        &self,
        call: impl Future<Output = Result<T, BackendError>>,
    ) -> Result<T, BackendError> {
        let Some(deadline) = self.deadline else {
            return call.await;
        };
        tokio::time::timeout(deadline, call)
            .await
            .unwrap_or_else(|_| {
                Err(BackendError {
                    kind: BackendErrorKind::Timeout,
                    message: format!("no answer within {}s", deadline.as_secs()),
                    source: None,
                })
            })
    }
}

/// The error as one stderr line. A 401 or 403 also names the fix. The
/// message can carry text from the server's response, so control characters
/// are replaced and cannot end the line or drive the terminal.
fn describe(e: &BackendError) -> String {
    let text: String = e
        .to_string()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if e.kind != BackendErrorKind::Authentication {
        return text;
    }
    format!(
        "{text}. CACHEKIT_API_KEY must be an SDK key (ck_sdk_...) or a ck_live_ key, \
         and a key limited to named namespaces must be granted ck-run"
    )
}

/// A tokio runtime that does not wait for its blocking pool when dropped. A
/// DNS lookup runs on that pool and is not cancelled by a timeout, so the
/// default drop would wait for a stuck lookup and undo the deadline.
struct Runtime(Option<tokio::runtime::Runtime>);

fn runtime(builder: &mut tokio::runtime::Builder) -> Result<Runtime, Fatal> {
    builder
        .on_thread_start(crate::child::block_handled_signals)
        .build()
        .map(|rt| Runtime(Some(rt)))
        .map_err(|e| Fatal(format!("cannot start the async runtime: {e}")))
}

impl Runtime {
    fn block_on<F: Future>(&self, future: F) -> F::Output {
        match &self.0 {
            Some(rt) => rt.block_on(future),
            None => unreachable!("the runtime is taken only on drop"),
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_authentication_error_names_the_fix() {
        let e = BackendError::from_http_status(403, b"namespace not granted");
        let line = describe(&e);
        assert!(line.contains("HTTP 403"), "{line}");
        assert!(
            line.contains("ck_sdk_") && line.contains("ck-run"),
            "{line}"
        );
        let e = BackendError::from_http_status(503, b"down");
        assert!(!describe(&e).contains("ck_sdk_"));
    }

    #[test]
    fn server_text_cannot_break_the_line() {
        let e = BackendError::from_http_status(500, b"one\ntwo\x1b[31m");
        let line = describe(&e);
        assert!(!line.chars().any(char::is_control), "{line:?}");
    }
}
