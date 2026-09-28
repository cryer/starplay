//! Discover supported audio files without following directory symlinks.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
}

/// Expand paths in their supplied order, sorting each directory by file name.
///
/// Tracks have canonical absolute paths. The first spelling of a file supplies
/// its title; canonical paths identify duplicates across overlapping inputs.
/// Directory symlinks are skipped, even when explicitly supplied. Symlinks to
/// supported regular files are accepted. An empty input scans `.`.
pub fn discover(paths: &[PathBuf], recursive: bool) -> Result<Vec<Track>, String> {
    let mut scanner = Scanner {
        recursive,
        seen: HashSet::new(),
        playlist_stack: HashSet::new(),
        playlist_entries: 0,
        tracks: Vec::new(),
    };
    if paths.is_empty() {
        scanner.visit(Path::new("."), true, true)?;
    } else {
        for path in paths {
            scanner.visit(path, true, true)?;
        }
    }
    Ok(scanner.tracks)
}

struct Scanner {
    recursive: bool,
    seen: HashSet<PathBuf>,
    playlist_stack: HashSet<PathBuf>,
    playlist_entries: usize,
    tracks: Vec<Track>,
}

impl Scanner {
    fn visit(&mut self, path: &Path, scan_directory: bool, explicit: bool) -> Result<(), String> {
        let link_metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            // Directory contents may change between read_dir and inspection.
            Err(error) if !explicit && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(())
            }
            Err(error) => return Err(format!("Cannot inspect '{}': {error}", path.display())),
        };
        let is_symlink = link_metadata.file_type().is_symlink();
        let metadata = if is_symlink {
            match fs::metadata(path) {
                Ok(metadata) => metadata,
                // Broken or inaccessible symlinks found inside directories are
                // not audio files. Explicit invalid inputs remain errors.
                Err(_) if !explicit => return Ok(()),
                Err(error) => {
                    return Err(format!("Cannot follow '{}': {error}", path.display()));
                }
            }
        } else {
            link_metadata
        };

        if metadata.is_dir() {
            if is_symlink || !scan_directory {
                return Ok(());
            }
            let entries = fs::read_dir(path)
                .map_err(|error| format!("Cannot read directory '{}': {error}", path.display()))?;
            let mut children = Vec::new();
            for entry in entries {
                children.push(
                    entry
                        .map_err(|error| {
                            format!("Cannot read entry in '{}': {error}", path.display())
                        })?
                        .path(),
                );
            }
            children.sort();
            for child in children {
                self.visit(&child, self.recursive, false)?;
            }
            return Ok(());
        }

        if !metadata.is_file() {
            return if explicit {
                Err(format!(
                    "Unsupported file '{}'; expected an audio file or playlist",
                    path.display()
                ))
            } else {
                Ok(())
            };
        }

        if playlist(path) {
            if !explicit {
                return Ok(());
            }
            let canonical = fs::canonicalize(path).map_err(|error| {
                format!("Cannot resolve playlist '{}': {error}", path.display())
            })?;
            if self.playlist_stack.len() >= crate::playlist::MAX_DEPTH {
                return Err(format!(
                    "Playlist nesting exceeds {} levels at '{}'",
                    crate::playlist::MAX_DEPTH,
                    path.display()
                ));
            }
            if !self.playlist_stack.insert(canonical.clone()) {
                return Err(format!("Playlist cycle detected at '{}'", path.display()));
            }
            let result = crate::playlist::load(path).and_then(|entries| {
                for entry in entries {
                    self.playlist_entries += 1;
                    if self.playlist_entries > crate::playlist::MAX_ENTRIES {
                        return Err(format!(
                            "Playlist expansion exceeds the {} entry limit",
                            crate::playlist::MAX_ENTRIES
                        ));
                    }
                    self.visit(&entry, false, true)?;
                }
                Ok(())
            });
            self.playlist_stack.remove(&canonical);
            return result;
        }

        if !supported(path) {
            return if explicit {
                Err(format!(
                    "Unsupported audio file '{}'; expected mp3, wav, flac, or ogg",
                    path.display()
                ))
            } else {
                Ok(())
            };
        }

        let canonical = fs::canonicalize(path)
            .map_err(|error| format!("Cannot resolve '{}': {error}", path.display()))?;
        let first_seen = self.seen.insert(canonical.clone());
        if first_seen || !self.playlist_stack.is_empty() {
            self.tracks.push(Track {
                path: canonical,
                title: path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            });
        }
        Ok(())
    }
}

fn playlist(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("m3u") || extension.eq_ignore_ascii_case("m3u8")
        })
}

fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["mp3", "wav", "flac", "ogg"]
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            // Stay inside the project and avoid process-wide current-directory
            // changes so these tests can safely run in parallel.
            let root = std::env::current_dir().unwrap().join(format!(
                ".starplay-library-test-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"discovery does not decode audio").unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn titles(tracks: &[Track]) -> Vec<&str> {
        tracks.iter().map(|track| track.title.as_str()).collect()
    }

    #[test]
    fn supports_only_the_four_extensions_case_insensitively() {
        for name in ["x.mp3", "x.WAV", "x.FlAc", "x.oGg"] {
            assert!(supported(Path::new(name)), "{name}");
        }
        for name in ["x", "x.txt", "x.mp4", "x.mp3.bak", ".mp3"] {
            assert!(!supported(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn directories_are_sorted_and_non_audio_is_ignored() {
        let fixture = Fixture::new();
        fixture.file("z.ogg");
        fixture.file("a.WAV");
        fixture.file("m.FlAc");
        fixture.file("b.mp3");
        fixture.file("readme.txt");
        fixture.file("no_extension");
        let tracks = discover(std::slice::from_ref(&fixture.0), false).unwrap();
        assert_eq!(titles(&tracks), ["a", "b", "m", "z"]);
        assert!(tracks.iter().all(|track| track.path.is_absolute()));
    }

    #[test]
    fn recursion_is_opt_in_and_depth_first_sorted() {
        let fixture = Fixture::new();
        fixture.file("z.mp3");
        fixture.file("a/y.wav");
        fixture.file("a/b/x.flac");
        assert_eq!(
            titles(&discover(std::slice::from_ref(&fixture.0), false).unwrap()),
            ["z"]
        );
        assert_eq!(
            titles(&discover(std::slice::from_ref(&fixture.0), true).unwrap()),
            ["x", "y", "z"]
        );
    }

    #[test]
    fn explicit_order_and_overlapping_directory_deduplication() {
        let fixture = Fixture::new();
        let a = fixture.file("a.mp3");
        let z = fixture.file("z.mp3");
        let alternate_z = fixture.0.join(".").join("z.mp3");
        let tracks = discover(&[z.clone(), alternate_z, fixture.0.clone(), a], true).unwrap();
        assert_eq!(titles(&tracks), ["z", "a"]);
        assert_eq!(tracks[0].path, fs::canonicalize(z).unwrap());
    }

    #[test]
    fn separate_directory_inputs_keep_input_order() {
        let fixture = Fixture::new();
        fixture.file("a/first.mp3");
        fixture.file("z/second.mp3");
        let tracks = discover(&[fixture.0.join("z"), fixture.0.join("a")], false).unwrap();
        assert_eq!(titles(&tracks), ["second", "first"]);
    }

    #[test]
    fn title_is_file_stem_not_full_name() {
        let fixture = Fixture::new();
        let path = fixture.file("艺术家.song.live.MP3");
        let tracks = discover(&[path], false).unwrap();
        assert_eq!(tracks[0].title, "艺术家.song.live");
    }

    #[test]
    fn explicit_missing_and_unsupported_files_are_errors() {
        let fixture = Fixture::new();
        assert!(discover(&[fixture.0.join("missing.mp3")], false).is_err());
        let unsupported = fixture.file("notes.txt");
        assert!(discover(&[unsupported], false).is_err());
    }

    #[test]
    fn disappeared_directory_entries_are_skipped_but_explicit_paths_fail() {
        let fixture = Fixture::new();
        let missing = fixture.0.join("removed.mp3");
        let mut scanner = Scanner {
            recursive: false,
            seen: HashSet::new(),
            playlist_stack: HashSet::new(),
            playlist_entries: 0,
            tracks: Vec::new(),
        };
        assert!(scanner.visit(&missing, false, false).is_ok());
        assert!(scanner.visit(&missing, false, true).is_err());
    }

    #[test]
    fn explicit_playlists_preserve_order_and_resolve_relative_entries() {
        let fixture = Fixture::new();
        let first = fixture.file("music/first.mp3");
        let second = fixture.file("music/second.ogg");
        let playlist = fixture.0.join("set.m3u8");
        fs::write(
            &playlist,
            format!("#EXTM3U\n{}\n{}\n", "music/second.ogg", "music/first.mp3"),
        )
        .unwrap();
        let tracks = discover(&[playlist], false).unwrap();
        assert_eq!(
            tracks.iter().map(|track| &track.path).collect::<Vec<_>>(),
            [
                &fs::canonicalize(second).unwrap(),
                &fs::canonicalize(first).unwrap(),
            ]
        );
    }

    #[test]
    fn directory_scans_ignore_playlists_but_explicit_playlist_urls_and_cycles_fail() {
        let fixture = Fixture::new();
        fixture.file("song.mp3");
        fs::write(fixture.0.join("ignored.m3u"), "song.mp3\n").unwrap();
        assert_eq!(
            discover(std::slice::from_ref(&fixture.0), true)
                .unwrap()
                .len(),
            1
        );

        let url = fixture.0.join("url.m3u");
        fs::write(&url, "https://example.test/song.mp3\n").unwrap();
        assert!(discover(&[url], false).unwrap_err().contains("URL"));

        let a = fixture.0.join("a.m3u8");
        let b = fixture.0.join("b.m3u8");
        fs::write(&a, "b.m3u8\n").unwrap();
        fs::write(&b, "a.m3u8\n").unwrap();
        assert!(discover(&[a], false).unwrap_err().contains("cycle"));
    }

    #[test]
    fn empty_directory_is_valid() {
        let fixture = Fixture::new();
        assert!(discover(std::slice::from_ref(&fixture.0), true)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn empty_paths_match_explicit_current_directory() {
        assert_eq!(
            discover(&[], false).unwrap(),
            discover(&[PathBuf::from(".")], false).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_are_not_followed_and_file_aliases_are_deduplicated() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let song = fixture.file("song.mp3");
        let alias = fixture.0.join("alias.mp3");
        symlink(&song, &alias).unwrap();
        symlink(&fixture.0, fixture.0.join("loop")).unwrap();
        symlink(fixture.0.join("missing"), fixture.0.join("broken.mp3")).unwrap();
        let tracks = discover(&[alias, fixture.0.clone()], true).unwrap();
        assert_eq!(titles(&tracks), ["alias"]);
        assert_eq!(tracks[0].path, fs::canonicalize(song).unwrap());
        assert!(discover(&[fixture.0.join("loop")], true)
            .unwrap()
            .is_empty());
        assert!(discover(&[fixture.0.join("broken.mp3")], true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_remain_usable() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let fixture = Fixture::new();
        let path = fixture.0.join(OsString::from_vec(b"song\xff.mp3".to_vec()));
        fs::write(&path, b"").unwrap();
        let tracks = discover(std::slice::from_ref(&path), false).unwrap();
        assert_eq!(tracks[0].path, fs::canonicalize(path).unwrap());
        assert_eq!(tracks[0].title, "song\u{fffd}");
    }
}
