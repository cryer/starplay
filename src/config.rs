//! Versioned settings stored only at the path chosen by the caller (normally the launch directory).
use crate::{player::Repeat, playlist::{read_bounded_utf8, ReadError}, visualizer::VisualMode};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub const FILE_NAME: &str = ".starplay.conf";
pub const DEFAULT_VOLUME: u8 = 50;
const MAX_FILE_SIZE: u64 = 16 * 1024;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn parse_volume(value: &str) -> Option<u8> {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| value.parse::<u8>().ok())
        .flatten()
        .filter(|volume| *volume <= 100)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub volume: u8,
    pub repeat: Repeat,
    pub visual_mode: VisualMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: DEFAULT_VOLUME,
            repeat: Repeat::Off,
            visual_mode: VisualMode::Spectrum,
        }
    }
}

fn config_error(path: &Path, message: impl std::fmt::Display) -> String {
    format!("Configuration '{}': {message}", path.display())
}

// symlink_metadata deliberately checks the directory entry, not a symlink's target.
fn metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(config_error(path, "symbolic links are not allowed"))
        }
        Ok(meta) if !meta.is_file() => Err(config_error(path, "expected a regular file")),
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(config_error(path, format!("cannot inspect file: {error}"))),
    }
}

/// Missing files use defaults. Existing files must contain exactly one `version=1`;
/// other settings are optional. Empty/comment-only files are invalid, not reset silently.
pub fn load(path: &Path) -> Result<Settings, String> {
    let Some(meta) = metadata(path)? else {
        return Ok(Settings::default());
    };
    if meta.len() > MAX_FILE_SIZE {
        return Err(config_error(path, "file exceeds the 16 KiB size limit"));
    }
    // Bound the actual read as well: the file may have grown since the metadata check.
    let text = read_bounded_utf8(path, MAX_FILE_SIZE).map_err(|error| {
        config_error(
            path,
            match error {
                ReadError::Open(error) => format!("cannot open file: {error}"),
                ReadError::Read(error) => format!("cannot read file: {error}"),
                ReadError::TooLarge => "file exceeds the 16 KiB size limit".to_string(),
                ReadError::Utf8 => "file must contain valid UTF-8 text".to_string(),
            },
        )
    })?;
    parse(&text).map_err(|error| config_error(path, error))
}

fn parse(text: &str) -> Result<Settings, String> {
    let mut settings = Settings::default();
    let mut seen = 0u8;
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {line_number}: expected key=value"))?;
        let (key, value) = (key.trim(), value.trim());
        let bit = match key {
            "version" => 1,
            "volume" => 2,
            "repeat" => 4,
            "visual" => 8,
            _ => return Err(format!("line {line_number}: unknown setting '{key}'")),
        };
        if seen & bit != 0 {
            return Err(format!("line {line_number}: duplicate setting '{key}'"));
        }
        seen |= bit;
        match key {
            "version" if value != "1" => {
                return Err(format!(
                    "line {line_number}: unsupported version '{value}' (expected 1)"
                ));
            }
            "version" => {}
            "volume" => {
                settings.volume = parse_volume(value).ok_or_else(|| {
                    format!("line {line_number}: volume must be an integer from 0 to 100")
                })?;
            }
            "repeat" => {
                settings.repeat = [Repeat::Off, Repeat::All, Repeat::One]
                    .into_iter()
                    .find(|repeat| repeat.label() == value)
                    .ok_or_else(|| {
                        format!("line {line_number}: repeat must be off, all or one")
                    })?;
            }
            "visual" => {
                settings.visual_mode = [VisualMode::Spectrum, VisualMode::Pulse, VisualMode::Off]
                    .into_iter()
                    .find(|mode| mode.as_str() == value)
                    // Migrate retired effects without discarding other preferences.
                    .or(match value {
                        "waveform" | "stereo" | "field" => Some(VisualMode::Spectrum),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        format!("line {line_number}: visual must be spectrum, pulse or off")
                    })?;
            }
            _ => unreachable!(),
        }
    }
    if seen & 1 == 0 {
        return Err("missing required version=1".into());
    }
    Ok(settings)
}

fn serialize(settings: &Settings) -> Result<String, String> {
    if settings.volume > 100 {
        return Err("volume must be an integer from 0 to 100".into());
    }
    Ok(format!(
        "version=1\nvolume={}\nrepeat={}\nvisual={}\n",
        settings.volume,
        settings.repeat.label(),
        settings.visual_mode.as_str()
    ))
}

struct TemporaryFile {
    path: PathBuf,
    file: Option<File>,
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        // Windows cannot remove an open file on all filesystems.
        self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}

fn temporary_file(path: &Path) -> Result<TemporaryFile, String> {
    let name = path
        .file_name()
        .ok_or_else(|| config_error(path, "expected a file name"))?;
    for _ in 0..128 {
        let mut temp_name = name.to_os_string();
        temp_name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let temp_path = path.with_file_name(temp_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => {
                return Ok(TemporaryFile {
                    path: temp_path,
                    file: Some(file),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(config_error(
                    path,
                    format!("cannot create same-directory temporary file: {error}"),
                ));
            }
        }
    }
    Err(config_error(
        path,
        "cannot allocate a unique temporary file",
    ))
}

fn check_destination(path: &Path) -> Result<(), String> {
    if let Some(meta) = metadata(path)? {
        if meta.permissions().readonly() {
            return Err(config_error(
                path,
                "file is read-only; refusing to replace it",
            ));
        }
        // Never replace a malformed or newer-version configuration, including one
        // changed externally after the application loaded its initial settings.
        load(path)?;
    }
    Ok(())
}

/// Replace atomically using a uniquely and exclusively created sibling temporary file.
/// No destination is removed first; on replacement failure the old file stays intact.
pub fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    let text = serialize(settings).map_err(|error| config_error(path, error))?;
    check_destination(path)?;
    let mut temporary = temporary_file(path)?;
    let file = temporary.file.as_mut().expect("temporary file is open");
    file.write_all(text.as_bytes())
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .map_err(|error| config_error(path, format!("cannot write temporary file: {error}")))?;
    temporary.file.take();
    check_destination(path)?;
    fs::rename(&temporary.path, path)
        .map_err(|error| config_error(path, format!("cannot replace file: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("config-tests");
            fs::create_dir_all(&root).unwrap();
            let path = root.join(format!(
                "{}-{}",
                std::process::id(),
                NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn config(&self) -> PathBuf {
            self.0.join(FILE_NAME)
        }

        fn write(&self, text: impl AsRef<[u8]>) {
            fs::write(self.config(), text).unwrap();
        }

        fn assert_only_config(&self) {
            let entries: Vec<_> = fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(entries, vec![std::ffi::OsString::from(FILE_NAME)]);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // A failed read-only test must not leave files behind on Windows.
            #[cfg(windows)]
            if let Ok(meta) = fs::symlink_metadata(self.config()) {
                if meta.is_file() && !meta.file_type().is_symlink() {
                    let mut permissions = meta.permissions();
                    // Windows-only fixture cleanup: clear the DOS read-only attribute.
                    #[allow(clippy::permissions_set_readonly_false)]
                    permissions.set_readonly(false);
                    let _ = fs::set_permissions(self.config(), permissions);
                }
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn defaults_and_missing_file_do_not_create_anything() {
        let fixture = Fixture::new();
        let expected = Settings {
            volume: 50,
            repeat: Repeat::Off,
            visual_mode: VisualMode::Spectrum,
        };
        assert_eq!(Settings::default(), expected);
        assert_eq!(load(&fixture.config()).unwrap(), expected);
        assert!(!fixture.config().exists());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
        assert_eq!(
            load(&fixture.0.join("absent-parent").join(FILE_NAME)).unwrap(),
            expected
        );
        assert!(!fixture.0.join("absent-parent").exists());
    }

    #[test]
    fn roundtrip_every_repeat_visual_mode_and_volume_boundary() {
        let fixture = Fixture::new();
        for repeat in [Repeat::Off, Repeat::All, Repeat::One] {
            for visual_mode in [VisualMode::Spectrum, VisualMode::Pulse, VisualMode::Off] {
                for volume in [0, 50, 100] {
                    let settings = Settings {
                        volume,
                        repeat,
                        visual_mode,
                    };
                    save(&fixture.config(), &settings).unwrap();
                    assert_eq!(load(&fixture.config()).unwrap(), settings);
                    fixture.assert_only_config();
                }
            }
        }
    }

    #[test]
    fn retired_effects_migrate_without_losing_preferences() {
        let fixture = Fixture::new();
        for mode in ["waveform", "stereo", "field"] {
            fixture.write(format!("version=1\nvolume=73\nrepeat=one\nvisual={mode}\n"));
            let settings = load(&fixture.config()).unwrap();
            assert_eq!(settings.visual_mode, VisualMode::Spectrum);
            assert_eq!(settings.volume, 73);
            assert_eq!(settings.repeat, Repeat::One);
            save(&fixture.config(), &settings).unwrap();
            assert!(fs::read_to_string(fixture.config())
                .unwrap()
                .contains("visual=spectrum"));
        }
    }

    #[test]
    fn serialized_format_is_stable_and_lowercase() {
        assert_eq!(
            serialize(&Settings::default()).unwrap(),
            "version=1\nvolume=50\nrepeat=off\nvisual=spectrum\n"
        );
        assert_eq!(
            serialize(&Settings {
                volume: 73,
                repeat: Repeat::One,
                visual_mode: VisualMode::Pulse,
            })
            .unwrap(),
            "version=1\nvolume=73\nrepeat=one\nvisual=pulse\n"
        );
    }

    #[test]
    fn partial_settings_use_defaults_and_allow_comments_whitespace_crlf() {
        let fixture = Fixture::new();
        for (text, expected) in [
            ("version=1\n", Settings::default()),
            (
                "# local preferences\r\n\r\n version = 1 # current schema\r\n volume = 9 \r\n",
                Settings {
                    volume: 9,
                    ..Settings::default()
                },
            ),
            (
                "repeat=all\n# version may appear last\nversion=1",
                Settings {
                    repeat: Repeat::All,
                    ..Settings::default()
                },
            ),
            (
                "version=1\nvisual=off\n",
                Settings {
                    visual_mode: VisualMode::Off,
                    ..Settings::default()
                },
            ),
        ] {
            fixture.write(text);
            assert_eq!(load(&fixture.config()).unwrap(), expected, "{text:?}");
        }
    }

    #[test]
    fn empty_comment_only_and_unversioned_files_are_not_silently_reset() {
        let fixture = Fixture::new();
        for text in ["", " \r\n\t\n", "# configuration\n", "volume=50\n"] {
            fixture.write(text);
            let error = load(&fixture.config()).unwrap_err();
            assert!(error.contains("missing required version=1"), "{error}");
            assert!(save(&fixture.config(), &Settings::default()).is_err());
            assert_eq!(fs::read_to_string(fixture.config()).unwrap(), text);
            fixture.assert_only_config();
        }
    }

    #[test]
    fn invalid_values_unknown_keys_duplicate_keys_and_versions_are_rejected() {
        let fixture = Fixture::new();
        for (text, expected) in [
            ("version=0", "unsupported version"),
            ("version=2", "unsupported version"),
            ("version=01", "unsupported version"),
            ("version=", "unsupported version"),
            ("version=1.0", "unsupported version"),
            ("version=1\nversion=1", "duplicate setting 'version'"),
            (
                "version=1\nvolume=5\nvolume=6",
                "duplicate setting 'volume'",
            ),
            (
                "version=1\nrepeat=off\nrepeat=off",
                "duplicate setting 'repeat'",
            ),
            (
                "version=1\nvisual=off\nvisual=pulse",
                "duplicate setting 'visual'",
            ),
            ("version=1\nvolume=-1", "volume must be"),
            ("version=1\nvolume=+1", "volume must be"),
            ("version=1\nvolume=101", "volume must be"),
            ("version=1\nvolume=256", "volume must be"),
            ("version=1\nvolume=2.5", "volume must be"),
            ("version=1\nvolume=NaN", "volume must be"),
            ("version=1\nvolume=", "volume must be"),
            ("version=1\nvolume=1 0", "volume must be"),
            ("version=1\nrepeat=OFF", "repeat must be"),
            ("version=1\nrepeat=none", "repeat must be"),
            ("version=1\nrepeat=", "repeat must be"),
            ("version=1\nvisual=SPECTRUM", "visual must be"),
            ("version=1\nvisual=bars", "visual must be"),
            ("version=1\nvisual=", "visual must be"),
            ("version=1\nshuffle=true", "unknown setting 'shuffle'"),
            ("version=1\nVolume=20", "unknown setting 'Volume'"),
            ("version=1\n=oops", "unknown setting ''"),
            ("version=1\nvolume", "expected key=value"),
            ("version=1\nvolume=5=6", "volume must be"),
        ] {
            fixture.write(text);
            let error = load(&fixture.config()).unwrap_err();
            assert!(error.contains(expected), "{text:?}: {error}");
            assert!(error.contains("line "), "{error}");
            assert!(error.contains(FILE_NAME), "{error}");
            assert!(save(&fixture.config(), &Settings::default()).is_err());
            assert_eq!(fs::read_to_string(fixture.config()).unwrap(), text);
            fixture.assert_only_config();
        }
    }

    #[test]
    fn invalid_utf8_and_oversized_files_are_preserved() {
        let fixture = Fixture::new();
        for (bytes, expected) in [
            (b"version=1\n# \xff".to_vec(), "UTF-8"),
            (vec![b' '; MAX_FILE_SIZE as usize + 1], "16 KiB"),
        ] {
            fixture.write(&bytes);
            assert!(load(&fixture.config()).unwrap_err().contains(expected));
            assert!(save(&fixture.config(), &Settings::default()).is_err());
            assert_eq!(fs::read(fixture.config()).unwrap(), bytes);
            fixture.assert_only_config();
        }
        let mut bytes = b"version=1\n#".to_vec();
        bytes.resize(MAX_FILE_SIZE as usize, b' ');
        fixture.write(bytes);
        assert_eq!(load(&fixture.config()).unwrap(), Settings::default());
    }

    #[test]
    fn replacement_overwrites_existing_file_without_leaving_temporary_files() {
        let fixture = Fixture::new();
        fixture.write("# old contents\nversion=1\nvolume=1\n");
        let updated = Settings {
            volume: 92,
            repeat: Repeat::All,
            visual_mode: VisualMode::Pulse,
        };
        // On Windows this verifies that std::fs::rename can replace an existing file.
        save(&fixture.config(), &updated).unwrap();
        assert_eq!(load(&fixture.config()).unwrap(), updated);
        assert_eq!(
            fs::read_to_string(fixture.config()).unwrap(),
            serialize(&updated).unwrap()
        );
        fixture.assert_only_config();
    }

    #[test]
    fn invalid_settings_are_not_written_or_used_to_replace_existing_file() {
        let fixture = Fixture::new();
        let invalid = Settings {
            volume: 101,
            ..Settings::default()
        };
        assert!(save(&fixture.config(), &invalid).is_err());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
        let original = "version=1\nvolume=25\n";
        fixture.write(original);
        assert!(save(&fixture.config(), &invalid).is_err());
        assert_eq!(fs::read_to_string(fixture.config()).unwrap(), original);
        fixture.assert_only_config();
    }

    #[test]
    fn readonly_existing_file_is_preserved() {
        let fixture = Fixture::new();
        let original = "version=1\nvolume=25\n";
        fixture.write(original);
        let permissions = fs::metadata(fixture.config()).unwrap().permissions();
        let mut readonly = permissions.clone();
        readonly.set_readonly(true);
        fs::set_permissions(fixture.config(), readonly).unwrap();
        let result = save(&fixture.config(), &Settings::default());
        fs::set_permissions(fixture.config(), permissions).unwrap();
        let error = result.unwrap_err();
        assert!(error.contains("read-only"), "{error}");
        assert_eq!(fs::read_to_string(fixture.config()).unwrap(), original);
        fixture.assert_only_config();
    }

    #[test]
    fn directory_destination_is_preserved() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.config()).unwrap();
        let preserved = fixture.config().join("keep.txt");
        fs::write(&preserved, b"must not be removed").unwrap();
        assert!(load(&fixture.config())
            .unwrap_err()
            .contains("regular file"));
        assert!(save(&fixture.config(), &Settings::default())
            .unwrap_err()
            .contains("regular file"));
        assert_eq!(fs::read(preserved).unwrap(), b"must not be removed");
        fixture.assert_only_config();
    }

    #[test]
    fn missing_parent_is_not_created_on_save_failure() {
        let fixture = Fixture::new();
        let parent = fixture.0.join("missing");
        let error = save(&parent.join(FILE_NAME), &Settings::default()).unwrap_err();
        assert!(
            error.contains("cannot create same-directory temporary file"),
            "{error}"
        );
        assert!(!parent.exists());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    }

    #[test]
    fn sibling_temporary_files_are_unique_exclusive_and_cleaned_on_drop() {
        let fixture = Fixture::new();
        let mut first = temporary_file(&fixture.config()).unwrap();
        let second = temporary_file(&fixture.config()).unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(first.path.parent(), Some(fixture.0.as_path()));
        assert_eq!(second.path.parent(), Some(fixture.0.as_path()));
        assert!(first
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&format!(".{}.", std::process::id())));
        first
            .file
            .as_mut()
            .unwrap()
            .write_all(b"preserved")
            .unwrap();
        let error = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&first.path)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&first.path).unwrap(), b"preserved");
        let first_path = first.path.clone();
        let second_path = second.path.clone();
        drop(first);
        drop(second);
        assert!(!first_path.exists());
        assert!(!second_path.exists());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn windows_failed_rename_preserves_original_and_cleans_temporary_file() {
        use std::os::windows::fs::OpenOptionsExt;

        let fixture = Fixture::new();
        let original = "version=1\nvolume=12\n";
        fixture.write(original);
        // Allow reads and writes but not deletion/rename while this handle is open.
        let handle = OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .open(fixture.config())
            .unwrap();
        let error = save(&fixture.config(), &Settings::default()).unwrap_err();
        assert!(error.contains("cannot replace file"), "{error}");
        assert_eq!(fs::read_to_string(fixture.config()).unwrap(), original);
        fixture.assert_only_config();
        drop(handle);
        save(&fixture.config(), &Settings::default()).unwrap();
        assert_eq!(load(&fixture.config()).unwrap(), Settings::default());
        fixture.assert_only_config();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn symlink_and_dangling_symlink_are_not_followed_or_overwritten() {
        let fixture = Fixture::new();
        let target = fixture.0.join("original.conf");
        let original = "version=1\nvolume=37\n";
        fs::write(&target, original).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, fixture.config()).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_file(&target, fixture.config()) {
            // Windows requires Developer Mode or the create-symlink privilege.
            if error.raw_os_error() == Some(1314) {
                eprintln!("symlink test skipped: Windows symlink privilege unavailable");
                return;
            }
            panic!("cannot create fixture symlink: {error}");
        }
        for dangling in [false, true] {
            if dangling {
                fs::remove_file(&target).unwrap();
            }
            assert!(load(&fixture.config())
                .unwrap_err()
                .contains("symbolic links"));
            assert!(save(&fixture.config(), &Settings::default())
                .unwrap_err()
                .contains("symbolic links"));
            assert!(fs::symlink_metadata(fixture.config())
                .unwrap()
                .file_type()
                .is_symlink());
            if !dangling {
                assert_eq!(fs::read_to_string(&target).unwrap(), original);
            } else {
                assert!(!target.exists());
            }
            assert_eq!(
                fs::read_dir(&fixture.0).unwrap().count(),
                if dangling { 1 } else { 2 }
            );
        }
    }
}
