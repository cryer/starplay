use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("cli-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_starplay"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }
    fn wav(&self, name: &str) {
        let samples = 800u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + samples * 2).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8000u32.to_le_bytes());
        bytes.extend_from_slice(&16000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(samples * 2).to_le_bytes());
        bytes.resize(44 + samples as usize * 2, 0);
        fs::write(self.0.join(name), bytes).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn help_and_version_work_without_music_or_audio_device() {
    let fixture = Fixture::new();
    let help = fixture.run(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: starplay"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("--no-config"));
    assert!(String::from_utf8_lossy(&help.stdout).contains(".starplay.conf"));
    assert!(String::from_utf8_lossy(&help.stdout).contains("V cycle spectrum/pulse/off"));
    let version = fixture.run(&["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn lists_and_checks_unicode_wav_without_a_terminal() {
    let fixture = Fixture::new();
    fixture.wav("测试音乐.wav");
    let list = fixture.run(&["--list"]);
    assert!(list.status.success(), "{:?}", list);
    assert!(String::from_utf8_lossy(&list.stdout).contains("测试音乐.wav"));
    let check = fixture.run(&["--check"]);
    assert!(check.status.success(), "{:?}", check);
    let text = String::from_utf8_lossy(&check.stdout);
    assert!(text.contains("8000 Hz"));
    assert!(text.contains("800 samples"));
    assert!(text.contains("0.10s"));
}

#[test]
fn corrupt_audio_returns_nonzero_but_checks_other_files() {
    let fixture = Fixture::new();
    fixture.wav("valid.wav");
    fs::write(fixture.0.join("broken.mp3"), b"not an audio file").unwrap();
    let result = fixture.run(&["--check"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("OK"));
    assert!(String::from_utf8_lossy(&result.stderr).contains("FAIL"));
    assert!(String::from_utf8_lossy(&result.stderr).contains("1 audio file(s)"));
}

#[test]
fn empty_directory_missing_path_and_invalid_options_fail() {
    let fixture = Fixture::new();
    for args in [
        &["--list"][..],
        &["--list", "missing.wav"],
        &["--volume", "101"],
        &["--list", "--check"],
    ] {
        assert!(!fixture.run(args).status.success(), "{args:?}");
    }
}

#[test]
fn redirected_playback_fails_cleanly() {
    let fixture = Fixture::new();
    fixture.wav("song.wav");
    let result = fixture.run(&[]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("requires a terminal"));
}

#[test]
fn noninteractive_modes_do_not_create_configuration_or_temporary_files() {
    let fixture = Fixture::new();
    fixture.wav("song.wav");
    for mode in ["--list", "--check", "--help", "--version"] {
        for no_config in [false, true] {
            let args = if no_config {
                vec![mode, "--no-config", "--volume", "67"]
            } else {
                vec![mode, "--volume", "67"]
            };
            let output = fixture.run(&args);
            assert!(output.status.success(), "{args:?}: {output:?}");
            assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
            assert!(!fixture.0.join(".starplay.conf").exists(), "{args:?}");
            assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1, "{args:?}");
        }
    }
}

#[test]
fn noninteractive_modes_ignore_and_preserve_existing_configuration() {
    let fixture = Fixture::new();
    fixture.wav("song.wav");
    let config = fixture.0.join(".starplay.conf");
    for contents in [
        b"version=1\nvolume=84\nrepeat=one\nvisual=off\n".as_slice(),
        b"version=999\nvolume=broken\nunknown=setting\n\xff".as_slice(),
        b"".as_slice(),
    ] {
        fs::write(&config, contents).unwrap();
        for mode in ["--list", "--check", "--help", "--version"] {
            for no_config in [false, true] {
                let args = if no_config {
                    vec![mode, "--no-config", "--volume", "50"]
                } else {
                    vec![mode]
                };
                let output = fixture.run(&args);
                assert!(output.status.success(), "{args:?}: {output:?}");
                // An attempted load of the bad file would emit a warning or fail.
                assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
                assert_eq!(fs::read(&config).unwrap(), contents, "{args:?}");
                assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2, "{args:?}");
            }
        }
    }
}

#[test]
fn redirected_playback_checks_terminal_before_reading_or_writing_configuration() {
    let fixture = Fixture::new();
    fixture.wav("song.wav");
    let config = fixture.0.join(".starplay.conf");
    for contents in [None, Some(b"version=999\nvolume=bad\n".as_slice())] {
        if let Some(bytes) = contents {
            fs::write(&config, bytes).unwrap();
        }
        for args in [&[][..], &["--no-config"][..], &["--volume", "72"][..]] {
            let output = fixture.run(args);
            assert!(!output.status.success(), "{args:?}: {output:?}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains("requires a terminal"), "{error}");
            assert!(!error.to_lowercase().contains("configuration"), "{error}");
            assert!(!error.to_lowercase().contains("warning"), "{error}");
            assert!(!error.contains(".starplay.conf"), "{error}");
            match contents {
                Some(bytes) => {
                    assert_eq!(fs::read(&config).unwrap(), bytes);
                    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 2);
                }
                None => {
                    assert!(!config.exists());
                    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
                }
            }
        }
    }
}

#[test]
fn recursive_listing_is_opt_in() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("nested")).unwrap();
    fixture.wav("nested/song.wav");
    assert!(!fixture.run(&["--list"]).status.success());
    assert!(fixture.run(&["--list", "--recursive"]).status.success());
}

#[test]
fn playlists_resolve_relative_paths_preserve_repeats_and_decode() {
    let fixture = Fixture::new();
    fixture.wav("first.wav");
    fixture.wav("第二.wav");
    fs::write(
        fixture.0.join("set.m3u8"),
        "\u{feff}#EXTM3U\n第二.wav\nfirst.wav\n第二.wav\n",
    )
    .unwrap();
    let list = fixture.run(&["--list", "set.m3u8"]);
    assert!(list.status.success(), "{list:?}");
    let text = String::from_utf8_lossy(&list.stdout);
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("第二.wav"));
    assert!(lines[1].contains("first.wav"));
    assert!(lines[2].contains("第二.wav"));
    let check = fixture.run(&["--check", "set.m3u8"]);
    assert!(check.status.success(), "{check:?}");
    fs::write(fixture.0.join("cycle.m3u"), "cycle.m3u\n").unwrap();
    assert!(!fixture.run(&["--list", "cycle.m3u"]).status.success());
    fs::write(fixture.0.join("url.m3u"), "https://example.com/song.mp3\n").unwrap();
    assert!(!fixture.run(&["--list", "url.m3u"]).status.success());
}
