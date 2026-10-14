//! Token generation, storage, and verification.
//!
//! A single bearer token grants full access, and verification is constant-time.
//!
//! Remote clients can also log in with a username and password; passwords are
//! hashed with argon2id and verified in constant time (see [`hash_password`] and
//! [`verify_password`]).

use std::path::Path;

use argon2::Argon2;
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// Output length, in bytes, of an argon2id password hash.
const PASSWORD_HASH_LEN: usize = 32;

/// Hash `password` with argon2id, returning `"<salt_hex>:<hash_hex>"`.
///
/// A fresh random 16-byte salt is drawn from a UUID v4 for every call, so the
/// same password never produces the same stored value twice.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = *Uuid::new_v4().as_bytes();
    let mut hash = [0u8; PASSWORD_HASH_LEN];
    Argon2::default()
        .hash_password_into(password.as_bytes(), &salt, &mut hash)
        .map_err(|e| anyhow::anyhow!("argon2 hashing failed: {e}"))?;
    Ok(format!("{}:{}", hex::encode(salt), hex::encode(hash)))
}

/// Verify `password` against a stored `"<salt_hex>:<hash_hex>"` value.
///
/// The final comparison is constant-time. A malformed stored value (missing
/// separator, non-hex, or the wrong length) is rejected without panicking.
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    let Some((salt_hex, expected_hex)) = stored_hash.split_once(':') else {
        return false;
    };
    let (Ok(salt), Ok(expected)) = (hex::decode(salt_hex), hex::decode(expected_hex)) else {
        return false;
    };
    if expected.len() != PASSWORD_HASH_LEN {
        return false;
    }
    let mut actual = [0u8; PASSWORD_HASH_LEN];
    if Argon2::default()
        .hash_password_into(password.as_bytes(), &salt, &mut actual)
        .is_err()
    {
        return false;
    }
    bool::from(actual.ct_eq(expected.as_slice()))
}

/// A bearer token used to authenticate remote (WebSocket/TCP) clients.
#[derive(Debug, Clone)]
pub struct Token {
    value: String,
}

impl Token {
    /// Generate a fresh high-entropy token.
    pub fn generate() -> Self {
        // Two UUIDs chained give 256 bits of entropy in a URL-safe shape.
        let value = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        Self { value }
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Constant-time comparison of a presented token against this one.
    pub fn verify(&self, presented: &str) -> bool {
        let a = self.value.as_bytes();
        let b = presented.as_bytes();
        (a.len() == b.len()) && bool::from(a.ct_eq(b))
    }

    /// Load a token from `path`, creating it (mode 0600) if missing.
    ///
    /// Synchronous on purpose: the file is tiny and this runs once at startup.
    pub fn load_or_create(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(Self {
                value: s.trim().to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let token = Self::generate();
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, format!("{}\n", token.as_str()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                }
                Ok(token)
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trips() {
        let hash = hash_password("correct horse battery staple").expect("hash");
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong password", &hash));
    }

    #[test]
    fn hashes_use_a_unique_salt() {
        let first = hash_password("same password").expect("hash");
        let second = hash_password("same password").expect("hash");
        assert_ne!(first, second, "each hash must use a fresh salt");
        assert!(verify_password("same password", &first));
        assert!(verify_password("same password", &second));
    }

    #[test]
    fn malformed_stored_hashes_are_rejected() {
        for bad in ["", "no-separator", "zz:zz", "abcd:", ":abcd", "abcd:ef"] {
            assert!(!verify_password("anything", bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn token_verify_rejects_a_wrong_value() {
        let token = Token::generate();
        assert!(token.verify(token.as_str()));
        assert!(!token.verify("not the token"));
    }
}
