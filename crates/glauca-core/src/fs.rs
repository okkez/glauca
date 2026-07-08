//! Small filesystem helpers shared across front-ends.

use std::io;
use std::path::Path;

/// Write `contents` to `path` atomically, creating the parent directory if
/// needed.
///
/// The write goes to a temp file next to `path` and is then `rename`d into
/// place, rather than a direct `fs::write`, which truncates then writes: a
/// crash or a concurrent writer mid-write could otherwise leave a half-written
/// file, and callers that treat any parse failure as "reset to defaults" would
/// silently lose every setting. `rename` within the same directory is atomic,
/// so a reader sees either the old file or the fully-written new one, never a
/// torn one. The temp name is process-scoped so two glauca instances don't
/// clobber each other's temp file.
///
/// On a failed `rename` the temp file is removed so it doesn't linger, and the
/// original error is returned.
///
/// Note: this is crash-atomic, not power-loss-durable — neither the temp file
/// nor the parent directory is `fsync`ed. That is fine for best-effort
/// presentation settings; add `fsync` here if durability ever matters.
pub fn atomic_write(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic_write: path has no file name",
        )
    })?;
    let tmp_name = format!("{}.{}.tmp", file_name.to_string_lossy(), std::process::id());
    let tmp_path = path.with_file_name(tmp_name);
    std::fs::write(&tmp_path, contents)?;
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_new_file_with_no_leftover_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");

        atomic_write(&path, "hello = 1").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello = 1");
        assert!(!has_tmp_leftover(dir.path()), "temp file lingered");
    }

    #[test]
    fn overwrites_existing_file_with_no_leftover_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "old = true").unwrap();

        atomic_write(&path, "new = true").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new = true");
        assert!(!has_tmp_leftover(dir.path()), "temp file lingered");
    }

    #[test]
    fn creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("settings.toml");

        atomic_write(&path, "x = 0").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x = 0");
    }

    /// True if the directory contains any `*.tmp` file (a leaked temp write).
    fn has_tmp_leftover(dir: &Path) -> bool {
        std::fs::read_dir(dir).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "tmp")
        })
    }
}
