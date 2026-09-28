//! Reusable row buffers and transactional, line-level terminal diffing.
use crossterm::{
    cursor::MoveTo,
    queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{Clear, ClearType},
};
use std::io::{self, Write};
use unicode_width::UnicodeWidthChar;

pub fn safe_text(text: &str) -> String {
    text.chars().map(safe_char).collect()
}

fn safe_char(ch: char) -> char {
    if ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
        ' '
    } else {
        ch
    }
}

fn fit_into(output: &mut String, text: &str, width: usize) {
    output.clear();
    let mut used = 0;
    for ch in text.chars().map(safe_char) {
        used += ch.width().unwrap_or(0);
        if used > width {
            break;
        }
        output.push(ch);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    text: String,
    color: Color,
}

impl Default for Row {
    fn default() -> Self {
        Self {
            text: String::new(),
            color: Color::White,
        }
    }
}

#[derive(Default)]
pub struct Screen {
    size: (u16, u16),
    front: Vec<Row>,
    back: Vec<Row>,
    bytes: Vec<u8>,
    valid: bool,
}

impl Screen {
    pub fn invalidate(&mut self) {
        self.valid = false;
    }

    pub fn begin(&mut self, size: (u16, u16)) {
        if self.size != size {
            self.size = size;
            self.invalidate();
        }
        self.back.resize_with(usize::from(size.1), Row::default);
        for row in &mut self.back {
            row.text.clear();
            row.color = Color::White;
        }
    }

    pub fn line(&mut self, row: u16, text: &str, color: Color) {
        if let Some(output) = self.back.get_mut(usize::from(row)) {
            fit_into(
                &mut output.text,
                text,
                usize::from(self.size.0.saturating_sub(1)),
            );
            output.color = color;
        }
    }

    /// Update the cache only after output succeeds; retries after partial writes repaint.
    pub fn present(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.bytes.clear();
        if !self.valid {
            queue!(&mut self.bytes, Clear(ClearType::All))?;
        }
        if self.size.0 > 0 {
            for (index, row) in self.back.iter().enumerate() {
                if self.valid && self.front.get(index) == Some(row) {
                    continue;
                }
                queue!(
                    &mut self.bytes,
                    MoveTo(0, index as u16),
                    SetForegroundColor(row.color),
                    Print(&row.text),
                    Clear(ClearType::UntilNewLine),
                    ResetColor
                )?;
            }
        }
        if !self.bytes.is_empty() {
            if let Err(error) = out.write_all(&self.bytes).and_then(|()| out.flush()) {
                self.invalidate();
                return Err(error);
            }
        }
        std::mem::swap(&mut self.front, &mut self.back);
        self.valid = true;
        Ok(())
    }

    #[cfg(test)]
    pub fn row_text(&self, row: usize) -> &str {
        &self.back[row].text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_frame_writes_nothing_and_changed_row_is_isolated() {
        let mut screen = Screen::default();
        let mut output = Vec::new();
        screen.begin((80, 4));
        screen.line(0, "Playlist", Color::Cyan);
        screen.line(1, "00:01", Color::Green);
        screen.present(&mut output).unwrap();
        assert!(String::from_utf8_lossy(&output).contains("Playlist"));
        output.clear();
        screen.begin((80, 4));
        screen.line(0, "Playlist", Color::Cyan);
        screen.line(1, "00:01", Color::Green);
        screen.present(&mut output).unwrap();
        assert!(output.is_empty());
        screen.begin((80, 4));
        screen.line(0, "Playlist", Color::Cyan);
        screen.line(1, "00:02", Color::Green);
        screen.present(&mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("00:02"));
        assert!(!text.contains("Playlist"));
        assert!(!text.contains("\x1b[2J"));
    }

    #[test]
    fn shorter_text_and_removed_rows_are_cleared() {
        let mut screen = Screen::default();
        let mut output = Vec::new();
        screen.begin((80, 4));
        screen.line(0, "a long song title", Color::White);
        screen.line(2, "old effect", Color::Cyan);
        screen.present(&mut output).unwrap();
        output.clear();
        screen.begin((80, 4));
        screen.line(0, "short", Color::White);
        screen.present(&mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("short"));
        assert_eq!(text.matches("\x1b[K").count(), 2);
    }

    #[test]
    fn resize_and_color_changes_invalidate_the_appropriate_rows() {
        let mut screen = Screen::default();
        let mut output = Vec::new();
        screen.begin((80, 2));
        screen.line(0, "song", Color::White);
        screen.present(&mut output).unwrap();
        output.clear();
        screen.begin((80, 2));
        screen.line(0, "song", Color::Cyan);
        screen.present(&mut output).unwrap();
        assert!(String::from_utf8_lossy(&output).contains("song"));
        output.clear();
        screen.begin((32, 1));
        screen.line(0, "song", Color::Cyan);
        screen.present(&mut output).unwrap();
        assert!(String::from_utf8_lossy(&output).contains("\x1b[2J"));
    }

    #[test]
    fn text_is_sanitized_clipped_and_does_not_wrap() {
        let mut screen = Screen::default();
        screen.begin((5, 2));
        screen.line(0, "如愿-王菲", Color::White);
        screen.line(1, "a\x1b\n\u{202e}b", Color::White);
        assert_eq!(screen.row_text(0), "如愿");
        assert_eq!(screen.row_text(1), "a   ");
        for size in [(0, 0), (0, 1), (1, 1)] {
            screen.begin(size);
            screen.line(0, "ignored", Color::White);
            screen.present(&mut Vec::new()).unwrap();
        }
    }

    struct BrokenWriter;
    impl Write for BrokenWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disconnected"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failed_output_does_not_commit_frame_cache() {
        let mut screen = Screen::default();
        screen.begin((32, 2));
        screen.line(0, "retry me", Color::White);
        assert!(screen.present(&mut BrokenWriter).is_err());
        let mut output = Vec::new();
        screen.present(&mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("\x1b[2J"));
        assert!(text.contains("retry me"));
    }
}
