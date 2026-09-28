//! Device-independent key bindings and repeat handling.
//!
//! Windows console input can report held keys as successive `Press` events, so
//! one-shot bindings remain down until `Release` (or [`Input::reset`]). Unix
//! terminals often omit releases entirely: there we only suppress explicit
//! `Repeat` events. Repeated Unix `Press` events cannot reliably be distinguished
//! from separate taps, and deliberately remain actionable.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::HashSet;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Action {
    Quit,
    SelectUp,
    SelectDown,
    SelectFirst,
    SelectLast,
    PlaySelected,
    TogglePause,
    Next,
    Previous,
    Seek(i64),
    ChangeVolume(i16),
    CycleVisual,
    CycleRepeat,
    Search,
    ToggleShuffle,
    Enqueue,
    ToggleQueue,
    RemoveQueued,
    ClearQueue,
    SavePlaylist,
    ToggleLyrics,
}

impl Action {
    fn repeatable(self) -> bool {
        matches!(
            self,
            Self::SelectUp
                | Self::SelectDown
                | Self::SelectFirst
                | Self::SelectLast
                | Self::Seek(_)
                | Self::ChangeVolume(_)
        )
    }
}

pub struct Input {
    track_releases: bool,
    pressed_one_shots: HashSet<KeyCode>,
}

impl Default for Input {
    fn default() -> Self {
        Self::with_release_tracking(cfg!(windows))
    }
}

impl Input {
    // Keep the platform policy injectable so both paths can be tested without
    // a terminal, an audio device, or a platform-specific test runner.
    fn with_release_tracking(track_releases: bool) -> Self {
        Self {
            track_releases,
            pressed_one_shots: HashSet::new(),
        }
    }

    /// Forget held keys, e.g. when the UI receives `Event::FocusLost`.
    pub fn reset(&mut self) {
        self.pressed_one_shots.clear();
    }

    pub fn action(&mut self, key: KeyEvent) -> Option<Action> {
        // Key-up modifiers/case can differ if Shift or Control was released
        // first. Release by normalized key code, even for an unbound combo.
        let code = normalized_code(key.code);
        if key.kind == KeyEventKind::Release {
            self.pressed_one_shots.remove(&code);
            return None;
        }

        let action = binding(key)?;
        if action.repeatable() {
            return Some(action);
        }

        // An explicit Repeat also indicates that the key is already held, even
        // if its initial Press was missed. Never act on that Repeat itself.
        if self.track_releases && !self.pressed_one_shots.insert(code) {
            return None;
        }
        (key.kind == KeyEventKind::Press).then_some(action)
    }
}

fn normalized_code(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(ch) => KeyCode::Char(ch.to_ascii_lowercase()),
        other => other,
    }
}

fn binding(key: KeyEvent) -> Option<Action> {
    // Shift is allowed for uppercase shortcuts and shifted punctuation. All
    // other modifiers are rejected, except Control (optionally Shift) + C.
    let modifiers = key.modifiers & !KeyModifiers::SHIFT;
    if modifiers == KeyModifiers::CONTROL && matches!(key.code, KeyCode::Char('c' | 'C')) {
        return Some(Action::Quit);
    }
    if !modifiers.is_empty() {
        return None;
    }

    Some(match key.code {
        KeyCode::Char('q' | 'Q') | KeyCode::Esc => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => Action::SelectUp,
        KeyCode::Down | KeyCode::Char('j') => Action::SelectDown,
        KeyCode::Home => Action::SelectFirst,
        KeyCode::End => Action::SelectLast,
        KeyCode::Enter => Action::PlaySelected,
        KeyCode::Char(' ') => Action::TogglePause,
        KeyCode::Char('n' | 'N') => Action::Next,
        KeyCode::Char('p' | 'P') => Action::Previous,
        KeyCode::Left => Action::Seek(-5),
        KeyCode::Right => Action::Seek(5),
        KeyCode::Char('+' | '=') => Action::ChangeVolume(5),
        KeyCode::Char('-' | '_') => Action::ChangeVolume(-5),
        KeyCode::Char('v' | 'V') => Action::CycleVisual,
        KeyCode::Char('r' | 'R') => Action::CycleRepeat,
        KeyCode::Char('/') => Action::Search,
        KeyCode::Char('s' | 'S') => Action::ToggleShuffle,
        KeyCode::Char('a' | 'A') => Action::Enqueue,
        KeyCode::Tab => Action::ToggleQueue,
        KeyCode::Delete => Action::RemoveQueued,
        KeyCode::Char('c' | 'C') => Action::ClearQueue,
        KeyCode::Char('w' | 'W') => Action::SavePlaylist,
        KeyCode::Char('l' | 'L') => Action::ToggleLyrics,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Preserve the old bindings, including lowercase-only Vim navigation and
    // both cases of the N/P/V/R/Q shortcuts.
    fn bindings() -> Vec<(KeyCode, KeyModifiers, Action)> {
        use Action::*;
        use KeyCode::*;
        let mut bindings: Vec<_> = [
            (Up, SelectUp),
            (Char('k'), SelectUp),
            (Down, SelectDown),
            (Char('j'), SelectDown),
            (Home, SelectFirst),
            (End, SelectLast),
            (Enter, PlaySelected),
            (Char(' '), TogglePause),
            (Char('n'), Next),
            (Char('N'), Next),
            (Char('p'), Previous),
            (Char('P'), Previous),
            (Left, Seek(-5)),
            (Right, Seek(5)),
            (Char('+'), ChangeVolume(5)),
            (Char('='), ChangeVolume(5)),
            (Char('-'), ChangeVolume(-5)),
            (Char('_'), ChangeVolume(-5)),
            (Char('v'), CycleVisual),
            (Char('V'), CycleVisual),
            (Char('r'), CycleRepeat),
            (Char('R'), CycleRepeat),
            (Char('q'), Quit),
            (Char('Q'), Quit),
            (Esc, Quit),
            (Char('/'), Search),
            (Char('s'), ToggleShuffle),
            (Char('a'), Enqueue),
            (Tab, ToggleQueue),
            (Delete, RemoveQueued),
            (Char('c'), ClearQueue),
            (Char('w'), SavePlaylist),
            (Char('l'), ToggleLyrics),
        ]
        .into_iter()
        .flat_map(|(code, action)| {
            [KeyModifiers::NONE, KeyModifiers::SHIFT].map(|modifiers| (code, modifiers, action))
        })
        .collect();
        for code in [Char('c'), Char('C')] {
            for modifiers in [
                KeyModifiers::CONTROL,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ] {
                bindings.push((code, modifiers, Quit));
            }
        }
        bindings
    }

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, modifiers, kind)
    }

    #[test]
    fn default_uses_host_platform_policy() {
        assert_eq!(Input::default().track_releases, cfg!(windows));
    }

    #[test]
    fn only_selection_seeking_and_volume_are_repeatable() {
        for action in [
            Action::SelectUp,
            Action::SelectDown,
            Action::SelectFirst,
            Action::SelectLast,
            Action::Seek(-5),
            Action::Seek(5),
            Action::ChangeVolume(-5),
            Action::ChangeVolume(5),
        ] {
            assert!(action.repeatable(), "{action:?}");
        }
        for action in [
            Action::Quit,
            Action::PlaySelected,
            Action::TogglePause,
            Action::Next,
            Action::Previous,
            Action::CycleVisual,
            Action::CycleRepeat,
        ] {
            assert!(!action.repeatable(), "{action:?}");
        }
    }

    #[test]
    fn every_binding_handles_press_repeat_and_release() {
        for track_releases in [false, true] {
            for (code, modifiers, expected) in bindings() {
                let mut input = Input::with_release_tracking(track_releases);
                assert_eq!(
                    input.action(key(code, modifiers, KeyEventKind::Press)),
                    Some(expected),
                    "Press {code:?} {modifiers:?}, release tracking: {track_releases}"
                );
                assert_eq!(
                    input.action(key(code, modifiers, KeyEventKind::Repeat)),
                    expected.repeatable().then_some(expected),
                    "Repeat {code:?} {modifiers:?}, release tracking: {track_releases}"
                );
                assert_eq!(
                    input.action(key(code, modifiers, KeyEventKind::Release)),
                    None,
                    "Release {code:?} {modifiers:?}"
                );
                assert_eq!(
                    input.action(key(code, modifiers, KeyEventKind::Press)),
                    Some(expected),
                    "Press after release {code:?} {modifiers:?}"
                );
            }
        }
    }

    #[test]
    fn windows_suppresses_repeated_presses_of_every_one_shot_until_release() {
        for (code, modifiers, expected) in bindings() {
            if expected.repeatable() {
                continue;
            }
            let mut input = Input::with_release_tracking(true);
            assert_eq!(
                input.action(key(code, modifiers, KeyEventKind::Press)),
                Some(expected)
            );
            for _ in 0..3 {
                assert_eq!(
                    input.action(key(code, modifiers, KeyEventKind::Press)),
                    None
                );
                assert_eq!(
                    input.action(key(code, modifiers, KeyEventKind::Repeat)),
                    None
                );
            }
            assert_eq!(
                input.action(key(code, modifiers, KeyEventKind::Release)),
                None
            );
            assert_eq!(
                input.action(key(code, modifiers, KeyEventKind::Press)),
                Some(expected)
            );
        }
    }

    #[test]
    fn unix_allows_consecutive_presses_without_releases() {
        let mut input = Input::with_release_tracking(false);
        for (code, modifiers, expected) in bindings() {
            for _ in 0..3 {
                assert_eq!(
                    input.action(key(code, modifiers, KeyEventKind::Press)),
                    Some(expected),
                    "{code:?} {modifiers:?}"
                );
            }
        }
        assert!(input.pressed_one_shots.is_empty());
    }

    #[test]
    fn navigation_seeking_and_volume_repeat_on_both_platforms() {
        for track_releases in [false, true] {
            let mut input = Input::with_release_tracking(track_releases);
            for (code, modifiers, expected) in bindings() {
                if !expected.repeatable() {
                    continue;
                }
                for _ in 0..3 {
                    for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
                        assert_eq!(input.action(key(code, modifiers, kind)), Some(expected));
                    }
                }
            }
            assert!(input.pressed_one_shots.is_empty());
        }
    }

    #[test]
    fn explicit_repeat_without_initial_press_never_fires_a_one_shot() {
        for track_releases in [false, true] {
            for (code, modifiers, expected) in bindings() {
                if expected.repeatable() {
                    continue;
                }
                let mut input = Input::with_release_tracking(track_releases);
                assert_eq!(
                    input.action(key(code, modifiers, KeyEventKind::Repeat)),
                    None
                );
                assert_eq!(
                    input.action(key(code, modifiers, KeyEventKind::Press)),
                    (!track_releases).then_some(expected)
                );
                assert_eq!(
                    input.action(key(code, modifiers, KeyEventKind::Release)),
                    None
                );
                assert_eq!(
                    input.action(key(code, modifiers, KeyEventKind::Press)),
                    Some(expected)
                );
            }
        }
    }

    #[test]
    fn windows_normalizes_case_and_releases_even_if_modifiers_changed() {
        for (code, modifiers, expected) in bindings() {
            if expected.repeatable() {
                continue;
            }
            let mut input = Input::with_release_tracking(true);
            let upper = match code {
                KeyCode::Char(ch) => KeyCode::Char(ch.to_ascii_uppercase()),
                other => other,
            };
            assert_eq!(
                input.action(key(code, modifiers, KeyEventKind::Press)),
                Some(expected)
            );
            assert_eq!(
                input.action(key(
                    upper,
                    modifiers | KeyModifiers::SHIFT,
                    KeyEventKind::Press
                )),
                None
            );
            // The release's Alt modifier is intentionally not a valid binding.
            assert_eq!(
                input.action(key(upper, KeyModifiers::ALT, KeyEventKind::Release)),
                None
            );
            assert_eq!(
                input.action(key(code, modifiers, KeyEventKind::Press)),
                Some(expected)
            );
        }
    }

    #[test]
    fn windows_tracks_independent_keys_not_shared_actions() {
        let mut input = Input::with_release_tracking(true);
        for code in [KeyCode::Char('q'), KeyCode::Esc, KeyCode::Char('c')] {
            let modifiers = if code == KeyCode::Char('c') {
                KeyModifiers::CONTROL
            } else {
                KeyModifiers::NONE
            };
            assert_eq!(
                input.action(key(code, modifiers, KeyEventKind::Press)),
                Some(Action::Quit)
            );
            assert_eq!(
                input.action(key(code, modifiers, KeyEventKind::Press)),
                None
            );
        }
        assert_eq!(
            input.action(key(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            )),
            None
        );
        assert_eq!(
            input.action(key(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Press
            )),
            Some(Action::Quit)
        );
        assert_eq!(
            input.action(key(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Press)),
            None
        );
    }

    #[test]
    fn reset_clears_all_held_keys_without_changing_platform_policy() {
        for track_releases in [false, true] {
            let mut input = Input::with_release_tracking(track_releases);
            for code in [KeyCode::Enter, KeyCode::Char(' '), KeyCode::Char('n')] {
                assert!(input
                    .action(KeyEvent::new(code, KeyModifiers::NONE))
                    .is_some());
            }
            input.reset();
            input.reset();
            assert!(input.pressed_one_shots.is_empty());
            assert_eq!(input.track_releases, track_releases);
            for code in [KeyCode::Enter, KeyCode::Char(' '), KeyCode::Char('n')] {
                assert!(input
                    .action(KeyEvent::new(code, KeyModifiers::NONE))
                    .is_some());
            }
        }
    }

    #[test]
    fn unrelated_modifiers_never_fire_or_latch_bindings() {
        for track_releases in [false, true] {
            let mut input = Input::with_release_tracking(track_releases);
            for (code, modifiers, _) in bindings() {
                for extra in [
                    KeyModifiers::ALT,
                    KeyModifiers::SUPER,
                    KeyModifiers::HYPER,
                    KeyModifiers::META,
                    KeyModifiers::CONTROL | KeyModifiers::ALT,
                ] {
                    for kind in [
                        KeyEventKind::Press,
                        KeyEventKind::Repeat,
                        KeyEventKind::Release,
                    ] {
                        assert_eq!(input.action(key(code, modifiers | extra, kind)), None);
                    }
                }
                if !matches!(code, KeyCode::Char('c' | 'C')) {
                    for kind in [
                        KeyEventKind::Press,
                        KeyEventKind::Repeat,
                        KeyEventKind::Release,
                    ] {
                        assert_eq!(
                            input.action(key(code, modifiers | KeyModifiers::CONTROL, kind)),
                            None
                        );
                    }
                }
            }
            assert!(input.pressed_one_shots.is_empty());
            assert_eq!(
                input.action(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
                Some(Action::Quit)
            );
        }
    }

    #[test]
    fn invalid_keys_never_produce_actions_or_state() {
        for track_releases in [false, true] {
            let mut input = Input::with_release_tracking(track_releases);
            for code in [
                KeyCode::Null,
                KeyCode::BackTab,
                KeyCode::Backspace,
                KeyCode::Insert,
                KeyCode::PageUp,
                KeyCode::PageDown,
                KeyCode::F(1),
                KeyCode::Char('J'),
                KeyCode::Char('K'),
                KeyCode::Char('x'),
                KeyCode::Char('1'),
                KeyCode::Char('é'),
            ] {
                for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
                    for kind in [
                        KeyEventKind::Press,
                        KeyEventKind::Repeat,
                        KeyEventKind::Release,
                    ] {
                        assert_eq!(input.action(key(code, modifiers, kind)), None, "{code:?}");
                    }
                }
            }
            assert!(input.pressed_one_shots.is_empty());
        }
    }
}
