//! The bootstrap access token (UI §3.1 / §6.10).
//!
//! Generated once on the first start, stored in `<data>/token` with 0600 and
//! printed exactly once as a `#token=` login link. `--rotate-token` replaces
//! it, which also invalidates every session. The value must never appear in
//! logs, events or error text (R8); [`TokenStore::current`] exists only so
//! the one-time link can be built.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rand::Rng;

/// Token entropy (256-bit), hex-encoded to 64 characters.
const TOKEN_BYTES: usize = 32;

/// File name of the token inside the data directory.
const TOKEN_FILE: &str = "token";

/// The current access token, backed by `<data>/token`.
#[derive(Debug)]
pub struct TokenStore {
    path: PathBuf,
    current: String,
}

impl TokenStore {
    /// Load `<data>/token`, creating it (0600) on the first start. The
    /// returned flag tells the caller to print the one-time login link.
    pub fn load_or_create(data_dir: &Path) -> io::Result<(Self, bool)> {
        let path = data_dir.join(TOKEN_FILE);
        if let Ok(text) = fs::read_to_string(&path) {
            let token = text.trim();
            if !token.is_empty() {
                return Ok((
                    Self {
                        path,
                        current: token.to_string(),
                    },
                    false,
                ));
            }
        }
        let store = Self {
            path,
            current: random_hex(TOKEN_BYTES),
        };
        store.persist()?;
        Ok((store, true))
    }

    /// Replace the token with a fresh one and persist it. Callers must also
    /// drop every session (see [`crate::state::AppState::rotate_token`]).
    pub fn rotate(&mut self) -> io::Result<()> {
        self.current = random_hex(TOKEN_BYTES);
        self.persist()
    }

    /// Constant-time check of a presented token.
    pub fn verify(&self, presented: &str) -> bool {
        constant_time_eq(presented.as_bytes(), self.current.as_bytes())
    }

    /// The raw token value. Only for building the one-time login link;
    /// never log or serialize it.
    pub fn current(&self) -> &str {
        &self.current
    }

    /// Write the token atomically: a 0600 temp file, then rename over the
    /// old value so a crash never leaves a truncated token behind.
    fn persist(&self) -> io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        write_private(&tmp, &self.current)?;
        fs::rename(&tmp, &self.path)
    }
}

/// Write `content` (plus newline) to `path` with mode 0600.
fn write_private(path: &Path, content: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// `n` random bytes as lowercase hex: tokens and session ids.
pub(super) fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time equality: no early exit on the first differing byte, so a
/// timing side channel cannot reveal how much of a guessed token matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_start_creates_a_0600_token_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let (store, created) = TokenStore::load_or_create(dir.path()).unwrap();
        assert!(created);
        assert_eq!(store.current().len(), TOKEN_BYTES * 2);

        use std::os::unix::fs::MetadataExt;
        let mode = fs::metadata(dir.path().join(TOKEN_FILE)).unwrap().mode();
        assert_eq!(mode & 0o777, 0o600);

        // A restart keeps the same token and does not ask for a re-print.
        let (reloaded, created_again) = TokenStore::load_or_create(dir.path()).unwrap();
        assert!(!created_again);
        assert_eq!(reloaded.current(), store.current());
    }

    #[test]
    fn rotate_replaces_the_token_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = TokenStore::load_or_create(dir.path()).unwrap();
        let before = store.current().to_string();
        store.rotate().unwrap();
        assert_ne!(store.current(), before);

        let (reloaded, _) = TokenStore::load_or_create(dir.path()).unwrap();
        assert_eq!(reloaded.current(), store.current());
        assert!(!reloaded.verify(&before));
        assert!(reloaded.verify(store.current()));
    }

    #[test]
    fn verify_rejects_prefixes_and_case_flips() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = TokenStore::load_or_create(dir.path()).unwrap();
        let token = store.current();
        assert!(!store.verify(&token[..token.len() - 1]));
        assert!(!store.verify(&token.to_uppercase()));
        assert!(!store.verify(""));
    }
}
