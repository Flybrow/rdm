//! Where downloads go on disk: a free or chosen name, the temporary files around them, the
//! Windows mark of a file from the Internet.

use super::*;

/// Every temporary file an unfinished download may leave behind, plus the file itself ("").
fn cleanup_suffixes() -> impl Iterator<Item = &'static str> {
    ["", engine::STATE_SUFFIX, ".rdm.tmp", ".video.part.rdm.tmp", ".audio.part.rdm.tmp"]
        .into_iter()
        .chain(engine::PART_SUFFIXES)
}

/// What removing an unfinished download (or deleting its file) erases: its temporary files, and
/// the file itself — unless it existed before ("overwrite") and the transfer has not written over
/// it yet (no resume point next to it: not started, failed before its first byte, or a split
/// download whose parts are still apart). That file is the user's, not this download's.
pub(super) fn leftovers(target: &Path, delete_file: bool, replaces: bool) -> Vec<PathBuf> {
    let untouched = replaces && !with_suffix(target, engine::STATE_SUFFIX).exists();
    let keep_file = !delete_file && untouched;
    cleanup_suffixes().filter(|s| !(keep_file && s.is_empty())).map(|s| with_suffix(target, s)).collect()
}

/// Chunks of an interrupted recording (`<name>.rec<N>.<track>`), e.g. left by a previous session.
pub(super) async fn remove_recording_leftovers(target: &Path) {
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else { return };
    let name = name.to_string_lossy();
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else { return };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if is_recording_part(&name, &entry.file_name().to_string_lossy()) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

/// `<name>.rec<digits>.video` / `.audio` exactly (see `Recording::part`): never another file
/// that merely starts the same way.
fn is_recording_part(name: &str, file: &str) -> bool {
    file.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix(".rec"))
        .and_then(|rest| rest.split_once('.'))
        .is_some_and(|(ms, track)| !ms.is_empty() && ms.bytes().all(|b| b.is_ascii_digit()) && matches!(track, "video" | "audio"))
}

/// Mark-of-the-Web, as browsers do: SmartScreen / Office Protected View then warn before running
/// downloaded executables or macros. Zone only — the source URL is not recorded (privacy).
pub(super) fn mark_from_internet(path: &Path) {
    if cfg!(windows) {
        let _ = fs::write(with_suffix(path, ":Zone.Identifier"), "[ZoneTransfer]\r\nZoneId=3\r\n");
    }
}

/// Where a new download of `name` goes, following the settings when a file of that name exists
/// (`None`: skip it). Overwriting never follows a symbolic link (it could point anywhere), nor
/// takes a name another download of the list will write.
pub(super) fn target_for(settings: &Settings, name: &str, taken: &[Entry], except: Option<DownloadId>) -> Option<PathBuf> {
    let dir = settings.target_dir(name);
    let path = dir.join(name);
    match settings.existing {
        // Asked beforehand when possible (see `resolved`, `add_from_browser`); otherwise a new name.
        ExistingFile::Rename | ExistingFile::Ask => Some(unique_path(&dir, name, taken, except)),
        ExistingFile::Skip if path.is_file() => None,
        ExistingFile::Skip => Some(unique_path(&dir, name, taken, except)),
        ExistingFile::Overwrite => {
            let link = fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
            // Another download of the list still writing (or to write) that file keeps it: two
            // transfers into one file would corrupt both. A completed one gives it up.
            let planned = taken.iter().any(|e| {
                e.download.target == path && Some(e.download.id) != except && *e.download.status() != Status::Completed
            });
            if link || planned || path.is_dir() {
                return Some(unique_path(&dir, name, taken, except));
            }
            // Leftovers of an earlier, unrelated download of that name must not be resumed.
            for suffix in cleanup_suffixes().filter(|s| !s.is_empty()) {
                let _ = fs::remove_file(with_suffix(&path, suffix));
            }
            Some(path)
        }
    }
}

/// A free path for `name` in `dir`: neither on disk (with or without leftovers of an unfinished
/// download) nor planned by another entry of the list (`except`: the entry being renamed).
pub(super) fn unique_path(dir: &Path, name: &str, taken: &[Entry], except: Option<DownloadId>) -> PathBuf {
    let (stem, ext) = name.rsplit_once('.').map_or((name, None), |(s, e)| (s, Some(e)));
    (0u32..)
        .map(|i| match (i, ext) {
            (0, _) => dir.join(name),
            (_, Some(ext)) => dir.join(format!("{stem} ({i}).{ext}")),
            (_, None) => dir.join(format!("{stem} ({i})")),
        })
        .find(|p| {
            !p.exists()
                && !with_suffix(p, engine::STATE_SUFFIX).exists()
                && !taken.iter().any(|e| &e.download.target == p && Some(e.download.id) != except)
        })
        .expect("unbounded range always yields a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_covers_every_temporary_file() {
        let all: Vec<_> = cleanup_suffixes().collect();
        for s in [".rdm", ".video.part", ".video.part.ok", ".audio.part.rdm", ".mux.tmp"] {
            assert!(all.contains(&s), "{s}");
        }
    }

    #[test]
    fn only_recording_chunks_count_as_leftovers() {
        let target = PathBuf::from("dl").join("Clip.mp4");
        let recording = Recording { id: DownloadId::new(), target, parts: HashMap::new(), last_data: Instant::now() };
        let part = recording.part(3, Track::Audio);
        assert!(is_recording_part("Clip.mp4", &part.file_name().unwrap().to_string_lossy()));
        assert!(is_recording_part("Clip.mp4", "Clip.mp4.rec12.video"));
        for other in ["Clip.mp4.recipe.txt", "Clip.mp4.rec.video", "Clip.mp4.rec1.video.bak", "Clip.mp4.rec1x.audio", "Clip.mp4"] {
            assert!(!is_recording_part("Clip.mp4", other), "{other}");
        }
    }

    /// Regression: with "overwrite", removing a download that failed before writing anything
    /// deleted the user's existing file of that name.
    #[test]
    fn removal_spares_a_file_the_download_has_not_written_over() {
        let dir = std::env::temp_dir().join(format!("rdm-leftovers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("report.pdf");
        std::fs::write(&target, b"the user's file").unwrap();
        let erases_file = |delete_file, replaces| leftovers(&target, delete_file, replaces).contains(&target);

        assert!(!erases_file(false, true), "not written over yet: kept");
        assert!(erases_file(true, true), "\"delete the file\" deletes it");
        assert!(erases_file(false, false), "a partial file of this download's own");
        std::fs::write(with_suffix(&target, engine::STATE_SUFFIX), b"[]").unwrap();
        assert!(erases_file(false, true), "the transfer has written over it: a partial file now");
        assert!(leftovers(&target, false, true).contains(&with_suffix(&target, engine::STATE_SUFFIX)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
