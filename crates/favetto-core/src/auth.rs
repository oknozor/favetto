//! Token generation, storage, and verification.
//!
//! A single bearer token grants full access, and verification is constant-time.

use std::path::Path;

use subtle::ConstantTimeEq;
use uuid::Uuid;

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
