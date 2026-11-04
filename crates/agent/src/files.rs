//! Writing files pushed from the hub.
//!
//! Atomicity is the whole point: the hub can push to a fleet at 3am, and a
//! half-written `/etc/nginx/nginx.conf` with a full disk behind it would take
//! the box out. So: temp file in the same directory, fsync, chmod, rename.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub fn write(path: &str, mode: u32, data: &[u8]) -> Result<u64> {
    let p = Path::new(path);
    if !p.is_absolute() {
        bail!("path must be absolute");
    }
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .context("path has no file name")?;
    let dir = p.parent().unwrap_or_else(|| Path::new("/"));
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    }

    let tmp = dir.join(format!(".{name}.hypha-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
        let mode = if mode == 0 { 0o644 } else { mode };
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, p).with_context(|| format!("rename onto {}", p.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(data.len() as u64)
}
