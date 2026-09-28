use crate::{
    browser::Browser,
    config::Settings,
    input::{Action, Input},
    library::Track,
    playback::{Command, Playback, PlaybackState},
    screen::Screen,
    visualizer::{VisualCanvas, VisualMode, Visualizer},
};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableFocusChange, EnableFocusChange, Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    style::{Color, ResetColor},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub use crate::screen::safe_text;

fn time_label(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn restore() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableFocusChange,
        ResetColor,
        Show,
        LeaveAlternateScreen
    );
}

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, Hide, EnableFocusChange)?;
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

fn effect_height(height: u16, mode: VisualMode) -> u16 {
    if height < 14 || mode == VisualMode::Off {
        0
    } else {
        (height - 12).min(6)
    }
}

fn effects_visible(width: u16, height: u16, mode: VisualMode) -> bool {
    width >= 32 && effect_height(height, mode) > 0
}

fn frame_interval(visible: bool, active: bool, animating: bool) -> Duration {
    if visible && (active || animating) {
        Duration::from_millis(33)
    } else {
        // Idle: the progress clock has one-second resolution and screen diffing
        // already suppresses redundant writes.
        Duration::from_secs(1)
    }
}

/// Compact terminals retain a bar and remaining time; wide terminals also show elapsed/total.
fn progress_line(position: Duration, duration: Option<Duration>, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let known = duration.filter(|d| !d.is_zero());
    let (fraction, percent, remaining) = match known {
        Some(total) => {
            let fraction = (position.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0);
            (
                fraction,
                format!("{:3}%", (fraction * 100.0).floor() as u8),
                time_label(total.saturating_sub(position)),
            )
        }
        None => (0.0, " --%".into(), "--:--".into()),
    };
    let suffix = format!(" {percent} -{remaining}");
    let prefix = if width >= 48 {
        format!(
            "{} / {} ",
            time_label(known.map_or(position, |total| position.min(total))),
            known.map(time_label).unwrap_or_else(|| "--:--".into())
        )
    } else {
        String::new()
    };
    let bar_width = width.saturating_sub(prefix.len() + suffix.len() + 2);
    if bar_width == 0 {
        return format!("{prefix}{suffix}").chars().take(width).collect();
    }
    let filled = (fraction * bar_width as f64).floor() as usize;
    let bar = if known.is_some() {
        format!("{}{}", "=".repeat(filled), "-".repeat(bar_width - filled))
    } else {
        "?".repeat(bar_width)
    };
    format!("{prefix}[{bar}]{suffix}")
}

fn draw_effect(
    screen: &mut Screen,
    visual: &Visualizer,
    canvas: &mut VisualCanvas,
    width: u16,
    height: u16,
) {
    screen.line(
        3,
        &format!("{} | V: switch/off", visual.mode.label()),
        Color::DarkCyan,
    );
    visual.render_into(
        usize::from(width.saturating_sub(1)),
        usize::from(height),
        canvas,
    );
    for (i, text) in canvas.rows.iter().enumerate() {
        let color = match visual.mode {
            VisualMode::Pulse => {
                let distance = (2 * i).abs_diff(canvas.rows.len() - 1);
                if distance * 3 < canvas.rows.len() {
                    Color::Yellow
                } else if distance * 3 < canvas.rows.len() * 2 {
                    Color::Magenta
                } else {
                    Color::Blue
                }
            }
            VisualMode::Spectrum => {
                // Six distinct color bands, from a cool base to warm tips.
                let palette = [
                    Color::Yellow,
                    Color::Magenta,
                    Color::DarkMagenta,
                    Color::Cyan,
                    Color::Blue,
                    Color::DarkBlue,
                ];
                let band = i * (palette.len() - 1) / (canvas.rows.len() - 1).max(1);
                palette[band]
            }
            VisualMode::Off => Color::Blue,
        };
        screen.line(4 + i as u16, text, color);
    }
}

struct View<'a> {
    tracks: &'a [Track],
    state: &'a PlaybackState,
    selected: usize,
    notice: &'a str,
    browser: Option<&'a Browser>,
    lyric: Option<&'a str>,
}

fn draw(
    screen: &mut Screen,
    view: &View<'_>,
    indices: Option<&[usize]>,
    visual: &Visualizer,
    canvas: &mut VisualCanvas,
    size: (u16, u16),
) {
    screen.begin(size);
    let (width, height) = size;
    if width < 32 || height < 10 {
        screen.line(0, "Resize terminal (32x10). Q quits.", Color::Yellow);
        return;
    }
    let player = view.state;
    let title = player
        .current
        .and_then(|i| view.tracks.get(i))
        .map(|t| t.title.as_str())
        .unwrap_or("No track");
    let state = if player.finished {
        "STOPPED"
    } else if player.paused {
        "PAUSED"
    } else {
        "PLAYING"
    };
    let header = if width >= 64 {
        format!(
            "StarPlay | Volume: {}% | repeat: {} | {} tracks",
            player.volume,
            player.repeat.label(),
            view.tracks.len()
        )
    } else {
        format!(
            "StarPlay V{}% R:{} #{}",
            player.volume,
            player.repeat.label(),
            view.tracks.len()
        )
    };
    screen.line(0, &header, Color::Cyan);
    screen.line(1, &format!("{state} | {title}"), Color::White);
    screen.line(
        2,
        &progress_line(player.position, player.duration, usize::from(width - 1)),
        Color::Green,
    );
    let effect_rows = effect_height(height, visual.mode);
    let heading = if effect_rows > 0 {
        draw_effect(screen, visual, canvas, width, effect_rows);
        4 + effect_rows
    } else {
        3
    };
    let fallback: Vec<usize>;
    let indices: &[usize] = match indices {
        Some(indices) => indices,
        None => {
            fallback = (0..view.tracks.len()).collect();
            &fallback
        }
    };
    let heading_text = if let Some(b) = view.browser {
        format!(
            "{} [{}] S:{} /{}{}",
            if b.queue { "Queue" } else { "Playlist" },
            indices.len(),
            if player.shuffle { "on" } else { "off" },
            b.query,
            if b.editing { "_" } else { "" }
        )
    } else {
        "Playlist (Enter: play selected)".into()
    };
    screen.line(heading, &heading_text, Color::DarkGrey);
    let playlist_start = heading + 1;
    let visible = usize::from(height.saturating_sub(playlist_start + 4));
    let start = view
        .selected
        .saturating_sub(visible / 2)
        .min(indices.len().saturating_sub(visible));
    for offset in 0..visible {
        let row_index = start + offset;
        if let Some(&index) = indices.get(row_index) {
            let track = &view.tracks[index];
            screen.line(
                playlist_start + offset as u16,
                &format!(
                    "{} {} {:>3}. {}",
                    if row_index == view.selected { ">" } else { " " },
                    if player.current == Some(index) {
                        "*"
                    } else {
                        " "
                    },
                    index + 1,
                    track.title
                ),
                if row_index == view.selected {
                    Color::Cyan
                } else {
                    Color::White
                },
            );
        }
    }
    screen.line(
        height - 4,
        if view.notice.is_empty() {
            view.lyric.unwrap_or("")
        } else {
            view.notice
        },
        Color::Yellow,
    );
    if width >= 64 {
        screen.line(
            height - 3,
            "Up/Down Enter Space | / search S shuffle A enqueue",
            Color::DarkGrey,
        );
        screen.line(
            height - 2,
            "N/P skip L/R seek +/- volume | Tab queue Del remove",
            Color::DarkGrey,
        );
        screen.line(
            height - 1,
            "V FX R repeat L lyrics C clear W save Q quit",
            Color::DarkGrey,
        );
    } else {
        screen.line(
            height - 3,
            "Up/Dn select Enter play Sp pause",
            Color::DarkGrey,
        );
        screen.line(height - 2, "N/P skip L/R seek +/- volume", Color::DarkGrey);
        screen.line(
            height - 1,
            "/ search Tab queue S shuffle Q",
            Color::DarkGrey,
        );
    }
}

fn select(action: Action, selected: usize, count: usize) -> Option<usize> {
    let last = count.saturating_sub(1);
    match action {
        Action::SelectUp => Some(selected.saturating_sub(1).min(last)),
        Action::SelectDown => Some(selected.saturating_add(1).min(last)),
        Action::SelectFirst => Some(0),
        Action::SelectLast => Some(last),
        _ => None,
    }
}

fn command(action: Action, selected: usize) -> Option<Command> {
    match action {
        Action::PlaySelected => Some(Command::Play(selected)),
        Action::TogglePause => Some(Command::TogglePause(selected)),
        Action::Next => Some(Command::Skip(true)),
        Action::Previous => Some(Command::Skip(false)),
        Action::Seek(seconds) => Some(Command::Seek(seconds)),
        Action::ChangeVolume(delta) => Some(Command::ChangeVolume(delta)),
        Action::CycleRepeat => Some(Command::CycleRepeat),
        Action::ToggleShuffle => Some(Command::ToggleShuffle),
        Action::Enqueue => Some(Command::Enqueue(selected)),
        Action::ClearQueue => Some(Command::ClearQueue),
        _ => None,
    }
}

pub fn run(
    mut tracks: Vec<Track>,
    settings: &mut Settings,
    startup_notice: &str,
) -> io::Result<()> {
    for track in &mut tracks {
        let metadata = crate::media::metadata(&track.path);
        let title = metadata.title.unwrap_or_else(|| track.title.clone());
        track.title = [Some(title), metadata.artist, metadata.album]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" - ");
    }
    let lower_titles: Vec<String> = tracks.iter().map(|t| t.title.to_lowercase()).collect();
    let tracks = Arc::new(tracks);
    let mut playback = Playback::start(tracks.clone(), settings.volume, settings.repeat)
        .map_err(io::Error::other)?;
    let mut state = playback.snapshot();
    let mut selected = state.current.unwrap_or(0);
    let mut last_current = state.current;
    let mut revision = state.revision;
    let mut seek_revision = state.seek_revision;
    let previous_hook = std::panic::take_hook();
    let main_thread = std::thread::current().id();
    let worker_panicked = Arc::new(AtomicBool::new(false));
    let hook_panicked = worker_panicked.clone();
    std::panic::set_hook(Box::new(move |info| {
        // Only the UI thread can safely restore the terminal; a panicked worker
        // just flags the loop to exit, where TerminalGuard's drop restores it.
        if std::thread::current().id() == main_thread {
            restore();
        } else {
            hook_panicked.store(true, Ordering::Relaxed);
        }
        eprintln!("StarPlay: {info}");
    }));
    let mut visual = Visualizer::default();
    visual.mode = settings.visual_mode;
    let result = (|| -> io::Result<()> {
        let _guard = TerminalGuard::enter()?;
        let mut out = io::stdout();
        let mut screen = Screen::default();
        let mut canvas = VisualCanvas::default();
        let mut input = Input::default();
        let mut browser = Browser {
            cursor: selected,
            ..Browser::default()
        };
        let mut lyrics_enabled = true;
        let mut lyrics = None;
        let mut lyric_track = None;
        let mut size = terminal::size()?;
        let mut last_update = Instant::now();
        let mut next_draw = last_update;
        let mut dirty = true;
        let mut previous_visible = false;
        let mut local_notice = String::new();
        loop {
            if worker_panicked.load(Ordering::Relaxed) {
                break;
            }
            // The worker publishes revisions lock-free; only clone the full state when
            // something changed, or while playing (the position advances every wakeup).
            let state_changed =
                playback.revision() != revision || playback.seek_revision() != seek_revision;
            if state_changed || !state.finished && !state.paused {
                state = playback.snapshot();
            }
            if state.current != lyric_track {
                lyric_track = state.current;
                lyrics = state
                    .current
                    .and_then(|i| tracks.get(i))
                    .and_then(|track| crate::media::Lyrics::load(&track.path).ok());
            }
            let indices = browser.indices(&lower_titles, &state.queue);
            browser.clamp(indices.len());
            selected = browser.cursor;
            if state.revision != revision {
                revision = state.revision;
                if state.current != last_current {
                    if !browser.queue && !browser.editing {
                        if let Some(index) = indices.iter().position(|i| Some(*i) == state.current)
                        {
                            browser.cursor = index;
                            selected = index;
                        }
                    }
                    last_current = state.current;
                }
                if state.seek_revision != seek_revision {
                    seek_revision = state.seek_revision;
                    visual.reset();
                }
                dirty = true;
            }
            let visible = effects_visible(size.0, size.1, visual.mode);
            if visible != previous_visible {
                visual.wait_for_fresh_audio(&state.audio);
                previous_visible = visible;
                dirty = true;
            }
            let active = !state.finished && !state.paused;
            state.capture.set_enabled(visible);
            if dirty || Instant::now() >= next_draw {
                let now = Instant::now();
                visual.update(
                    &state.audio,
                    visible && active,
                    state.volume,
                    now.saturating_duration_since(last_update),
                );
                last_update = now; // Includes drawing time in the next animation step.
                let notice = if !local_notice.is_empty() {
                    local_notice.as_str()
                } else if !state.notice.is_empty() {
                    state.notice.as_str()
                } else {
                    startup_notice
                };
                draw(
                    &mut screen,
                    &View {
                        tracks: &tracks,
                        state: &state,
                        selected,
                        notice,
                        browser: Some(&browser),
                        lyric: if lyrics_enabled {
                            lyrics.as_ref().and_then(|l| l.current(state.position))
                        } else {
                            None
                        },
                    },
                    Some(&indices),
                    &visual,
                    &mut canvas,
                    size,
                );
                screen.present(&mut out)?;
                next_draw = now
                    + frame_interval(visible, active && state.volume > 0, visual.is_animating());
                dirty = false;
            }
            // The worker advances playback independently. Poll often enough to display its
            // command results promptly, without forcing a redraw or FFT on every wakeup.
            let wait = next_draw
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100));
            if !event::poll(wait)? {
                continue;
            }
            match event::read()? {
                Event::Resize(width, height) => {
                    size = (width, height);
                    screen.invalidate();
                    dirty = true;
                }
                Event::FocusLost => input.reset(),
                Event::Key(key) => {
                    if browser.editing {
                        if key.kind == KeyEventKind::Release {
                            input.action(key);
                            continue;
                        }
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(key.code, KeyCode::Char('c' | 'C'))
                        {
                            break;
                        }
                        match key.code {
                            KeyCode::Esc => {
                                browser.query.clear();
                                browser.editing = false;
                                input.reset();
                            }
                            KeyCode::Enter => {
                                browser.editing = false;
                                input.reset();
                            }
                            KeyCode::Backspace => {
                                browser.query.pop();
                            }
                            KeyCode::Char(ch)
                                if (key.modifiers & !KeyModifiers::SHIFT).is_empty()
                                    && !ch.is_control() =>
                            {
                                browser.query.push(ch);
                            }
                            _ => {}
                        }
                        browser.cursor = 0;
                        dirty = true;
                        continue;
                    }
                    let Some(action) = input.action(key) else {
                        continue;
                    };
                    if action == Action::Quit {
                        break;
                    }
                    dirty = true;
                    local_notice.clear();
                    if let Some(index) = select(action, selected, indices.len()) {
                        browser.cursor = index;
                    } else if action == Action::Search {
                        browser.queue = false;
                        browser.editing = true;
                        browser.cursor = 0;
                    } else if action == Action::ToggleQueue {
                        browser.queue = !browser.queue;
                        browser.cursor = 0;
                    } else if action == Action::ToggleLyrics {
                        lyrics_enabled = !lyrics_enabled;
                    } else if action == Action::RemoveQueued {
                        if browser.queue && !indices.is_empty() {
                            if let Err(error) = playback.send(Command::RemoveQueued(selected)) {
                                local_notice = error;
                            }
                        }
                    } else if action == Action::SavePlaylist {
                        let export: Vec<_> = indices.iter().map(|i| tracks[*i].clone()).collect();
                        local_notice = match crate::playlist::save(
                            std::path::Path::new("starplay-playlist.m3u8"),
                            &export,
                        ) {
                            Ok(()) => "Saved starplay-playlist.m3u8".into(),
                            Err(error) => error,
                        };
                    } else if action == Action::CycleVisual {
                        visual.mode = visual.mode.next();
                        visual.reset();
                    } else if matches!(action, Action::PlaySelected | Action::Enqueue)
                        && indices.is_empty()
                    {
                        local_notice = "No track selected".into();
                    } else if let Some(command) = command(
                        action,
                        indices
                            .get(selected)
                            .copied()
                            .or(state.current)
                            .unwrap_or(0),
                    ) {
                        if let Err(error) = playback.send(command) {
                            local_notice = error;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    std::panic::set_hook(previous_hook);
    let shutdown = playback.shutdown().map_err(io::Error::other);
    let final_state = playback.snapshot();
    settings.volume = final_state.volume;
    settings.repeat = final_state.repeat;
    settings.visual_mode = visual.mode;
    result.and(shutdown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_handles_beginning_middle_end_unknown_and_zero_duration() {
        let total = Some(Duration::from_secs(120));
        let begin = progress_line(Duration::ZERO, total, 79);
        assert!(begin.contains("00:00 / 02:00"));
        assert!(begin.contains("  0% -02:00"));
        assert!(!begin.contains('='));
        let middle = progress_line(Duration::from_secs(60), total, 79);
        assert!(middle.contains(" 50% -01:00"));
        assert!(middle.contains('='));
        let end = progress_line(Duration::from_secs(150), total, 79);
        assert!(end.contains("02:00 / 02:00"));
        assert!(end.contains("100% -00:00"));
        for total in [None, Some(Duration::ZERO)] {
            let unknown = progress_line(Duration::from_secs(60), total, 79);
            assert!(unknown.contains("01:00 / --:--"));
            assert!(unknown.contains('?'));
            assert!(unknown.contains("--% ---:--"));
        }
    }

    #[test]
    fn progress_fits_tiny_compact_wide_and_very_long_tracks() {
        for width in [0, 1, 12, 31, 47, 48, 79, 200] {
            for total in [
                None,
                Some(Duration::ZERO),
                Some(Duration::from_secs(120)),
                Some(Duration::MAX),
            ] {
                let text = progress_line(Duration::MAX, total, width);
                assert!(text.is_ascii());
                assert!(text.len() <= width, "{width}: {text}");
            }
        }
        let compact = progress_line(Duration::from_secs(60), Some(Duration::from_secs(120)), 31);
        assert!(compact.starts_with('['));
        assert!(compact.contains("50% -01:00"));
    }

    #[test]
    fn effects_leave_room_for_playlist_and_controls() {
        for height in 10..100 {
            for mode in [VisualMode::Spectrum, VisualMode::Pulse, VisualMode::Off] {
                let rows = effect_height(height, mode);
                let start = if rows > 0 { 5 + rows } else { 4 };
                assert!(height - start - 4 >= 2);
                assert!(rows <= 6);
                if height < 14 || mode == VisualMode::Off {
                    assert_eq!(rows, 0);
                }
            }
        }
    }

    #[test]
    fn pause_and_mute_keep_fast_decay_then_drop_to_idle_refresh() {
        assert!(!effects_visible(31, 20, VisualMode::Spectrum));
        assert!(!effects_visible(32, 13, VisualMode::Spectrum));
        assert!(effects_visible(32, 14, VisualMode::Pulse));
        assert!(!effects_visible(80, 24, VisualMode::Off));
        assert_eq!(frame_interval(true, true, false), Duration::from_millis(33));
        assert_eq!(frame_interval(true, false, true), Duration::from_millis(33));
        assert_eq!(frame_interval(true, false, false), Duration::from_secs(1));
        assert_eq!(frame_interval(false, true, true), Duration::from_secs(1));
    }

    #[test]
    fn selection_and_command_mapping_handle_empty_and_end_boundaries() {
        assert_eq!(select(Action::SelectUp, 0, 2), Some(0));
        assert_eq!(select(Action::SelectDown, 1, 2), Some(1));
        assert_eq!(select(Action::SelectLast, 0, 2), Some(1));
        assert_eq!(select(Action::SelectFirst, 1, 2), Some(0));
        assert_eq!(select(Action::SelectDown, usize::MAX, 0), Some(0));
        assert_eq!(select(Action::TogglePause, 0, 2), None);
        assert!(matches!(
            command(Action::PlaySelected, 1),
            Some(Command::Play(1))
        ));
        assert!(matches!(
            command(Action::TogglePause, 1),
            Some(Command::TogglePause(1))
        ));
        assert!(matches!(
            command(Action::Seek(-5), 1),
            Some(Command::Seek(-5))
        ));
        assert!(command(Action::CycleVisual, 1).is_none());
    }

    #[test]
    fn effects_render_pulse_with_ascii_sized_colored_rows() {
        let mut visual = Visualizer::default();
        visual.mode = VisualMode::Pulse;
        let mut screen = Screen::default();
        screen.begin((80, 24));
        let mut canvas = VisualCanvas::default();
        draw_effect(&mut screen, &visual, &mut canvas, 80, 6);
        let mut bytes = Vec::new();
        screen.present(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("PULSE | V: switch/off"));
        for color in [Color::Yellow, Color::Magenta, Color::Blue] {
            let mut expected = Vec::new();
            crossterm::queue!(&mut expected, crossterm::style::SetForegroundColor(color)).unwrap();
            assert!(text.contains(&String::from_utf8(expected).unwrap()));
        }
        assert!(canvas
            .rows
            .iter()
            .all(|row| row.len() == 79 && row.is_ascii()));
    }

    fn tracks() -> Vec<Track> {
        (0..12)
            .map(|i| Track {
                path: format!("song-{i}.wav").into(),
                title: format!("如愿-{i}"),
            })
            .collect()
    }

    #[test]
    fn complete_layout_handles_all_modes_resize_selection_pause_and_stop() {
        use crate::player::Repeat;
        let tracks = tracks();
        let mut state = PlaybackState::empty(50, Repeat::Off);
        state.current = Some(0);
        state.finished = false;
        state.duration = Some(Duration::from_secs(120));
        state.position = Duration::from_secs(60);
        let mut screen = Screen::default();
        let mut canvas = VisualCanvas::default();
        let mut visual = Visualizer::default();
        for size in [
            (0, 0),
            (1, 1),
            (31, 9),
            (32, 10),
            (32, 14),
            (80, 24),
            (120, 40),
        ] {
            for mode in [VisualMode::Spectrum, VisualMode::Pulse, VisualMode::Off] {
                visual.mode = mode;
                draw(
                    &mut screen,
                    &View {
                        tracks: &tracks,
                        state: &state,
                        selected: 11,
                        notice: "test notice",
                        browser: None,
                        lyric: None,
                    },
                    None,
                    &visual,
                    &mut canvas,
                    size,
                );
                if size.0 >= 32 && size.1 >= 10 {
                    assert!(screen.row_text(1).contains("PLAYING"));
                    assert!(screen.row_text(2).contains("50%"));
                    assert!(screen
                        .row_text(usize::from(size.1 - 4))
                        .contains("test notice"));
                    assert!(screen.row_text(usize::from(size.1 - 1)).contains("Q"));
                    let start = if effect_height(size.1, mode) > 0 {
                        5 + effect_height(size.1, mode)
                    } else {
                        4
                    };
                    assert!((start..size.1 - 4)
                        .any(|row| screen.row_text(usize::from(row)).contains("如愿-11")));
                }
                screen.present(&mut Vec::new()).unwrap();
            }
        }
        state.paused = true;
        draw(
            &mut screen,
            &View {
                tracks: &tracks,
                state: &state,
                selected: 0,
                notice: "",
                browser: None,
                lyric: None,
            },
            None,
            &visual,
            &mut canvas,
            (80, 24),
        );
        assert!(screen.row_text(1).starts_with("PAUSED"));
        state.finished = true;
        state.position = Duration::from_secs(120);
        draw(
            &mut screen,
            &View {
                tracks: &tracks,
                state: &state,
                selected: 0,
                notice: "",
                browser: None,
                lyric: None,
            },
            None,
            &visual,
            &mut canvas,
            (80, 24),
        );
        assert!(screen.row_text(1).starts_with("STOPPED"));
        assert!(screen.row_text(2).contains("100% -00:00"));
    }

    #[test]
    fn static_player_frames_produce_no_output_and_seek_changes_only_progress() {
        use crate::player::Repeat;
        let tracks = tracks();
        let mut state = PlaybackState::empty(50, Repeat::Off);
        state.current = Some(0);
        state.paused = true;
        state.finished = false;
        state.duration = Some(Duration::from_secs(120));
        let mut screen = Screen::default();
        let mut visual = Visualizer::default();
        visual.mode = VisualMode::Off;
        let mut canvas = VisualCanvas::default();
        let mut output = Vec::new();
        for _ in 0..2 {
            output.clear();
            draw(
                &mut screen,
                &View {
                    tracks: &tracks,
                    state: &state,
                    selected: 0,
                    notice: "",
                    browser: None,
                    lyric: None,
                },
                None,
                &visual,
                &mut canvas,
                (80, 24),
            );
            screen.present(&mut output).unwrap();
        }
        assert!(output.is_empty());
        state.position = Duration::from_secs(30);
        draw(
            &mut screen,
            &View {
                tracks: &tracks,
                state: &state,
                selected: 0,
                notice: "",
                browser: None,
                lyric: None,
            },
            None,
            &visual,
            &mut canvas,
            (80, 24),
        );
        screen.present(&mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("25%"));
        assert!(!text.contains("Playlist"));
        assert!(!text.contains("PAUSED"));
        assert!(!text.contains("如愿"));
    }

    #[test]
    fn keyboard_to_command_sequence_preserves_release_repeat_semantics() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let mut input = Input::default();
        let mut selected = 0;
        let mut commands = Vec::new();
        for (code, kind) in [
            (KeyCode::Down, KeyEventKind::Press),
            (KeyCode::Down, KeyEventKind::Repeat),
            (KeyCode::Enter, KeyEventKind::Press),
            (KeyCode::Enter, KeyEventKind::Repeat),
            (KeyCode::Enter, KeyEventKind::Release),
            (KeyCode::Char(' '), KeyEventKind::Press),
            (KeyCode::Char(' '), KeyEventKind::Repeat),
            (KeyCode::Char(' '), KeyEventKind::Release),
            (KeyCode::Right, KeyEventKind::Press),
            (KeyCode::Right, KeyEventKind::Repeat),
            (KeyCode::Char('+'), KeyEventKind::Repeat),
        ] {
            if let Some(action) =
                input.action(KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind))
            {
                if let Some(index) = select(action, selected, 3) {
                    selected = index;
                }
                if let Some(command) = command(action, selected) {
                    commands.push(command);
                }
            }
        }
        assert_eq!(selected, 2);
        assert_eq!(
            commands,
            [
                Command::Play(2),
                Command::TogglePause(2),
                Command::Seek(5),
                Command::Seek(5),
                Command::ChangeVolume(5)
            ]
        );
    }

    #[test]
    fn filtered_and_queue_views_keep_library_identity_and_show_lyrics() {
        let tracks = tracks();
        let mut state = PlaybackState::empty(50, crate::player::Repeat::Off);
        state.current = Some(11);
        state.queue = vec![11, 3, 11];
        state.shuffle = true;
        let mut browser = Browser {
            query: "如愿-11".into(),
            ..Browser::default()
        };
        let mut screen = Screen::default();
        let mut visual = Visualizer::default();
        visual.mode = VisualMode::Off;
        let mut canvas = VisualCanvas::default();
        let lower_titles: Vec<String> = tracks.iter().map(|t| t.title.to_lowercase()).collect();
        for queue in [false, true] {
            browser.queue = queue;
            let indices = browser.indices(&lower_titles, &state.queue);
            draw(
                &mut screen,
                &View {
                    tracks: &tracks,
                    state: &state,
                    selected: 0,
                    notice: "",
                    browser: Some(&browser),
                    lyric: Some("当前歌词"),
                },
                Some(&indices),
                &visual,
                &mut canvas,
                (80, 24),
            );
            assert!(screen.row_text(3).contains("S:on"));
            assert!(screen.row_text(4).contains("*  12. 如愿-11"));
            assert!(screen.row_text(20).contains("当前歌词"));
            if queue {
                assert!(screen.row_text(5).contains("如愿-3"));
            }
        }
        browser.queue = false;
        browser.query = "no matches".into();
        let indices = browser.indices(&lower_titles, &state.queue);
        draw(
            &mut screen,
            &View {
                tracks: &tracks,
                state: &state,
                selected: 0,
                notice: "",
                browser: Some(&browser),
                lyric: None,
            },
            Some(&indices),
            &visual,
            &mut canvas,
            (32, 10),
        );
        assert!(screen.row_text(3).contains("[0]"));
        assert!(screen.row_text(4).is_empty());
    }

    #[test]
    fn sanitizes_control_characters_and_formats_time() {
        assert_eq!(safe_text("a\x1b[31m\n\tb\u{202e}"), "a [31m  b ");
        assert_eq!(time_label(Duration::from_secs(65)), "01:05");
        assert_eq!(time_label(Duration::from_secs(3601)), "60:01");
    }
}
