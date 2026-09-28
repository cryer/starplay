mod audio_visual;
mod browser;
mod cli;
mod config;
mod input;
mod library;
mod media;
mod playback;
mod player;
mod playlist;
mod screen;
mod ui;
mod visualizer;

use cli::Mode;
use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
};

fn run() -> Result<(), String> {
    let options = cli::parse(std::env::args_os().skip(1))?;
    match options.mode {
        Mode::Help => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        Mode::Version => {
            println!("StarPlay {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }
    let tracks = library::discover(&options.paths, options.recursive)?;
    if tracks.is_empty() {
        return Err("No supported audio files found (MP3, WAV, FLAC, OGG).".into());
    }
    match options.mode {
        Mode::List => {
            let mut out = io::BufWriter::new(io::stdout().lock());
            for (i, track) in tracks.iter().enumerate() {
                writeln!(
                    out,
                    "{}. {}",
                    i + 1,
                    ui::safe_text(&track.path.display().to_string())
                )
                .map_err(|e| e.to_string())?;
            }
            out.flush().map_err(|e| e.to_string())
        }
        Mode::Check => {
            let mut failures = 0;
            for track in &tracks {
                match player::check_audio(&track.path) {
                    Ok(info) => println!(
                        "OK  {} | {:.2}s | {} Hz | {} channels | {} samples",
                        ui::safe_text(&track.path.display().to_string()),
                        info.duration.as_secs_f64(),
                        info.sample_rate,
                        info.channels,
                        info.samples
                    ),
                    Err(error) => {
                        failures += 1;
                        eprintln!(
                            "FAIL {}: {}",
                            ui::safe_text(&track.path.display().to_string()),
                            error
                        );
                    }
                }
            }
            if failures == 0 {
                Ok(())
            } else {
                Err(format!("{failures} audio file(s) failed validation."))
            }
        }
        Mode::Play => {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                return Err("Interactive playback requires a terminal. Use --list or --check for non-interactive operation.".into());
            }
            let path = std::path::Path::new(config::FILE_NAME);
            let mut save_config = !options.no_config;
            let mut warning = String::new();
            let mut settings = if options.no_config {
                config::Settings::default()
            } else {
                match config::load(path) {
                    Ok(settings) => settings,
                    Err(error) => {
                        // Never replace a broken/unreadable user configuration with defaults.
                        save_config = false;
                        warning = format!("Config ignored (not overwritten): {error}");
                        config::Settings::default()
                    }
                }
            };
            if options.volume_explicit {
                settings.volume = options.volume;
            }
            let result = ui::run(tracks, &mut settings, &warning).map_err(|e| e.to_string());
            if save_config {
                // Persist the final settings even when the UI errored; the UI error wins.
                if let Err(save_error) = config::save(path, &settings) {
                    result?;
                    return Err(save_error);
                }
            }
            result
        }
        _ => unreachable!(),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("StarPlay: {}", ui::safe_text(&error));
            ExitCode::FAILURE
        }
    }
}
