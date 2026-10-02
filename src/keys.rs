//! The master key, the auto-created file key, and the cache key.

use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use blake2::digest::consts::U32;
use blake2::digest::Mac;
use blake2::Blake2bMac;

use crate::Fatal;

/// The encryption tenant and the HKDF salt context. A fixed constant: deriving
/// it from an API key would orphan every entry when the key rotates.
pub const TENANT: &str = "ck-run";

/// Every value key starts with this. `v1` is the one version authority for
/// the envelope and marker formats, and it is bound into the AAD.
const KEY_PREFIX: &str = "ns:ck-run:v1:";

pub const MASTER_KEY_ENV: &str = "CACHEKIT_MASTER_KEY";

/// `CACHEKIT_MASTER_KEY` when set, otherwise the file key in `config_dir`,
/// created on first use.
pub fn master_key(config_dir: &Path) -> Result<Vec<u8>, Fatal> {
    if let Some(hex_key) = std::env::var_os(MASTER_KEY_ENV) {
        return decode_master_key(&hex_key);
    }
    file_key(config_dir)
}

fn decode_master_key(hex_key: &OsString) -> Result<Vec<u8>, Fatal> {
    let bad = || {
        Fatal(format!(
            "{MASTER_KEY_ENV} must be at least 32 bytes, hex-encoded (generate one with `openssl rand -hex 32`)"
        ))
    };
    let bytes = hex::decode(hex_key.as_bytes()).map_err(|_| bad())?;
    if bytes.len() < 32 {
        return Err(bad());
    }
    Ok(bytes)
}

/// Read `file.key`, creating it first if it does not exist.
///
/// Creation is publish-by-`link()`: the key is written and synced under a
/// unique temp name, then hard-linked to its final name. A racing first run
/// either wins the link or reads the winner's complete file; it can never
/// read a half-written one, and a crash leaves either no key or a whole key.
pub fn file_key(config_dir: &Path) -> Result<Vec<u8>, Fatal> {
    let path = config_dir.join("file.key");
    let unusable = |why: &str| {
        Fatal(format!(
            "{} {why}. Delete it and ck creates a new one; every entry cached under the old key becomes a miss",
            path.display()
        ))
    };
    // O_NOFOLLOW, then fstat and read the same descriptor: the file whose
    // owner and mode are checked is the file whose key is used.
    let open = || {
        fs::OpenOptions::new()
            .read(true)
            // O_NONBLOCK so a FIFO planted at the path cannot hang the open;
            // the fstat below rejects anything but a regular file.
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(&path)
    };
    let mut file = match open() {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            create_file_key(config_dir, &path)?;
            open()
        }
        other => other,
    }
    .map_err(|e| match e.raw_os_error() {
        Some(libc::ELOOP) => unusable("is a symbolic link"),
        _ => unusable(&format!("cannot be read ({e})")),
    })?;
    let meta = file.metadata().map_err(|e| unusable(&e.to_string()))?;
    if !meta.is_file() {
        return Err(unusable("is not a regular file"));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(unusable("is not owned by you"));
    }
    if meta.mode() & 0o777 != 0o600 {
        return Err(unusable("must have mode 0600"));
    }
    let mut text = Vec::new();
    file.read_to_end(&mut text)
        .map_err(|e| unusable(&e.to_string()))?;
    match hex::decode(&text) {
        Ok(key) if key.len() == 32 => Ok(key),
        _ => Err(unusable("is not 64 hex characters")),
    }
}

fn create_file_key(dir: &Path, path: &Path) -> Result<(), Fatal> {
    let fail =
        |what: &str, e: std::io::Error| Fatal(format!("cannot {what} {}: {e}", path.display()));

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| fail("create the directory for", e))?;

    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut secret)
        .map_err(|e| Fatal(format!("cannot generate a file key: {e}")))?;

    let temp = dir.join(format!(
        ".file.key.{}.{}",
        std::process::id(),
        unique_suffix()
    ));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut f| {
            f.write_all(hex::encode(secret).as_bytes())?;
            f.sync_all()
        });
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(fail("write", e));
    }

    let linked = fs::hard_link(&temp, path);
    // The temp name goes whether the link won or lost. A failure to remove it
    // leaves an inert file of the same mode in the same directory.
    let _ = fs::remove_file(&temp);
    match linked {
        // Another first run won the link: its key is complete, use it.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(fail("create", e)),
        Ok(()) => fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| fail("sync the directory of", e)),
    }
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// The keyed hash behind every cache key, derived from the master key with
/// the protocol's `cache_key_salt` derivation.
pub struct KeyHasher([u8; 32]);

impl KeyHasher {
    pub fn new(master_key: &[u8]) -> Result<Self, Fatal> {
        cachekit_core::encryption::derive_domain_key(master_key, "cache_keys", TENANT.as_bytes())
            .map(Self)
            .map_err(|e| Fatal(format!("cannot derive the cache-key hash key: {e}")))
    }

    /// `ns:ck-run:v1:<64 hex>`: a keyed BLAKE2b-256 over the length-prefixed
    /// scope, then each argv element. Every field is length-prefixed, so the
    /// encoding is injective: no scope or argv produces another entry's key.
    pub fn value_key(&self, scope: &OsString, argv: &[OsString]) -> String {
        let mut mac = Blake2bMac::<U32>::new_from_slice(&self.0)
            .unwrap_or_else(|_| unreachable!("a 32-byte key is within BLAKE2b's 64-byte limit"));
        for field in std::iter::once(scope).chain(argv) {
            let bytes = field.as_bytes();
            mac.update(&(bytes.len() as u64).to_be_bytes());
            mac.update(bytes);
        }
        format!("{KEY_PREFIX}{}", hex::encode(mac.finalize().into_bytes()))
    }
}

/// The marker sits beside the value, under the same keyed hash.
pub fn marker_key(value_key: &str) -> String {
    format!("{value_key}:neg")
}

/// The 64-hex hash part of a key from [`KeyHasher::value_key`], used to name
/// its fill lock.
pub fn key_hash(value_key: &str) -> &str {
    &value_key[KEY_PREFIX.len()..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hasher() -> KeyHasher {
        KeyHasher::new(&[7u8; 32]).unwrap()
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn key_shape() {
        let key = hasher().value_key(&"".into(), &os(&["op", "read"]));
        let hash = key.strip_prefix("ns:ck-run:v1:").unwrap();
        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(key_hash(&key), hash);
        assert_eq!(marker_key(&key), format!("{key}:neg"));
    }

    #[test]
    fn field_boundaries_do_not_collide() {
        let h = hasher();
        let keys = [
            h.value_key(&"".into(), &os(&["ab", "c"])),
            h.value_key(&"".into(), &os(&["a", "bc"])),
            h.value_key(&"".into(), &os(&["abc"])),
            h.value_key(&"a".into(), &os(&["bc"])),
            h.value_key(&"".into(), &os(&["", "abc"])),
        ];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn different_master_keys_give_disjoint_keys() {
        let other = KeyHasher::new(&[8u8; 32]).unwrap();
        let argv = os(&["op", "read"]);
        assert_ne!(
            hasher().value_key(&"".into(), &argv),
            other.value_key(&"".into(), &argv)
        );
    }

    #[test]
    fn master_key_hex() {
        assert!(decode_master_key(&"ab".repeat(32).into()).is_ok());
        assert!(decode_master_key(&"ab".repeat(31).into()).is_err());
        assert!(decode_master_key(&"zz".repeat(32).into()).is_err());
    }
}
