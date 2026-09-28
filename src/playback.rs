//! Playback owns its audio device on a worker thread, independent of terminal drawing.
//!
//! Navigation policy: the editable FIFO queue takes priority on Next and EOF, even with
//! repeat-one. Without queued tracks, repeat-one restarts only at EOF; manual Next ignores
//! it. Shuffle visits each track once per bag, stopping at exhaustion with repeat-off at
//! EOF, or starting a new cycle for manual Next / repeat-all. Explicit plays, queued
//! duplicates and Previous may intentionally repeat tracks; they remove the played track
//! from the current bag. Previous retraces successful track changes without consuming the
//! queue; with no history it steps backward in normal order, or restarts in shuffle mode.
//! Failed queued entries are consumed and skipped, as are failed Next/EOF candidates;
//! attempts are bounded, and the last error is published if no candidate can be played.
use crate::{
    audio_visual::{CaptureControl, SharedAudio},
    library::Track,
    player::{Player, Repeat},
};
use std::{
    collections::{hash_map::RandomState, HashSet},
    hash::BuildHasher,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const COMMAND_CAPACITY: usize = 32;
const TICK: Duration = Duration::from_millis(20);
const HISTORY_LIMIT: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Play(usize),
    TogglePause(usize),
    Skip(bool),
    Seek(i64),
    ChangeVolume(i16),
    CycleRepeat,
    ToggleShuffle,
    /// Append a library track index. Duplicates are allowed; playback is not interrupted.
    Enqueue(usize),
    /// Remove a zero-based position in the upcoming queue (not a library track index).
    RemoveQueued(usize),
    ClearQueue,
}

#[derive(Clone)]
pub struct PlaybackState {
    pub current: Option<usize>,
    pub duration: Option<Duration>,
    pub volume: u8,
    pub repeat: Repeat,
    pub shuffle: bool,
    /// Pending library indices in FIFO order; duplicates (even the current index) are allowed.
    pub queue: Vec<usize>,
    pub finished: bool,
    pub paused: bool,
    pub position: Duration,
    pub audio: SharedAudio,
    pub capture: CaptureControl,
    pub notice: String,
    pub revision: u64,
    pub seek_revision: u64,
}

impl PlaybackState {
    pub(crate) fn empty(volume: u8, repeat: Repeat) -> Self {
        Self {
            current: None,
            duration: None,
            volume: volume.min(100),
            repeat,
            shuffle: false,
            queue: Vec::new(),
            finished: true,
            paused: false,
            position: Duration::ZERO,
            audio: SharedAudio::default(),
            capture: CaptureControl::default(),
            notice: String::new(),
            revision: 0,
            seek_revision: 0,
        }
    }
}

type Snapshot = Arc<Mutex<PlaybackState>>;

/// Revision counters readable without locking the state, so the UI only clones
/// the snapshot when something actually changed. Stores happen under the lock.
#[derive(Default)]
struct Revisions {
    revision: AtomicU64,
    seek_revision: AtomicU64,
}

/// Only brief state copies hold the mutex; decoding, seeking, commands and drawing never do.
pub struct Playback {
    commands: Option<mpsc::SyncSender<Command>>,
    snapshot: Snapshot,
    worker: Option<JoinHandle<()>>,
    revisions: Arc<Revisions>,
    worker_died: AtomicBool,
}

impl Playback {
    pub fn start(tracks: Arc<Vec<Track>>, volume: u8, repeat: Repeat) -> Result<Self, String> {
        // Construct OutputStream inside the worker; it need not implement Send.
        Self::spawn(move || {
            if tracks.is_empty() {
                return Err("No playable tracks were supplied.".into());
            }
            let mut player = Player::new(tracks, volume)?;
            player.repeat = repeat;
            let mut last_error = String::new();
            for index in 0..player.tracks.len() {
                match player.play(index) {
                    Ok(()) => return Ok(player),
                    Err(error) => last_error = error,
                }
            }
            Err(last_error)
        })
    }

    fn spawn<B, F>(create: F) -> Result<Self, String>
    where
        B: Backend + 'static,
        F: FnOnce() -> Result<B, String> + Send + 'static,
    {
        let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let snapshot = Arc::new(Mutex::new(PlaybackState::empty(
            crate::config::DEFAULT_VOLUME,
            Repeat::Off,
        )));
        let revisions = Arc::new(Revisions::default());
        let published = snapshot.clone();
        let published_revisions = revisions.clone();
        let worker = thread::Builder::new()
            .name("starplay-playback".into())
            .spawn(move || {
                let backend = match create() {
                    Ok(backend) => backend,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let mut engine = Engine::new(backend);
                publish(&published, &engine.state, &published_revisions);
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                worker_loop(&mut engine, receiver, &published, &published_revisions);
            })
            .map_err(|error| format!("Cannot start playback worker: {error}"))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                commands: Some(commands),
                snapshot,
                worker: Some(worker),
                revisions,
                worker_died: AtomicBool::new(false),
            }),
            result => {
                let _ = worker.join();
                Err(match result {
                    Ok(Err(error)) => error,
                    _ => "Playback worker stopped during startup.".into(),
                })
            }
        }
    }

    /// Bump the published revision once if the worker died, so the UI notices
    /// without locking; later calls are no-ops instead of perpetual redraws.
    fn notice_worker_death(&self) {
        if self.commands.is_some()
            && self.worker.as_ref().is_some_and(JoinHandle::is_finished)
            && !self.worker_died.swap(true, Ordering::AcqRel)
        {
            self.revisions.revision.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn revision(&self) -> u64 {
        self.notice_worker_death();
        self.revisions.revision.load(Ordering::Acquire)
    }

    pub fn seek_revision(&self) -> u64 {
        self.revisions.seek_revision.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> PlaybackState {
        self.notice_worker_death();
        let mut state = self
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if self.worker_died.load(Ordering::Acquire) {
            state.finished = true;
            state.notice =
                "Playback worker stopped unexpectedly. Quit and restart the player.".into();
            state.revision = self.revisions.revision.load(Ordering::Acquire);
        }
        state
    }

    pub fn send(&self, command: Command) -> Result<(), String> {
        self.commands
            .as_ref()
            .ok_or("Playback has been shut down.")?
            .try_send(command)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => "Playback command queue is busy.".into(),
                mpsc::TrySendError::Disconnected(_) => "Playback worker has stopped.".into(),
            })
    }

    /// Disconnect, drain the finite accepted commands, and join. This ensures a volume/repeat
    /// key immediately followed by Q is applied before the caller persists final settings.
    pub fn shutdown(&mut self) -> Result<(), String> {
        self.commands.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| "Playback worker terminated unexpectedly.".to_string())?;
        }
        Ok(())
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

trait Backend {
    fn snapshot(&self) -> PlaybackState;
    fn execute(&mut self, command: Command) -> Result<(), String>;
    fn tick(&mut self) -> Result<bool, String>;

    /// Opt into engine-owned navigation. Such backends must only mark EOF in `tick`,
    /// not advance tracks themselves. The default preserves generic/legacy backends.
    fn track_count(&self) -> Option<usize> {
        None
    }
}

impl Backend for Player {
    fn snapshot(&self) -> PlaybackState {
        PlaybackState {
            current: self.current,
            duration: self.duration,
            volume: self.volume,
            repeat: self.repeat,
            shuffle: false,
            queue: Vec::new(),
            finished: self.finished,
            paused: self.paused(),
            position: self.position(),
            audio: self.audio.clone(),
            capture: self.capture.clone(),
            notice: String::new(),
            revision: 0,
            seek_revision: 0,
        }
    }
    fn execute(&mut self, command: Command) -> Result<(), String> {
        match command {
            Command::Play(index) => self.play(index),
            Command::TogglePause(index) => self.toggle_pause(index),
            Command::Seek(seconds) => self.seek(seconds),
            Command::ChangeVolume(delta) => {
                self.change_volume(delta);
                Ok(())
            }
            Command::CycleRepeat => {
                self.repeat = self.repeat.next();
                Ok(())
            }
            Command::Skip(_)
            | Command::ToggleShuffle
            | Command::Enqueue(_)
            | Command::RemoveQueued(_)
            | Command::ClearQueue => unreachable!("navigation is owned by Engine"),
        }
    }
    fn tick(&mut self) -> Result<bool, String> {
        Ok(self.poll_finished())
    }

    fn track_count(&self) -> Option<usize> {
        Some(self.tracks.len())
    }
}

struct Engine<B> {
    backend: B,
    state: PlaybackState,
    history: Vec<usize>,
    shuffle_bag: Vec<usize>,
    random: RandomState,
    shuffle_cycle: u64,
}
impl<B: Backend> Engine<B> {
    fn new(backend: B) -> Self {
        Self {
            state: backend.snapshot(),
            backend,
            history: Vec::new(),
            shuffle_bag: Vec::new(),
            random: RandomState::new(),
            shuffle_cycle: 0,
        }
    }

    fn refill_bag(&mut self, count: usize, first_cycle: bool) {
        self.shuffle_cycle = self.shuffle_cycle.wrapping_add(1);
        self.shuffle_bag = (0..count)
            .filter(|index| !first_cycle || Some(*index) != self.state.current)
            .collect();
        // RandomState supplies a per-engine seed without adding a random dependency.
        for end in (1..self.shuffle_bag.len()).rev() {
            let pick =
                (self.random.hash_one((self.shuffle_cycle, end)) % (end as u64 + 1)) as usize;
            self.shuffle_bag.swap(end, pick);
        }
        // Pop from the end; avoid an immediate repeat across cycle boundaries.
        let len = self.shuffle_bag.len();
        if len > 1 && self.shuffle_bag.last().copied() == self.state.current {
            self.shuffle_bag.swap(0, len - 1);
        }
    }

    fn record_play(&mut self, previous: Option<usize>, current: Option<usize>) {
        if previous != current {
            if let Some(index) = previous {
                self.history.push(index);
                if self.history.len() > HISTORY_LIMIT {
                    self.history.remove(0);
                }
            }
        }
        if self.state.shuffle {
            self.shuffle_bag.retain(|index| Some(*index) != current);
        }
    }

    fn play(&mut self, index: usize) -> Result<(), String> {
        self.backend.execute(Command::Play(index))?;
        self.record_play(self.state.current, Some(index));
        Ok(())
    }

    fn next(&mut self, automatic: bool, count: usize) -> Result<bool, String> {
        let mut last_error = None;
        // Every queued entry is attempted once, even under repeat-one. A failed entry
        // must not wedge future Next/EOF transitions behind an unplayable queue head.
        let queued = std::mem::take(&mut self.state.queue);
        for (position, index) in queued.iter().copied().enumerate() {
            match self.play(index) {
                Ok(()) => {
                    self.state.queue = queued[position + 1..].to_vec();
                    return Ok(true);
                }
                Err(error) => last_error = Some(error),
            }
        }
        if count == 0 {
            return last_error.map_or(Ok(false), Err);
        }
        if automatic && self.state.repeat == Repeat::One {
            if let Some(current) = self.state.current {
                return self.play(current).map(|()| true);
            }
        }
        if self.state.shuffle {
            let mut failed = HashSet::new();
            for _ in 0..count {
                if self.shuffle_bag.is_empty() {
                    if automatic && self.state.repeat != Repeat::All {
                        break;
                    }
                    self.refill_bag(count, false);
                    // A broken tail of one cycle must not prevent repeat-all from
                    // reaching playable tracks in the next. Never retry an index
                    // within this transition, bounding all attempts to `count`.
                    self.shuffle_bag.retain(|index| !failed.contains(index));
                }
                let Some(index) = self.shuffle_bag.pop() else {
                    break;
                };
                match self.play(index) {
                    Ok(()) => return Ok(true),
                    Err(error) => {
                        last_error = Some(error);
                        failed.insert(index);
                    }
                }
            }
        } else {
            let mut current = self.state.current;
            for _ in 0..count {
                let next = match current {
                    None => Some(0),
                    Some(index) if !automatic => Some((index + 1) % count),
                    Some(index) => crate::player::next_index(index, count, self.state.repeat),
                };
                let Some(index) = next else { break };
                match self.play(index) {
                    Ok(()) => return Ok(true),
                    Err(error) => last_error = Some(error),
                }
                current = Some(index);
            }
        }
        last_error.map_or(Ok(false), Err)
    }

    fn previous(&mut self, count: usize) -> Result<(), String> {
        if let Some(&index) = self.history.last() {
            self.backend.execute(Command::Play(index))?;
            self.history.pop();
            self.shuffle_bag.retain(|entry| *entry != index);
            return Ok(());
        }
        if count == 0 {
            return Ok(());
        }
        let current = self.state.current.unwrap_or(0);
        let index = if self.state.shuffle {
            current
        } else {
            (current + count - 1) % count
        };
        // Do not push Previous into history: repeated Previous must not bounce.
        self.backend.execute(Command::Play(index))?;
        self.shuffle_bag.retain(|entry| *entry != index);
        Ok(())
    }

    fn execute(&mut self, command: Command) -> Result<(), String> {
        let count = self.backend.track_count();
        match command {
            Command::ToggleShuffle => {
                let count = count.ok_or("This backend does not support shuffle.")?;
                self.state.shuffle = !self.state.shuffle;
                if self.state.shuffle {
                    self.refill_bag(count, true);
                } else {
                    self.shuffle_bag.clear();
                }
                Ok(())
            }
            Command::Enqueue(index) => {
                if index >= count.ok_or("This backend does not support queuing.")? {
                    return Err("Track index is out of range.".into());
                }
                self.state.queue.push(index);
                Ok(())
            }
            Command::RemoveQueued(position) => {
                if position >= self.state.queue.len() {
                    return Err("Queue position is out of range.".into());
                }
                self.state.queue.remove(position);
                Ok(())
            }
            Command::ClearQueue => {
                self.state.queue.clear();
                Ok(())
            }
            Command::Skip(true) if count.is_some() => self.next(false, count.unwrap()).map(|_| ()),
            Command::Skip(false) if count.is_some() => self.previous(count.unwrap()),
            Command::Play(index) => self.play(index),
            _ => {
                self.backend.execute(command)?;
                // TogglePause can start a selected track after EOF; legacy backends
                // can also navigate themselves. Record only successful changes.
                let current = self.backend.snapshot().current;
                self.record_play(self.state.current, current);
                Ok(())
            }
        }
    }

    fn refresh(&mut self) -> bool {
        let mut next = self.backend.snapshot();
        let new_track = !Arc::ptr_eq(&next.audio, &self.state.audio);
        let changed = new_track
            || next.current != self.state.current
            || next.finished != self.state.finished
            || next.paused != self.state.paused;
        if next.finished && !new_track {
            // A backend may reset its position at EOF; retain the last position for unknown durations.
            next.position = next
                .duration
                .unwrap_or_else(|| next.position.max(self.state.position));
        }
        if let Some(total) = next.duration {
            next.position = next.position.min(total);
        }
        next.shuffle = self.state.shuffle;
        next.queue = std::mem::take(&mut self.state.queue);
        next.notice.clone_from(&self.state.notice);
        next.revision = self.state.revision;
        next.seek_revision = self.state.seek_revision;
        self.state = next;
        changed
    }

    fn command(&mut self, command: Command) {
        match self.execute(command) {
            Ok(()) => {
                self.state.notice.clear();
                if matches!(command, Command::Seek(_)) {
                    self.state.seek_revision = self.state.seek_revision.wrapping_add(1);
                }
            }
            Err(error) => self.state.notice = error,
        }
        self.refresh();
        self.state.revision = self.state.revision.wrapping_add(1);
    }

    fn tick(&mut self) {
        let was_finished = self.state.finished;
        let previous = self.state.current;
        let mut result = self.backend.tick();
        let mut changed = self.refresh();
        if result.is_ok() {
            if let Some(count) = self.backend.track_count() {
                if !was_finished && self.state.finished {
                    result = self.next(true, count);
                    changed |= self.refresh();
                }
            } else {
                self.record_play(previous, self.state.current);
            }
        }
        match result {
            Ok(true) => {
                changed = true;
                self.state.notice.clear();
            }
            Err(error) => {
                changed |= self.state.notice != error;
                self.state.notice = error;
            }
            _ => {}
        }
        if changed {
            self.state.revision = self.state.revision.wrapping_add(1);
        }
    }
}

fn publish(snapshot: &Snapshot, state: &PlaybackState, revisions: &Revisions) {
    let mut guard = snapshot.lock().unwrap_or_else(|e| e.into_inner());
    guard.clone_from(state);
    revisions.revision.store(state.revision, Ordering::Release);
    revisions
        .seek_revision
        .store(state.seek_revision, Ordering::Release);
}

fn worker_loop<B: Backend>(
    engine: &mut Engine<B>,
    receiver: mpsc::Receiver<Command>,
    snapshot: &Snapshot,
    revisions: &Revisions,
) {
    let mut next_tick = Instant::now() + TICK;
    loop {
        // Limit each drain so a continuous input stream cannot starve EOF checks.
        for _ in 0..COMMAND_CAPACITY {
            match receiver.try_recv() {
                Ok(command) => {
                    engine.command(command);
                    publish(snapshot, &engine.state, revisions);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        let now = Instant::now();
        if now >= next_tick {
            engine.tick();
            publish(snapshot, &engine.state, revisions);
            next_tick = Instant::now() + TICK;
        }
        match receiver.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
            Ok(command) => {
                engine.command(command);
                publish(snapshot, &engine.state, revisions);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        state: PlaybackState,
        ticks: Arc<AtomicUsize>,
        log: Arc<Mutex<Vec<Command>>>,
        auto_end: bool,
        fail_seek: bool,
        track_count: Option<usize>,
        fail_play: Vec<usize>,
    }
    impl Fake {
        fn new() -> Self {
            let mut state = PlaybackState::empty(50, Repeat::Off);
            state.current = Some(0);
            state.finished = false;
            state.duration = Some(Duration::from_secs(10));
            Self {
                state,
                ticks: Arc::new(AtomicUsize::new(0)),
                log: Arc::default(),
                auto_end: false,
                fail_seek: false,
                track_count: None,
                fail_play: Vec::new(),
            }
        }

        fn managed(count: usize) -> Self {
            let mut backend = Self::new();
            backend.track_count = Some(count);
            if count == 0 {
                backend.state.current = None;
                backend.state.finished = true;
            }
            backend
        }
    }
    impl Backend for Fake {
        fn snapshot(&self) -> PlaybackState {
            self.state.clone()
        }
        fn execute(&mut self, command: Command) -> Result<(), String> {
            self.log.lock().unwrap().push(command);
            match command {
                Command::Play(index) => {
                    if self.track_count.is_some_and(|count| index >= count)
                        || self.fail_play.contains(&index)
                    {
                        return Err(format!("cannot play track {index}"));
                    }
                    self.state.current = Some(index);
                    self.state.finished = false;
                    self.state.paused = false;
                    self.state.audio = SharedAudio::default();
                    self.state.position = Duration::ZERO;
                }
                Command::TogglePause(index) if self.state.finished => {
                    self.execute(Command::Play(index))?
                }
                Command::TogglePause(_) => self.state.paused = !self.state.paused,
                Command::Skip(forward) => {
                    self.execute(Command::Play(if forward { 1 } else { 0 }))?
                }
                Command::Seek(_) if self.fail_seek || self.state.finished => {
                    return Err("seek failed".into())
                }
                Command::Seek(seconds) => {
                    self.state.position = if seconds < 0 {
                        self.state
                            .position
                            .saturating_sub(Duration::from_secs(seconds.unsigned_abs()))
                    } else {
                        self.state
                            .position
                            .saturating_add(Duration::from_secs(seconds as u64))
                    };
                }
                Command::ChangeVolume(delta) => {
                    self.state.volume = (i16::from(self.state.volume) + delta).clamp(0, 100) as u8
                }
                Command::CycleRepeat => self.state.repeat = self.state.repeat.next(),
                Command::ToggleShuffle
                | Command::Enqueue(_)
                | Command::RemoveQueued(_)
                | Command::ClearQueue => panic!("engine command leaked to the backend"),
            }
            Ok(())
        }
        fn tick(&mut self) -> Result<bool, String> {
            self.ticks.fetch_add(1, Ordering::Relaxed);
            if self.state.paused || self.state.finished {
                return Ok(false);
            }
            self.state.position += TICK;
            if self.auto_end {
                self.execute(Command::Play(1))?;
                self.auto_end = false;
                return Ok(true);
            }
            if self
                .state
                .duration
                .is_some_and(|total| self.state.position >= total)
            {
                self.state.finished = true;
                self.state.position = Duration::ZERO;
            }
            Ok(false)
        }
        fn track_count(&self) -> Option<usize> {
            self.track_count
        }
    }

    fn eof(engine: &mut Engine<Fake>) {
        engine.backend.state.position = engine.backend.state.duration.unwrap();
        engine.tick();
    }

    #[test]
    fn queue_edits_are_positional_validated_and_survive_refresh() {
        let mut engine = Engine::new(Fake::managed(5));
        let audio = engine.state.audio.clone();
        engine.command(Command::TogglePause(0));
        for index in [3, 1, 3] {
            engine.command(Command::Enqueue(index));
        }
        engine.tick();
        assert_eq!(engine.state.queue, [3, 1, 3]);
        assert!(engine.state.paused);
        assert!(Arc::ptr_eq(&audio, &engine.state.audio));
        assert_eq!(engine.state.current, Some(0));
        assert_eq!(engine.state.position, Duration::ZERO);
        engine.command(Command::RemoveQueued(1));
        assert_eq!(engine.state.queue, [3, 3]);
        let revision = engine.state.revision;
        engine.command(Command::Enqueue(5));
        assert_eq!(engine.state.queue, [3, 3]);
        assert!(engine.state.notice.contains("Track index"));
        assert_eq!(engine.state.revision, revision + 1);
        engine.command(Command::RemoveQueued(usize::MAX));
        assert_eq!(engine.state.queue, [3, 3]);
        assert!(engine.state.notice.contains("Queue position"));
        engine.command(Command::ClearQueue);
        engine.tick();
        assert!(engine.state.queue.is_empty());
        assert!(engine.state.notice.is_empty());
        assert!(engine.state.paused);
        engine.command(Command::ClearQueue); // Idempotent, including an empty queue.
        assert!(engine.state.queue.is_empty());
    }

    #[test]
    fn queue_overrides_manual_next_and_eof_in_every_repeat_mode() {
        for repeat in [Repeat::Off, Repeat::All, Repeat::One] {
            let mut backend = Fake::managed(4);
            backend.state.repeat = repeat;
            let mut engine = Engine::new(backend);
            engine.command(Command::Enqueue(3));
            engine.command(Command::Enqueue(1));
            engine.command(Command::Skip(true));
            assert_eq!(engine.state.current, Some(3));
            assert_eq!(engine.state.queue, [1]);
            eof(&mut engine); // Queue wins even at the playlist boundary / repeat-one.
            assert_eq!(engine.state.current, Some(1));
            assert!(engine.state.queue.is_empty());
            assert!(!engine.state.finished);
            let audio = engine.state.audio.clone();
            let revision = engine.state.revision;
            eof(&mut engine);
            assert_eq!(
                engine.state.current,
                Some(if repeat == Repeat::One { 1 } else { 2 })
            );
            assert!(!Arc::ptr_eq(&audio, &engine.state.audio));
            assert_eq!(engine.state.revision, revision + 1);
            engine.command(Command::Skip(true)); // Manual Next never repeats one.
            assert_eq!(
                engine.state.current,
                Some(if repeat == Repeat::One { 2 } else { 3 })
            );
        }
    }

    #[test]
    fn shuffle_bag_exhausts_once_then_repeat_off_stops() {
        let mut engine = Engine::new(Fake::managed(8));
        engine.command(Command::ToggleShuffle);
        assert!(engine.state.shuffle);
        let mut seen = vec![0];
        for _ in 1..8 {
            eof(&mut engine);
            assert!(!engine.state.finished);
            let index = engine.state.current.unwrap();
            assert!(
                !seen.contains(&index),
                "repeat before bag exhaustion: {seen:?}"
            );
            seen.push(index);
        }
        assert!(engine.shuffle_bag.is_empty());
        eof(&mut engine);
        assert!(engine.state.finished);
        let revision = engine.state.revision;
        engine.tick();
        assert_eq!(engine.state.revision, revision);
        let previous = engine.state.current;
        engine.command(Command::Skip(true)); // Manual Next starts another cycle.
        assert!(!engine.state.finished);
        assert_ne!(engine.state.current, previous);
    }

    #[test]
    fn shuffle_cycles_are_unique_and_avoid_boundary_repeats() {
        for automatic in [false, true] {
            let mut backend = Fake::managed(7);
            backend.state.repeat = Repeat::All;
            let mut engine = Engine::new(backend);
            engine.command(Command::ToggleShuffle);
            let mut cycle = vec![0];
            let mut previous = Some(0);
            for _ in 0..40 {
                if automatic {
                    eof(&mut engine);
                } else {
                    engine.command(Command::Skip(true));
                }
                assert_ne!(engine.state.current, previous);
                let index = engine.state.current.unwrap();
                assert!(!cycle.contains(&index));
                cycle.push(index);
                if cycle.len() == 7 {
                    cycle.clear();
                }
                previous = Some(index);
            }
        }
    }

    #[test]
    fn queue_and_explicit_play_remove_tracks_from_shuffle_bag() {
        let mut engine = Engine::new(Fake::managed(6));
        engine.command(Command::ToggleShuffle);
        engine.command(Command::Enqueue(4));
        engine.command(Command::Enqueue(2));
        engine.command(Command::Enqueue(2)); // Explicit duplicates intentionally repeat.
        engine.command(Command::Skip(true));
        assert_eq!(engine.state.current, Some(4));
        eof(&mut engine);
        assert_eq!(engine.state.current, Some(2));
        let audio = engine.state.audio.clone();
        eof(&mut engine);
        assert_eq!(engine.state.current, Some(2));
        assert!(!Arc::ptr_eq(&audio, &engine.state.audio));
        engine.command(Command::Play(5));
        assert!(engine.state.queue.is_empty());
        let mut remaining = Vec::new();
        for _ in 0..2 {
            eof(&mut engine);
            remaining.push(engine.state.current.unwrap());
        }
        remaining.sort_unstable();
        assert_eq!(remaining, [1, 3]);
        eof(&mut engine);
        assert!(engine.state.finished);
    }

    #[test]
    fn repeat_one_restarts_without_draining_shuffle_bag() {
        let mut backend = Fake::managed(4);
        backend.state.repeat = Repeat::One;
        let mut engine = Engine::new(backend);
        engine.command(Command::ToggleShuffle);
        let bag = engine.shuffle_bag.clone();
        eof(&mut engine);
        assert_eq!(engine.state.current, Some(0));
        assert_eq!(engine.shuffle_bag, bag);
        engine.command(Command::Skip(true));
        assert_ne!(engine.state.current, Some(0));
        assert_eq!(engine.shuffle_bag.len(), 2);
        engine.command(Command::ToggleShuffle);
        engine.tick();
        assert!(!engine.state.shuffle);
        assert!(engine.shuffle_bag.is_empty());
    }

    #[test]
    fn previous_retraces_actual_history_and_never_consumes_queue() {
        for shuffle in [false, true] {
            let mut engine = Engine::new(Fake::managed(6));
            if shuffle {
                engine.command(Command::ToggleShuffle);
            }
            engine.command(Command::Play(4));
            engine.command(Command::Enqueue(2));
            eof(&mut engine);
            engine.command(Command::Play(1));
            engine.command(Command::Enqueue(5));
            engine.backend.fail_play.push(3);
            engine.command(Command::Play(3));
            assert_eq!(engine.state.current, Some(1));
            for index in [2, 4, 0] {
                engine.command(Command::Skip(false));
                assert_eq!(engine.state.current, Some(index));
                assert_eq!(engine.state.queue, [5]);
            }
            engine.command(Command::Skip(false));
            assert_eq!(engine.state.current, Some(if shuffle { 0 } else { 5 }));
            engine.command(Command::Skip(true));
            assert_eq!(engine.state.current, Some(5));
            assert!(engine.state.queue.is_empty());
        }
    }

    #[test]
    fn failed_previous_keeps_history_and_current_track() {
        let mut engine = Engine::new(Fake::managed(3));
        engine.command(Command::Play(2));
        engine.backend.fail_play.push(0);
        engine.command(Command::Skip(false));
        assert_eq!(engine.state.current, Some(2));
        assert_eq!(engine.history, [0]);
        assert!(engine.state.notice.contains("cannot play"));
        engine.backend.fail_play.clear();
        engine.command(Command::Skip(false));
        assert_eq!(engine.state.current, Some(0));
        assert!(engine.history.is_empty());
    }

    #[test]
    fn unplayable_queue_entries_are_consumed_and_later_entries_play() {
        let mut engine = Engine::new(Fake::managed(4));
        engine.backend.fail_play.push(1);
        for index in [1, 1, 3] {
            engine.command(Command::Enqueue(index));
        }
        eof(&mut engine);
        assert_eq!(engine.state.current, Some(3));
        assert!(engine.state.queue.is_empty());
        assert!(engine.state.notice.is_empty());
        let plays: Vec<_> = engine.backend.log.lock().unwrap().iter().copied().collect();
        assert_eq!(
            plays,
            [Command::Play(1), Command::Play(1), Command::Play(3)]
        );
        assert_eq!(engine.history, [0]);
    }

    #[test]
    fn all_failed_eof_candidates_stop_once_with_error_instead_of_spinning() {
        for shuffle in [false, true] {
            let mut backend = Fake::managed(4);
            backend.state.repeat = Repeat::All;
            backend.fail_play = vec![0, 1, 2, 3];
            let mut engine = Engine::new(backend);
            if shuffle {
                engine.command(Command::ToggleShuffle);
            }
            engine.command(Command::Enqueue(2));
            eof(&mut engine);
            assert!(engine.state.finished);
            assert!(engine.state.queue.is_empty());
            assert!(engine.state.notice.contains("cannot play"));
            assert!(engine.history.is_empty());
            let attempts = engine.backend.log.lock().unwrap().len();
            assert!(attempts <= 5);
            let revision = engine.state.revision;
            for _ in 0..4 {
                engine.tick();
            }
            assert_eq!(engine.backend.log.lock().unwrap().len(), attempts);
            assert_eq!(engine.state.revision, revision);
        }
    }

    #[test]
    fn shuffle_wraps_past_a_broken_cycle_tail_but_repeat_off_stops() {
        for repeat in [Repeat::Off, Repeat::All] {
            let mut backend = Fake::managed(5);
            backend.state.repeat = repeat;
            backend.fail_play = vec![1, 2, 3, 4];
            let mut engine = Engine::new(backend);
            engine.command(Command::ToggleShuffle);
            eof(&mut engine);
            assert_eq!(engine.state.current, Some(0));
            assert_eq!(engine.state.finished, repeat == Repeat::Off);
            let attempts = engine.backend.log.lock().unwrap().len();
            assert!(attempts <= 5);
            engine.command(Command::Skip(true));
            assert!(!engine.state.finished);
            assert_eq!(engine.state.current, Some(0));
        }
    }

    #[test]
    fn ordered_eof_skips_broken_tracks_stops_at_end_and_manual_next_wraps() {
        let mut engine = Engine::new(Fake::managed(4));
        engine.backend.fail_play.push(1);
        eof(&mut engine);
        assert_eq!(engine.state.current, Some(2));
        assert_eq!(engine.history, [0]);
        eof(&mut engine);
        assert_eq!(engine.state.current, Some(3));
        eof(&mut engine);
        assert!(engine.state.finished);
        engine.command(Command::Skip(true));
        assert_eq!(engine.state.current, Some(0));
        assert!(!engine.state.finished);
    }

    #[test]
    fn explicit_play_and_pause_leave_upcoming_queue_intact() {
        let mut engine = Engine::new(Fake::managed(4));
        engine.command(Command::Enqueue(2));
        engine.command(Command::Play(3));
        assert_eq!(engine.state.queue, [2]);
        engine.command(Command::TogglePause(3));
        engine.backend.state.position = Duration::from_secs(10);
        engine.tick();
        assert_eq!(engine.state.current, Some(3));
        assert_eq!(engine.state.queue, [2]);
        engine.command(Command::Skip(true));
        assert_eq!(engine.state.current, Some(2));
        assert!(!engine.state.paused);
        assert!(engine.state.queue.is_empty());
        eof(&mut engine);
        eof(&mut engine);
        assert!(engine.state.finished);
        engine.command(Command::Enqueue(1));
        engine.tick(); // Editing the queue does not implicitly start a finished player.
        assert!(engine.state.finished);
        assert_eq!(engine.state.queue, [1]);
        engine.command(Command::TogglePause(0));
        assert_eq!(engine.state.current, Some(0));
        assert_eq!(engine.state.queue, [1]);
    }

    #[test]
    fn empty_and_single_track_navigation_is_safe() {
        let mut empty = Engine::new(Fake::managed(0));
        for command in [
            Command::ToggleShuffle,
            Command::Skip(true),
            Command::Skip(false),
        ] {
            empty.command(command);
            assert!(empty.state.notice.is_empty());
        }
        empty.command(Command::Enqueue(0));
        assert!(!empty.state.notice.is_empty());
        assert!(empty.state.queue.is_empty());
        assert_eq!(empty.state.current, None);

        for repeat in [Repeat::Off, Repeat::All, Repeat::One] {
            let mut backend = Fake::managed(1);
            backend.state.repeat = repeat;
            let mut engine = Engine::new(backend);
            engine.command(Command::ToggleShuffle);
            eof(&mut engine);
            assert_eq!(engine.state.finished, repeat == Repeat::Off);
            assert_eq!(engine.state.current, Some(0));
            engine.command(Command::Skip(true));
            assert!(!engine.state.finished);
            assert_eq!(engine.state.current, Some(0));
            assert!(engine.history.is_empty());
        }
    }

    #[test]
    fn shuffle_previous_without_an_active_track_counts_as_a_bag_visit() {
        let mut backend = Fake::managed(3);
        backend.state.current = None;
        backend.state.finished = true;
        let mut engine = Engine::new(backend);
        engine.command(Command::ToggleShuffle);
        engine.command(Command::Skip(false));
        assert_eq!(engine.state.current, Some(0));
        let mut remaining = Vec::new();
        for _ in 0..2 {
            engine.command(Command::Skip(true));
            remaining.push(engine.state.current.unwrap());
        }
        remaining.sort_unstable();
        assert_eq!(remaining, [1, 2]);
    }

    #[test]
    fn worker_publishes_queue_and_shuffle_and_drains_edits_on_shutdown() {
        let mut playback = Playback::spawn(|| Ok(Fake::managed(5))).unwrap();
        for command in [
            Command::ToggleShuffle,
            Command::Enqueue(4),
            Command::Enqueue(1),
            Command::Enqueue(3),
            Command::RemoveQueued(1),
            Command::Skip(true),
        ] {
            playback.send(command).unwrap();
        }
        playback.shutdown().unwrap();
        let state = playback.snapshot();
        assert_eq!(state.current, Some(4));
        assert_eq!(state.queue, [3]);
        assert!(state.shuffle);
        assert!(state.notice.is_empty());
    }

    #[test]
    fn progress_ticks_do_not_invalidate_static_ui_but_eof_and_same_index_restart_do() {
        let mut engine = Engine::new(Fake::new());
        engine.tick();
        assert_eq!(engine.state.revision, 0);
        assert_eq!(engine.state.position, TICK);
        engine.backend.state.position = Duration::from_secs(10);
        engine.tick();
        assert!(engine.state.finished);
        assert_eq!(engine.state.position, Duration::from_secs(10));
        assert_eq!(engine.state.revision, 1);
        engine.command(Command::TogglePause(0));
        assert!(!engine.state.finished);
        assert_eq!(engine.state.position, Duration::ZERO);
        assert_eq!(engine.state.revision, 2);
        engine.backend.auto_end = true;
        engine.backend.state.current = Some(1);
        engine.tick();
        assert_eq!(engine.state.revision, 3);
    }

    #[test]
    fn pause_seek_failure_success_and_resume_preserve_expected_state() {
        let mut engine = Engine::new(Fake::new());
        engine.command(Command::TogglePause(0));
        engine.tick();
        assert!(engine.state.paused);
        assert_eq!(engine.state.position, Duration::ZERO);
        engine.backend.fail_seek = true;
        engine.command(Command::Seek(5));
        assert_eq!(engine.state.seek_revision, 0);
        assert_eq!(engine.state.notice, "seek failed");
        engine.tick();
        assert_eq!(engine.state.notice, "seek failed");
        engine.backend.fail_seek = false;
        engine.command(Command::Seek(5));
        assert!(engine.state.paused);
        assert_eq!(engine.state.seek_revision, 1);
        assert_eq!(engine.state.position, Duration::from_secs(5));
        assert!(engine.state.notice.is_empty());
        engine.command(Command::TogglePause(0));
        engine.tick();
        assert!(!engine.state.paused);
        assert!(engine.state.position > Duration::from_secs(5));
        engine.command(Command::Seek(50));
        engine.tick();
        assert!(engine.state.finished);
        assert_eq!(engine.state.position, Duration::from_secs(10));
    }

    #[test]
    fn unknown_duration_eof_retains_last_position() {
        let mut engine = Engine::new(Fake::new());
        engine.backend.state.duration = None;
        engine.backend.state.position = Duration::from_secs(3);
        engine.tick();
        let last_position = engine.state.position;
        engine.backend.state.finished = true;
        engine.backend.state.position = Duration::ZERO;
        engine.tick();
        assert_eq!(engine.state.position, last_position);
    }

    #[test]
    fn worker_advances_and_autoskips_without_ui_snapshot_consumption() {
        let mut backend = Fake::new();
        backend.auto_end = true;
        let ticks = backend.ticks.clone();
        let mut playback = Playback::spawn(move || Ok(backend)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while ticks.load(Ordering::Relaxed) < 3 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        playback.shutdown().unwrap();
        assert!(ticks.load(Ordering::Relaxed) >= 3);
        assert_eq!(playback.snapshot().current, Some(1));
    }

    #[test]
    fn commands_are_fifo_and_shutdown_drains_accepted_settings() {
        let backend = Fake::new();
        let log = backend.log.clone();
        let mut playback = Playback::spawn(move || Ok(backend)).unwrap();
        let commands = [
            Command::ChangeVolume(5),
            Command::CycleRepeat,
            Command::ChangeVolume(-10),
        ];
        for command in commands {
            playback.send(command).unwrap();
        }
        playback.shutdown().unwrap();
        assert_eq!(*log.lock().unwrap(), commands);
        assert_eq!(playback.snapshot().volume, 45);
        assert_eq!(playback.snapshot().repeat, Repeat::All);
        assert!(playback.send(Command::CycleRepeat).is_err());
        playback.shutdown().unwrap();
    }

    #[test]
    fn bounded_queue_rejects_overflow_without_waiting() {
        let (tx, rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        let mut playback = Playback {
            commands: Some(tx),
            snapshot: Arc::new(Mutex::new(PlaybackState::empty(50, Repeat::Off))),
            worker: None,
            revisions: Arc::new(Revisions::default()),
            worker_died: AtomicBool::new(false),
        };
        for _ in 0..COMMAND_CAPACITY {
            playback.send(Command::CycleRepeat).unwrap();
        }
        assert!(playback
            .send(Command::CycleRepeat)
            .unwrap_err()
            .contains("busy"));
        drop(rx);
        assert!(playback
            .send(Command::CycleRepeat)
            .unwrap_err()
            .contains("stopped"));
        playback.shutdown().unwrap();
    }

    #[test]
    fn startup_errors_are_propagated_and_joined() {
        let result = Playback::spawn::<Fake, _>(|| Err("no device".into()));
        assert!(matches!(result, Err(error) if error == "no device"));
    }

    #[test]
    fn input_is_processed_before_a_due_eof_tick() {
        let backend = Fake::new();
        let log = backend.log.clone();
        let mut engine = Engine::new(backend);
        engine.backend.auto_end = true;
        engine.command(Command::TogglePause(0));
        engine.tick();
        assert_eq!(engine.state.current, Some(0));
        assert_eq!(*log.lock().unwrap(), [Command::TogglePause(0)]);
    }
}
