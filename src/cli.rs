use std::{ffi::OsString, path::PathBuf};

pub const HELP: &str = "StarPlay - terminal music player

Usage: starplay [OPTIONS] [FILE_OR_DIRECTORY ...]

Without paths, scan the current directory for MP3, WAV, FLAC and OGG.
Paths containing spaces should be quoted. UTF-8 M3U/M3U8 playlists are accepted.

Options:
  -r, --recursive       Include subdirectories (do not follow directory links)
      --volume NUMBER   Initial volume, 0 to 100 (overrides saved value; default: 50)
      --no-config       Do not read or save .starplay.conf in the launch directory
      --list            Print the playlist without opening an audio device
      --check           Decode every track without playback; fail on empty audio
  -h, --help            Show this help
  -V, --version         Show the version
      --                Treat remaining arguments as paths

Controls:
  Up/Down, J/K select; Enter play; Space pause/resume; N/P next/previous
  Left/Right seek 5 seconds; +/- volume; R repeat off/all/one
  V cycle spectrum/pulse/off; Home/End first/last; Q, Esc or Ctrl+C quit
  / search (Enter apply, Esc clear); S shuffle; A add selected to upcoming queue
  Tab library/queue; Delete remove queued item; C clear queue; L lyrics
  W export current view to starplay-playlist.m3u8 (existing files are preserved)

--check checks decoder output, not strict bitstream integrity.
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Play,
    List,
    Check,
    Help,
    Version,
}

#[derive(Debug)]
pub struct Options {
    pub paths: Vec<PathBuf>,
    pub recursive: bool,
    pub volume: u8,
    pub volume_explicit: bool,
    pub no_config: bool,
    pub mode: Mode,
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Options, String> {
    let mut options = Options {
        paths: Vec::new(),
        recursive: false,
        volume: 50,
        volume_explicit: false,
        no_config: false,
        mode: Mode::Play,
    };
    let mut args = args.into_iter();
    let mut positional = false;
    while let Some(arg) = args.next() {
        if positional {
            options.paths.push(arg.into());
            continue;
        }
        match arg.to_str() {
            Some("--") => positional = true,
            Some("-r" | "--recursive") => options.recursive = true,
            Some("--no-config") => options.no_config = true,
            Some("--volume") => {
                let value = args
                    .next()
                    .ok_or("--volume requires a number from 0 to 100.")?;
                options.volume = value
                    .to_str()
                    .and_then(|s| s.parse::<u8>().ok())
                    .filter(|v| *v <= 100)
                    .ok_or("--volume must be an integer from 0 to 100.")?;
                options.volume_explicit = true;
            }
            Some(flag @ ("--list" | "--check" | "-h" | "--help" | "-V" | "--version")) => {
                let mode = match flag {
                    "--list" => Mode::List,
                    "--check" => Mode::Check,
                    "-h" | "--help" => Mode::Help,
                    _ => Mode::Version,
                };
                if options.mode != Mode::Play && options.mode != mode {
                    return Err("Use only one of --list, --check, --help or --version.".into());
                }
                options.mode = mode;
            }
            _ if arg.to_string_lossy().starts_with('-') => {
                return Err(format!(
                    "Unknown option '{}'. Use --help or put -- before a path beginning with '-'.",
                    arg.to_string_lossy()
                ));
            }
            _ => options.paths.push(arg.into()),
        }
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn opts(args: &[&str]) -> Result<Options, String> {
        parse(args.iter().map(OsString::from))
    }
    #[test]
    fn defaults() {
        let result = opts(&[]).unwrap();
        assert_eq!(result.mode, Mode::Play);
        assert_eq!(result.volume, 50);
        assert!(!result.volume_explicit);
        assert!(!result.no_config);
        assert!(!result.recursive);
        assert!(result.paths.is_empty());
    }
    #[test]
    fn parses_paths_and_flags() {
        let result = opts(&[
            "--volume",
            "75",
            "如愿-王菲.mp3",
            "-r",
            "--list",
            "music folder",
        ])
        .unwrap();
        assert_eq!(result.volume, 75);
        assert!(result.volume_explicit);
        assert!(!result.no_config);
        assert!(result.recursive);
        assert_eq!(result.mode, Mode::List);
        assert_eq!(
            result.paths,
            vec![
                PathBuf::from("如愿-王菲.mp3"),
                PathBuf::from("music folder")
            ]
        );
    }
    #[test]
    fn validates_volume() {
        for value in ["-1", "101", "256", "x", "1.5", ""] {
            assert!(opts(&["--volume", value]).is_err());
        }
        assert!(opts(&["--volume"]).is_err());
        assert_eq!(opts(&["--volume", "0"]).unwrap().volume, 0);
        assert_eq!(opts(&["--volume", "100"]).unwrap().volume, 100);
    }
    #[test]
    fn explicit_volume_including_default_value_is_distinguished() {
        for value in ["0", "50", "100"] {
            let result = opts(&["--volume", value]).unwrap();
            assert!(result.volume_explicit);
            assert_eq!(result.volume, value.parse::<u8>().unwrap());
        }
        let result = opts(&["--volume", "12", "--volume", "50"]).unwrap();
        assert!(result.volume_explicit);
        assert_eq!(result.volume, 50);
    }
    #[test]
    fn no_config_is_independent_of_mode_and_explicit_volume() {
        for (args, mode, volume, explicit) in [
            (vec!["--no-config"], Mode::Play, 50, false),
            (vec!["--no-config", "--volume", "33"], Mode::Play, 33, true),
            (vec!["--volume", "50", "--no-config"], Mode::Play, 50, true),
            (vec!["--no-config", "--list"], Mode::List, 50, false),
            (vec!["--check", "--no-config"], Mode::Check, 50, false),
            (vec!["--no-config", "--help"], Mode::Help, 50, false),
            (vec!["--version", "--no-config"], Mode::Version, 50, false),
            (vec!["--no-config", "--no-config"], Mode::Play, 50, false),
        ] {
            let result = opts(&args).unwrap();
            assert!(result.no_config, "{args:?}");
            assert_eq!(result.mode, mode, "{args:?}");
            assert_eq!(result.volume, volume, "{args:?}");
            assert_eq!(result.volume_explicit, explicit, "{args:?}");
        }
        assert!(HELP.contains("--no-config"));
        assert!(HELP.contains(".starplay.conf"));
    }
    #[test]
    fn configuration_flags_after_separator_are_paths() {
        let result = opts(&["--", "--no-config", "--volume", "25"]).unwrap();
        assert!(!result.no_config);
        assert!(!result.volume_explicit);
        assert_eq!(result.volume, 50);
        assert_eq!(
            result.paths,
            vec![
                PathBuf::from("--no-config"),
                PathBuf::from("--volume"),
                PathBuf::from("25")
            ]
        );
    }
    #[test]
    fn validates_modes() {
        for args in [["--list", "--check"], ["--help", "--version"]] {
            assert!(opts(&args).is_err());
        }
        for (flag, expected) in [
            ("--check", Mode::Check),
            ("-h", Mode::Help),
            ("--help", Mode::Help),
            ("-V", Mode::Version),
        ] {
            assert_eq!(opts(&[flag]).unwrap().mode, expected);
        }
        assert!(opts(&["--unknown"]).is_err());
    }
    #[test]
    fn separator_preserves_literal_paths() {
        let result = opts(&["--", "--list", "-song.mp3"]).unwrap();
        assert_eq!(result.mode, Mode::Play);
        assert_eq!(
            result.paths,
            vec![PathBuf::from("--list"), PathBuf::from("-song.mp3")]
        );
    }
    #[cfg(unix)]
    #[test]
    fn accepts_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(b"music\xff.mp3".to_vec());
        assert_eq!(
            parse([path.clone()]).unwrap().paths,
            vec![PathBuf::from(path)]
        );
    }
}
