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

/// Each saas round trip gets this long, enforced around the whole call,
/// download included.
pub(crate) const SAAS_DEADLINE: Duration = Duration::from_secs(1);

/// The saas capture cap. The deadline covers the download too, so an entry
/// one host can write must be one every host can read within it: past this
/// size, a slow link would see a healthy backend time out on every call, and
/// never recover while faster hosts keep it fresh.
pub(crate) const SAAS_MAX_OUTPUT: usize = 1024 * 1024;

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

    /// Both reads at once: one round trip of latency on saas. The two can
    /// fail independently, and a fresh value is served whatever happened to
    /// the marker, so each keeps its own result.
    #[allow(clippy::type_complexity)]
    pub fn value_and_marker(
        &self,
        key: &str,
        marker_key: &str,
    ) -> (
        Result<Option<Envelope>, ReadError>,
        Result<Option<Marker>, ReadError>,
    ) {
        let (value, marker) = self
            .runtime
            .block_on(async { tokio::join!(self.get(key), self.get(marker_key)) });
        (
            value.map(|v| v.and_then(Envelope::decode)),
            marker.map(|m| m.as_deref().and_then(Marker::decode)),
        )
    }

    /// The most output this backend stores. Capture stops there and streams
    /// the rest, so output that cannot be stored is never held back.
    pub fn max_output(&self) -> usize {
        self.deadline
            .map_or(crate::child::OUTPUT_CAP, |_| SAAS_MAX_OUTPUT)
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

/// The error as one stderr line, with the chain of causes behind it: an HTTP
/// client error names only the request, and the cause (a DNS failure, a
/// refused connection, a certificate it does not trust) sits in its sources.
/// A 401 or 403 also names the fix. The text can come from the server's
/// response, so control characters are replaced and cannot end the line or
/// drive the terminal.
fn describe(e: &BackendError) -> String {
    let mut text = e.to_string();
    let mut cause = std::error::Error::source(e);
    while let Some(c) = cause {
        let part = c.to_string();
        // The client error's own text is already in the message.
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        cause = c.source();
    }
    let text: String = text
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

    /// The key from `keys::tests::value_key_known_answer`.
    const KNOWN_ANSWER_KEY: &str =
        "ns:ck-run:v1:041385329ef05862f32fad778ee6c2a69581637fd18926b3b0e8819aee6662ec";
    /// The master key `OLD_CIPHERTEXT` was written under.
    const OLD_MASTER_KEY: [u8; 32] = [7; 32];
    /// `EncryptionLayer::encrypt(b"ck known answer", KNOWN_ANSWER_KEY)` under
    /// `OLD_MASTER_KEY`, on cachekit-rs 0.9.0 and cachekit-core 0.6.0.
    const OLD_CIPHERTEXT: &str =
        "c90083142ccd441f0000000081198d2ce5728477cfab8528cb949c6393c0eeecc2dce5218b717e7fb3ea36";

    /// A ciphertext from the same `EncryptionLayer` call ck makes, on
    /// cachekit-rs 0.9.0 and cachekit-core 0.6.0, still decrypts. Its
    /// plaintext is a fixed string, not an `Envelope`.
    #[test]
    fn an_older_entry_still_decrypts() {
        let layer = EncryptionLayer::new(&OLD_MASTER_KEY, TENANT).unwrap();
        let ciphertext = hex::decode(OLD_CIPHERTEXT).unwrap();
        assert_eq!(
            layer.decrypt(&ciphertext, KNOWN_ANSWER_KEY).unwrap(),
            b"ck known answer"
        );
    }

    /// The subset of the frozen encryption format that ck writes, rebuilt
    /// from the protocol spec on RustCrypto alone. Neither cachekit-rs nor
    /// cachekit-core derives the key or opens the ciphertext here, so a change
    /// to their writer cannot carry this reader along with it.
    mod frozen {
        use aes_gcm::aead::{Aead, KeyInit, Payload};
        use aes_gcm::Aes256Gcm;
        use hkdf::Hkdf;
        use sha2::Sha256;

        /// HKDF-SHA256 for the `encryption` domain. The salt is
        /// `"cachekit_v1_" ‖ u8 len ‖ domain ‖ u16be len ‖ tenant`, and the
        /// info is the domain.
        fn key(master_key: &[u8], tenant: &str) -> [u8; 32] {
            let domain = b"encryption";
            let mut salt = b"cachekit_v1_".to_vec();
            salt.push(domain.len() as u8);
            salt.extend_from_slice(domain);
            salt.extend_from_slice(&(tenant.len() as u16).to_be_bytes());
            salt.extend_from_slice(tenant.as_bytes());
            let mut key = [0; 32];
            Hkdf::<Sha256>::new(Some(&salt), master_key)
                .expand(domain, &mut key)
                .unwrap();
            key
        }

        /// The four-part v0x03 AAD ck writes, for an uncompressed msgpack
        /// entry: `0x03`, then tenant, cache key, `"msgpack"` and `"False"`,
        /// each after a u32be length.
        pub fn aad(tenant: &str, cache_key: &str) -> Vec<u8> {
            let mut aad = vec![0x03];
            for part in [
                tenant.as_bytes(),
                cache_key.as_bytes(),
                b"msgpack",
                b"False",
            ] {
                aad.extend_from_slice(&(part.len() as u32).to_be_bytes());
                aad.extend_from_slice(part);
            }
            aad
        }

        /// Opens `nonce(12) ‖ ciphertext ‖ tag(16)`, with nothing before or
        /// after it, under the AAD ck writes.
        pub fn open(
            master_key: &[u8],
            tenant: &str,
            cache_key: &str,
            stored: &[u8],
        ) -> Option<Vec<u8>> {
            let (nonce, sealed) = stored.split_at_checked(12)?;
            let aad = aad(tenant, cache_key);
            Aes256Gcm::new_from_slice(&key(master_key, tenant))
                .unwrap()
                .decrypt(
                    nonce.try_into().ok()?,
                    Payload {
                        msg: sealed,
                        aad: &aad,
                    },
                )
                .ok()
        }
    }

    /// 32 distinct bytes: the protocol's `master_key_every_hex_digit` key. A
    /// key that repeats one byte reads the same reversed, so it cannot show a
    /// byte-order mistake in the key's path into HKDF.
    const MIXED_MASTER_KEY: &str =
        "007f80ffa55ac33c1ee12dd24bb469968778f00f01102332455467768998abba";

    /// The independent reader agrees with the protocol's published vectors
    /// (`test-vectors/encryption.json` 1.5.0, `basic_bytes` and
    /// `master_key_every_hex_digit`) and with the ciphertext an earlier
    /// cachekit-rs wrote, so it reads the frozen format and not just whatever
    /// today's writer produces.
    #[test]
    fn the_frozen_reader_reads_the_protocol_vectors() {
        let master_key = [0x61u8; 32];
        let tenant = "cross-sdk-test";
        assert_eq!(
            hex::encode(frozen::aad(tenant, "test:vector:1")),
            "030000000e63726f73732d73646b2d746573740000000d746573743a766563746f723a31\
             000000076d73677061636b0000000546616c7365"
        );
        let ciphertext =
            hex::decode("f8f69c70000000000000000087d9ec09f2c2347a4b37d5330ccf50e3c31e69642801bef5")
                .unwrap();
        assert_eq!(
            frozen::open(&master_key, tenant, "test:vector:1", &ciphertext),
            Some(vec![1, 2, 3, 4, 5, 6, 7, 8])
        );
        let cache_key =
            "users:get_all:8064bf5f945dba521b017bb3f32b31a293a6e16944838d171ea9e77fab085e8e";
        let ciphertext = hex::decode(
            "3a3b3c3d3e3f404142434445d282555c393cc14a53d488e005e6db441d92e9fe089e97b3ecc99a86014810c745",
        )
        .unwrap();
        assert_eq!(
            frozen::open(
                &hex::decode(MIXED_MASTER_KEY).unwrap(),
                "default",
                cache_key,
                &ciphertext
            ),
            Some(hex::decode("9501a374776fcb400c000000000000c0c3").unwrap())
        );
        let ciphertext = hex::decode(OLD_CIPHERTEXT).unwrap();
        assert_eq!(
            frozen::open(&OLD_MASTER_KEY, TENANT, KNOWN_ANSWER_KEY, &ciphertext).as_deref(),
            Some(&b"ck known answer"[..])
        );
    }

    /// The bytes ck's `put` hands to the backend open under the frozen
    /// encryption format. This catches a writer change that the same release's
    /// reader accepts, which `an_older_entry_still_decrypts` cannot. The
    /// master key has 32 distinct bytes. Besides a short text value, it
    /// writes compressible zeros at every power of 4 from 1 byte to 16 MiB,
    /// and at the most output this store keeps. So a writer branch gated on
    /// size, such as compression past a threshold, passes unseen only if its
    /// whole range falls between two neighbouring sizes.
    #[test]
    fn what_ck_writes_the_frozen_format_reads() {
        let master_key = hex::decode(MIXED_MASTER_KEY).unwrap();
        let dir = tempfile::tempdir().unwrap();
        // The file backend refuses a group- or other-writable directory, and
        // the tempdir's mode follows the umask.
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let store = Store::open_file(dir.path(), &master_key).unwrap();
        let sizes = (0..=12)
            .map(|k| 1 << (2 * k))
            .chain([0, store.max_output()]);
        let values = std::iter::once(b"ck known answer".to_vec()).chain(sizes.map(|n| vec![0; n]));
        for stdout in values {
            let plaintext = Envelope::encode(1, &stdout);
            store
                .set(KNOWN_ANSWER_KEY, &plaintext, Duration::from_secs(60))
                .unwrap();
            let stored = store
                .runtime
                .block_on(store.backend.get(KNOWN_ANSWER_KEY))
                .unwrap()
                .unwrap();
            let opened = frozen::open(&master_key, TENANT, KNOWN_ANSWER_KEY, &stored);
            assert!(
                opened.as_ref() == Some(&plaintext),
                "a {}-byte envelope was stored as {} bytes that the frozen format does not open \
                 to it; they start {}",
                plaintext.len(),
                stored.len(),
                hex::encode(&stored[..stored.len().min(32)])
            );
        }
    }

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
    fn the_cause_behind_a_client_error_is_named() {
        #[derive(Debug)]
        struct Wrapper(std::io::Error);
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("error sending request")
            }
        }
        impl std::error::Error for Wrapper {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let refused =
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let e = BackendError {
            kind: BackendErrorKind::Transient,
            message: "error sending request".into(),
            source: Some(Box::new(Wrapper(refused))),
        };
        let line = describe(&e);
        assert!(
            line.ends_with("error sending request: connection refused"),
            "{line}"
        );
        assert_eq!(line.matches("error sending request").count(), 1, "{line}");
    }

    #[test]
    fn server_text_cannot_break_the_line() {
        let e = BackendError::from_http_status(500, b"one\ntwo\x1b[31m");
        let line = describe(&e);
        assert!(!line.chars().any(char::is_control), "{line:?}");
    }
}
