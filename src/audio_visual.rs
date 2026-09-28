//! A transparent audio tap. Only bounded sample copies run on the audio thread;
//! frequency analysis belongs to the UI. Give each track its own `SharedAudio`.

use rodio::{source::SeekError, Source};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

pub const FFT_SIZE: usize = 2048;
const SNAPSHOT_MILLIS: u64 = 15;

#[derive(Clone)]
pub struct AudioFrame {
    /// The valid prefix is ordered from oldest to newest; the rest is zero.
    /// `samples` remains the left/mono channel for compatibility.
    pub samples: [f32; FFT_SIZE],
    /// The matching right-channel window. Mono sources mirror the left channel.
    pub right_samples: [f32; FFT_SIZE],
    pub len: usize,
    pub sample_rate: u32,
    pub sequence: u64,
}

impl Default for AudioFrame {
    fn default() -> Self {
        Self {
            samples: [0.0; FFT_SIZE],
            right_samples: [0.0; FFT_SIZE],
            len: 0,
            sample_rate: 0,
            sequence: 0,
        }
    }
}

pub type SharedAudio = Arc<Mutex<AudioFrame>>;

/// Bit zero enables capture, the other bits identify visibility epochs. Even a
/// quick off/on toggle between audio callbacks invalidates the old window.
#[derive(Clone)]
pub struct CaptureControl(Arc<AtomicU64>);

impl Default for CaptureControl {
    fn default() -> Self {
        Self(Arc::new(AtomicU64::new(1)))
    }
}

impl CaptureControl {
    pub fn set_enabled(&self, enabled: bool) {
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |state| {
                ((state & 1 != 0) != enabled)
                    .then(|| (state.wrapping_add(2) & !1) | u64::from(enabled))
            });
    }
}

/// Passes every sample through unchanged, capturing only channel zero.
///
/// The ring and publication counters are private to this source, never seeded
/// from a previous snapshot. Publication uses audio time (not wall-clock time),
/// at roughly 15 ms intervals. A busy or poisoned UI mutex never stalls audio.
pub struct VisualSource<S> {
    source: S,
    audio: SharedAudio,
    ring: [[f32; FFT_SIZE]; 2],
    len: usize,
    write: usize,
    channels: u16,
    channel: u16,
    sample_rate: u32,
    interval: usize,
    since_attempt: usize,
    dirty: bool,
    capture: CaptureControl,
    capture_epoch: u64,
}

impl<S: Source<Item = f32>> VisualSource<S> {
    #[cfg(test)]
    pub fn new(source: S, audio: SharedAudio) -> Self {
        Self::with_control(source, audio, CaptureControl::default())
    }

    pub fn with_control(source: S, audio: SharedAudio, capture: CaptureControl) -> Self {
        let channels = source.channels();
        let sample_rate = source.sample_rate();
        let mut result = Self {
            source,
            audio,
            ring: [[0.0; FFT_SIZE]; 2],
            len: 0,
            write: 0,
            channels,
            channel: 0,
            sample_rate,
            interval: Self::interval(sample_rate),
            since_attempt: 0,
            dirty: true,
            capture_epoch: capture.0.load(Ordering::Relaxed),
            capture,
        };
        // Clear even a reused snapshot, without waiting if the UI is reading.
        result.publish();
        result
    }

    fn interval(sample_rate: u32) -> usize {
        // u64 arithmetic also handles u32::MAX; at very low rates one sample
        // is already longer than the desired interval. Zero rates are ignored.
        (u64::from(sample_rate) * SNAPSHOT_MILLIS / 1000).max(1) as usize
    }

    fn reset(&mut self, channels: u16, sample_rate: u32) {
        self.len = 0;
        self.write = 0;
        self.channel = 0;
        self.channels = channels;
        self.sample_rate = sample_rate;
        self.interval = Self::interval(sample_rate);
        self.since_attempt = 0;
        self.dirty = true;
        // Old ring bytes are no longer valid; publish() only copies len bytes
        // and zeroes the tail. A failed try_lock leaves this new epoch dirty.
        self.publish();
    }

    fn publish(&mut self) {
        if !self.dirty {
            return;
        }
        let Ok(mut frame) = self.audio.try_lock() else {
            return;
        };
        let start = (self.write + FFT_SIZE - self.len) % FFT_SIZE;
        let first = self.len.min(FFT_SIZE - start);
        let AudioFrame {
            samples: left,
            right_samples: right,
            ..
        } = &mut *frame;
        for (output, ring) in [(left, &self.ring[0]), (right, &self.ring[1])] {
            output[..first].copy_from_slice(&ring[start..start + first]);
            if self.len > first {
                output[first..self.len].copy_from_slice(&ring[..self.len - first]);
            }
            output[self.len..].fill(0.0);
        }
        frame.len = self.len;
        frame.sample_rate = self.sample_rate;
        // Increment the shared revision, including clears, so reused snapshots
        // and seeks cannot accidentally look unchanged to the UI.
        frame.sequence = frame.sequence.wrapping_add(1);
        self.dirty = false;
    }
}

impl<S: Source<Item = f32>> Iterator for VisualSource<S> {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let epoch = self.capture.0.load(Ordering::Relaxed);
        if epoch != self.capture_epoch {
            // Do not reset channel alignment when toggling capture mid stereo frame.
            self.len = 0;
            self.write = 0;
            self.since_attempt = 0;
            self.dirty = true;
            self.capture_epoch = epoch;
            self.publish();
        }
        // A Source can expose the next format before next(), or only refill an
        // exhausted frame inside next(). Preserve the former sample's format
        // when next() itself advances past the last sample of a nonempty frame.
        let exhausted_frame = self.source.current_frame_len() == Some(0);
        let mut channels = self.source.channels();
        let mut sample_rate = self.source.sample_rate();
        let Some(sample) = self.source.next() else {
            // Includes short sources, incomplete final channel frames, and a
            // deferred seek clear at EOF. Retry on later next() calls if busy.
            self.publish();
            return None;
        };
        if exhausted_frame {
            channels = self.source.channels();
            sample_rate = self.source.sample_rate();
        }
        if channels != self.channels || sample_rate != self.sample_rate {
            // Never combine samples from different rates or channel layouts.
            self.reset(channels, sample_rate);
        }
        if channels != 0 && sample_rate != 0 {
            if epoch & 1 != 0 {
                self.ring[usize::from(self.channel.min(1))][self.write] = sample;
                if channels == 1 {
                    self.ring[1][self.write] = sample;
                }
                if self.channel == 0 {
                    self.write = (self.write + 1) % FFT_SIZE;
                    self.len = (self.len + 1).min(FFT_SIZE);
                    self.dirty = true;
                    self.since_attempt += 1;
                    if self.since_attempt >= self.interval {
                        // Keep attempts bounded even if a reader holds the lock.
                        self.since_attempt = 0;
                        self.publish();
                    }
                }
            }
            self.channel += 1;
            if self.channel == channels {
                self.channel = 0;
            }
        }
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.source.size_hint()
    }
}

impl<S: Source<Item = f32>> Source for VisualSource<S> {
    fn current_frame_len(&self) -> Option<usize> {
        self.source.current_frame_len()
    }

    fn channels(&self) -> u16 {
        self.source.channels()
    }

    fn sample_rate(&self) -> u32 {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        // A failed seek must not erase our history or change channel alignment.
        self.source.try_seek(pos)?;
        self.reset(self.source.channels(), self.source.sample_rate());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread};

    struct TestSource {
        samples: Vec<f32>,
        pos: usize,
        channels: u16,
        rate: u32,
        seekable: bool,
        last_seek: Option<Duration>,
    }

    impl TestSource {
        fn new(samples: Vec<f32>, channels: u16, rate: u32) -> Self {
            Self {
                samples,
                pos: 0,
                channels,
                rate,
                seekable: true,
                last_seek: None,
            }
        }
    }

    impl Iterator for TestSource {
        type Item = f32;

        fn next(&mut self) -> Option<f32> {
            let sample = *self.samples.get(self.pos)?;
            self.pos += 1;
            Some(sample)
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let remaining = self.samples.len() - self.pos;
            (remaining, Some(remaining))
        }
    }

    impl Source for TestSource {
        fn current_frame_len(&self) -> Option<usize> {
            Some(self.samples.len() - self.pos)
        }

        fn channels(&self) -> u16 {
            self.channels
        }

        fn sample_rate(&self) -> u32 {
            self.rate
        }

        fn total_duration(&self) -> Option<Duration> {
            let denominator = u64::from(self.rate) * u64::from(self.channels);
            (denominator != 0)
                .then(|| Duration::from_secs_f64(self.samples.len() as f64 / denominator as f64))
        }

        fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
            self.last_seek = Some(pos);
            if !self.seekable {
                return Err(SeekError::NotSupported {
                    underlying_source: "TestSource",
                });
            }
            let frames = (pos.as_secs_f64() * f64::from(self.rate)).round() as usize;
            self.pos = frames
                .saturating_mul(usize::from(self.channels))
                .min(self.samples.len());
            Ok(())
        }
    }

    fn snapshot(audio: &SharedAudio) -> AudioFrame {
        audio.lock().unwrap().clone()
    }

    fn assert_samples(frame: &AudioFrame, expected: &[f32], rate: u32) {
        assert_eq!(frame.len, expected.len());
        assert_eq!(&frame.samples[..frame.len], expected);
        assert!(frame.samples[frame.len..].iter().all(|&s| s == 0.0));
        assert_eq!(frame.sample_rate, rate);
    }

    #[test]
    fn disabled_capture_stops_snapshots_and_reenable_discards_old_samples() {
        let audio = SharedAudio::default();
        let control = CaptureControl::default();
        let samples: Vec<_> = (0..6000).map(|i| i as f32).collect();
        let source = TestSource::new(samples.clone(), 2, 1000);
        let mut tap = VisualSource::with_control(source, audio.clone(), control.clone());
        let mut output = Vec::new();
        output.extend(tap.by_ref().take(101));
        control.set_enabled(false);
        output.push(tap.next().unwrap());
        let sequence = audio.lock().unwrap().sequence;
        assert_eq!(audio.lock().unwrap().len, 0);
        output.extend(tap.by_ref().take(2000));
        assert_eq!(audio.lock().unwrap().sequence, sequence);
        control.set_enabled(true);
        output.extend(tap.by_ref().take(30));
        let frame = audio.lock().unwrap().clone();
        assert_eq!(frame.len, 15);
        assert_eq!(frame.samples[0], 2102.0);
        assert!(frame.samples[..frame.len]
            .iter()
            .all(|sample| *sample >= 2102.0));
        control.set_enabled(false);
        control.set_enabled(true); // No callback between toggles: epoch must still reset.
        output.extend(tap.by_ref().take(30));
        let frame = audio.lock().unwrap().clone();
        assert_eq!(frame.len, 15);
        assert_eq!(frame.samples[0], 2132.0);
        output.extend(tap);
        assert_eq!(output, samples);
    }

    #[test]
    fn shared_frame_supports_default_and_clone() {
        let audio = SharedAudio::default();
        let frame = snapshot(&audio);
        assert_samples(&frame, &[], 0);
        assert_eq!(frame.sequence, 0);
        assert_eq!(FFT_SIZE, 2048);
    }

    #[test]
    fn output_is_bit_exact_even_for_nonfinite_samples() {
        let samples = vec![
            0.0,
            -0.0,
            f32::from_bits(0x7fc0_1234),
            f32::INFINITY,
            f32::NEG_INFINITY,
            -3.5,
            0.125,
        ];
        let audio = SharedAudio::default();
        let source = TestSource::new(samples.clone(), 1, 48_000);
        let actual: Vec<_> = VisualSource::new(source, audio.clone()).collect();
        assert_eq!(
            actual.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
            samples.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
        );
        assert_eq!(
            snapshot(&audio).samples[..samples.len()]
                .iter()
                .map(|s| s.to_bits())
                .collect::<Vec<_>>(),
            samples.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn stereo_captures_only_first_channel_including_partial_last_frame() {
        let samples = vec![1.0, -10.0, 2.0, -20.0, 3.0, -30.0, 4.0];
        let audio = SharedAudio::default();
        let output: Vec<_> =
            VisualSource::new(TestSource::new(samples.clone(), 2, 48_000), audio.clone()).collect();
        assert_eq!(output, samples);
        let frame = snapshot(&audio);
        assert_samples(&frame, &[1.0, 2.0, 3.0, 4.0], 48_000);
        assert_eq!(
            &frame.right_samples[..frame.len],
            &[0.0, -10.0, -20.0, -30.0]
        );
    }

    #[test]
    fn mono_audio_mirrors_into_both_visual_channels() {
        let audio = SharedAudio::default();
        let samples = [0.25, -0.5, 0.75];
        let _: Vec<_> =
            VisualSource::new(TestSource::new(samples.to_vec(), 1, 48_000), audio.clone())
                .collect();
        let frame = snapshot(&audio);
        assert_eq!(&frame.samples[..frame.len], &samples);
        assert_eq!(&frame.right_samples[..frame.len], &samples);
    }

    #[test]
    fn latest_window_stays_bounded_and_chronological_across_wraps() {
        let count = FFT_SIZE * 3 + 97;
        let samples: Vec<_> = (0..count).map(|i| i as f32).collect();
        let audio = SharedAudio::default();
        let mut source =
            VisualSource::new(TestSource::new(samples.clone(), 1, 1000), audio.clone());
        for consumed in 1..=count {
            assert_eq!(source.next(), Some(samples[consumed - 1]));
            if consumed % 15 == 0 {
                assert_samples(
                    &snapshot(&audio),
                    &samples[consumed.saturating_sub(FFT_SIZE)..consumed],
                    1000,
                );
            }
        }
        assert_eq!(source.next(), None);
        assert_samples(&snapshot(&audio), &samples[count - FFT_SIZE..], 1000);
        let sequence = snapshot(&audio).sequence;
        assert_eq!(source.next(), None);
        assert_eq!(snapshot(&audio).sequence, sequence);
    }

    #[test]
    fn publication_uses_about_fifteen_ms_of_first_channel_samples() {
        for rate in [1000, 44_100, 48_000, 96_000] {
            let interval = VisualSource::<TestSource>::interval(rate);
            let audio = SharedAudio::default();
            let mut source = VisualSource::new(
                TestSource::new(vec![0.5; interval * 4], 2, rate),
                audio.clone(),
            );
            let initial = snapshot(&audio).sequence;
            // No publication before interval first-channel samples, regardless
            // of how quickly the iterator is driven by this test.
            for _ in 0..(interval - 1) * 2 {
                source.next();
            }
            assert_eq!(snapshot(&audio).sequence, initial);
            source.next();
            let frame = snapshot(&audio);
            assert_eq!(frame.sequence, initial.wrapping_add(1));
            assert_eq!(frame.len, interval.min(FFT_SIZE));
            assert!((10..=20).contains(&(interval * 1000 / rate as usize)));
        }
    }

    #[test]
    fn short_source_publishes_at_eof_not_only_at_full_windows() {
        let audio = SharedAudio::default();
        let mut source = VisualSource::new(
            TestSource::new(vec![0.25, -0.5, 0.75], 1, 48_000),
            audio.clone(),
        );
        for _ in 0..3 {
            source.next();
        }
        assert_eq!(snapshot(&audio).len, 0);
        assert_eq!(source.next(), None);
        assert_samples(&snapshot(&audio), &[0.25, -0.5, 0.75], 48_000);
    }

    #[test]
    fn reused_snapshot_and_empty_source_never_publish_previous_track() {
        let audio = SharedAudio::default();
        VisualSource::new(TestSource::new(vec![9.0; 24], 1, 1000), audio.clone()).for_each(drop);
        let previous = snapshot(&audio).sequence;
        let mut empty = VisualSource::new(TestSource::new(vec![], 1, 48_000), audio.clone());
        assert_samples(&snapshot(&audio), &[], 48_000);
        assert_ne!(snapshot(&audio).sequence, previous);
        assert_eq!(empty.next(), None);
        let output: Vec<_> =
            VisualSource::new(TestSource::new(vec![1.0, 2.0], 1, 48_000), audio.clone()).collect();
        assert_eq!(output, [1.0, 2.0]);
        assert_samples(&snapshot(&audio), &[1.0, 2.0], 48_000);
    }

    #[test]
    fn locked_snapshot_does_not_block_creation_iteration_or_eof() {
        let audio = SharedAudio::default();
        let guard = audio.lock().unwrap();
        let worker_audio = audio.clone();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut source = VisualSource::new(
                TestSource::new((0..4097).map(|i| i as f32).collect(), 1, 1000),
                worker_audio,
            );
            let count = source.by_ref().count();
            tx.send((source, count)).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(2));
        // Always release the lock before joining, so a blocking regression
        // fails the test rather than leaving the suite permanently hung.
        drop(guard);
        worker.join().unwrap();
        let (mut source, count) = result.expect("audio tap waited for the snapshot mutex");
        assert_eq!(count, 4097);
        assert_eq!(source.next(), None); // Retry a final snapshot skipped at EOF.
        let expected: Vec<_> = (4097 - FFT_SIZE..4097).map(|i| i as f32).collect();
        assert_samples(&snapshot(&audio), &expected, 1000);
    }

    #[test]
    fn successful_seek_clears_history_and_shared_snapshot() {
        let audio = SharedAudio::default();
        let mut source = VisualSource::new(
            TestSource::new((0..64).map(|i| i as f32).collect(), 1, 1000),
            audio.clone(),
        );
        for _ in 0..30 {
            source.next();
        }
        let sequence = snapshot(&audio).sequence;
        let target = Duration::from_millis(50);
        source.try_seek(target).unwrap();
        assert_eq!(source.source.last_seek, Some(target));
        assert_samples(&snapshot(&audio), &[], 1000);
        assert_ne!(snapshot(&audio).sequence, sequence);
        let output: Vec<_> = source.collect();
        let expected: Vec<_> = (50..64).map(|i| i as f32).collect();
        assert_eq!(output, expected);
        assert_samples(&snapshot(&audio), &expected, 1000);
    }

    #[test]
    fn failed_seek_preserves_snapshot_history_and_channel_alignment() {
        let audio = SharedAudio::default();
        let samples: Vec<_> = (0..40).flat_map(|i| [i as f32, -99.0]).collect();
        let mut input = TestSource::new(samples, 2, 1000);
        input.seekable = false;
        let mut source = VisualSource::new(input, audio.clone());
        for _ in 0..35 {
            source.next();
        }
        let before = snapshot(&audio);
        let target = Duration::from_millis(25);
        let error = source.try_seek(target).unwrap_err();
        assert!(matches!(
            error,
            SeekError::NotSupported {
                underlying_source: "TestSource"
            }
        ));
        assert_eq!(source.source.last_seek, Some(target));
        let after = snapshot(&audio);
        assert_eq!(before.samples, after.samples);
        assert_eq!(before.len, after.len);
        assert_eq!(before.sample_rate, after.sample_rate);
        assert_eq!(before.sequence, after.sequence);
        source.for_each(drop);
        let expected: Vec<_> = (0..40).map(|i| i as f32).collect();
        assert_samples(&snapshot(&audio), &expected, 1000);
    }

    #[test]
    fn contended_seek_never_blocks_or_republishes_pre_seek_samples() {
        for seek_to_end in [false, true] {
            let audio = SharedAudio::default();
            let mut source = VisualSource::new(
                TestSource::new((0..64).map(|i| i as f32).collect(), 1, 1000),
                audio.clone(),
            );
            for _ in 0..30 {
                source.next();
            }
            let guard = audio.lock().unwrap();
            let (tx, rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let millis = if seek_to_end { 64 } else { 50 };
                source.try_seek(Duration::from_millis(millis)).unwrap();
                source.by_ref().for_each(drop);
                tx.send(source).unwrap();
            });
            let result = rx.recv_timeout(Duration::from_secs(2));
            drop(guard);
            worker.join().unwrap();
            let mut source = result.expect("seek or post-seek audio waited for the mutex");
            source.next();
            let expected: Vec<_> = if seek_to_end {
                vec![]
            } else {
                (50..64).map(|i| i as f32).collect()
            };
            assert_samples(&snapshot(&audio), &expected, 1000);
        }
    }

    #[test]
    fn metadata_and_size_hint_are_transparently_forwarded() {
        let audio = SharedAudio::default();
        let mut source = VisualSource::new(TestSource::new(vec![1.0; 12], 3, 8000), audio);
        for _ in 0..14 {
            assert_eq!(source.size_hint(), source.source.size_hint());
            assert_eq!(
                source.current_frame_len(),
                source.source.current_frame_len()
            );
            assert_eq!(source.channels(), 3);
            assert_eq!(source.sample_rate(), 8000);
            assert_eq!(source.total_duration(), source.source.total_duration());
            assert_eq!(source.total_duration(), Some(Duration::from_micros(500)));
            source.next();
        }
        source.try_seek(Duration::ZERO).unwrap();
        assert_eq!(source.size_hint(), (12, Some(12)));
        assert_eq!(source.current_frame_len(), Some(12));
    }

    struct Segment {
        channels: u16,
        rate: u32,
        samples: Vec<f32>,
    }

    struct ChangingSource {
        segments: Vec<Segment>,
        segment: usize,
        pos: usize,
        eager: bool,
    }

    impl ChangingSource {
        fn advance(&mut self) {
            while self
                .segments
                .get(self.segment)
                .is_some_and(|s| self.pos == s.samples.len())
            {
                self.segment += 1;
                self.pos = 0;
            }
        }
    }

    impl Iterator for ChangingSource {
        type Item = f32;

        fn next(&mut self) -> Option<f32> {
            self.advance();
            let sample = *self.segments.get(self.segment)?.samples.get(self.pos)?;
            self.pos += 1;
            if self.eager {
                self.advance();
            }
            Some(sample)
        }
    }

    impl Source for ChangingSource {
        fn current_frame_len(&self) -> Option<usize> {
            self.segments
                .get(self.segment)
                .map(|s| s.samples.len() - self.pos)
        }

        fn channels(&self) -> u16 {
            self.segments.get(self.segment).map_or(0, |s| s.channels)
        }

        fn sample_rate(&self) -> u32 {
            self.segments.get(self.segment).map_or(0, |s| s.rate)
        }

        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    #[test]
    fn format_changes_reset_window_for_eager_and_lazy_sources() {
        for eager in [false, true] {
            for (channels, rate, samples, expected) in [
                (1, 1000, vec![7.0, 8.0, 9.0], vec![7.0, 8.0, 9.0]),
                (2, 2000, vec![7.0, -7.0, 8.0, -8.0], vec![7.0, 8.0]),
                (1, 2000, vec![7.0, 8.0, 9.0], vec![7.0, 8.0, 9.0]),
            ] {
                let audio = SharedAudio::default();
                let mut all = vec![1.0, -1.0, 2.0, -2.0];
                all.extend_from_slice(&samples);
                let input = ChangingSource {
                    segments: vec![
                        Segment {
                            channels: 2,
                            rate: 1000,
                            samples: vec![1.0, -1.0, 2.0, -2.0],
                        },
                        Segment {
                            channels,
                            rate,
                            samples,
                        },
                    ],
                    segment: 0,
                    pos: 0,
                    eager,
                };
                let mut source = VisualSource::new(input, audio.clone());
                let mut output = Vec::new();
                loop {
                    assert_eq!(source.channels(), source.source.channels());
                    assert_eq!(source.sample_rate(), source.source.sample_rate());
                    assert_eq!(
                        source.current_frame_len(),
                        source.source.current_frame_len()
                    );
                    assert_eq!(source.total_duration(), None);
                    assert_eq!(source.size_hint(), (0, None));
                    match source.next() {
                        Some(sample) => output.push(sample),
                        None => break,
                    }
                }
                assert_eq!(output, all);
                assert_samples(&snapshot(&audio), &expected, rate);
            }
        }
    }

    #[test]
    fn invalid_and_extreme_parameters_remain_safe_and_transparent() {
        for (channels, rate) in [(0, 0), (0, 48_000), (2, 0), (u16::MAX, u32::MAX), (1, 1)] {
            let samples = vec![1.0, 2.0, 3.0];
            let audio = SharedAudio::default();
            let source = VisualSource::new(
                TestSource::new(samples.clone(), channels, rate),
                audio.clone(),
            );
            assert_eq!(source.channels(), channels);
            assert_eq!(source.sample_rate(), rate);
            assert_eq!(source.collect::<Vec<_>>(), samples);
            let expected: &[f32] = if channels == 0 || rate == 0 {
                &[]
            } else if channels == 1 {
                &[1.0, 2.0, 3.0]
            } else {
                &[1.0]
            };
            assert_samples(&snapshot(&audio), expected, rate);
        }
    }
}
