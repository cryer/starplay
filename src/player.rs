use crate::{
    audio_visual::{CaptureControl, SharedAudio, VisualSource},
    library::Track,
};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};
use std::{fs::File, io::BufReader, path::Path, sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    Off,
    All,
    One,
}
impl Repeat {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::All => "all",
            Self::One => "one",
        }
    }
}

pub fn next_index(current: usize, count: usize, repeat: Repeat) -> Option<usize> {
    if count == 0 || current >= count {
        return None;
    }
    match repeat {
        Repeat::One => Some(current),
        _ if current + 1 < count => Some(current + 1),
        Repeat::All => Some(0),
        _ => None,
    }
}

fn decode(path: &Path) -> Result<Decoder<BufReader<File>>, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    Decoder::new(BufReader::new(file)).map_err(|e| format!("Cannot decode {}: {e}", path.display()))
}

/// A decoder with its first sample buffered, so an empty file is detected without
/// decoding twice; Source metadata is forwarded from the decoder.
struct PeekedSource {
    first: Option<i16>,
    inner: Decoder<BufReader<File>>,
}

impl PeekedSource {
    fn new(path: &Path) -> Result<Self, String> {
        let mut inner = decode(path)?;
        let first = inner.next();
        Ok(Self { first, inner })
    }
}

impl Iterator for PeekedSource {
    type Item = i16;
    fn next(&mut self) -> Option<Self::Item> {
        self.first.take().or_else(|| self.inner.next())
    }
}

impl Source for PeekedSource {
    fn current_frame_len(&self) -> Option<usize> {
        self.inner
            .current_frame_len()
            .map(|len| len + usize::from(self.first.is_some()))
    }
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        // A seek moves past the start; the buffered first sample no longer applies.
        self.inner.try_seek(pos)?;
        self.first = None;
        Ok(())
    }
}

pub struct AudioInfo {
    pub channels: u16,
    pub sample_rate: u32,
    pub samples: u64,
    pub duration: Duration,
}

pub fn check_audio(path: &Path) -> Result<AudioInfo, String> {
    let decoder = decode(path)?;
    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    if channels == 0 || sample_rate == 0 {
        return Err("Invalid audio stream parameters.".into());
    }
    let samples = decoder.fold(0u64, |count, _| count + 1);
    if samples == 0 {
        return Err("The decoder produced no audio samples.".into());
    }
    Ok(AudioInfo {
        channels,
        sample_rate,
        samples,
        duration: Duration::from_secs_f64(
            samples as f64 / f64::from(channels) / f64::from(sample_rate),
        ),
    })
}

pub struct Player {
    // Keep the stream alive until after its sink is dropped.
    sink: Option<Sink>,
    handle: OutputStreamHandle,
    _stream: OutputStream,
    pub tracks: Arc<Vec<Track>>,
    pub current: Option<usize>,
    pub duration: Option<Duration>,
    pub volume: u8,
    pub repeat: Repeat,
    pub finished: bool,
    pub audio: SharedAudio,
    pub capture: CaptureControl,
}

impl Player {
    pub fn new(tracks: Arc<Vec<Track>>, volume: u8) -> Result<Self, String> {
        let (stream, handle) = OutputStream::try_default()
            .map_err(|e| format!("Cannot open the default audio output device: {e}"))?;
        Ok(Self {
            sink: None,
            handle,
            _stream: stream,
            tracks,
            current: None,
            duration: None,
            volume: volume.min(100),
            repeat: Repeat::Off,
            finished: true,
            audio: SharedAudio::default(),
            capture: CaptureControl::default(),
        })
    }

    pub fn play(&mut self, index: usize) -> Result<(), String> {
        let track = self
            .tracks
            .get(index)
            .ok_or("Track index is out of range.")?;
        let source = PeekedSource::new(&track.path)?;
        if source.first.is_none() {
            return Err("The file contains no playable audio samples.".into());
        }
        let duration = source.total_duration();
        let sink = Sink::try_new(&self.handle).map_err(|e| e.to_string())?;
        sink.pause();
        sink.set_volume(f32::from(self.volume) / 100.0);
        // Each track owns its snapshot, so a stopped source cannot publish into the next song.
        let audio = SharedAudio::default();
        sink.append(VisualSource::with_control(
            source.convert_samples::<f32>(),
            audio.clone(),
            self.capture.clone(),
        ));
        if let Some(old) = self.sink.take() {
            old.stop();
        }
        self.sink = Some(sink);
        self.audio = audio;
        self.current = Some(index);
        self.duration = duration;
        self.finished = false;
        self.sink.as_ref().unwrap().play();
        Ok(())
    }

    pub fn paused(&self) -> bool {
        self.sink.as_ref().is_some_and(Sink::is_paused)
    }
    pub fn position(&self) -> Duration {
        if self.finished {
            return self.duration.unwrap_or_default();
        }
        self.sink.as_ref().map(Sink::get_pos).unwrap_or_default()
    }
    pub fn toggle_pause(&mut self, selected: usize) -> Result<(), String> {
        if self.finished || self.sink.is_none() {
            return self.play(selected);
        }
        if let Some(sink) = &self.sink {
            if sink.is_paused() {
                sink.play();
            } else {
                sink.pause();
            }
        }
        Ok(())
    }
    pub fn change_volume(&mut self, delta: i16) {
        self.volume = (i16::from(self.volume) + delta).clamp(0, 100) as u8;
        if let Some(sink) = &self.sink {
            sink.set_volume(f32::from(self.volume) / 100.0);
        }
    }
    pub fn seek(&self, seconds: i64) -> Result<(), String> {
        if self.finished {
            return Err("No active track to seek.".into());
        }
        let sink = self
            .sink
            .as_ref()
            .ok_or_else(|| "No active track to seek.".to_string())?;
        let pos = sink.get_pos();
        let step = Duration::from_secs(seconds.unsigned_abs());
        let mut target = if seconds < 0 {
            pos.saturating_sub(step)
        } else {
            pos.saturating_add(step)
        };
        if let Some(duration) = self.duration {
            target = target.min(duration);
        }
        sink.try_seek(target)
            .map_err(|e| format!("Seeking unavailable: {e}"))
    }
    /// Mark EOF without choosing the next track; navigation belongs to the worker.
    pub fn poll_finished(&mut self) -> bool {
        if self.finished || !self.sink.as_ref().is_some_and(Sink::empty) {
            return false;
        }
        self.finished = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeat_cycle() {
        assert_eq!(Repeat::Off.next(), Repeat::All);
        assert_eq!(Repeat::All.next(), Repeat::One);
        assert_eq!(Repeat::One.next(), Repeat::Off);
    }
    #[test]
    fn playlist_boundaries() {
        assert_eq!(next_index(0, 2, Repeat::Off), Some(1));
        assert_eq!(next_index(1, 2, Repeat::Off), None);
        assert_eq!(next_index(1, 2, Repeat::All), Some(0));
        assert_eq!(next_index(1, 2, Repeat::One), Some(1));
        assert_eq!(next_index(0, 0, Repeat::All), None);
        assert_eq!(next_index(4, 2, Repeat::All), None);
        assert_eq!(next_index(0, 1, Repeat::All), Some(0));
    }
}
