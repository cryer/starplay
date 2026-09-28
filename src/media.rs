//! Lightweight local metadata and synchronized lyric parsing.
//!
//! The player deliberately keeps this dependency-free. Metadata is best effort:
//! malformed tags fall back to the file name and never prevent playback.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

const MAX_TAG_BYTES: u64 = 1_048_576;
const MAX_LYRIC_BYTES: u64 = 1_048_576;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

pub fn metadata(path: &Path) -> Metadata {
    let Ok(mut file) = File::open(path) else {
        return Metadata::default();
    };
    let mut head = Vec::new();
    if (&mut file)
        .take(MAX_TAG_BYTES)
        .read_to_end(&mut head)
        .is_err()
    {
        return Metadata::default();
    }
    let mut result = if head.starts_with(b"ID3") {
        parse_id3v2(&head)
    } else {
        Metadata::default()
    };
    if result.title.is_none() || result.artist.is_none() || result.album.is_none() {
        // ID3v1 is a fixed 128-byte trailer; shorter files cannot carry one.
        let mut tail = Vec::new();
        if file.seek(SeekFrom::End(-128)).is_ok() && file.read_to_end(&mut tail).is_ok() {
            let tag = parse_id3v1(&tail);
            result.title = result.title.or(tag.title);
            result.artist = result.artist.or(tag.artist);
            result.album = result.album.or(tag.album);
        }
    }
    result
}

fn clean(bytes: &[u8]) -> Option<String> {
    let bytes = bytes.split(|b| *b == 0).next().unwrap_or(bytes);
    let text = if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        String::from_utf8_lossy(&bytes[3..]).into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn parse_id3v1(bytes: &[u8]) -> Metadata {
    let Some(tag) = bytes.get(bytes.len().saturating_sub(128)..) else {
        return Metadata::default();
    };
    if tag.len() != 128 || tag.get(..3) != Some(b"TAG") {
        return Metadata::default();
    }
    Metadata {
        title: clean(&tag[3..33]),
        artist: clean(&tag[33..63]),
        album: clean(&tag[63..93]),
    }
}

fn synchsafe(bytes: &[u8]) -> Option<usize> {
    (bytes.len() == 4 && bytes.iter().all(|b| b & 0x80 == 0)).then(|| {
        (usize::from(bytes[0]) << 21)
            | (usize::from(bytes[1]) << 14)
            | (usize::from(bytes[2]) << 7)
            | usize::from(bytes[3])
    })
}

fn decode_text(bytes: &[u8]) -> Option<String> {
    let (&encoding, data) = bytes.split_first()?;
    let text = match encoding {
        0 => String::from_utf8_lossy(data).into_owned(),
        1 if data.len() >= 2 && data.starts_with(&[0xff, 0xfe]) => String::from_utf16_lossy(
            &data[2..]
                .chunks(2)
                .filter(|p| p.len() == 2)
                .map(|p| u16::from_le_bytes([p[0], p[1]]))
                .collect::<Vec<_>>(),
        ),
        2 if data.len() >= 2 && data.starts_with(&[0xfe, 0xff]) => String::from_utf16_lossy(
            &data[2..]
                .chunks(2)
                .filter(|p| p.len() == 2)
                .map(|p| u16::from_be_bytes([p[0], p[1]]))
                .collect::<Vec<_>>(),
        ),
        _ => String::from_utf8_lossy(data).into_owned(),
    };
    let text = text.trim_matches('\0').trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn parse_id3v2(bytes: &[u8]) -> Metadata {
    if bytes.len() < 10 {
        return Metadata::default();
    }
    let Some(size) = synchsafe(&bytes[6..10]) else {
        return Metadata::default();
    };
    let end = 10usize.saturating_add(size).min(bytes.len());
    let mut at = 10;
    let mut result = Metadata::default();
    while at + 10 <= end {
        let id = &bytes[at..at + 4];
        if id.iter().all(|b| *b == 0) {
            break;
        }
        let Some(frame_size) = synchsafe(&bytes[at + 4..at + 8]) else {
            break;
        };
        let frame_end = at.saturating_add(10).saturating_add(frame_size);
        if frame_end > end {
            break;
        }
        let value = decode_text(&bytes[at + 10..frame_end]);
        match id {
            b"TIT2" => result.title = value,
            b"TPE1" => result.artist = value,
            b"TALB" => result.album = value,
            _ => {}
        }
        at = frame_end;
    }
    result
}

#[derive(Debug, Clone)]
pub struct Lyrics {
    lines: Vec<(Duration, String)>,
}

impl Lyrics {
    pub fn load(audio: &Path) -> Result<Self, String> {
        let path = audio.with_extension("lrc");
        let text = crate::playlist::read_bounded_utf8(&path, MAX_LYRIC_BYTES).map_err(|error| {
            use crate::playlist::ReadError;
            match error {
                ReadError::Open(error) | ReadError::Read(error) => {
                    format!("Cannot read '{}': {error}", path.display())
                }
                ReadError::TooLarge => format!(
                    "Lyrics '{}' exceed the {} byte limit",
                    path.display(),
                    MAX_LYRIC_BYTES
                ),
                ReadError::Utf8 => format!("Lyrics '{}' are not UTF-8", path.display()),
            }
        })?;
        Ok(Self::parse(&text))
    }

    fn parse(text: &str) -> Self {
        let mut lines = Vec::new();
        for raw in text.strip_prefix('\u{feff}').unwrap_or(text).lines() {
            let mut rest = raw;
            let mut times = Vec::new();
            while let Some(close) = rest.strip_prefix('[').and_then(|s| s.find(']')) {
                let stamp = &rest[1..close + 1];
                if let Some(time) = parse_timestamp(stamp) {
                    times.push(time);
                }
                rest = &rest[close + 2..];
            }
            let lyric = rest.trim().to_string();
            for time in times {
                lines.push((time, lyric.clone()));
            }
        }
        lines.sort_by_key(|(time, _)| *time);
        Self { lines }
    }

    pub fn current(&self, position: Duration) -> Option<&str> {
        let index = self.lines.partition_point(|(time, _)| *time <= position);
        Some(self.lines.get(index.checked_sub(1)?)?.1.as_str())
    }
}

fn parse_timestamp(value: &str) -> Option<Duration> {
    let (minutes, seconds) = value.split_once(':')?;
    let minutes: u64 = minutes.parse().ok()?;
    let mut fraction = seconds.split('.');
    let seconds: u64 = fraction.next()?.parse().ok()?;
    if seconds >= 60 {
        return None;
    }
    let decimal = fraction.next().unwrap_or("0");
    if fraction.next().is_some() || !decimal.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis = match decimal.len() {
        0 => 0,
        1 => decimal.parse::<u64>().ok()? * 100,
        2 => decimal.parse::<u64>().ok()? * 10,
        _ => decimal[..3].parse::<u64>().ok()?,
    };
    Some(Duration::from_millis(
        minutes
            .checked_mul(60_000)?
            .checked_add(seconds * 1_000)?
            .checked_add(millis)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lrc_supports_multiple_timestamps_and_fractional_seconds() {
        let lyrics = Lyrics::parse("[00:01.20]one\n[00:03.5][00:04.00]two");
        assert_eq!(lyrics.current(Duration::from_secs(1)), None);
        assert_eq!(lyrics.current(Duration::from_millis(1200)), Some("one"));
        assert_eq!(lyrics.current(Duration::from_secs(3)), Some("one"));
        assert_eq!(lyrics.current(Duration::from_millis(3500)), Some("two"));
    }

    #[test]
    fn id3v1_short_tag_is_ignored() {
        for len in 3..128 {
            let mut bytes = vec![0; len];
            bytes[..3].copy_from_slice(b"TAG");
            assert_eq!(parse_id3v1(&bytes), Metadata::default());
        }
    }

    #[test]
    fn malformed_lrc_timestamp_is_ignored() {
        for stamp in ["00:01.2.3", "00:01.1中文", "18446744073709551615:00"] {
            assert_eq!(parse_timestamp(stamp), None);
        }
        let lyrics = Lyrics::parse("[00:01.2.3]bad\n[00:02.3456]good");
        assert_eq!(lyrics.current(Duration::from_secs(2)), None);
        assert_eq!(lyrics.current(Duration::from_millis(2345)), Some("good"));
    }

    #[test]
    fn id3v1_is_used_as_fallback() {
        let mut bytes = vec![0u8; 128];
        bytes[..3].copy_from_slice(b"TAG");
        bytes[3..8].copy_from_slice(b"Title");
        bytes[33..39].copy_from_slice(b"Artist");
        bytes[63..68].copy_from_slice(b"Album");
        let result = parse_id3v1(&bytes);
        assert_eq!(result.title.as_deref(), Some("Title"));
        assert_eq!(result.artist.as_deref(), Some("Artist"));
        assert_eq!(result.album.as_deref(), Some("Album"));
    }
}
