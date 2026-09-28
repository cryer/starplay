use crate::audio_visual::{AudioFrame, SharedAudio, FFT_SIZE};
use std::{f32::consts::PI, sync::Arc, time::Duration};

const BANDS: usize = 24;
const MAX_RIPPLES: usize = 6;

#[derive(Clone, Copy, Default)]
struct Ripple {
    radius: f32,
    strength: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VisualMode {
    #[default]
    Spectrum,
    Pulse,
    Off,
}

impl VisualMode {
    pub fn next(self) -> Self {
        match self {
            Self::Spectrum => Self::Pulse,
            Self::Pulse => Self::Off,
            Self::Off => Self::Spectrum,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Spectrum => "SPECTRUM",
            Self::Pulse => "PULSE",
            Self::Off => "OFF",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spectrum => "spectrum",
            Self::Pulse => "pulse",
            Self::Off => "off",
        }
    }
}

/// Reused across frames, including resizes; rendering does not allocate a new grid each tick.
#[derive(Default)]
pub struct VisualCanvas {
    grid: Vec<Vec<char>>,
    pub rows: Vec<String>,
}

/// All analysis runs on the UI thread; the audio callback only copies a bounded window.
pub struct Visualizer {
    pub mode: VisualMode,
    bands: [f32; BANDS],
    peaks: [f32; BANDS],
    targets: [f32; BANDS],
    energy: f32,
    energy_target: f32,
    energy_floor: f32,
    previous_energy: f32,
    beat_cooldown: f32,
    ripples: [Ripple; MAX_RIPPLES],
    source: Option<SharedAudio>,
    sequence: Option<u64>,
    stale: f32,
}

impl Default for Visualizer {
    fn default() -> Self {
        Self {
            mode: VisualMode::default(),
            bands: [0.0; BANDS],
            peaks: [0.0; BANDS],
            targets: [0.0; BANDS],
            energy: 0.0,
            energy_target: 0.0,
            energy_floor: 0.0,
            previous_energy: 0.0,
            beat_cooldown: 0.0,
            ripples: [Ripple::default(); MAX_RIPPLES],
            source: None,
            sequence: None,
            stale: 0.0,
        }
    }
}

impl Visualizer {
    /// Keep fast refresh only while visible motion remains after pausing/muting.
    pub fn is_animating(&self) -> bool {
        match self.mode {
            VisualMode::Spectrum => self
                .bands
                .iter()
                .chain(self.peaks.iter())
                .any(|v| *v > 0.002),
            VisualMode::Pulse => {
                self.energy > 0.002 || self.ripples.iter().any(|r| r.strength > 0.015)
            }
            VisualMode::Off => false,
        }
    }

    pub fn reset(&mut self) {
        *self = Self {
            mode: self.mode,
            ..Self::default()
        };
    }

    /// On visibility changes, don't animate a snapshot captured before the new epoch.
    pub fn wait_for_fresh_audio(&mut self, audio: &SharedAudio) {
        self.reset();
        self.source = Some(audio.clone());
        self.sequence = audio.try_lock().ok().map(|frame| frame.sequence);
    }

    pub fn update(&mut self, audio: &SharedAudio, active: bool, volume: u8, elapsed: Duration) {
        if !self
            .source
            .as_ref()
            .is_some_and(|previous| Arc::ptr_eq(previous, audio))
        {
            self.reset();
            self.source = Some(audio.clone());
        }
        let dt = elapsed.as_secs_f32().min(0.25);
        self.stale += elapsed.as_secs_f32();
        self.beat_cooldown = (self.beat_cooldown - dt).max(0.0);
        let volume = f32::from(volume.min(100)) / 100.0;
        let mut beat = false;
        if self.mode != VisualMode::Off && active {
            // Release the lock before FFT work. A busy audio thread never holds up drawing.
            let frame = audio
                .try_lock()
                .ok()
                .and_then(|frame| (Some(frame.sequence) != self.sequence).then(|| frame.clone()));
            if let Some(frame) = frame {
                self.sequence = Some(frame.sequence);
                self.stale = 0.0;
                // Only analyze what is on screen. Pulse mode needs no FFT.
                match self.mode {
                    VisualMode::Spectrum => self.targets = spectrum(&frame),
                    VisualMode::Pulse => {
                        let energy = rms(&frame);
                        // Relative onset detection plus a noise gate and refractory period.
                        // A sustained tone must not manufacture a stream of fake beats.
                        beat = volume > 0.0
                            && self.beat_cooldown == 0.0
                            && energy > 0.012
                            && energy > self.energy_floor * 1.4 + 0.008
                            && energy > self.previous_energy * 1.18 + 0.005;
                        self.energy_floor +=
                            (energy - self.energy_floor) * (1.0 - (-3.0 * dt).exp());
                        self.previous_energy = energy;
                        self.energy_target = (energy * 3.0).min(1.0);
                    }
                    VisualMode::Off => unreachable!(),
                }
            }
        }
        let gain = if active && self.mode != VisualMode::Off && self.stale < 0.3 {
            volume
        } else {
            0.0
        };
        for i in 0..BANDS {
            let target = self.targets[i] * gain;
            let speed = if target > self.bands[i] { 26.0 } else { 8.0 };
            self.bands[i] += (target - self.bands[i]) * (1.0 - (-speed * dt).exp());
            self.peaks[i] = (self.peaks[i] - dt * 0.42).max(self.bands[i]);
        }
        self.energy += (self.energy_target * gain - self.energy) * (1.0 - (-12.0 * dt).exp());
        if gain == 0.0 {
            self.energy_floor *= (-3.0 * dt).exp();
            self.previous_energy *= (-12.0 * dt).exp();
        }
        for ripple in &mut self.ripples {
            ripple.radius += dt * 1.15;
            ripple.strength *= (-dt * if gain > 0.0 { 1.8 } else { 10.0 }).exp();
            if gain > 0.0 {
                ripple.strength = ripple.strength.min(gain);
            }
            if ripple.radius > 1.6 || ripple.strength < 0.015 {
                *ripple = Ripple::default();
            }
        }
        if beat {
            let ripple = self
                .ripples
                .iter_mut()
                .min_by(|a, b| a.strength.total_cmp(&b.strength))
                .unwrap();
            *ripple = Ripple {
                radius: 0.05,
                strength: self.energy_target.sqrt() * gain,
            };
            self.beat_cooldown = 0.18;
        }
    }

    #[cfg(test)]
    pub fn render(&self, width: usize, height: usize) -> Vec<String> {
        let mut canvas = VisualCanvas::default();
        self.render_into(width, height, &mut canvas);
        canvas.rows
    }

    /// ASCII glyphs remain usable in CMD and terminals without block-character fonts.
    pub fn render_into(&self, width: usize, height: usize, canvas: &mut VisualCanvas) {
        if width == 0 || height == 0 || self.mode == VisualMode::Off {
            canvas.rows.clear();
            return;
        }
        canvas.grid.resize_with(height, Vec::new);
        for row in &mut canvas.grid {
            row.resize(width, ' ');
            row.fill(' ');
        }
        let grid = &mut canvas.grid;
        match self.mode {
            VisualMode::Spectrum => {
                let count = BANDS.min(width.div_ceil(2));
                for bar in 0..count {
                    let lo = bar * BANDS / count;
                    let hi = ((bar + 1) * BANDS / count).max(lo + 1);
                    let level = self.bands[lo..hi].iter().copied().fold(0.0, f32::max);
                    let peak = self.peaks[lo..hi].iter().copied().fold(0.0, f32::max);
                    let filled = (level * height as f32).round() as usize;
                    let peak_row = height.saturating_sub((peak * height as f32).ceil() as usize);
                    let start = bar * width / count;
                    let end = ((bar + 1) * width / count)
                        .saturating_sub(1)
                        .max(start + 1)
                        .min(width);
                    for (row, cells) in grid.iter_mut().enumerate() {
                        let visible = row >= height.saturating_sub(filled)
                            || (peak > 0.02 && row == peak_row);
                        if visible {
                            // Air between dots keeps wide bars from becoming solid bands.
                            for col in (start..end).step_by(2) {
                                cells[col] = '.';
                            }
                        }
                    }
                }
            }
            VisualMode::Pulse => {
                let center_x = (width - 1) as f32 / 2.0;
                let center_y = (height - 1) as f32 / 2.0;
                let thickness = (1.4 / height as f32).min(0.45);
                for (y, row) in grid.iter_mut().enumerate() {
                    for (x, cell) in row.iter_mut().enumerate() {
                        // Normalized coordinates keep the pulse centered after resizing.
                        let dx = (x as f32 - center_x) / (width as f32 / 2.0).max(1.0);
                        let dy = (y as f32 - center_y) / (height as f32 / 2.0).max(1.0);
                        let distance = dx.hypot(dy);
                        let core =
                            self.energy * (1.0 - distance / (0.1 + self.energy * 0.38)).max(0.0);
                        let brightness = self.ripples.iter().fold(core, |value, ripple| {
                            let edge =
                                (1.0 - (distance - ripple.radius).abs() / thickness).max(0.0);
                            value.max(edge * ripple.strength)
                        });
                        *cell = glow(brightness);
                    }
                }
            }
            VisualMode::Off => unreachable!(),
        }
        canvas.rows.resize_with(height, String::new);
        for (output, row) in canvas.rows.iter_mut().zip(grid.iter()) {
            output.clear();
            output.extend(row.iter());
        }
    }
}

// Intensity ramp for pulse rings and their central glow.
fn glow(brightness: f32) -> char {
    match brightness {
        v if v > 0.8 => '@',
        v if v > 0.5 => 'O',
        v if v > 0.3 => '*',
        v if v > 0.15 => ':',
        v if v > 0.04 => '.',
        _ => ' ',
    }
}

fn clean(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// Broadband signal energy, excluding DC and invalid decoder samples.
fn rms(frame: &AudioFrame) -> f32 {
    let len = frame.len.min(FFT_SIZE);
    if len < 2 || frame.sample_rate == 0 {
        return 0.0;
    }
    let mean = frame.samples[..len].iter().copied().map(clean).sum::<f32>() / len as f32;
    (frame.samples[..len]
        .iter()
        .copied()
        .map(|sample| (clean(sample) - mean).powi(2))
        .sum::<f32>()
        / len as f32)
        .sqrt()
        .min(1.0)
}

fn spectrum(frame: &AudioFrame) -> [f32; BANDS] {
    let mut result = [0.0; BANDS];
    let len = frame.len.min(FFT_SIZE);
    if len < 2 || frame.sample_rate == 0 {
        return result;
    }
    let mut real = [0.0; FFT_SIZE];
    let mut imag = [0.0; FFT_SIZE];
    let mean = frame.samples[..len].iter().copied().map(clean).sum::<f32>() / len as f32;
    let mut window_sum = 0.0;
    for (i, output) in real.iter_mut().take(len).enumerate() {
        let window = 0.5 * (1.0 - (2.0 * PI * i as f32 / (len - 1) as f32).cos());
        *output = (clean(frame.samples[i]) - mean) * window;
        window_sum += window;
    }
    fft(&mut real, &mut imag);
    let upper = (frame.sample_rate as f32 / 2.0).min(16_000.0);
    if upper <= 40.0 {
        return result;
    }
    for (band, output) in result.iter_mut().enumerate() {
        let lower_hz = 40.0 * (upper / 40.0).powf(band as f32 / BANDS as f32);
        let upper_hz = 40.0 * (upper / 40.0).powf((band + 1) as f32 / BANDS as f32);
        let lo = ((lower_hz * FFT_SIZE as f32 / frame.sample_rate as f32).ceil() as usize)
            .clamp(1, FFT_SIZE / 2);
        let hi = ((upper_hz * FFT_SIZE as f32 / frame.sample_rate as f32).ceil() as usize)
            .clamp(lo + 1, FFT_SIZE / 2 + 1);
        let magnitude = (lo..hi)
            .map(|bin| real[bin].hypot(imag[bin]))
            .fold(0.0, f32::max)
            * 2.0
            / window_sum.max(1.0);
        *output = ((20.0 * magnitude.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
    }
    result
}

/// In-place radix-2 FFT; fixed-size stack buffers avoid per-frame transform allocations.
fn fft(real: &mut [f32; FFT_SIZE], imag: &mut [f32; FFT_SIZE]) {
    for i in 0..FFT_SIZE {
        let j = i.reverse_bits() >> (usize::BITS - FFT_SIZE.trailing_zeros());
        if i < j {
            real.swap(i, j);
            imag.swap(i, j);
        }
    }
    let mut size = 2;
    while size <= FFT_SIZE {
        let half = size / 2;
        let angle = -2.0 * PI / size as f32;
        let (sin, cos) = angle.sin_cos();
        for start in (0..FFT_SIZE).step_by(size) {
            let (mut wr, mut wi) = (1.0, 0.0);
            for offset in 0..half {
                let a = start + offset;
                let b = a + half;
                let tr = wr * real[b] - wi * imag[b];
                let ti = wr * imag[b] + wi * real[b];
                real[b] = real[a] - tr;
                imag[b] = imag[a] - ti;
                real[a] += tr;
                imag[a] += ti;
                (wr, wi) = (wr * cos - wi * sin, wr * sin + wi * cos);
            }
        }
        size *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(bin: usize) -> AudioFrame {
        let mut frame = AudioFrame {
            len: FFT_SIZE,
            sample_rate: 48_000,
            sequence: 1,
            ..AudioFrame::default()
        };
        for (i, sample) in frame.samples.iter_mut().enumerate() {
            *sample = (2.0 * PI * bin as f32 * i as f32 / FFT_SIZE as f32).sin() * 0.8;
        }
        frame
    }

    #[test]
    fn fft_resolves_a_known_tone() {
        let mut real = tone(32).samples;
        let mut imag = [0.0; FFT_SIZE];
        fft(&mut real, &mut imag);
        let strongest = (1..FFT_SIZE / 2)
            .max_by(|&a, &b| real[a].hypot(imag[a]).total_cmp(&real[b].hypot(imag[b])))
            .unwrap();
        assert_eq!(strongest, 32);
        assert!((imag[32].abs() - FFT_SIZE as f32 * 0.4).abs() < 0.1);
    }

    #[test]
    fn silence_dc_and_invalid_frames_stay_quiet() {
        assert_eq!(spectrum(&AudioFrame::default()), [0.0; BANDS]);
        let mut frame = tone(32);
        frame.samples.fill(0.5);
        assert_eq!(spectrum(&frame), [0.0; BANDS]);
        frame.samples.fill(f32::NAN);
        assert_eq!(spectrum(&frame), [0.0; BANDS]);
    }

    #[test]
    fn frequency_moves_across_bands_and_values_are_bounded() {
        let low = spectrum(&tone(4));
        let high = spectrum(&tone(256));
        let peak = |values: [f32; BANDS]| {
            values
                .into_iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .unwrap()
                .0
        };
        assert!(peak(low) < peak(high));
        assert!(low
            .iter()
            .chain(high.iter())
            .all(|v| (0.0..=1.0).contains(v)));
    }

    #[test]
    fn pause_mute_stale_and_new_track_clear_energy() {
        let audio = SharedAudio::default();
        *audio.lock().unwrap() = tone(32);
        let mut visual = Visualizer::default();
        visual.update(&audio, true, 100, Duration::from_millis(50));
        assert!(visual.bands.iter().any(|&v| v > 0.1));
        for _ in 0..60 {
            visual.update(&audio, false, 100, Duration::from_millis(50));
        }
        assert!(visual.peaks.iter().all(|&v| v < 0.001));
        visual.reset();
        visual.update(&audio, true, 0, Duration::from_millis(50));
        assert_eq!(visual.bands, [0.0; BANDS]);
        visual.reset();
        for _ in 0..60 {
            visual.update(&audio, true, 100, Duration::from_millis(50));
        }
        assert!(visual.bands.iter().all(|&v| v < 0.001));
        visual.update(
            &SharedAudio::default(),
            true,
            100,
            Duration::from_millis(50),
        );
        assert_eq!(visual.bands, [0.0; BANDS]);
    }

    #[test]
    fn rendering_handles_small_sizes_and_mode_cycle() {
        let mut visual = Visualizer::default();
        assert_eq!(
            VisualMode::Spectrum
                .next()
                .next()
                .next()
                .next()
                .next()
                .next(),
            VisualMode::Spectrum
        );
        let audio = SharedAudio::default();
        *audio.lock().unwrap() = tone(32);
        for mode in [VisualMode::Spectrum, VisualMode::Pulse] {
            visual.mode = mode;
            visual.reset();
            visual.update(&audio, true, 100, Duration::from_millis(100));
            for width in [1, 2, 7, 31, 79, 200] {
                for height in [1, 2, 6] {
                    let rows = visual.render(width, height);
                    assert_eq!(rows.len(), height);
                    assert!(rows.iter().all(|row| row.len() == width && row.is_ascii()));
                }
            }
        }
        assert!(visual.render(0, 8).is_empty());
        assert!(visual.render(80, 0).is_empty());
        visual.mode = VisualMode::Off;
        assert!(visual.render(80, 8).is_empty());
    }

    #[test]
    fn audio_pipeline_produces_visible_effects_without_a_device() {
        use crate::audio_visual::VisualSource;
        use rodio::buffer::SamplesBuffer;

        let audio = SharedAudio::default();
        let samples = tone(32).samples.to_vec();
        let source = SamplesBuffer::new(1, 48_000, samples.clone());
        let output: Vec<_> = VisualSource::new(source, audio.clone()).collect();
        assert_eq!(output, samples);
        let mut visual = Visualizer::default();
        visual.update(&audio, true, 100, Duration::from_millis(100));
        assert!(visual.render(79, 6).iter().any(|row| row.contains('.')));
    }

    #[test]
    fn decoded_stereo_wav_drives_all_effects_without_an_audio_device() {
        use crate::audio_visual::VisualSource;
        use rodio::{Decoder, Source};
        use std::io::Cursor;

        let data_size = (FFT_SIZE * 4) as u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&192_000u32.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_size.to_le_bytes());
        for sample in tone(32).samples {
            let sample = (sample * i16::MAX as f32) as i16;
            bytes.extend_from_slice(&sample.to_le_bytes());
            bytes.extend_from_slice(&(-sample).to_le_bytes());
        }
        let decoder = Decoder::new(Cursor::new(bytes)).unwrap();
        let audio = SharedAudio::default();
        assert_eq!(
            VisualSource::new(decoder.convert_samples::<f32>(), audio.clone()).count(),
            FFT_SIZE * 2
        );
        for mode in [VisualMode::Spectrum, VisualMode::Pulse] {
            let mut visual = Visualizer {
                mode,
                ..Visualizer::default()
            };
            visual.update(&audio, true, 100, Duration::from_millis(100));
            let rows = visual.render(79, 6);
            match mode {
                VisualMode::Spectrum => assert!(rows.iter().any(|row| row.contains('.'))),
                VisualMode::Pulse => assert!(visual.energy > 0.5),
                VisualMode::Off => unreachable!(),
            }
            assert!(rows.iter().any(|row| !row.trim().is_empty()));
        }
    }

    #[test]
    fn pulse_detects_onsets_not_sustained_tones() {
        let audio = SharedAudio::default();
        let mut visual = Visualizer {
            mode: VisualMode::Pulse,
            ..Visualizer::default()
        };
        let dt = Duration::from_millis(33);
        for sequence in 1..=12 {
            let mut frame = tone(32);
            frame.sequence = sequence;
            *audio.lock().unwrap() = frame;
            visual.update(&audio, true, 100, dt);
        }
        assert!(visual.energy > 0.9);
        assert_eq!(
            visual.ripples.iter().filter(|r| r.strength > 0.0).count(),
            1
        );
        assert!(visual
            .render(79, 6)
            .iter()
            .any(|row| !row.trim().is_empty()));
        assert_eq!(visual.targets, [0.0; BANDS]); // Pulse does not calculate FFT.

        for sequence in 13..=30 {
            *audio.lock().unwrap() = AudioFrame {
                sequence,
                ..AudioFrame::default()
            };
            visual.update(&audio, true, 100, dt);
        }
        let mut frame = tone(32);
        frame.sequence = 31;
        *audio.lock().unwrap() = frame;
        visual.update(&audio, true, 100, dt);
        assert!(visual
            .ripples
            .iter()
            .any(|r| r.radius == 0.05 && r.strength > 0.9));
    }

    #[test]
    fn pulse_expands_and_fades_for_pause_mute_stale_and_stop() {
        let dt = Duration::from_millis(33);
        for (active, volume) in [(false, 100), (true, 0), (true, 100)] {
            let audio = SharedAudio::default();
            *audio.lock().unwrap() = tone(32);
            let mut visual = Visualizer {
                mode: VisualMode::Pulse,
                ..Visualizer::default()
            };
            visual.update(&audio, true, 100, dt);
            let before = visual.render(79, 6);
            for _ in 0..4 {
                visual.update(&audio, true, 100, dt);
            }
            assert_ne!(before, visual.render(79, 6));
            for _ in 0..100 {
                // No fresh sequence also models EOF/stale snapshots.
                visual.update(&audio, active, volume, dt);
            }
            assert!(visual.energy < 0.001);
            assert!(visual.ripples.iter().all(|r| r.strength == 0.0));
            assert!(visual.render(79, 6).iter().all(|row| row.trim().is_empty()));
        }
    }

    #[test]
    fn pulse_obeys_volume_noise_gate_and_resets_on_new_track() {
        let audio = SharedAudio::default();
        *audio.lock().unwrap() = tone(32);
        let mut quiet = Visualizer {
            mode: VisualMode::Pulse,
            ..Visualizer::default()
        };
        let mut loud = Visualizer {
            mode: VisualMode::Pulse,
            ..Visualizer::default()
        };
        let dt = Duration::from_millis(100);
        quiet.update(&audio, true, 25, dt);
        loud.update(&audio, true, 100, dt);
        assert!((quiet.energy * 4.0 - loud.energy).abs() < 0.001);
        assert!((quiet.ripples[0].strength * 4.0 - loud.ripples[0].strength).abs() < 0.001);
        loud.update(&SharedAudio::default(), true, 100, dt);
        assert_eq!(loud.energy, 0.0);
        assert!(loud.ripples.iter().all(|r| r.strength == 0.0));
        assert_eq!(loud.mode, VisualMode::Pulse);

        let mut frame = tone(32);
        for sample in &mut frame.samples {
            *sample *= 0.001;
        }
        *audio.lock().unwrap() = frame;
        loud.update(&audio, true, 100, dt);
        assert!(loud.ripples.iter().all(|r| r.strength == 0.0));
    }

    #[test]
    fn rms_is_bounded_and_rejects_silence_dc_and_invalid_samples() {
        assert!((rms(&tone(32)) - 0.8 / 2.0_f32.sqrt()).abs() < 0.001);
        assert_eq!(rms(&AudioFrame::default()), 0.0);
        let mut frame = tone(32);
        for sample in [0.0, 0.5, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            frame.samples.fill(sample);
            assert_eq!(rms(&frame), 0.0);
        }
        for (i, sample) in frame.samples.iter_mut().enumerate() {
            *sample = if i % 2 == 0 { 100.0 } else { -100.0 };
        }
        frame.len = usize::MAX;
        assert_eq!(rms(&frame), 1.0);
    }

    #[test]
    fn off_skips_analysis_and_mode_cycle_includes_pulse() {
        assert_eq!(VisualMode::Spectrum.next(), VisualMode::Pulse);
        assert_eq!(VisualMode::Pulse.next(), VisualMode::Off);
        assert_eq!(VisualMode::Off.next(), VisualMode::Spectrum);
        let audio = SharedAudio::default();
        *audio.lock().unwrap() = tone(32);
        let mut visual = Visualizer {
            mode: VisualMode::Off,
            ..Visualizer::default()
        };
        visual.update(&audio, true, 100, Duration::from_millis(200));
        assert_eq!(visual.sequence, None);
        assert_eq!(visual.targets, [0.0; BANDS]);
        assert_eq!(visual.energy, 0.0);
        visual.mode = visual.mode.next();
        visual.reset();
        visual.update(&audio, true, 100, Duration::from_millis(33));
        assert!(visual.bands.iter().any(|&band| band > 0.1));
    }

    #[test]
    fn visibility_resume_waits_for_new_snapshot_instead_of_replaying_old_energy() {
        let audio = SharedAudio::default();
        *audio.lock().unwrap() = tone(32);
        let mut visual = Visualizer::default();
        visual.wait_for_fresh_audio(&audio);
        visual.update(&audio, true, 100, Duration::from_millis(100));
        assert!(!visual.is_animating());
        audio.lock().unwrap().sequence += 1;
        visual.update(&audio, true, 100, Duration::from_millis(33));
        assert!(visual.is_animating());
    }

    #[test]
    fn animations_settle_and_can_wake_again_in_every_mode() {
        for mode in [VisualMode::Spectrum, VisualMode::Pulse] {
            let audio = SharedAudio::default();
            *audio.lock().unwrap() = tone(32);
            let mut visual = Visualizer {
                mode,
                ..Visualizer::default()
            };
            assert!(!visual.is_animating());
            visual.update(&audio, true, 100, Duration::from_millis(100));
            assert!(visual.is_animating());
            let mut canvas = VisualCanvas::default();
            visual.render_into(79, 6, &mut canvas);
            let capacities: Vec<_> = canvas.rows.iter().map(String::capacity).collect();
            visual.render_into(79, 6, &mut canvas);
            assert_eq!(
                capacities,
                canvas.rows.iter().map(String::capacity).collect::<Vec<_>>()
            );
            for _ in 0..120 {
                visual.update(&audio, false, 100, Duration::from_millis(33));
            }
            assert!(!visual.is_animating());
            audio.lock().unwrap().sequence += 1;
            visual.update(&audio, true, 100, Duration::from_millis(33));
            assert!(visual.is_animating());
            visual.reset();
            visual.render_into(1, 1, &mut canvas);
            assert_eq!(canvas.rows.len(), 1);
            assert_eq!(canvas.rows[0].len(), 1);
            visual.mode = VisualMode::Off;
            visual.render_into(79, 6, &mut canvas);
            assert!(canvas.rows.is_empty());
        }
    }

    #[test]
    fn spectrum_uses_spaced_dots_and_floating_peaks() {
        let mut visual = Visualizer::default();
        visual.bands.fill(1.0);
        visual.peaks.fill(1.0);
        let rows = visual.render(48, 6);
        assert!(rows.iter().all(|row| row.contains('.')));
        assert!(rows.iter().all(|row| !row.contains("..")));
        assert!(rows
            .iter()
            .flat_map(|r| r.chars())
            .all(|c| " .".contains(c)));
        visual.bands.fill(0.0);
        let peaks = visual.render(48, 6);
        assert!(peaks[0].contains('.'));
        assert!(peaks[1..].iter().all(|r| r.trim().is_empty()));
        visual.peaks.fill(0.0);
        assert!(visual.render(48, 6).iter().all(|r| r.trim().is_empty()));
    }

    #[test]
    fn analysis_does_not_wait_for_audio_lock() {
        let audio = SharedAudio::default();
        let _lock = audio.lock().unwrap();
        Visualizer::default().update(&audio, true, 100, Duration::from_millis(33));
    }
}
