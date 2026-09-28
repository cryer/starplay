//! UTF-8 M3U playlist parsing and safe writing.

use crate::library::Track;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const MAX_BYTES: u64 = 1_048_576;
pub(crate) const MAX_ENTRIES: usize = 10_000;
pub(crate) const MAX_DEPTH: usize = 32;

/// A bounded UTF-8 file read. The error names the failing stage so each caller
/// keeps its own message wording.
pub(crate) enum ReadError {
    Open(io::Error),
    Read(io::Error),
    TooLarge,
    Utf8,
}

pub(crate) fn read_bounded_utf8(path: &Path, max: u64) -> Result<String, ReadError> {
    let file = fs::File::open(path).map_err(ReadError::Open)?;
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(ReadError::Read)?;
    if bytes.len() as u64 > max {
        return Err(ReadError::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| ReadError::Utf8)
}

/// Read a playlist's ordered, non-empty, non-comment entries.
pub(crate) fn load(path: &Path) -> Result<Vec<PathBuf>, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Cannot inspect playlist '{}': {error}", path.display()))?;
    if metadata.len() > MAX_BYTES {
        return Err(format!(
            "Playlist '{}' exceeds the {} byte limit",
            path.display(),
            MAX_BYTES
        ));
    }
    let text = read_bounded_utf8(path, MAX_BYTES).map_err(|error| match error {
        ReadError::Open(error) | ReadError::Read(error) => {
            format!("Cannot read playlist '{}': {error}", path.display())
        }
        ReadError::TooLarge => "Playlist exceeds size limit".to_string(),
        ReadError::Utf8 => format!("Playlist '{}' is not valid UTF-8", path.display()),
    })?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut entries = Vec::new();
    for (line_number, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.contains("://") || line.starts_with("\\\\") {
            return Err(format!(
                "Playlist '{}' line {} contains an unsupported URL",
                path.display(),
                line_number + 1
            ));
        }
        if entries.len() == MAX_ENTRIES {
            return Err(format!(
                "Playlist '{}' exceeds the {} entry limit",
                path.display(),
                MAX_ENTRIES
            ));
        }
        let entry = PathBuf::from(line);
        entries.push(if entry.is_absolute() {
            entry
        } else {
            base.join(entry)
        });
    }
    Ok(entries)
}

/// Save an M3U8 playlist without replacing an existing file.
pub fn save(path: &Path, tracks: &[Track]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Cannot create playlist '{}': {error}", path.display()))?;
    if let Err(error) = write_tracks(&mut file, tracks) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(format!(
            "Cannot write playlist '{}': {error}",
            path.display()
        ));
    }
    Ok(())
}

fn write_tracks(file: &mut fs::File, tracks: &[Track]) -> io::Result<()> {
    let mut text = String::from("#EXTM3U\n");
    for track in tracks {
        let path = track
            .path
            .to_str()
            .ok_or_else(|| io::Error::other("Playlist paths must be UTF-8"))?;
        if path.contains(['\n', '\r']) || path.trim() != path || path.starts_with('#') {
            return Err(io::Error::other("Path cannot be represented in M3U"));
        }
        text.push_str(path);
        text.push('\n');
    }
    if tracks.len() > MAX_ENTRIES || text.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("Playlist exceeds size or entry limit"));
    }
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_path(name: &str) -> PathBuf {
        std::env::current_dir().unwrap().join(format!(
            "starplay-playlist-test-{}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed),
            name
        ))
    }

    #[test]
    fn loads_utf8_entries_and_skips_bom_comments_and_empty_lines() {
        let path = temp_path("input.m3u8");
        fs::write(
            &path,
            "\u{feff}#EXTM3U\n\n songs/a.mp3 \n# note\n音乐.ogg\n",
        )
        .unwrap();
        assert_eq!(
            load(&path).unwrap(),
            [
                path.parent().unwrap().join("songs/a.mp3"),
                path.parent().unwrap().join("音乐.ogg")
            ]
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn save_is_create_only_and_writes_order() {
        let path = temp_path("output.m3u8");
        let tracks = [
            Track {
                path: PathBuf::from("b.mp3"),
                title: "b".into(),
            },
            Track {
                path: PathBuf::from("a.mp3"),
                title: "a".into(),
            },
        ];
        save(&path, &tracks).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "#EXTM3U\nb.mp3\na.mp3\n"
        );
        assert!(save(&path, &tracks).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "#EXTM3U\nb.mp3\na.mp3\n"
        );
        let _ = fs::remove_file(path);
    }
}
