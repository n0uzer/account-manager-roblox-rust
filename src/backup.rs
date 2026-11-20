//! Rolling backups of the encrypted store.
//!
//! A backup is a byte-for-byte copy of `accounts.dat` as it was *before* it got
//! overwritten, so it is exactly as protected as the original: encrypted with the
//! same master password (and DPAPI binding, if any). Nothing here ever reads or
//! writes plaintext.
//!
//! Copies live in `backups/` next to the store, named `accounts-<unix time>.dat`.
//! Saving happens after every small change, so copies are throttled: a new one is
//! taken only when the newest is at least [`MIN_INTERVAL_SECS`] old, and only the
//! last [`KEEP`] are kept.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// How many copies are kept.
pub const KEEP: usize = 5;
/// Minimum age of the newest copy before another one is taken.
pub const MIN_INTERVAL_SECS: u64 = 600;

const PREFIX: &str = "accounts-";
const SUFFIX: &str = ".dat";

/// `<store folder>/backups`
pub fn dir_for(store_path: &Path) -> PathBuf {
    store_path
        .parent()
        .map(|parent| parent.join("backups"))
        .unwrap_or_else(|| PathBuf::from("backups"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

/// Take a copy of the store file if one is due. Best effort: a failed backup must
/// never block saving, so errors are swallowed here.
pub fn snapshot_if_due(store_path: &Path) {
    let _ = snapshot(store_path, now_unix(), KEEP, MIN_INTERVAL_SECS);
}

/// Copy `store_path` into the backup folder.
///
/// Returns the new file, or `None` when there is nothing to copy yet or the
/// newest copy is still fresh.
fn snapshot(
    store_path: &Path,
    now: u64,
    keep: usize,
    min_interval: u64,
) -> io::Result<Option<PathBuf>> {
    if !store_path.is_file() {
        return Ok(None);
    }
    let dir = dir_for(store_path);
    fs::create_dir_all(&dir)?;

    let existing = list(&dir);
    if let Some((newest, _)) = existing.last() {
        if now.saturating_sub(*newest) < min_interval {
            return Ok(None);
        }
    }

    let target = dir.join(format!("{PREFIX}{now:010}{SUFFIX}"));
    if target.exists() {
        return Ok(None);
    }
    fs::copy(store_path, &target)?;
    prune(&dir, keep);
    Ok(Some(target))
}

/// Backups in `dir`, oldest first, as `(unix time, path)`.
fn list(dir: &Path) -> Vec<(u64, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let stamp = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
            Some((stamp.parse::<u64>().ok()?, entry.path()))
        })
        .collect();
    found.sort();
    found
}

/// Delete the oldest copies beyond `keep`.
fn prune(dir: &Path, keep: usize) {
    let all = list(dir);
    let surplus = all.len().saturating_sub(keep);
    for (_, path) in all.into_iter().take(surplus) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rbm-backup-{:016x}", rand::random::<u64>()));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn nothing_to_copy_without_a_store_file() {
        let dir = temp_dir();
        let result = snapshot(&dir.join("accounts.dat"), 1_000, 5, 600).expect("no error");
        assert!(result.is_none());
        assert!(!dir.join("backups").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn copy_is_byte_for_byte() {
        let dir = temp_dir();
        let store = dir.join("accounts.dat");
        fs::write(&store, b"RBM1-encrypted-bytes").expect("write");

        let copy = snapshot(&store, 1_000, 5, 600)
            .expect("ok")
            .expect("a copy was due");
        assert_eq!(fs::read(copy).expect("read"), b"RBM1-encrypted-bytes");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fresh_copy_throttles_the_next_one() {
        let dir = temp_dir();
        let store = dir.join("accounts.dat");
        fs::write(&store, b"v1").expect("write");

        assert!(snapshot(&store, 1_000, 5, 600).expect("ok").is_some());
        // 100 s later: still fresh.
        assert!(snapshot(&store, 1_100, 5, 600).expect("ok").is_none());
        // 600 s later: due again.
        assert!(snapshot(&store, 1_600, 5, 600).expect("ok").is_some());
        assert_eq!(list(&dir_for(&store)).len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn only_the_newest_copies_survive() {
        let dir = temp_dir();
        let store = dir.join("accounts.dat");
        fs::write(&store, b"data").expect("write");

        for step in 0..8u64 {
            snapshot(&store, 1_000 + step * 1_000, 3, 600).expect("ok");
        }
        let kept: Vec<u64> = list(&dir_for(&store))
            .into_iter()
            .map(|(stamp, _)| stamp)
            .collect();
        assert_eq!(kept, vec![6_000, 7_000, 8_000]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn foreign_files_in_the_folder_are_ignored() {
        let dir = temp_dir();
        let store = dir.join("accounts.dat");
        fs::write(&store, b"data").expect("write");
        let backups = dir_for(&store);
        fs::create_dir_all(&backups).expect("dir");
        fs::write(backups.join("notes.txt"), b"mine").expect("write");
        fs::write(backups.join("accounts-abc.dat"), b"junk").expect("write");

        snapshot(&store, 5_000, 1, 600).expect("ok");
        assert!(backups.join("notes.txt").exists());
        assert!(backups.join("accounts-abc.dat").exists());
        assert_eq!(list(&backups).len(), 1);
        let _ = fs::remove_dir_all(dir);
    }
}
