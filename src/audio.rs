//! Voice playback: a 500 ms stereo ring buffer at 48 kHz, a summing mixer and
//! a cpal output stream. Discord sends 48 kHz Opus, so the mixer never
//! resamples on the network side; the device callback does the one conversion
//! it cannot avoid (44.1 kHz cards and mono outputs are common).
//!
//! cpal streams are not `Send` on every backend, so the stream is created and
//! owned by its own std thread. The network task only pushes PCM into the
//! shared ring.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Discord voice is always 48 kHz.
pub(crate) const SAMPLE_RATE: u32 = 48_000;
/// Discord voice is always stereo.
pub(crate) const CHANNELS: usize = 2;
/// One second of stereo audio: room for a pre-buffer plus a device that pulls
/// large callbacks, still without leaving audible delay in the normal case.
pub(crate) const RING_FRAMES: usize = 48_000;
/// Playback starts only once this much is queued. Without it the first packet
/// underruns immediately and the stream crackles. A device whose callback is
/// longer than this raises the threshold, see `Mixer::target`.
pub(crate) const PREBUFFER_FRAMES: usize = 4_800;
/// Loudest a volume slider may go: ×2, or +6 dB.
pub(crate) const MAX_GAIN: f32 = 2.0;

/// Sound settings the audio threads read. The settings window changes them
/// while a call and a notification chime may already be running, so they live
/// in atomics instead of being copied in at start-up.
struct Live {
    others: AtomicU32,
    own: AtomicU32,
    notify: AtomicU32,
    listen_self: AtomicBool,
    output: Mutex<Option<String>>,
    input: Mutex<Option<String>>,
}

fn live() -> &'static Live {
    static LIVE: OnceLock<Live> = OnceLock::new();
    LIVE.get_or_init(|| Live {
        others: AtomicU32::new(1.0f32.to_bits()),
        own: AtomicU32::new(1.0f32.to_bits()),
        notify: AtomicU32::new(1.0f32.to_bits()),
        listen_self: AtomicBool::new(false),
        output: Mutex::new(None),
        input: Mutex::new(None),
    })
}

/// A stored volume, clamped to `0..=max`; absent means full volume.
fn volume_or(value: Option<f32>, max: f32) -> f32 {
    value.unwrap_or(1.0).clamp(0.0, max)
}

fn volume(cell: &AtomicU32) -> f32 {
    f32::from_bits(cell.load(Ordering::Relaxed))
}

fn set_name(slot: &Mutex<Option<String>>, name: Option<&str>) {
    if let Ok(mut slot) = slot.lock() {
        *slot = name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
    }
}

fn configured_name(slot: &Mutex<Option<String>>) -> Option<String> {
    slot.lock().ok().and_then(|slot| slot.clone())
}

/// Applies the persisted sound settings to the running audio threads. Called
/// on start-up and after every settings write, so a slider is audible at once.
pub(crate) fn apply_settings(settings: &crate::settings::Settings) {
    let live = live();
    live.others.store(
        volume_or(settings.voice_volume, MAX_GAIN).to_bits(),
        Ordering::Relaxed,
    );
    live.own.store(
        volume_or(settings.mic_volume, MAX_GAIN).to_bits(),
        Ordering::Relaxed,
    );
    live.notify.store(
        volume_or(settings.notify_volume, 1.0).to_bits(),
        Ordering::Relaxed,
    );
    live.listen_self
        .store(settings.listen_self, Ordering::Relaxed);
    set_name(&live.output, settings.output_device.as_deref());
    set_name(&live.input, settings.input_device.as_deref());
}

pub(crate) fn others_volume() -> f32 {
    volume(&live().others)
}

pub(crate) fn own_volume() -> f32 {
    volume(&live().own)
}

pub(crate) fn notify_volume() -> f32 {
    volume(&live().notify)
}

pub(crate) fn listen_self() -> bool {
    live().listen_self.load(Ordering::Relaxed)
}

/// Output devices cpal currently sees, for the settings list.
pub(crate) fn output_devices() -> Vec<String> {
    use cpal::traits::HostTrait;
    let host = cpal::default_host();
    let mut names: Vec<String> = host
        .output_devices()
        .map(|devices| devices.map(|device| device.to_string()).collect())
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

/// Input devices cpal currently sees, for the settings list.
pub(crate) fn input_devices() -> Vec<String> {
    use cpal::traits::HostTrait;
    let host = cpal::default_host();
    let mut names: Vec<String> = host
        .input_devices()
        .map(|devices| devices.map(|device| device.to_string()).collect())
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

/// Summing mixer behind the audio callback. Samples are accumulated in `i32`
/// so two or three simultaneous speakers do not clip before the final clamp.
pub(crate) struct Mixer {
    samples: Vec<i32>,
    /// Absolute frame counter of the next frame a writer fills.
    write: i64,
    /// Absolute (fractional) frame the callback is reading: 48 kHz source.
    read: f64,
    started: bool,
    /// Next frame to wipe after playback passed it. Without this the ring
    /// would add new audio on top of the samples from the previous lap.
    cleared: i64,
    /// Times playback was re-synced because the queue grew too long.
    dropped: u64,
    /// Times the queue ran dry and playback waited for the pre-buffer again.
    underruns: u64,
    /// Packets refused because their position was already played.
    late: u64,
    /// Packets refused because they landed beyond the ring window.
    ahead: u64,
    /// Frames per output callback the device last asked for.
    callback_frames: usize,
}

impl Mixer {
    pub(crate) fn new() -> Self {
        Self {
            samples: vec![0; RING_FRAMES * CHANNELS],
            write: 0,
            read: 0.0,
            started: false,
            cleared: 0,
            dropped: 0,
            underruns: 0,
            late: 0,
            ahead: 0,
            callback_frames: 0,
        }
    }

    /// Ring slot of an absolute frame.
    fn slot(frame: i64) -> usize {
        (frame.rem_euclid(RING_FRAMES as i64) as usize) * CHANNELS
    }

    /// Adds one decoder's interleaved stereo PCM at `start` (absolute 48 kHz
    /// frame, from the RTP timestamp). Packets from different speakers overlap
    /// in the ring and therefore add up.
    pub(crate) fn push_at(&mut self, start: i64, pcm: &[i16]) {
        self.push_at_gain(start, pcm, 1.0);
    }

    /// Same as [`Self::push_at`], with a per-source volume. Applying the gain
    /// before the sum keeps one loud source from drowning out the others and
    /// lets self-listen have its own level.
    pub(crate) fn push_at_gain(&mut self, start: i64, pcm: &[i16], gain: f32) {
        let frames = pcm.len() / CHANNELS;
        if frames == 0 {
            return;
        }
        let gain = if gain.is_finite() {
            gain.clamp(0.0, MAX_GAIN)
        } else {
            1.0
        };
        let end = start + frames as i64;
        let window_start = self.read.floor() as i64;
        let window_end = window_start + RING_FRAMES as i64;
        // Entirely late or too far ahead to fit: dropping is better than
        // wrapping over audio someone is hearing right now.
        if end <= window_start {
            self.late += 1;
            return;
        }
        if end > window_end {
            self.ahead += 1;
            return;
        }
        for (i, frame) in pcm.chunks_exact(CHANNELS).enumerate() {
            let pos = start + i as i64;
            if pos < window_start || pos >= window_end {
                continue;
            }
            let slot = Self::slot(pos);
            self.samples[slot] += (frame[0] as f32 * gain) as i32;
            self.samples[slot + 1] += (frame[1] as f32 * gain) as i32;
        }
        // The watermark is what the callback may read. A packet placed ahead
        // of it leaves silence for the gap, which is exactly the intent.
        if end > self.write {
            self.write = end;
        }
    }

    /// Current playback position in absolute 48 kHz frames.
    pub(crate) fn position(&self) -> i64 {
        self.read.round() as i64
    }

    /// Frames playback needs buffered before it starts. The pre-buffer alone is
    /// not enough when the device asks for a large callback: the callback would
    /// drain the queue and leave silence in the same buffer, then the packets
    /// that arrive meanwhile would be late. Two callbacks always leave one
    /// callback of slack.
    fn target(&self) -> f64 {
        let two_callbacks = (self.callback_frames * 2) as f64;
        (PREBUFFER_FRAMES as f64)
            .max(two_callbacks)
            .min((RING_FRAMES / 2) as f64)
    }

    /// Fills `out` with `out.len() / channels` interleaved frames, resampling
    /// 48 kHz to the device rate (`step = 48000 / device_rate`).
    pub(crate) fn pull(&mut self, out: &mut [f32], channels: usize, step: f64) {
        if channels == 0 || step <= 0.0 {
            out.fill(0.0);
            return;
        }
        self.callback_frames = out.len() / channels;
        let target = self.target();
        // Queue cap: a stalled callback or a burst must not become seconds of
        // delay once playback resumes.
        if self.write as f64 - self.read > RING_FRAMES as f64 {
            self.read = self.write as f64 - target;
            self.started = false;
            self.dropped += 1;
        }
        if self.started && self.write as f64 - self.read < 1.0 {
            // Underrun: re-arm the pre-buffer so the next burst starts clean.
            self.read = self.write as f64;
            self.started = false;
            self.underruns += 1;
        }
        if !self.started {
            if self.write as f64 - self.read < target {
                out.fill(0.0);
                return;
            }
            self.started = true;
        }

        for frame in out.chunks_mut(channels) {
            frame.fill(0.0);
            if self.read + 1.0 >= self.write as f64 {
                continue;
            }
            let base = self.read.floor() as i64;
            let frac = (self.read - base as f64) as f32;
            let a = Self::slot(base);
            let b = Self::slot(base + 1);
            let left = lerp(self.samples[a] as f32, self.samples[b] as f32, frac);
            let right = lerp(self.samples[a + 1] as f32, self.samples[b + 1] as f32, frac);
            frame[0] = clamp(left);
            if channels > 1 {
                frame[1] = clamp(right);
            }
            self.read += step;
        }
        // Everything strictly behind the interpolation window is dead audio:
        // wipe it so the next lap starts from zero. Frame `floor(read)` stays
        // because the next callback still interpolates into it.
        let upto = self.read.floor() as i64;
        while self.cleared < upto {
            let slot = Self::slot(self.cleared);
            self.samples[slot] = 0;
            self.samples[slot + 1] = 0;
            self.cleared += 1;
        }
    }

    /// Source frames still queued for playback; used by tests.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> f64 {
        (self.write as f64 - self.read).max(0.0)
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Converts an accumulated sample to `[-1, 1]`, clamping the sum of several
/// speakers instead of letting it wrap.
fn clamp(value: f32) -> f32 {
    (value / 32_768.0).clamp(-1.0, 1.0)
}

/// Playback handle: shared mixer, stop flag and the threads that own the cpal
/// streams. Dropping it ends playback and closes the microphone.
pub(crate) struct Player {
    mixer: Arc<Mutex<Mixer>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    monitor: Option<std::thread::JoinHandle<()>>,
}

impl Player {
    /// Opens the configured (or default) output device and starts pulling from
    /// the mixer. The error string is user-facing: the voice bar shows it.
    /// Self-listen is possible on this player because it belongs to a call.
    pub(crate) fn start() -> Result<Self, String> {
        Self::start_inner(true)
    }

    /// Output only: no microphone thread ever starts. Used for the notification
    /// chime, which must not open the mic just because self-listen is ticked.
    pub(crate) fn start_output() -> Result<Self, String> {
        Self::start_inner(false)
    }

    fn start_inner(with_monitor: bool) -> Result<Self, String> {
        let mixer = Arc::new(Mutex::new(Mixer::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let ring = Arc::clone(&mixer);
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("wyvern-audio".into())
            .spawn(move || {
                let result = run_stream(ring, Arc::clone(&flag), ready_tx.clone());
                // A failure after `play()` is reported through the same channel
                // only if nobody has listened yet; otherwise it is silent.
                if let Err(e) = result {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| format!("audio thread: {}", e))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                // The microphone is opened on demand: the monitor thread waits
                // for self-listen to be switched on, so the OS mic indicator
                // stays dark until the user asks for it.
                let monitor = if with_monitor {
                    match std::thread::Builder::new()
                        .name("wyvern-monitor".into())
                        .spawn({
                            let ring = Arc::clone(&mixer);
                            let flag = Arc::clone(&stop);
                            move || run_monitor(ring, flag)
                        }) {
                        Ok(handle) => Some(handle),
                        Err(e) => {
                            eprintln!("[audio] monitor thread: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };
                Ok(Self { mixer, stop, thread: Some(thread), monitor })
            }
            Ok(Err(e)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("audio device did not start".to_string())
            }
        }
    }

    /// Queues decoded stereo PCM at an absolute 48 kHz frame.
    pub(crate) fn push_at(&self, start: i64, pcm: &[i16]) {
        self.push_at_gain(start, pcm, 1.0);
    }

    /// Queues PCM with a volume, used for other people's voices.
    pub(crate) fn push_at_gain(&self, start: i64, pcm: &[i16], gain: f32) {
        if let Ok(mut mixer) = self.mixer.lock() {
            mixer.push_at_gain(start, pcm, gain);
        }
    }

    /// Current playback position, used to anchor RTP timestamps.
    pub(crate) fn position(&self) -> i64 {
        self.mixer.lock().map(|m| m.position()).unwrap_or(0)
    }

    /// How much audio is queued for playback, in 48 kHz frames.
    pub(crate) fn buffered(&self) -> i64 {
        self.mixer
            .lock()
            .map(|m| m.write - m.read as i64)
            .unwrap_or(0)
    }

    /// Playback stalls (queue ran dry), forced resyncs (queue too long) and
    /// packets refused as late or too far ahead.
    pub(crate) fn stalls(&self) -> (u64, u64, u64, u64) {
        self.mixer
            .lock()
            .map(|m| (m.underruns, m.dropped, m.late, m.ahead))
            .unwrap_or((0, 0, 0, 0))
    }

    /// Size of the last output callback in 48 kHz frames; 0 before the device
    /// has pulled once.
    pub(crate) fn callback_frames(&self) -> i64 {
        self.mixer
            .lock()
            .map(|m| m.callback_frames as i64)
            .unwrap_or(0)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(monitor) = self.monitor.take() {
            let _ = monitor.join();
        }
    }
}

/// Builds the output stream and keeps it alive until `stop` is set.
fn run_stream(
    ring: Arc<Mutex<Mixer>>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, StreamTrait};

    let host = cpal::default_host();
    let device = pick_output(&host).ok_or("no audio output device")?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("output config: {}", e))?;
    let sample_format = supported.sample_format();
    let config = supported.config();
    let channels = config.channels as usize;
    let step = SAMPLE_RATE as f64 / config.sample_rate as f64;
    let err_cb = |e: cpal::Error| eprintln!("[audio] stream error: {}", e);

    // The callback runs on the audio thread: it reuses one scratch buffer, so
    // only the first buffer-size change allocates.
    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            let ring = Arc::clone(&ring);
            let mut scratch: Vec<f32> = Vec::new();
            device.build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let len = data.len();
                    fill(&ring, &mut scratch, len, channels, step);
                    data.copy_from_slice(&scratch[..len]);
                },
                err_cb,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let ring = Arc::clone(&ring);
            let mut scratch: Vec<f32> = Vec::new();
            device.build_output_stream(
                config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let len = data.len();
                    fill(&ring, &mut scratch, len, channels, step);
                    for (dst, src) in data.iter_mut().zip(&scratch[..len]) {
                        *dst = (src.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                },
                err_cb,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let ring = Arc::clone(&ring);
            let mut scratch: Vec<f32> = Vec::new();
            device.build_output_stream(
                config,
                move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                    let len = data.len();
                    fill(&ring, &mut scratch, len, channels, step);
                    for (dst, src) in data.iter_mut().zip(&scratch[..len]) {
                        let value = src.clamp(-1.0, 1.0) * 0.5 + 0.5;
                        *dst = (value * u16::MAX as f32) as u16;
                    }
                },
                err_cb,
                None,
            )
        }
        other => return Err(format!("unsupported sample format: {:?}", other)),
    }
    .map_err(|e| format!("output stream: {}", e))?;

    stream.play().map_err(|e| format!("play: {}", e))?;
    let _ = ready.send(Ok(()));
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stream);
    Ok(())
}

/// Pulls `len` interleaved `f32` samples from the mixer into `scratch`.
fn fill(ring: &Mutex<Mixer>, scratch: &mut Vec<f32>, len: usize, channels: usize, step: f64) {
    if scratch.len() < len {
        scratch.resize(len, 0.0);
    }
    match ring.lock() {
        Ok(mut mixer) => mixer.pull(&mut scratch[..len], channels, step),
        Err(_) => scratch[..len].fill(0.0),
    }
}

/// Output device by saved name, falling back to the system default. A device
/// that was unplugged since the setting was saved must not break voice.
fn pick_output(host: &cpal::Host) -> Option<cpal::Device> {
    use cpal::traits::HostTrait;
    if let Some(wanted) = configured_name(&live().output) {
        if let Ok(devices) = host.output_devices() {
            for device in devices {
                if device.to_string() == wanted {
                    return Some(device);
                }
            }
        }
    }
    host.default_output_device()
}

fn pick_input(host: &cpal::Host) -> Option<cpal::Device> {
    use cpal::traits::HostTrait;
    if let Some(wanted) = configured_name(&live().input) {
        if let Ok(devices) = host.input_devices() {
            for device in devices {
                if device.to_string() == wanted {
                    return Some(device);
                }
            }
        }
    }
    host.default_input_device()
}

/// Self-listen. Opens the microphone only while the setting is on, converts
/// whatever the device delivers into 48 kHz stereo and writes it into the
/// player's own ring, so it comes out of the same output stream as voice.
fn run_monitor(mixer: Arc<Mutex<Mixer>>, stop: Arc<AtomicBool>) {
    use cpal::traits::StreamTrait;

    let host = cpal::default_host();
    let mut complained = false;
    while !stop.load(Ordering::Relaxed) {
        if !listen_self() {
            complained = false;
            std::thread::sleep(Duration::from_millis(150));
            continue;
        }
        match open_capture(&host, Arc::clone(&mixer)) {
            Some(stream) => {
                if let Err(e) = stream.play() {
                    if !complained {
                        eprintln!("[audio] self-listen: {}", e);
                        complained = true;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                while !stop.load(Ordering::Relaxed) && listen_self() {
                    std::thread::sleep(Duration::from_millis(50));
                }
                drop(stream);
            }
            None => {
                if !complained {
                    eprintln!("[audio] self-listen: no input device");
                    complained = true;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

/// Builds an input stream for the device's default format, or `None` when the
/// device cannot be opened. Sample formats other than F32/I16 still work: cpal
/// converts through `dasp_sample`.
fn open_capture(host: &cpal::Host, mixer: Arc<Mutex<Mixer>>) -> Option<cpal::Stream> {
    use cpal::traits::DeviceTrait;

    let device = pick_input(host)?;
    let supported = device.default_input_config().ok()?;
    let format = supported.sample_format();
    let config = supported.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    let stream = match format {
        cpal::SampleFormat::I8 => capture_stream::<i8>(&device, &config, channels, rate, mixer),
        cpal::SampleFormat::I16 => capture_stream::<i16>(&device, &config, channels, rate, mixer),
        cpal::SampleFormat::I24 => capture_stream::<cpal::I24>(&device, &config, channels, rate, mixer),
        cpal::SampleFormat::I32 => capture_stream::<i32>(&device, &config, channels, rate, mixer),
        cpal::SampleFormat::I64 => capture_stream::<i64>(&device, &config, channels, rate, mixer),
        cpal::SampleFormat::U8 => capture_stream::<u8>(&device, &config, channels, rate, mixer),
        cpal::SampleFormat::U16 => capture_stream::<u16>(&device, &config, channels, rate, mixer),
        cpal::SampleFormat::U24 => capture_stream::<cpal::U24>(&device, &config, channels, rate, mixer),
        cpal::SampleFormat::U32 => capture_stream::<u32>(&device, &config, channels, rate, mixer),
        cpal::SampleFormat::U64 => capture_stream::<u64>(&device, &config, channels, rate, mixer),
        cpal::SampleFormat::F32 => capture_stream::<f32>(&device, &config, channels, rate, mixer),
        cpal::SampleFormat::F64 => capture_stream::<f64>(&device, &config, channels, rate, mixer),
        _ => return None,
    };
    stream.ok()
}

fn capture_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    rate: u32,
    mixer: Arc<Mutex<Mixer>>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::traits::DeviceTrait;
    use cpal::Sample as _;

    let mut state = CaptureState::new();
    let mut input: Vec<f32> = Vec::new();
    device.build_input_stream(
        config.clone(),
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            if !listen_self() {
                return;
            }
            input.clear();
            input.extend(data.iter().map(|sample| f32::from_sample(*sample)));
            state.push(&mixer, channels, rate, &input);
        },
        |e| eprintln!("[audio] input stream: {}", e),
        None,
    )
}

/// Microphone path state carried between callbacks: a fractional resampler
/// position and the mixer frame the next chunk belongs to.
struct CaptureState {
    pos: f64,
    anchor: Option<i64>,
    resampled: Vec<i16>,
}

impl CaptureState {
    fn new() -> Self {
        Self {
            pos: 0.0,
            anchor: None,
            resampled: Vec::new(),
        }
    }

    fn push(&mut self, mixer: &Mutex<Mixer>, channels: usize, rate: u32, input: &[f32]) {
        self.resampled.clear();
        resample_capture(input, channels, rate, own_volume(), &mut self.pos, &mut self.resampled);
        if self.resampled.is_empty() {
            return;
        }
        let Ok(mut mixer) = mixer.lock() else { return };
        let position = mixer.position();
        let start = match self.anchor {
            // A long stall (or a device reset) leaves the old anchor behind
            // the playback head; jump back onto the pre-buffer.
            Some(anchor) if anchor >= position && anchor <= position + RING_FRAMES as i64 => anchor,
            _ => position + PREBUFFER_FRAMES as i64,
        };
        let frames = (self.resampled.len() / CHANNELS) as i64;
        mixer.push_at(start, &self.resampled);
        self.anchor = Some(start + frames);
    }
}

/// Appends the input buffer to `out` as interleaved 48 kHz stereo, scaled by
/// `gain`. Linear interpolation, like playback, with `pos` carried across
/// callbacks so a 44.1 kHz microphone does not drift against the output.
pub(crate) fn resample_capture(
    input: &[f32],
    channels: usize,
    rate: u32,
    gain: f32,
    pos: &mut f64,
    out: &mut Vec<i16>,
) {
    let frames = input.len() / channels;
    if channels == 0 || frames == 0 {
        return;
    }
    let step = rate as f64 / SAMPLE_RATE as f64;
    let gain = if gain.is_finite() {
        gain.clamp(0.0, MAX_GAIN)
    } else {
        1.0
    };
    // The epsilon keeps an exact 44.1 kHz ratio from emitting a 481st frame
    // whenever floating point lands a hair below the boundary.
    while *pos < frames as f64 - 1e-9 {
        let base = *pos as usize;
        // The last sample has no right neighbour; holding it is inaudible and
        // keeps exactly `frames` output frames at 48 kHz.
        let next = (base + 1).min(frames - 1);
        let frac = (*pos - base as f64) as f32;
        let i = base * channels;
        let j = next * channels;
        let left = lerp(input[i], input[j], frac) * gain;
        let right = if channels > 1 {
            lerp(input[i + 1], input[j + 1], frac) * gain
        } else {
            left
        };
        out.push(to_i16(left));
        out.push(to_i16(right));
        *pos += step;
    }
    *pos = (*pos - frames as f64).max(0.0);
}

fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// Plays the notification chime through the configured output device. The
/// tone is synthesized, so its volume is ours to control — the OS system sound
/// cannot be turned down from here. One chime at a time.
pub(crate) fn chime() {
    static PLAYING: AtomicBool = AtomicBool::new(false);
    if notify_volume() <= 0.001 {
        return;
    }
    if PLAYING.swap(true, Ordering::Relaxed) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("wyvern-chime".into())
        .spawn(|| {
            let samples = chime_samples(notify_volume());
            if let Ok(player) = Player::start_output() {
                let start = player.position() + PREBUFFER_FRAMES as i64;
                player.push_at(start, &samples);
                std::thread::sleep(Duration::from_millis(450));
            }
            PLAYING.store(false, Ordering::Relaxed);
        });
    if spawned.is_err() {
        PLAYING.store(false, Ordering::Relaxed);
    }
}

/// Two short tones (A5 then E6), 250 ms total, with 5 ms fades so neither
/// edge clicks. Peak amplitude is a quarter of full scale times `volume`.
pub(crate) fn chime_samples(volume: f32) -> Vec<i16> {
    const FIRST: f32 = 880.0;
    const SECOND: f32 = 1318.5;
    let volume = volume.clamp(0.0, 1.0);
    let total = (SAMPLE_RATE as usize) / 4;
    let half = total / 2;
    let fade = (SAMPLE_RATE as f32 * 0.005) as usize;
    let mut out = Vec::with_capacity(total * CHANNELS);
    for frame in 0..total {
        let (freq, local, len) = if frame < half {
            (FIRST, frame, half)
        } else {
            (SECOND, frame - half, total - half)
        };
        let envelope = if local < fade {
            local as f32 / fade as f32
        } else if local + fade >= len {
            (len - 1 - local) as f32 / fade as f32
        } else {
            1.0
        };
        let time = local as f32 / SAMPLE_RATE as f32;
        let sample =
            ((time * freq * std::f32::consts::TAU).sin() * 0.25 * volume * envelope * 32_767.0)
                as i16;
        out.push(sample);
        out.push(sample);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second of a constant sample, enough to pass the pre-buffer.
    fn push_constant(mixer: &mut Mixer, value: i16, frames: usize) {
        let pcm = vec![value; frames * CHANNELS];
        mixer.push_at(0, &pcm);
    }

    #[test]
    fn mixer_waits_for_the_prebuffer() {
        let mut mixer = Mixer::new();
        push_constant(&mut mixer, 1_000, 100);
        let mut out = [0.0f32; 64];
        mixer.pull(&mut out, 2, 1.0);
        assert!(out.iter().all(|&s| s == 0.0), "до пребуфера должен быть нуль");
        assert_eq!(mixer.queued(), 100.0, "очередь не должна расходоваться");
    }

    #[test]
    fn prebuffer_covers_device_callbacks_longer_than_the_packets() {
        // A 200 ms callback with only 100 ms queued would read past the
        // watermark, leave a hole and make the packets that arrive during the
        // callback late. Waiting for two callbacks avoids that.
        let mut mixer = Mixer::new();
        let packet = vec![1_000i16; 960 * CHANNELS];
        for i in 0..5 {
            mixer.push_at(PREBUFFER_FRAMES as i64 + i * 960, &packet);
        }
        let mut out = vec![0.0f32; 9_600 * CHANNELS];
        mixer.pull(&mut out, CHANNELS, 1.0);
        assert!(out.iter().all(|&s| s == 0.0), "100 мс звука мало для колбэка в 200 мс");
        assert_eq!(mixer.underruns, 0, "ожидание пребуфера — не провал");

        // Two callbacks' worth is enough to start.
        for i in 5..25 {
            mixer.push_at(PREBUFFER_FRAMES as i64 + i * 960, &packet);
        }
        mixer.pull(&mut out, CHANNELS, 1.0);
        assert!(
            out[PREBUFFER_FRAMES * CHANNELS..].iter().all(|&s| s > 0.0),
            "после двух колбэков звук должен идти без дыр"
        );
    }

    #[test]
    fn mixer_sums_speakers_and_clamps() {
        let mut mixer = Mixer::new();
        // Two speakers at the same RTP position: 20000 + 20000 overflows
        // i16, but the mixer accumulates in i32 and clamps once at output.
        push_constant(&mut mixer, 20_000, PREBUFFER_FRAMES + 8);
        push_constant(&mut mixer, 20_000, PREBUFFER_FRAMES + 8);
        let mut out = [0.0f32; 16];
        mixer.pull(&mut out, 2, 1.0);
        assert!(out[0] > 0.99, "сумма двух говорящих должна клиппироваться в 1.0: {}", out[0]);
        assert!(out[0] <= 1.0 && out[1] >= -1.0, "выход за пределы [-1, 1]");

        let mut quiet = Mixer::new();
        push_constant(&mut quiet, 1_000, PREBUFFER_FRAMES + 8);
        push_constant(&mut quiet, 1_000, PREBUFFER_FRAMES + 8);
        let mut out = [0.0f32; 4];
        quiet.pull(&mut out, 2, 1.0);
        let expected = 2_000.0 / 32_768.0;
        assert!((out[0] - expected).abs() < 1e-4, "тихая сумма должна складываться: {}", out[0]);
    }

    #[test]
    fn mixer_ignores_packets_outside_the_ring() {
        let mut mixer = Mixer::new();
        let pcm = vec![9_000i16; 960 * CHANNELS];
        mixer.push_at(-5_000, &pcm);
        mixer.push_at(RING_FRAMES as i64 + 5_000, &pcm);
        assert_eq!(mixer.queued(), 0.0, "пакеты вне окна не должны попадать в кольцо");
        assert_eq!(mixer.position(), 0, "позиция не должна сдвигаться");
    }

    #[test]
    fn mixer_resamples_to_the_device_rate() {
        let mut mixer = Mixer::new();
        push_constant(&mut mixer, 1_000, RING_FRAMES - 100);
        let mut out = vec![0.0f32; 441 * 2];
        // 44.1 kHz device: 441 output frames must consume 480 source frames.
        mixer.pull(&mut out, 2, 48_000.0 / 44_100.0);
        let consumed = (RING_FRAMES - 100) as f64 - mixer.queued();
        assert!((consumed - 480.0).abs() < 0.5, "ресемплер съел {} кадров вместо 480", consumed);
    }

    #[test]
    fn mixer_clears_consumed_frames() {
        let mut mixer = Mixer::new();
        push_constant(&mut mixer, 1_000, PREBUFFER_FRAMES + 480);
        let mut out = vec![0.0f32; 1_000 * 2];
        mixer.pull(&mut out, 2, 1.0);
        for frame in 0..900 {
            let slot = Mixer::slot(frame);
            assert_eq!(mixer.samples[slot], 0, "кадр {} не обнулён", frame);
            assert_eq!(mixer.samples[slot + 1], 0, "правый канал кадра {} не обнулён", frame);
        }
    }

    #[test]
    fn mixer_writes_mono_without_panicking() {
        let mut mixer = Mixer::new();
        push_constant(&mut mixer, 5_000, PREBUFFER_FRAMES + 4);
        let mut out = vec![0.0f32; 8];
        mixer.pull(&mut out, 1, 1.0);
        assert!(out.iter().all(|&s| s > 0.0), "моно-выход должен получать звук");
    }

    #[test]
    fn volumes_default_to_full_and_clamp() {
        assert_eq!(volume_or(None, MAX_GAIN), 1.0, "без значения громкость полная");
        assert_eq!(volume_or(Some(0.25), MAX_GAIN), 0.25, "сохранённая громкость должна сохраняться");
        assert_eq!(volume_or(Some(-3.0), MAX_GAIN), 0.0, "минус — это тишина");
        assert_eq!(volume_or(Some(9.0), MAX_GAIN), MAX_GAIN, "выше максимума не поднимаем");
        assert_eq!(volume_or(Some(9.0), 1.0), 1.0, "уведомления не громче 100%");
    }

    #[test]
    fn mixer_scales_each_source_by_its_gain() {
        let mut mixer = Mixer::new();
        let pcm = vec![10_000i16; (PREBUFFER_FRAMES + 8) * CHANNELS];
        mixer.push_at_gain(0, &pcm, 0.5);
        let mut out = [0.0f32; 4];
        mixer.pull(&mut out, 2, 1.0);
        let expected = 5_000.0 / 32_768.0;
        assert!(
            (out[0] - expected).abs() < 1e-4,
            "громкость 50% должна уменьшить сигнал вдвое: {}",
            out[0]
        );

        let mut muted = Mixer::new();
        muted.push_at_gain(0, &pcm, 0.0);
        let mut out = [0.0f32; 4];
        muted.pull(&mut out, 2, 1.0);
        assert!(out.iter().all(|&s| s == 0.0), "нулевая громкость — тишина");

        let mut loud = Mixer::new();
        loud.push_at_gain(0, &pcm, 10.0);
        let mut out = [0.0f32; 4];
        loud.pull(&mut out, 2, 1.0);
        let max = 10_000.0 * MAX_GAIN as f64 / 32_768.0;
        assert!(
            (out[0] as f64 - max).abs() < 1e-4,
            "громкость должна ограничиваться максимумом: {}",
            out[0]
        );
    }

    #[test]
    fn capture_resample_keeps_the_frame_count() {
        // 44.1 kHz mono in ten callbacks: 4410 frames must become 4800.
        let input = vec![0.5f32; 441];
        let mut pos = 0.0;
        let mut out = Vec::new();
        for _ in 0..10 {
            resample_capture(&input, 1, 44_100, 1.0, &mut pos, &mut out);
        }
        assert_eq!(
            out.len() / CHANNELS,
            4_800,
            "44.1 кГц должно превращаться в 48 кГц без дрейфа"
        );
        // Mono is duplicated to both channels, and the level survives.
        let expected = (0.5 * i16::MAX as f32) as i16;
        assert!((out[0] - expected).abs() <= 1, "моно должно дублироваться: {}", out[0]);
        assert!((out[1] - expected).abs() <= 1, "правый канал потерялся");
    }

    #[test]
    fn capture_resample_scales_and_clips() {
        let input = vec![1.0f32; 480 * CHANNELS];
        let mut pos = 0.0;
        let mut out = Vec::new();
        resample_capture(&input, 2, 48_000, 0.5, &mut pos, &mut out);
        assert_eq!(out.len(), 480 * CHANNELS, "48 кГц стерео должно проходить как есть");
        assert_eq!(
            out[0],
            (0.5 * i16::MAX as f32).round() as i16,
            "громкость не применилась"
        );

        let mut pos = 0.0;
        let mut out = Vec::new();
        resample_capture(&input, 2, 48_000, MAX_GAIN, &mut pos, &mut out);
        assert_eq!(out[0], i16::MAX, "перегрузка должна ограничиваться, а не заворачиваться");    }

    #[test]
    fn chime_is_short_and_fades_in_and_out() {
        let samples = chime_samples(1.0);
        assert_eq!(
            samples.len(),
            (SAMPLE_RATE as usize / 4) * CHANNELS,
            "звук уведомления должен быть четверть секунды"
        );
        assert_eq!(samples[0], 0, "начало должно быть без щелчка");
        assert_eq!(samples[samples.len() - 1], 0, "конец должен быть без щелчка");
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        let expected = (0.25 * i16::MAX as f32) as u16;
        assert!(
            peak.abs_diff(expected) <= 2,
            "пик должен быть около четверти шкалы: {} вместо {}",
            peak,
            expected
        );

        let quiet = chime_samples(0.5);
        let quiet_peak = quiet.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(
            quiet_peak.abs_diff(expected / 2) <= 2,
            "половинная громкость должна давать половинный пик: {}",
            quiet_peak
        );

        assert!(
            chime_samples(0.0).iter().all(|&s| s == 0),
            "нулевая громкость — беззвучное уведомление"
        );
    }
}

