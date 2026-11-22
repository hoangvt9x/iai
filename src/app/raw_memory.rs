//! Develop settings remembered per RAW file, the way Camera Raw keeps them in
//! a sidecar: «Open Image» writes each session image's settings, and the next
//! Develop session on that file starts from them. Stored under
//! `%APPDATA%\IAI\raw_settings\`, one small JSON file per RAW, so nothing is
//! written next to the photos. A file that changed size since is not the same
//! photo any more and starts from zero.

use std::path::{Path, PathBuf};

use crate::core::develop::DevelopSettings;

/// Most remembered files kept; the oldest are dropped past it.
const MAX_FILES: usize = 5000;

#[derive(serde::Serialize, serde::Deserialize)]
struct Remembered {
    path: String,
    size: u64,
    settings: DevelopSettings,
}

fn key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

fn entry_file(dir: &Path, path: &Path) -> PathBuf {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in key(path).bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    dir.join(format!("{hash:016x}.json"))
}

fn dir() -> Option<PathBuf> {
    Some(crate::app::autosave::iai_data_dir()?.join("raw_settings"))
}

/// The settings last opened for `path`, if any.
pub(in crate::app) fn recall(path: &Path) -> Option<DevelopSettings> {
    recall_in(&dir()?, path)
}

/// Remember `settings` for `path`; a neutral set forgets the file.
pub(in crate::app) fn remember(path: &Path, settings: &DevelopSettings) {
    if let Some(dir) = dir() {
        remember_in(&dir, path, settings);
    }
}

fn recall_in(dir: &Path, path: &Path) -> Option<DevelopSettings> {
    let size = std::fs::metadata(path).ok()?.len();
    let text = std::fs::read_to_string(entry_file(dir, path)).ok()?;
    let entry: Remembered = serde_json::from_str(&text).ok()?;
    (entry.path == key(path) && entry.size == size && !entry.settings.is_neutral())
        .then_some(entry.settings)
}

fn remember_in(dir: &Path, path: &Path, settings: &DevelopSettings) {
    let file = entry_file(dir, path);
    if settings.is_neutral() {
        let _ = std::fs::remove_file(file);
        return;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let entry = Remembered {
        path: key(path),
        size: meta.len(),
        settings: settings.clone(),
    };
    let Ok(text) = serde_json::to_string(&entry) else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = file.with_extension("tmp");
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &file).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    prune(dir);
}

/// Keep at most `MAX_FILES` remembered files, dropping the oldest.
fn prune(dir: &Path) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = read
        .flatten()
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();
    if entries.len() <= MAX_FILES {
        return;
    }
    entries.sort_by_key(|(modified, _)| *modified);
    let surplus = entries.len() - MAX_FILES;
    for (_, path) in entries.into_iter().take(surplus) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_come_back_for_the_same_file_only() {
        let root = std::env::temp_dir().join(format!("iai-raw-memory-{}", std::process::id()));
        let dir = root.join("mem");
        std::fs::create_dir_all(&root).unwrap();
        let photo = root.join("IMG_0001.CR2");
        std::fs::write(&photo, [1u8; 64]).unwrap();
        let mut settings = DevelopSettings::default();
        settings.exposure = 0.75;

        assert!(recall_in(&dir, &photo).is_none());
        remember_in(&dir, &photo, &settings);
        assert_eq!(recall_in(&dir, &photo), Some(settings.clone()));
        // Same name in another case: the same file on Windows (and on a Mac
        // disk as it comes); Linux keeps the two names apart.
        if cfg!(any(windows, target_os = "macos")) {
            let upper = root.join("img_0001.cr2");
            assert_eq!(recall_in(&dir, &upper), Some(settings.clone()));
        }

        // Replaced by a different photo of another size: forgotten.
        std::fs::write(&photo, [1u8; 65]).unwrap();
        assert!(recall_in(&dir, &photo).is_none());

        // Reset to neutral and opened: forgotten.
        remember_in(&dir, &photo, &settings);
        remember_in(&dir, &photo, &DevelopSettings::default());
        assert!(recall_in(&dir, &photo).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
