//! Small shared helpers: ids and random strings.

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

