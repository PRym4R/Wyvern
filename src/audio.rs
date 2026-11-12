//! Voice playback: a 500 ms stereo ring buffer at 48 kHz, a summing mixer and
//! a cpal output stream. Discord sends 48 kHz Opus, so the mixer never
//! resamples on the network side; the device callback does the one conversion
//! it cannot avoid (44.1 kHz cards and mono outputs are common).
//!
//! cpal streams are not `Send` on every backend, so the stream is created and
//! owned by its own std thread. The network task only pushes PCM into the
//! shared ring.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Discord voice is always 48 kHz.
pub(crate) const SAMPLE_RATE: u32 = 48_000;
/// Discord voice is always stereo.
pub(crate) const CHANNELS: usize = 2;
/// 500 ms of stereo audio: enough headroom for jitter without audible delay.
pub(crate) const RING_FRAMES: usize = 24_000;
/// Playback starts only once this much is queued. Without it the first packet
/// underruns immediately and the stream crackles.
pub(crate) const PREBUFFER_FRAMES: usize = 4_800;

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
        let frames = pcm.len() / CHANNELS;
        if frames == 0 {
            return;
        }
        let end = start + frames as i64;
        let window_start = self.read.floor() as i64;
        let window_end = window_start + RING_FRAMES as i64;
        // Entirely late or too far ahead to fit: dropping is better than
        // wrapping over audio someone is hearing right now.
        if end <= window_start || end > window_end {
            return;
        }
        for (i, frame) in pcm.chunks_exact(CHANNELS).enumerate() {
            let pos = start + i as i64;
            if pos < window_start || pos >= window_end {
                continue;
            }
            let slot = Self::slot(pos);
            self.samples[slot] += frame[0] as i32;
            self.samples[slot + 1] += frame[1] as i32;
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

    /// Fills `out` with `out.len() / channels` interleaved frames, resampling
    /// 48 kHz to the device rate (`step = 48000 / device_rate`).
    pub(crate) fn pull(&mut self, out: &mut [f32], channels: usize, step: f64) {
        if channels == 0 || step <= 0.0 {
            out.fill(0.0);
            return;
        }
        // Queue cap: a stalled callback or a burst must not become seconds of
        // delay once playback resumes.
        if self.write as f64 - self.read > RING_FRAMES as f64 {
            self.read = self.write as f64 - PREBUFFER_FRAMES as f64;
            self.started = false;
            self.dropped += 1;
        }
        if self.started && self.write as f64 - self.read < 1.0 {
            // Underrun: re-arm the pre-buffer so the next burst starts clean.
            self.read = self.write as f64;
            self.started = false;
        }
        if !self.started {
            if self.write as f64 - self.read < PREBUFFER_FRAMES as f64 {
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

/// Playback handle: shared mixer, stop flag and the thread that owns the cpal
/// stream. Dropping it ends playback.
pub(crate) struct Player {
    mixer: Arc<Mutex<Mixer>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Player {
    /// Opens the default output device and starts pulling from the mixer.
    /// The error string is user-facing: the voice bar shows it as is.
    pub(crate) fn start() -> Result<Self, String> {
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
            Ok(Ok(())) => Ok(Self { mixer, stop, thread: Some(thread) }),
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
        if let Ok(mut mixer) = self.mixer.lock() {
            mixer.push_at(start, pcm);
        }
    }

    /// Current playback position, used to anchor RTP timestamps.
    pub(crate) fn position(&self) -> i64 {
        self.mixer.lock().map(|m| m.position()).unwrap_or(0)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Builds the output stream and keeps it alive until `stop` is set.
fn run_stream(
    ring: Arc<Mutex<Mixer>>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no audio output device")?;
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
}

