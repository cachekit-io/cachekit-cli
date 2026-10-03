//! The SaaS backend: its credentials, and the CacheKit client.

use cachekit::backend::cachekitio::CachekitIO;
use cachekit::backend::Backend;

use crate::keys::{self, MASTER_KEY_ENV};
use crate::Fatal;

pub(crate) const API_KEY_ENV: &str = "CACHEKIT_API_KEY";

/// Fixed in code. ck never reads `CACHEKIT_API_URL` and never allows a custom
/// host, so a repository's `.envrc` cannot send a production key elsewhere.
pub(crate) const API_URL: &str = "https://api.cachekit.io";

/// Builds the backend from an API key. [`crate::main`] uses [`connect`];
/// tests pass a fake.
pub type Connect = dyn Fn(&str) -> Result<Box<dyn Backend>, Fatal>;

pub(crate) struct Credentials {
    pub(crate) api_key: String,
    pub(crate) master_key: Vec<u8>,
}

/// Both keys, from the environment. Either one missing or unusable is exit
/// 125: saas never falls back to the file key, and a key that cannot write
/// ck's namespace is refused before anything runs.
pub(crate) fn credentials() -> Result<Credentials, Fatal> {
    let Some(master_key) = std::env::var_os(MASTER_KEY_ENV) else {
        return Err(Fatal(format!(
            "--backend saas needs {MASTER_KEY_ENV}, 64 hex characters from `openssl rand -hex 32`. \
             Every host that shares the cache needs the same one; the local file key is never used for saas"
        )));
    };
    let master_key = keys::decode_master_key(&master_key)?;

    let api_key = match std::env::var(API_KEY_ENV) {
        Ok(k) if !k.is_empty() => k,
        Ok(_) | Err(std::env::VarError::NotPresent) => {
            return Err(Fatal(format!(
                "--backend saas needs {API_KEY_ENV}, an SDK API key (ck_sdk_...) from the CacheKit dashboard"
            )))
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(Fatal(format!("{API_KEY_ENV} is not valid UTF-8")))
        }
    };
    // A ck_api_ key can read `ns:` keys but never write them, so every fill
    // would fail. Refuse it rather than warn on every call.
    if api_key.starts_with("ck_api_") {
        return Err(Fatal(format!(
            "{API_KEY_ENV} is a ck_api_ key, which cannot store ck's entries. Use an SDK key (ck_sdk_...) or a ck_live_ key"
        )));
    }
    Ok(Credentials {
        api_key,
        master_key,
    })
}

pub fn connect(api_key: &str) -> Result<Box<dyn Backend>, Fatal> {
    Ok(Box::new(client(api_key)?))
}

fn client(api_key: &str) -> Result<CachekitIO, Fatal> {
    CachekitIO::builder()
        .api_key(api_key)
        .api_url(API_URL)
        .build()
        .map_err(|e| Fatal(format!("cannot set up the CacheKit client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cachekit_api_url_is_ignored() {
        // No other test reads this variable.
        std::env::set_var("CACHEKIT_API_URL", "https://api.example.com");
        assert_eq!(client("ck_sdk_test").unwrap().api_url(), API_URL);
    }
}
