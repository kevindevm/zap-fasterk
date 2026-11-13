//! Audio playback and voice-message recording.
//!
//! Input and output devices are opened on demand and released when idle.

use std::collections::HashMap;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rodio::Source;
use rodio::buffer::SamplesBuffer;

use crate::backend::Waker;
use crate::voice;

/// Maximum recording length. The phone uses a shorter limit.
const LONGEST_RECORDING: Duration = Duration::from_secs(15 * 60);

fn mono() -> NonZero<u16> {
    NonZero::<u16>::MIN
}

fn rate() -> NonZero<u32> {
    NonZero::new(voice::RATE).expect("48 kHz is not zero")
}

/// Opens the default output device for playback.
///
/// rodio reports the sink's drop through `stderr` by default. A desktop launch
/// can have that closed: Zap Fasterk inherits `stderr` from whatever started it,
/// and that process can exit while Zap Fasterk runs on. Rust ignores `SIGPIPE`, so
/// the next write there fails with `Broken pipe` and the print macro panics,
/// which aborts the whole app in a release build. Keep it off, and report
/// failures of our own through the log instead.
pub struct Output {
    sink: rodio::MixerDeviceSink,
    device: Option<rodio::cpal::DeviceId>,
    failed: Arc<AtomicBool>,
    checked: Instant,
    preferred: Option<String>,
    notice: Option<String>,
    fallback: bool,
}

impl Output {
    pub fn mixer(&self) -> &rodio::mixer::Mixer {
        self.sink.mixer()
    }

    /// Active playback already has a clock. Check routing at most once per
    /// second on that clock, without adding an idle repaint timer.
    pub fn route_changed(&mut self) -> bool {
        use rodio::cpal::traits::DeviceTrait;
        if self.failed.load(Ordering::Acquire) {
            return true;
        }
        if self.checked.elapsed() < Duration::from_secs(1) {
            return false;
        }
        self.checked = Instant::now();
        let current = selected_output(if self.fallback {
            None
        } else {
            self.preferred.as_deref()
        })
        .and_then(|device| device.id().ok());
        current != self.device
    }
}

fn selected_output(preferred: Option<&str>) -> Option<rodio::cpal::Device> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    let host = rodio::cpal::default_host();
    preferred
        .and_then(|name| {
            host.output_devices().ok()?.find(|device| {
                device
                    .description()
                    .is_ok_and(|description| description.name() == name)
            })
        })
        .or_else(|| host.default_output_device())
}

pub fn open_output() -> Result<Output, rodio::DeviceSinkError> {
    open_output_on(None)
}

pub fn open_output_on(preferred: Option<&str>) -> Result<Output, rodio::DeviceSinkError> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    let host = rodio::cpal::default_host();
    let selected = preferred.and_then(|name| {
        host.output_devices().ok()?.find(|device| {
            device
                .description()
                .is_ok_and(|description| description.name() == name)
        })
    });
    let missing = preferred.is_some() && selected.is_none();
    let device = selected
        .or_else(|| host.default_output_device())
        .ok_or(rodio::DeviceSinkError::NoDevice)?;
    let open = |device: rodio::cpal::Device| {
        let id = device.id().ok();
        let failed = Arc::new(AtomicBool::new(false));
        let report = failed.clone();
        let mut sink = rodio::DeviceSinkBuilder::from_device(device)?
            .with_error_callback(move |_| {
                report.store(true, Ordering::Release);
            })
            .open_stream()?;
        sink.log_on_drop(false);
        Ok::<_, rodio::DeviceSinkError>(Output {
            sink,
            device: id,
            failed,
            checked: Instant::now(),
            preferred: preferred.map(str::to_owned),
            notice: None,
            fallback: false,
        })
    };
    let mut output = match open(device) {
        Ok(output) => output,
        Err(error) if preferred.is_some() => {
            let Some(device) = host.default_output_device() else {
                return Err(error);
            };
            let mut output = open(device)?;
            output.fallback = true;
            output.notice=Some("The selected audio output could not open. Playback uses the system default; choose an output in Settings.".into());
            output
        }
        Err(error) => return Err(error),
    };
    if missing {
        output.notice=Some("The selected audio output is unavailable. Playback uses the system default; choose an output in Settings.".into());
    }
    Ok(output)
}

impl Output {
    pub fn matches_preference(&self, preferred: Option<&str>) -> bool {
        self.preferred.as_deref() == preferred
    }
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Loading,
    Playing,
    Paused,
}

/// Playback state for one message.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub state: State,
    pub position: Duration,
    pub total: Duration,
}

impl Status {
    const IDLE: Self = Self {
        state: State::Idle,
        position: Duration::ZERO,
        total: Duration::ZERO,
    };
}

/// Playback speeds, in ascending order, matching the phone.
pub const SPEEDS: [f32; 5] = [1.0, 1.25, 1.5, 1.75, 2.0];

/// Speeds the speed chip cycles through, as on the phone. The others are
/// chosen from the message menu.
pub const CYCLED_SPEEDS: [f32; 3] = [1.0, 1.5, 2.0];

/// The speed after `speed` when the chip is clicked: the next faster cycled
/// speed, wrapping from 2x back to 1x.
pub fn next_cycled_speed(speed: f32) -> f32 {
    CYCLED_SPEEDS
        .into_iter()
        .find(|&candidate| candidate > speed)
        .unwrap_or(CYCLED_SPEEDS[0])
}

/// The supported speed nearest to `speed`; non-finite speeds give 1x.
pub fn supported_speed(speed: f32) -> f32 {
    if !speed.is_finite() {
        return SPEEDS[0];
    }
    SPEEDS
        .into_iter()
        .min_by(|a, b| (a - speed).abs().total_cmp(&(b - speed).abs()))
        .unwrap_or(SPEEDS[0])
}

/// Label for a playback speed, like `1x`, `1.25x`, or `1.5x`.
pub fn speed_label(speed: f32) -> String {
    if speed.fract() == 0.0 {
        format!("{}x", speed as i32)
    } else {
        // Keep both decimals for 1.25 and 1.75; drop the trailing zero on 1.5.
        let text = format!("{speed:.2}");
        let text = text.trim_end_matches('0').trim_end_matches('.');
        format!("{text}x")
    }
}

type Decoded = Arc<Mutex<Option<Result<Vec<f32>, String>>>>;

/// Plays one clip at a time through the default output device.
pub struct Player {
    waker: Waker,
    output: Option<(Output, rodio::Player)>,
    loaded: Option<Loaded>,
    decoding: Option<Decoding>,
    /// Playback speed applied to the current clip and to later ones.
    speed: f32,
    /// Time-compressed copies of the loaded clip, one per speed already
    /// built, dropped when the clip changes.
    stretches: Vec<(f32, Arc<Vec<f32>>)>,
    /// Compression being built for the loaded message.
    stretching: Option<Stretching>,
    /// Generated waveforms for clips that did not include one.
    bars: HashMap<String, Vec<u8>>,
    /// Message whose clip just played to its end, waiting to be taken.
    finished: Option<String>,
    output_device: Option<String>,
    notice: Option<String>,
}

struct Loaded {
    message: String,
    samples: Arc<Vec<f32>>,
    /// Samples queued in the sink: the clip itself or its compression.
    buffer: Arc<Vec<f32>>,
    /// Speed the queued buffer represents; 1 plays the clip as recorded.
    factor: f32,
    /// Restart position in the clip's own timeline.
    base: Duration,
    paused: bool,
    done: bool,
}

struct Stretching {
    factor: f32,
    slot: StretchedSlot,
    /// Set when this job is replaced or the clip changes, so the worker
    /// stops instead of piling up behind the next one.
    cancelled: Arc<AtomicBool>,
}

impl Drop for Stretching {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

type StretchedSlot = Arc<Mutex<Option<Arc<Vec<f32>>>>>;

struct Decoding {
    message: String,
    /// Requested start position after decoding, from 0 to 1.
    start: f32,
    slot: Decoded,
}

impl Player {
    pub fn new(waker: Waker) -> Self {
        Self {
            waker,
            output: None,
            loaded: None,
            decoding: None,
            speed: SPEEDS[0],
            stretches: Vec::new(),
            stretching: None,
            bars: HashMap::new(),
            finished: None,
            output_device: None,
            notice: None,
        }
    }

    pub fn set_output_device(&mut self, device: Option<String>) {
        self.output_device = device;
    }
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    /// Current playback speed multiplier.
    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// Sets the playback speed for the clip playing now and for later ones.
    ///
    /// Speeds above 1x play a time-compressed copy of the clip, once it has
    /// been built, so the voice keeps its pitch. Until then playback
    /// continues at the speed already queued. Any other speed, such as a
    /// hand-edited setting, snaps to the nearest one in [`SPEEDS`], so a speed
    /// control always shows it. Returns the speed that applies.
    pub fn set_speed(&mut self, speed: f32) -> f32 {
        self.speed = supported_speed(speed);
        self.apply_speed();
        self.ensure_stretch();
        self.speed
    }

    /// Whether `message` is still playing at an earlier speed while the
    /// compression for the chosen one builds.
    pub fn preparing_speed(&self, message: &str) -> bool {
        self.loaded.as_ref().is_some_and(|loaded| {
            loaded.message == message && !loaded.done && loaded.factor != self.speed
        })
    }

    /// The samples that play at `speed` and the speed they represent: the
    /// clip itself at 1x, its compression once built, and otherwise whatever
    /// is queued, so a speed still building does not drop playback to 1x.
    fn buffer_for(
        loaded: &Loaded,
        stretches: &[(f32, Arc<Vec<f32>>)],
        speed: f32,
    ) -> (Arc<Vec<f32>>, f32) {
        if speed <= 1.0 {
            return (Arc::clone(&loaded.samples), 1.0);
        }
        match stretches.iter().find(|(factor, _)| *factor == speed) {
            Some((factor, compressed)) => (Arc::clone(compressed), *factor),
            None => (Arc::clone(&loaded.buffer), loaded.factor),
        }
    }

    /// Restarts playback on the buffer for the current speed, keeping the
    /// position, when it differs from what is queued.
    fn apply_speed(&mut self) {
        let Some(loaded) = self.loaded.as_ref() else {
            return;
        };
        let (wanted, _) = Self::buffer_for(loaded, &self.stretches, self.speed);
        if self.output.is_none() || Arc::ptr_eq(&wanted, &loaded.buffer) {
            return;
        }
        let total = clip_length(loaded.samples.len());
        let fraction = if total > Duration::ZERO {
            (self.status(&loaded.message).position.as_secs_f64() / total.as_secs_f64()) as f32
        } else {
            0.0
        }
        .clamp(0.0, 1.0);
        let paused = loaded.paused;
        if self.restart(fraction).is_ok() && paused {
            if let Some((_, sink)) = &self.output {
                sink.pause();
            }
            if let Some(loaded) = self.loaded.as_mut() {
                loaded.paused = true;
            }
        }
    }

    /// Builds the compression for the current speed in the background, if it
    /// is still missing. Replacing an outstanding job cancels it, and so does
    /// going back to 1x, which needs none.
    fn ensure_stretch(&mut self) {
        let factor = self.speed;
        if factor <= 1.0 {
            self.stretching = None;
            return;
        }
        if self.stretches.iter().any(|(built, _)| *built == factor)
            || self
                .stretching
                .as_ref()
                .is_some_and(|job| job.factor == factor)
        {
            return;
        }
        let Some(loaded) = &self.loaded else {
            return;
        };
        let samples = Arc::clone(&loaded.samples);
        let waker = self.waker.clone();
        let slot: StretchedSlot = Default::default();
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_slot = Arc::clone(&slot);
        let thread_cancelled = Arc::clone(&cancelled);
        let spawned = std::thread::Builder::new()
            .name("voice-stretch".to_owned())
            .spawn(move || {
                let Some(compressed) =
                    crate::timestretch::speed_up_unless(&samples, factor, &thread_cancelled)
                else {
                    return;
                };
                *thread_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(compressed));
                waker.wake();
            });
        if spawned.is_ok() {
            self.stretching = Some(Stretching {
                factor,
                slot,
                cancelled,
            });
        }
    }

    /// Plays `samples`, mono 48 kHz, as the clip of `message`, from a
    /// fraction from 0 to 1, without decoding a file.
    pub fn play_samples(
        &mut self,
        message: &str,
        samples: Arc<Vec<f32>>,
        start: f32,
    ) -> Result<(), String> {
        self.stop();
        if samples.is_empty() {
            return Err("The clip is empty".to_owned());
        }
        self.loaded = Some(Loaded {
            message: message.to_owned(),
            buffer: Arc::clone(&samples),
            samples,
            factor: 1.0,
            base: Duration::ZERO,
            paused: false,
            done: false,
        });
        self.restart(start)?;
        self.ensure_stretch();
        Ok(())
    }

    /// Whether the clip of `message` is in the player, playing, paused or
    /// played to its end.
    pub fn holds(&self, message: &str) -> bool {
        self.loaded
            .as_ref()
            .is_some_and(|loaded| loaded.message == message)
    }

    /// Plays or pauses a message. Finished clips restart; new clips decode first.
    pub fn toggle(&mut self, message: &str, path: &Path) -> Result<(), String> {
        match self.loaded.as_mut() {
            Some(loaded) if loaded.message == message => {
                if loaded.done {
                    return self.restart(0.0);
                }
                if let Some((_, sink)) = &self.output {
                    if loaded.paused {
                        sink.play();
                    } else {
                        sink.pause();
                    }
                    loaded.paused = !loaded.paused;
                }
                Ok(())
            }
            _ => self.load(message, path, 0.0),
        }
    }

    /// Seeks to a fraction from 0 to 1 and starts playback.
    pub fn seek(&mut self, message: &str, path: &Path, fraction: f32) -> Result<(), String> {
        match &self.loaded {
            Some(loaded) if loaded.message == message => self.restart(fraction),
            _ => self.load(message, path, fraction),
        }
    }

    /// Clears the loaded clip and releases the output device.
    pub fn stop(&mut self) {
        self.output = None;
        self.loaded = None;
        self.decoding = None;
        self.stretches.clear();
        self.stretching = None;
        // A clip the reader stopped is not one that played to its end.
        self.finished = None;
    }

    /// Takes the message whose clip just played to its end, once. The app uses
    /// it to carry on with the next unplayed voice message.
    pub fn take_finished(&mut self) -> Option<String> {
        self.finished.take()
    }

    /// Whether audio is currently playing.
    pub fn is_playing(&self) -> bool {
        self.decoding.is_some()
            || self
                .loaded
                .as_ref()
                .is_some_and(|loaded| !loaded.paused && !loaded.done)
    }

    pub fn status(&self, message: &str) -> Status {
        if let Some(decoding) = &self.decoding
            && decoding.message == message
        {
            return Status {
                state: State::Loading,
                ..Status::IDLE
            };
        }
        match &self.loaded {
            Some(loaded) if loaded.message == message => {
                let total = clip_length(loaded.samples.len());
                if loaded.done {
                    return Status {
                        state: State::Idle,
                        position: Duration::ZERO,
                        total,
                    };
                }
                let position = self
                    .output
                    .as_ref()
                    .map(|(_, sink)| loaded.base + sink.get_pos().mul_f32(loaded.factor))
                    .unwrap_or(loaded.base)
                    .min(total);
                Status {
                    state: if loaded.paused {
                        State::Paused
                    } else {
                        State::Playing
                    },
                    position,
                    total,
                }
            }
            _ => Status::IDLE,
        }
    }

    /// Generated waveform for a decoded clip.
    pub fn bars(&self, message: &str) -> Option<&[u8]> {
        self.bars.get(message).map(Vec::as_slice)
    }

    /// Handles completed decodes and finished playback once per frame.
    pub fn poll(&mut self) -> Result<(), String> {
        let decoded = self.decoding.as_ref().and_then(|decoding| {
            decoding
                .slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
        });
        if let Some(result) = decoded {
            let Decoding { message, start, .. } = self.decoding.take().expect("just seen");
            let samples = result?;
            if samples.is_empty() {
                return Err("The clip is empty".to_owned());
            }
            let samples = Arc::new(samples);
            self.bars
                .entry(message.clone())
                .or_insert_with(|| voice::waveform(&samples));
            self.loaded = Some(Loaded {
                message,
                buffer: Arc::clone(&samples),
                factor: 1.0,
                samples,
                base: Duration::ZERO,
                paused: false,
                done: false,
            });
            self.restart(start)?;
            self.ensure_stretch();
        }
        let compressed = self
            .stretching
            .as_ref()
            .and_then(|job| job.slot.lock().unwrap_or_else(|p| p.into_inner()).take());
        if let Some(samples) = compressed {
            let factor = self.stretching.take().expect("just seen").factor;
            self.stretches.push((factor, samples));
            self.apply_speed();
            // The speed may have moved on while this compression built.
            self.ensure_stretch();
        }
        if self.output.as_mut().is_some_and(|(output, _)| {
            !output.matches_preference(self.output_device.as_deref()) || output.route_changed()
        }) {
            let status = self
                .loaded
                .as_ref()
                .map(|loaded| self.status(&loaded.message));
            self.output = None;
            if let Some(status) = status {
                let fraction = if status.total.is_zero() {
                    0.0
                } else {
                    status.position.as_secs_f32() / status.total.as_secs_f32()
                };
                if let Err(error) = self.restart(fraction) {
                    if let Some(loaded) = self.loaded.as_mut() {
                        loaded.paused = true;
                        loaded.base = status.position;
                    }
                    return Err(error);
                }
                if status.state == State::Paused {
                    if let Some((_, sink)) = self.output.as_ref() {
                        sink.pause();
                    }
                    if let Some(loaded) = self.loaded.as_mut() {
                        loaded.paused = true;
                    }
                }
            }
        }
        let ended = match (&mut self.loaded, &self.output) {
            (Some(loaded), Some((_, sink))) if !loaded.done && !loaded.paused && sink.empty() => {
                loaded.done = true;
                true
            }
            _ => false,
        };
        if ended {
            // Release the device after playback ends.
            self.output = None;
            self.finished = self.loaded.as_ref().map(|loaded| loaded.message.clone());
        }
        Ok(())
    }

    fn load(&mut self, message: &str, path: &Path, start: f32) -> Result<(), String> {
        self.stop();
        let slot: Decoded = Default::default();
        let path = path.to_owned();
        let waker = self.waker.clone();
        let thread_slot = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("voice-decode".to_owned())
            .spawn(move || {
                let result = decode_file(&path);
                *thread_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
                waker.wake();
            });
        if let Err(error) = spawned {
            return Err(format!("Could not decode audio: {error}"));
        }
        self.decoding = Some(Decoding {
            message: message.to_owned(),
            start,
            slot,
        });
        Ok(())
    }

    /// Plays the loaded clip from a fraction from 0 to 1.
    fn restart(&mut self, fraction: f32) -> Result<(), String> {
        let Some(loaded) = self.loaded.as_mut() else {
            return Ok(());
        };
        // The sink always plays at 1x: running it faster sharpens the voice,
        // so speeds above 1x queue a time-compressed copy of the clip.
        let (buffer, factor) = Self::buffer_for(loaded, &self.stretches, self.speed);
        let total = clip_length(loaded.samples.len());
        let offset = ((fraction.clamp(0.0, 1.0) * buffer.len() as f32) as usize).min(buffer.len());
        if self.output.is_none() {
            let mut device = open_output_on(self.output_device.as_deref()).map_err(|_| {
                "Could not open sound output. Choose an output in Settings and resume playback."
                    .to_owned()
            })?;
            self.notice = device.take_notice();
            let sink = rodio::Player::connect_new(device.mixer());
            self.output = Some((device, sink));
        }
        let (_, sink) = self.output.as_ref().expect("just opened");
        sink.clear();
        sink.append(SamplesBuffer::new(
            mono(),
            rate(),
            buffer[offset..].to_vec(),
        ));
        sink.play();
        loaded.buffer = buffer;
        loaded.factor = factor;
        loaded.base =
            Duration::from_secs_f64(fraction.clamp(0.0, 1.0) as f64 * total.as_secs_f64());
        loaded.paused = false;
        loaded.done = false;
        Ok(())
    }
}

fn clip_length(samples: usize) -> Duration {
    Duration::from_secs_f64(samples as f64 / f64::from(voice::RATE))
}

/// Decodes a file to mono 48 kHz samples. OGG/Opus uses `voice`; other
/// supported formats use rodio.
fn decode_file(path: &Path) -> Result<Vec<f32>, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("Could not read the audio: {error}"))?;
    if bytes.starts_with(b"OggS")
        && let Ok(samples) = voice::decode(&bytes)
    {
        return Ok(samples);
    }
    let file =
        std::fs::File::open(path).map_err(|error| format!("Could not read the audio: {error}"))?;
    let decoder = rodio::Decoder::new(std::io::BufReader::new(file))
        .map_err(|error| format!("Could not decode the audio: {error}"))?;
    let channels = decoder.channels().get();
    let rate = decoder.sample_rate().get();
    let interleaved: Vec<f32> = decoder.collect();
    Ok(voice::mono_at_rate(&interleaved, channels, rate))
}

type Outcome = Arc<Mutex<Option<Result<Vec<f32>, String>>>>;

/// Pause state shared with the recording thread: whether the take is paused,
/// how long pauses lasted, and the take so far for listening back.
#[derive(Default)]
struct Pause {
    paused: AtomicBool,
    pauses: Mutex<Pauses>,
    so_far: Mutex<Option<Arc<Vec<f32>>>>,
}

#[derive(Default)]
struct Pauses {
    since: Option<Instant>,
    total: Duration,
}

impl Pauses {
    /// How long the take has been paused in all.
    fn paused_for(&self) -> Duration {
        self.total + self.since.map_or(Duration::ZERO, |since| since.elapsed())
    }
}

impl Pause {
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Publishes `heard`, interleaved in the microphone's format, as the take
    /// so far in mono 48 kHz.
    fn publish(&self, heard: &[f32], channels: u16, rate: u32) {
        let mono = voice::mono_at_rate(heard, channels, rate);
        *self.so_far.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(mono));
    }

    fn forget_so_far(&self) {
        *self.so_far.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// What [`Recorder`] calls the paused take it plays back.
const TAKE_SO_FAR: &str = "voice-take-so-far";

/// Records from the chosen microphone until told to stop.
pub struct Recorder {
    started: Instant,
    stop: Arc<AtomicBool>,
    /// Loudness for each recorded 50 ms segment.
    levels: Arc<Mutex<Vec<f32>>>,
    /// While set, a new level waits for the window's next frame instead of
    /// asking for one, so the waveform steps with a screen reader attached.
    stepped: Arc<AtomicBool>,
    outcome: Outcome,
    thread: Option<std::thread::JoinHandle<()>>,
    pause: Arc<Pause>,
    waker: Waker,
    /// Plays the take so far back while paused.
    playback: Option<Player>,
}

impl Recorder {
    pub fn start(waker: Waker) -> Self {
        Self::start_on(waker, None)
    }

    pub fn start_on(waker: Waker, device: Option<String>) -> Self {
        Self::spawn(waker, move |stop, levels, pause, wake| {
            record(stop, levels, pause, wake, device.as_deref())
        })
    }

    /// Records a synthetic voice instead of the microphone, at the pace a
    /// real take would, for offline demos: the waveform grows while it runs
    /// and sending it yields that many seconds of a speech-like tone.
    #[cfg(any(test, feature = "demo"))]
    pub fn simulated(waker: Waker) -> Self {
        Self::spawn(waker, rehearse)
    }

    fn spawn(
        waker: Waker,
        body: impl FnOnce(&AtomicBool, &Mutex<Vec<f32>>, &Pause, &dyn Fn()) -> Result<Vec<f32>, String>
        + Send
        + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let levels: Arc<Mutex<Vec<f32>>> = Default::default();
        let stepped = Arc::new(AtomicBool::new(false));
        let outcome: Outcome = Default::default();
        let pause: Arc<Pause> = Default::default();
        let playback_waker = waker.clone();
        let spawned = {
            let stop = Arc::clone(&stop);
            let levels = Arc::clone(&levels);
            let stepped = Arc::clone(&stepped);
            let outcome = Arc::clone(&outcome);
            let pause = Arc::clone(&pause);
            std::thread::Builder::new()
                .name("voice-record".to_owned())
                .spawn(move || {
                    let wake = || {
                        if !stepped.load(Ordering::Relaxed) {
                            waker.wake();
                        }
                    };
                    let result = body(&stop, &levels, &pause, &wake);
                    *outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
                    waker.wake();
                })
        };
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(error) => {
                *outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(Err(error.to_string()));
                None
            }
        };
        Self {
            started: Instant::now(),
            stop,
            levels,
            stepped,
            outcome,
            thread,
            pause,
            waker: playback_waker,
            playback: None,
        }
    }

    /// Simulated recorder for demos and tests.
    #[cfg(any(test, feature = "demo"))]
    pub fn rehearsal() -> Self {
        let levels: Vec<f32> = (0..90)
            .map(|index| 0.05 + 0.2 * ((index as f32 * 0.6).sin().abs()))
            .collect();
        Self {
            started: Instant::now() - Duration::from_millis(4_500),
            stop: Arc::new(AtomicBool::new(true)),
            levels: Arc::new(Mutex::new(levels)),
            stepped: Default::default(),
            outcome: Default::default(),
            thread: None,
            pause: Default::default(),
            waker: Waker::default(),
            playback: None,
        }
    }

    /// A simulated take paused at 4.5 seconds, with that much of a
    /// speech-like tone to listen back to.
    #[cfg(any(test, feature = "demo"))]
    pub fn paused_rehearsal() -> Self {
        let recorder = Self::rehearsal();
        let mut take = Vec::new();
        for segment in 0..90 {
            take.extend(rehearsal_segment(segment * voice::RATE as usize / 20));
        }
        *recorder
            .pause
            .so_far
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(take));
        recorder.pause.paused.store(true, Ordering::Relaxed);
        *recorder
            .pause
            .pauses
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Pauses {
            since: Some(Instant::now()),
            total: Duration::ZERO,
        };
        recorder
    }

    /// Creates a completed synthetic recording without opening an audio device.
    #[cfg(test)]
    pub(crate) fn recorded_fixture(samples: Vec<f32>) -> Self {
        let recorder = Self::rehearsal();
        *recorder.outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(Ok(samples));
        recorder
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed().saturating_sub(
            self.pause
                .pauses
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .paused_for(),
        )
    }

    /// Pauses the take: what the microphone hears from now on is dropped
    /// until [`Recorder::resume`], and what was recorded can be played back
    /// once [`Recorder::so_far`] has it. A take with no levels yet has
    /// nothing to pause.
    pub fn pause(&mut self) {
        if self
            .levels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty()
            || self.is_paused()
        {
            return;
        }
        self.pause
            .pauses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .since = Some(Instant::now());
        self.pause.paused.store(true, Ordering::Relaxed);
    }

    /// Records again after a pause, stopping any playback of the take.
    pub fn resume(&mut self) {
        if !self.is_paused() {
            return;
        }
        self.playback = None;
        self.pause.forget_so_far();
        {
            let mut pauses = self.pause.pauses.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(since) = pauses.since.take() {
                pauses.total += since.elapsed();
            }
        }
        self.pause.paused.store(false, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.pause.is_paused()
    }

    /// The take so far as mono 48 kHz samples, once a pause has taken effect.
    pub fn so_far(&self) -> Option<Arc<Vec<f32>>> {
        self.pause
            .so_far
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Plays or pauses the paused take. Does nothing until the take so far
    /// is ready, and while recording.
    pub fn toggle_playback(&mut self) -> Result<(), String> {
        let Some(take) = self.so_far().filter(|_| self.is_paused()) else {
            return Ok(());
        };
        let waker = self.waker.clone();
        let player = self.playback.get_or_insert_with(|| Player::new(waker));
        if player.holds(TAKE_SO_FAR) {
            player.toggle(TAKE_SO_FAR, Path::new(""))
        } else {
            player.play_samples(TAKE_SO_FAR, take, 0.0)
        }
    }

    /// Plays the paused take from a fraction from 0 to 1.
    pub fn seek_playback(&mut self, fraction: f32) -> Result<(), String> {
        let Some(take) = self.so_far().filter(|_| self.is_paused()) else {
            return Ok(());
        };
        let waker = self.waker.clone();
        let player = self.playback.get_or_insert_with(|| Player::new(waker));
        if player.holds(TAKE_SO_FAR) {
            player.seek(TAKE_SO_FAR, Path::new(""), fraction)
        } else {
            player.play_samples(TAKE_SO_FAR, take, fraction)
        }
    }

    /// Pauses playback of the take, if it is playing.
    pub fn hold_playback(&mut self) {
        if let Some(player) = self.playback.as_mut()
            && player.status(TAKE_SO_FAR).state == State::Playing
            && let Err(error) = player.toggle(TAKE_SO_FAR, Path::new(""))
        {
            log::debug!("recording: could not pause playback: {error}");
        }
    }

    /// Where playback of the paused take is, and how long the take is.
    pub fn playback(&self) -> Status {
        match &self.playback {
            Some(player) => player.status(TAKE_SO_FAR),
            None => Status {
                total: self
                    .so_far()
                    .map_or(Duration::ZERO, |take| clip_length(take.len())),
                ..Status::IDLE
            },
        }
    }

    /// Whether the paused take is playing.
    pub fn is_playing_back(&self) -> bool {
        self.playback.as_ref().is_some_and(Player::is_playing)
    }

    /// Notices playback reaching the end of the take, once per frame.
    pub fn poll_playback(&mut self) -> Result<(), String> {
        match self.playback.as_mut() {
            Some(player) => {
                let polled = player.poll();
                // The take is not a message: nothing plays after it.
                player.take_finished();
                polled
            }
            None => Ok(()),
        }
    }

    /// Stops new levels from asking for a frame of their own while
    /// `stepped`: the window then draws them on its stepped schedule.
    pub fn set_stepped(&self, stepped: bool) {
        self.stepped.store(stepped, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn is_stepped(&self) -> bool {
        self.stepped.load(Ordering::Relaxed)
    }

    pub fn levels(&self) -> Vec<f32> {
        self.levels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Error that stopped recording early.
    pub fn failure(&self) -> Option<String> {
        match self
            .outcome
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            Some(Err(error)) => Some(error.clone()),
            _ => None,
        }
    }

    /// Stops and returns mono 48 kHz samples.
    pub fn finish(mut self) -> Result<Vec<f32>, String> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.outcome
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_else(|| Err("No audio was recorded".to_owned()))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A speech-like tone for [`Recorder::simulated`], one level per 50 ms.
#[cfg(any(test, feature = "demo"))]
fn rehearse(
    stop: &AtomicBool,
    levels: &Mutex<Vec<f32>>,
    pause: &Pause,
    wake: &dyn Fn(),
) -> Result<Vec<f32>, String> {
    let mut samples = Vec::new();
    let mut published = false;
    while !stop.load(Ordering::Relaxed) {
        if pause.is_paused() {
            if !published {
                pause.publish(&samples, 1, voice::RATE);
                published = true;
                wake();
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        published = false;
        let start = samples.len();
        samples.extend(rehearsal_segment(start));
        let peak = samples[start..]
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        levels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(peak * 0.7);
        wake();
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(samples)
}

/// 50 ms of [`rehearse`]'s tone, from sample `start` of the take.
#[cfg(any(test, feature = "demo"))]
fn rehearsal_segment(start: usize) -> impl Iterator<Item = f32> {
    let segment = voice::RATE as usize / 20;
    (start..start + segment).map(|index| {
        let t = index as f32 / voice::RATE as f32;
        (t * 180.0 * std::f32::consts::TAU).sin() * 0.35 * ((t * 2.3).sin() * (t * 0.9).cos()).abs()
    })
}

fn record(
    stop: &AtomicBool,
    levels: &Mutex<Vec<f32>>,
    pause: &Pause,
    wake: &dyn Fn(),
    preferred: Option<&str>,
) -> Result<Vec<f32>, String> {
    use rodio::cpal::{
        SampleFormat,
        traits::{DeviceTrait, HostTrait, StreamTrait},
    };
    if stop.load(Ordering::Acquire) {
        return Ok(Vec::new());
    }
    let host = rodio::cpal::default_host();
    let device = match preferred {
        Some(name) => rodio::microphone::available_inputs()
            .map_err(|_| {
                "The microphone devices could not be listed. Check audio permissions.".to_owned()
            })?
            .into_iter()
            .find(|input| input.to_string() == name)
            .map(|input| input.into_inner())
            .ok_or_else(|| {
                "The selected microphone is unavailable. Choose a microphone in Settings."
                    .to_owned()
            })?,
        None => host.default_input_device().ok_or_else(|| {
            "No microphone is available. Connect one and check microphone permissions.".to_owned()
        })?,
    };
    let supported = match device.default_input_config() {
        Ok(config) => config,
        Err(_) => device
            .supported_input_configs()
            .map_err(|_| {
                "The microphone formats could not be read. Check microphone permissions.".to_owned()
            })?
            .next()
            .map(|config| {
                let rate = voice::RATE.clamp(config.min_sample_rate(), config.max_sample_rate());
                config.with_sample_rate(rate)
            })
            .ok_or_else(|| {
                "The microphone has no supported format. Choose another microphone.".to_owned()
            })?,
    };
    let config: rodio::cpal::StreamConfig = supported.clone().into();
    let channels = config.channels;
    let rate = config.sample_rate;
    if channels == 0 || channels > 32 || rate == 0 || rate > 192_000 {
        return Err(
            "The microphone format exceeds the supported limits. Choose another microphone.".into(),
        );
    }
    let (send, receive) = std::sync::mpsc::sync_channel(8);
    let failed = Arc::new(AtomicBool::new(false));
    if stop.load(Ordering::Acquire) {
        return Ok(Vec::new());
    }
    let stream = match supported.sample_format() {
        SampleFormat::F32 => record_stream::<f32>(&device, &config, send, failed.clone()),
        SampleFormat::F64 => record_stream::<f64>(&device, &config, send, failed.clone()),
        SampleFormat::I8 => record_stream::<i8>(&device, &config, send, failed.clone()),
        SampleFormat::I16 => record_stream::<i16>(&device, &config, send, failed.clone()),
        SampleFormat::I32 => record_stream::<i32>(&device, &config, send, failed.clone()),
        SampleFormat::I64 => record_stream::<i64>(&device, &config, send, failed.clone()),
        SampleFormat::U8 => record_stream::<u8>(&device, &config, send, failed.clone()),
        SampleFormat::U16 => record_stream::<u16>(&device, &config, send, failed.clone()),
        SampleFormat::U32 => record_stream::<u32>(&device, &config, send, failed.clone()),
        SampleFormat::U64 => record_stream::<u64>(&device, &config, send, failed.clone()),
        _ => {
            return Err(
                "The microphone sample format is unsupported. Choose another microphone.".into(),
            );
        }
    }
    .map_err(|_| {
        "Could not open the microphone. Check microphone permissions and other apps using it."
            .to_owned()
    })?;
    if stop.load(Ordering::Acquire) {
        return Ok(Vec::new());
    }
    stream
        .play()
        .map_err(|_| "Could not start the microphone. Check audio permissions.".to_owned())?;
    let result = collect_recording(
        stop,
        levels,
        pause,
        wake,
        &receive,
        &failed,
        (channels, rate),
    );
    drop(stream);
    result
}

fn record_stream<T: rodio::cpal::SizedSample>(
    device: &rodio::cpal::Device,
    config: &rodio::cpal::StreamConfig,
    send: std::sync::mpsc::SyncSender<Vec<f32>>,
    failed: Arc<AtomicBool>,
) -> Result<rodio::cpal::Stream, rodio::cpal::BuildStreamError>
where
    f32: rodio::cpal::FromSample<T>,
{
    use rodio::cpal::traits::DeviceTrait;
    let overflow = failed.clone();
    device.build_input_stream(
        config,
        move |samples: &[T], _| {
            if samples.len() > 192000 {
                overflow.store(true, Ordering::Release);
                return;
            }
            let values = samples
                .iter()
                .copied()
                .map(|sample| sample.to_sample::<f32>())
                .collect();
            if send.try_send(values).is_err() {
                overflow.store(true, Ordering::Release);
            }
        },
        move |_| {
            failed.store(true, Ordering::Release);
        },
        None,
    )
}

fn collect_recording(
    stop: &AtomicBool,
    levels: &Mutex<Vec<f32>>,
    pause: &Pause,
    wake: &dyn Fn(),
    receive: &std::sync::mpsc::Receiver<Vec<f32>>,
    failed: &AtomicBool,
    format: (u16, u32),
) -> Result<Vec<f32>, String> {
    let (channels, rate) = format;
    let started = Instant::now();
    let mut heard = Vec::new();
    let mut last_audio = Instant::now();
    let window = (rate as usize * channels as usize / 20).max(1);
    let mut energy = 0.0;
    let mut count = 0;
    // Pauses as this loop saw them: the longest take counts recorded time.
    let mut paused_since: Option<Instant> = None;
    let mut paused_for = Duration::ZERO;
    let mut published = false;
    let recorded = |paused_since: Option<Instant>, paused_for: Duration| {
        started.elapsed().saturating_sub(
            paused_for + paused_since.map_or(Duration::ZERO, |since| since.elapsed()),
        )
    };
    while !stop.load(Ordering::Acquire) && recorded(paused_since, paused_for) < LONGEST_RECORDING {
        if failed.load(Ordering::Acquire) {
            return Err("The microphone stopped recording. Check its connection, permissions, and audio settings.".into());
        }
        match (pause.is_paused(), paused_since) {
            (true, None) => {
                // The take so far is ready even if no more audio arrives.
                paused_since = Some(Instant::now());
                if !published && !heard.is_empty() {
                    pause.publish(&heard, channels, rate);
                    published = true;
                    wake();
                }
            }
            (false, Some(since)) => {
                paused_for += since.elapsed();
                paused_since = None;
                published = false;
            }
            _ => {}
        }
        // While paused the microphone is still read, so it neither backs up
        // nor looks stalled.
        if pause.is_paused() {
            last_audio = Instant::now();
        }
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(values) => {
                if values.is_empty() {
                    continue;
                }
                // While paused what the microphone hears is dropped.
                if pause.is_paused() {
                    if !published && !heard.is_empty() {
                        pause.publish(&heard, channels, rate);
                        published = true;
                        wake();
                    }
                    continue;
                }
                if heard.len().saturating_add(values.len())
                    > 256 * 1024 * 1024 / std::mem::size_of::<f32>()
                {
                    return Err(
                        "The recording reached its memory limit. Record a shorter voice message."
                            .into(),
                    );
                }
                last_audio = Instant::now();
                for sample in &values {
                    energy += sample * sample;
                    count += 1;
                    if count == window {
                        levels
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push((energy / count as f32).sqrt());
                        energy = 0.0;
                        count = 0;
                        wake();
                    }
                }
                heard.extend(values);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if last_audio.elapsed() >= Duration::from_secs(5) {
                    return Err("The microphone stopped providing audio. Check its connection and permissions.".into());
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("The microphone disconnected before recording finished.".into());
            }
        }
    }
    if failed.load(Ordering::Acquire) {
        return Err("The microphone stopped recording. Check its connection, permissions, and audio settings.".into());
    }
    if heard.is_empty() {
        return Err(
            "The microphone did not record any audio. Check its input level and permissions."
                .into(),
        );
    }
    Ok(voice::mono_at_rate(&heard, channels, rate))
}

/// Temporary recording path used before sending and archiving.
#[allow(dead_code)]
pub fn recording_path(dir: &Path) -> PathBuf {
    dir.join(format!("voice-{}.ogg", crate::util::now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_stream_errors_never_return_a_partial_take() {
        let (send, receive) = std::sync::mpsc::sync_channel(8);
        send.send(vec![0.5; 2400]).unwrap();
        let stop = AtomicBool::new(false);
        let failed = AtomicBool::new(false);
        let result = collect_recording(
            &stop,
            &Mutex::new(Vec::new()),
            &Pause::default(),
            &|| {
                failed.store(true, Ordering::Release);
                stop.store(true, Ordering::Release);
            },
            &receive,
            &failed,
            (1, 48_000),
        );
        assert!(result.unwrap_err().contains("stopped recording"));
    }

    #[test]
    fn stopped_and_disconnected_recorders_finish_without_native_devices() {
        let (send, receive) = std::sync::mpsc::sync_channel(8);
        let stop = AtomicBool::new(true);
        let failed = AtomicBool::new(false);
        assert!(
            collect_recording(
                &stop,
                &Mutex::new(Vec::new()),
                &Pause::default(),
                &|| {},
                &receive,
                &failed,
                (1, 48_000)
            )
            .unwrap_err()
            .contains("did not record")
        );
        drop(send);
        stop.store(false, Ordering::Release);
        assert!(
            collect_recording(
                &stop,
                &Mutex::new(Vec::new()),
                &Pause::default(),
                &|| {},
                &receive,
                &failed,
                (1, 48_000)
            )
            .unwrap_err()
            .contains("disconnected")
        );
    }

    #[test]
    fn negotiated_stereo_recording_keeps_fifty_millisecond_levels_and_resamples() {
        let (send, receive) = std::sync::mpsc::sync_channel(8);
        send.send(vec![0.25; 1600]).unwrap();
        let stop = AtomicBool::new(false);
        let levels = Mutex::new(Vec::new());
        let result = collect_recording(
            &stop,
            &levels,
            &Pause::default(),
            &|| {
                stop.store(true, Ordering::Release);
            },
            &receive,
            &AtomicBool::new(false),
            (2, 16_000),
        )
        .unwrap();
        assert_eq!(levels.lock().unwrap().len(), 1);
        assert!((result.len() as isize - 2400).abs() <= 3);
        assert!(result.iter().all(|sample| (*sample - 0.25).abs() < 0.001));
    }

    /// Waits up to a second for `ready`.
    fn wait_for(ready: impl Fn() -> bool) -> bool {
        let started = Instant::now();
        while !ready() {
            if started.elapsed() > Duration::from_secs(1) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    #[test]
    fn audio_heard_while_paused_is_dropped_and_the_take_so_far_published() {
        let stop = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(Pause::default());
        let levels = Arc::new(Mutex::new(Vec::new()));
        let (samples, read) = std::sync::mpsc::channel();
        let capturing = {
            let stop = Arc::clone(&stop);
            let levels = Arc::clone(&levels);
            let pause = Arc::clone(&pause);
            std::thread::spawn(move || {
                collect_recording(
                    &stop,
                    &levels,
                    &pause,
                    &|| {},
                    &read,
                    &AtomicBool::new(false),
                    (1, 48_000),
                )
            })
        };
        let window = 48_000usize / 20;
        samples.send(vec![0.1; window]).unwrap();
        assert!(wait_for(|| levels.lock().unwrap().len() == 1));
        pause.paused.store(true, Ordering::Relaxed);
        assert!(wait_for(|| pause.so_far.lock().unwrap().is_some()));
        assert_eq!(
            pause.so_far.lock().unwrap().as_deref().map(Vec::as_slice),
            Some([0.1; 2400].as_slice()),
            "the take so far is what was heard before the pause"
        );
        samples.send(vec![0.9; window]).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(levels.lock().unwrap().len(), 1, "no levels while paused");
        pause.paused.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(30));
        samples.send(vec![0.3; window]).unwrap();
        assert!(wait_for(|| levels.lock().unwrap().len() == 2));
        stop.store(true, Ordering::Release);
        let heard = capturing.join().unwrap().unwrap();
        assert_eq!(
            heard,
            [vec![0.1; window], vec![0.3; window]].concat(),
            "the paused audio is not kept"
        );
    }

    #[test]
    fn stopping_while_paused_drops_audio_still_arriving() {
        let stop = AtomicBool::new(true);
        let pause = Pause::default();
        pause.paused.store(true, Ordering::Relaxed);
        let (samples, read) = std::sync::mpsc::channel();
        samples.send(vec![0.1, 0.2]).unwrap();
        let result = collect_recording(
            &stop,
            &Mutex::new(Vec::new()),
            &pause,
            &|| {},
            &read,
            &AtomicBool::new(false),
            (1, 48_000),
        );
        assert!(
            result.unwrap_err().contains("did not record"),
            "nothing heard before the pause means no take"
        );
    }

    #[test]
    fn a_paused_take_counts_only_recorded_time() {
        let mut recorder = Recorder::rehearsal();
        recorder.pause();
        assert!(recorder.is_paused());
        let paused_at = recorder.elapsed();
        std::thread::sleep(Duration::from_millis(80));
        let still = recorder.elapsed();
        assert!(
            still.saturating_sub(paused_at) < Duration::from_millis(20),
            "{paused_at:?} became {still:?} while paused"
        );
        recorder.resume();
        assert!(!recorder.is_paused());
        std::thread::sleep(Duration::from_millis(80));
        let resumed = recorder.elapsed();
        assert!(resumed >= still + Duration::from_millis(70), "{resumed:?}");
        assert!(
            resumed < paused_at + Duration::from_millis(150),
            "the pause is not counted: {resumed:?}"
        );
    }

    #[test]
    fn a_take_that_is_not_listening_yet_cannot_pause() {
        let mut recorder = Recorder::rehearsal();
        recorder
            .levels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        recorder.pause();
        assert!(!recorder.is_paused());
    }

    #[test]
    fn a_simulated_take_pauses_resumes_and_keeps_both_parts() {
        let mut recorder = Recorder::simulated(Waker::default());
        assert!(wait_for(|| recorder.levels().len() >= 3));
        recorder.pause();
        assert!(wait_for(|| recorder.so_far().is_some()));
        let before = recorder.so_far().unwrap().len();
        assert_eq!(before, recorder.levels().len() * voice::RATE as usize / 20);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            recorder.levels().len() * voice::RATE as usize / 20,
            before,
            "nothing is recorded while paused"
        );
        assert_eq!(
            recorder.playback().total,
            clip_length(before),
            "playback offers the whole take so far"
        );
        recorder.resume();
        assert!(recorder.so_far().is_none());
        assert!(wait_for(|| recorder.levels().len() * voice::RATE as usize
            / 20
            > before));
        let take = recorder.finish().unwrap();
        assert!(take.len() > before);
        assert!(take.len() < before + voice::RATE as usize, "{}", take.len());
    }

    #[test]
    #[ignore = "opens physical output with synthetic silence only"]
    fn synthetic_silence_uses_the_current_default_and_stream_errors_request_reopening() {
        use rodio::cpal::traits::{DeviceTrait, HostTrait};
        let expected = rodio::cpal::default_host()
            .default_output_device()
            .expect("default output")
            .id()
            .ok();
        let mut output = open_output().expect("open selected output");
        assert_eq!(output.device, expected);
        let player = rodio::Player::connect_new(output.mixer());
        player.append(SamplesBuffer::new(mono(), rate(), vec![0.0; 2400]));
        player.sleep_until_end();
        output.failed.store(true, Ordering::Release);
        assert!(output.route_changed());
        drop(output);
        let reopened = open_output().expect("reopen selected output");
        assert_eq!(reopened.device, expected);
    }

    /// A stepped recorder keeps recording, but its levels stop asking the
    /// window for frames of their own; unstepped, they ask again.
    #[test]
    fn a_stepped_recorder_records_without_asking_for_frames() {
        let ctx = egui::Context::default();
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        ctx.set_request_repaint_callback(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        // Once egui asks for no frame, the next request calls back.
        let settle = || crate::motion::tests::settle(&ctx);
        let waker = Waker::default();
        waker.attach(&ctx);
        let recorder = Recorder::simulated(waker);
        recorder.set_stepped(true);
        // Let a level already under way land first.
        std::thread::sleep(Duration::from_millis(80));
        settle();
        asked.store(0, Ordering::SeqCst);
        let before = recorder.levels().len();
        // Busy machines oversleep, so wait for progress instead of a fixed time.
        let waited = |done: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !done() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            done()
        };
        assert!(
            waited(&|| recorder.levels().len() >= before + 4),
            "still recording"
        );
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        recorder.set_stepped(false);
        assert!(waited(&|| asked.load(Ordering::SeqCst) >= 1));
        recorder.finish().expect("recorded");
    }

    #[test]
    fn a_clip_that_finished_is_handed_back_once() {
        let mut player = Player::new(crate::backend::Waker::default());
        player.finished = Some("clip".into());
        assert_eq!(player.take_finished().as_deref(), Some("clip"));
        assert_eq!(player.take_finished(), None, "the app takes it once");
        player.finished = Some("other".into());
        player.stop();
        assert_eq!(
            player.take_finished(),
            None,
            "a clip the reader stopped is not one that played to its end"
        );
    }

    #[test]
    fn speed_labels_match_the_button() {
        assert_eq!(speed_label(SPEEDS[0]), "1x");
        assert_eq!(speed_label(SPEEDS[1]), "1.25x");
        assert_eq!(speed_label(SPEEDS[2]), "1.5x");
        assert_eq!(speed_label(SPEEDS[3]), "1.75x");
        assert_eq!(speed_label(SPEEDS[4]), "2x");
    }

    #[test]
    fn the_chip_cycles_like_the_phone() {
        assert_eq!(next_cycled_speed(1.0), 1.5);
        assert_eq!(next_cycled_speed(1.5), 2.0);
        assert_eq!(next_cycled_speed(2.0), 1.0);
        // A speed chosen from the menu moves on to the next faster one.
        assert_eq!(next_cycled_speed(1.25), 1.5);
        assert_eq!(next_cycled_speed(1.75), 2.0);
    }

    #[test]
    fn unsupported_speeds_snap_to_the_nearest_supported_one() {
        assert_eq!(supported_speed(1.3), 1.25);
        assert_eq!(supported_speed(1.4), 1.5);
        assert_eq!(supported_speed(1.8), 1.75);
        assert_eq!(supported_speed(0.5), 1.0);
        assert_eq!(supported_speed(4.0), 2.0);
        assert_eq!(supported_speed(f32::INFINITY), 1.0);
        for speed in SPEEDS {
            assert_eq!(supported_speed(speed), speed);
        }
        let mut player = Player::new(Waker::default());
        assert_eq!(player.set_speed(1.3), 1.25);
        assert_eq!(player.speed(), 1.25);
    }

    #[test]
    fn setting_a_speed_clamps_to_the_supported_range() {
        let mut player = Player::new(Waker::default());
        assert_eq!(player.speed(), SPEEDS[0]);
        player.set_speed(1.75);
        assert_eq!(player.speed(), 1.75);
        // Beyond the fastest speed clamps to it.
        player.set_speed(4.0);
        assert_eq!(player.speed(), SPEEDS[SPEEDS.len() - 1]);
        // A non-finite speed plays at 1x.
        player.set_speed(f32::NAN);
        assert_eq!(player.speed(), SPEEDS[0]);
    }

    #[test]
    fn a_speed_still_building_keeps_the_queued_one() {
        let samples = Arc::new(vec![0.0; 12]);
        let one_and_a_half = Arc::new(vec![0.0; 8]);
        let double = Arc::new(vec![0.0; 6]);
        let loaded = Loaded {
            message: "clip".to_owned(),
            samples: Arc::clone(&samples),
            buffer: Arc::clone(&one_and_a_half),
            factor: 1.5,
            base: Duration::ZERO,
            paused: false,
            done: false,
        };
        let mut stretches = vec![(1.5, Arc::clone(&one_and_a_half))];

        let (buffer, factor) = Player::buffer_for(&loaded, &stretches, 2.0);
        assert!(Arc::ptr_eq(&buffer, &one_and_a_half));
        assert_eq!(factor, 1.5);

        stretches.push((2.0, Arc::clone(&double)));
        let (buffer, factor) = Player::buffer_for(&loaded, &stretches, 2.0);
        assert!(Arc::ptr_eq(&buffer, &double));
        assert_eq!(factor, 2.0);
        // Both built speeds stay available when cycling back.
        let (buffer, _) = Player::buffer_for(&loaded, &stretches, 1.5);
        assert!(Arc::ptr_eq(&buffer, &one_and_a_half));
        let (buffer, factor) = Player::buffer_for(&loaded, &stretches, 1.0);
        assert!(Arc::ptr_eq(&buffer, &samples));
        assert_eq!(factor, 1.0);
    }

    #[test]
    fn a_speed_is_preparing_until_the_clip_plays_at_it() {
        let samples = Arc::new(vec![0.0; 12]);
        let mut player = Player::new(Waker::default());
        player.loaded = Some(Loaded {
            message: "clip".to_owned(),
            buffer: Arc::clone(&samples),
            samples,
            factor: 1.0,
            base: Duration::ZERO,
            paused: true,
            done: false,
        });
        assert!(!player.preparing_speed("clip"));
        player.speed = 2.0;
        assert!(player.preparing_speed("clip"));
        assert!(!player.preparing_speed("another clip"));
        if let Some(loaded) = player.loaded.as_mut() {
            loaded.factor = 2.0;
        }
        assert!(!player.preparing_speed("clip"));
    }

    #[test]
    fn unusable_speeds_are_kept_in_range() {
        let mut player = Player::new(Waker::default());
        player.set_speed(f32::NAN);
        assert_eq!(player.speed(), 1.0);
        player.set_speed(f32::INFINITY);
        assert_eq!(player.speed(), 1.0);
        player.set_speed(50.0);
        assert_eq!(player.speed(), 2.0);
        player.set_speed(-3.0);
        assert_eq!(player.speed(), 1.0);
    }

    #[test]
    fn going_back_to_one_x_cancels_the_outstanding_compression() {
        let mut player = Player::new(Waker::default());
        let cancelled = Arc::new(AtomicBool::new(false));
        player.set_speed(2.0);
        player.stretching = Some(Stretching {
            factor: 2.0,
            slot: Default::default(),
            cancelled: Arc::clone(&cancelled),
        });
        player.set_speed(1.0);
        assert!(player.stretching.is_none());
        assert!(cancelled.load(Ordering::Relaxed));
    }

    /// Plays a one-second test tone:
    /// `cargo test audio::tests::plays -- --ignored --nocapture`.
    #[test]
    #[ignore = "makes a sound on this machine"]
    fn plays_a_clip_on_this_machine() {
        let dir = std::env::temp_dir();
        let path = dir.join("zapfasterk-audio-test.ogg");
        let tone: Vec<f32> = (0..voice::RATE)
            .map(|i| (i as f32 * 330.0 * std::f32::consts::TAU / voice::RATE as f32).sin() * 0.3)
            .collect();
        std::fs::write(&path, voice::encode(&tone).expect("encodes")).expect("written");
        let mut player = Player::new(Waker::default());
        player.toggle("clip", &path).expect("starts decoding");
        assert_eq!(player.status("clip").state, State::Loading);
        let started = Instant::now();
        let mut seen_playing = false;
        while started.elapsed() < Duration::from_secs(3) {
            player.poll().expect("plays");
            let status = player.status("clip");
            if status.state == State::Playing && status.position > Duration::from_millis(300) {
                seen_playing = true;
                eprintln!("playing at {:?} of {:?}", status.position, status.total);
            }
            if seen_playing && status.state == State::Idle {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(seen_playing, "never heard it playing");
        assert_eq!(player.status("clip").state, State::Idle, "ends on its own");
        assert_eq!(player.bars("clip").map(<[u8]>::len), Some(voice::BARS));
        let _ = std::fs::remove_file(path);
    }

    /// Plays a two-second tone at double speed and checks the position
    /// outruns the clock:
    /// `cargo test audio::tests::doubles -- --ignored --nocapture`.
    #[test]
    #[ignore = "makes a sound on this machine"]
    fn doubles_the_position_rate_on_this_machine() {
        let dir = std::env::temp_dir();
        let path = dir.join("zapfasterk-audio-speed-test.ogg");
        let tone: Vec<f32> = (0..voice::RATE * 2)
            .map(|i| (i as f32 * 330.0 * std::f32::consts::TAU / voice::RATE as f32).sin() * 0.3)
            .collect();
        std::fs::write(&path, voice::encode(&tone).expect("encodes")).expect("written");
        let mut player = Player::new(Waker::default());
        player.set_speed(2.0);
        player.toggle("clip", &path).expect("starts decoding");
        let started = Instant::now();
        let mut seen: Vec<(Duration, Duration)> = Vec::new();
        while started.elapsed() < Duration::from_secs(6) {
            player.poll().expect("plays");
            let status = player.status("clip");
            // The clip starts at 1x while its compression builds; measure
            // only after the compressed buffer has taken over.
            if status.state == State::Playing && status.position > Duration::from_millis(600) {
                seen.push((started.elapsed(), status.position));
            }
            if status.state == State::Idle && !seen.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let (first_wall, first_position) = seen.first().expect("played");
        let (last_wall, last_position) = seen.last().expect("played");
        let wall = *last_wall - *first_wall;
        let advanced = *last_position - *first_position;
        assert!(wall > Duration::from_millis(200), "played for {wall:?}");
        assert!(
            advanced.as_secs_f32() >= 1.5 * wall.as_secs_f32(),
            "position advanced {advanced:?} over {wall:?} of wall time"
        );
        let _ = std::fs::remove_file(path);
    }

    /// Records one second from the default microphone:
    /// `cargo test audio::tests::records -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a microphone"]
    fn records_a_second_on_this_machine() {
        let recorder = Recorder::start(Waker::default());
        std::thread::sleep(Duration::from_millis(1_000));
        assert!(recorder.failure().is_none(), "{:?}", recorder.failure());
        let levels = recorder.levels();
        let heard = recorder.finish().expect("something was heard");
        eprintln!("{} samples, {} level readings", heard.len(), levels.len());
        assert!(
            heard.len() > voice::RATE as usize * 8 / 10,
            "{}",
            heard.len()
        );
        assert!(levels.len() >= 15, "{}", levels.len());
    }
}
