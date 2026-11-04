//! Small shared helpers: ids, password hashing, platform detection.

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::distributions::Alphanumeric;
use rand::Rng;

/// `<prefix>_<n random lowercase alnum>` — short, greppable, no dashes so it
/// survives being pasted into shells and URLs alike.
pub fn gen_id(prefix: &str, n: usize) -> String {
    let s: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(|c| (c as char).to_ascii_lowercase())
        .collect();
    format!("{prefix}_{s}")
}

pub fn hash_password(pw: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .unwrap_or_default()
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    if hash.is_empty() {
        return false;
    }
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(pw.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

pub fn random_password() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(18)
        .map(|c| c as char)
        .collect()
}
