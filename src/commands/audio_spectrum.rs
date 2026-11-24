//! Local output levels for the activity island.

use std::{
    ptr, slice,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
            WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
        },
        System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
    },
    core::GUID,
};

use super::enumerator;

const POLL_INTERVAL: Duration = Duration::from_millis(16);
const DEVICE_INTERVAL: Duration = Duration::from_secs(1);

type Levels = [AtomicU32; 4];

struct Subscription {
    levels: Arc<Levels>,
    listeners: usize,
}

struct Worker {
    started: bool,
    subscription: Option<Subscription>,
}

static WORKER: Mutex<Worker> = Mutex::new(Worker {
    started: false,
    subscription: None,
});
static CHANGED: Condvar = Condvar::new();

/// A subscription to the default output's four frequency bands.
pub struct Spectrum(Arc<Levels>);

impl Spectrum {
    pub fn subscribe() -> Option<Self> {
        let mut worker = WORKER.lock().unwrap();
        if !worker.started {
            thread::Builder::new()
                .name("wisp-audio-spectrum".into())
                .spawn(background)
                .ok()?;
            worker.started = true;
        }
        let subscription = worker.subscription.get_or_insert_with(|| Subscription {
            levels: Arc::new(Levels::default()),
            listeners: 0,
        });
        subscription.listeners += 1;
        CHANGED.notify_one();
        Some(Self(subscription.levels.clone()))
    }

    pub fn levels(&self) -> [f32; 4] {
        std::array::from_fn(|band| f32::from_bits(self.0[band].load(Ordering::Relaxed)))
    }
}

impl Drop for Spectrum {
    fn drop(&mut self) {
        let mut worker = WORKER.lock().unwrap();
        if let Some(subscription) = worker.subscription.as_mut() {
            subscription.listeners -= 1;
            if subscription.listeners == 0 {
                worker.subscription = None;
                CHANGED.notify_one();
            }
        }
    }
}

impl Worker {
    fn serves(&self, levels: &Arc<Levels>) -> bool {
        self.subscription
            .as_ref()
            .is_some_and(|subscription| Arc::ptr_eq(&subscription.levels, levels))
    }
}

fn background() {
    loop {
        // Sleep without COM or a capture stream while no island is listening.
        let levels = {
            let worker = CHANGED
                .wait_while(WORKER.lock().unwrap(), |worker| worker.subscription.is_none())
                .unwrap();
            worker.subscription.as_ref().unwrap().levels.clone()
        };
        if unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok() {
            run_capture(&levels);
            unsafe { CoUninitialize() };
        }
        // Retry failed initialization, or proceed directly to a new listener group.
        wait_for_listeners(&levels, DEVICE_INTERVAL);
    }
}

fn wait_for_listeners(levels: &Arc<Levels>, interval: Duration) -> bool {
    let (worker, _) = CHANGED
        .wait_timeout_while(WORKER.lock().unwrap(), interval, |worker| worker.serves(levels))
        .unwrap();
    worker.serves(levels)
}

fn run_capture(levels: &Arc<Levels>) {
    let Ok(enumerator) = enumerator() else { return };
    let mut capture: Option<Capture> = None;
    let mut next_device_check = Instant::now();
    loop {
        let now = Instant::now();
        if now >= next_device_check {
            next_device_check = now + DEVICE_INTERVAL;
            let device = default_endpoint(&enumerator);
            let open_id = capture.as_ref().map(|capture| &capture.device_id);
            let default_id = device.as_ref().map(|(_, id)| id);
            if open_id != default_id {
                drop(capture.take());
                capture = device.and_then(|(device, id)| Capture::open(&device, id));
            }
        }

        let current = capture.as_mut().and_then(Capture::read);
        if current.is_none() {
            capture = None;
        }
        for (value, level) in levels.iter().zip(current.unwrap_or_default()) {
            value.store(level.to_bits(), Ordering::Relaxed);
        }

        let interval = if capture.is_some() {
            POLL_INTERVAL
        } else {
            next_device_check.saturating_duration_since(Instant::now())
        };
        if !wait_for_listeners(levels, interval) {
            break;
        }
    }
}

fn default_endpoint(enumerator: &IMMDeviceEnumerator) -> Option<(IMMDevice, String)> {
    unsafe {
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let id = device.GetId().ok()?;
        let text = id.to_string();
        CoTaskMemFree(Some(id.0.cast()));
        Some((device, text.ok()?))
    }
}

struct Capture {
    device_id: String,
    client: IAudioClient,
    packets: IAudioCaptureClient,
    format: MixFormat,
    max_frames: u32,
    bands: Bands,
    updated: Instant,
}

impl Capture {
    fn open(device: &IMMDevice, device_id: String) -> Option<Self> {
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
            let mix = client.GetMixFormat().ok()?;
            let format = MixFormat::from_wave_format(mix);
            let init = format.map(|_| {
                // Shared loopback, with a 100 ms buffer for scheduling jitter.
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    1_000_000,
                    0,
                    mix,
                    None,
                )
            });
            CoTaskMemFree(Some(mix.cast()));
            init?.ok()?;
            let format = format?;
            let max_frames = client.GetBufferSize().ok()?;
            let packets: IAudioCaptureClient = client.GetService().ok()?;
            let bands = Bands::new(format.sample_rate, format.channels);
            client.Start().ok()?;
            Some(Self {
                device_id,
                client,
                packets,
                format,
                max_frames,
                bands,
                updated: Instant::now(),
            })
        }
    }

    fn read(&mut self) -> Option<[f32; 4]> {
        // Bound work per tick even if Windows keeps producing new packets.
        for _ in 0..16 {
            unsafe {
                if self.packets.GetNextPacketSize().ok()? == 0 {
                    break;
                }
                let mut data = ptr::null_mut();
                let (mut frames, mut flags) = (0, 0);
                self.packets
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .ok()?;
                if frames == 0 {
                    break;
                }
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let len = (frames as usize)
                    .checked_mul(self.format.frame_bytes)
                    .filter(|&len| frames <= self.max_frames && len <= isize::MAX as usize);
                let Some(len) = len.filter(|_| silent || !data.is_null()) else {
                    self.packets.ReleaseBuffer(0).ok()?;
                    return None;
                };
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 || silent {
                    self.bands.reset_filters();
                }
                if silent {
                    self.bands.samples += frames as usize * self.format.channels;
                } else {
                    // WASAPI owns these bytes. They are valid only until ReleaseBuffer below.
                    let bytes = slice::from_raw_parts(data, len);
                    for frame in bytes.chunks_exact(self.format.frame_bytes) {
                        for (channel, sample) in frame.chunks_exact(self.format.encoding.bytes()).enumerate() {
                            self.bands.push(channel, self.format.encoding.decode(sample));
                        }
                    }
                }
                self.packets.ReleaseBuffer(frames).ok()?;
            }
        }
        let now = Instant::now();
        let levels = self.bands.levels((now - self.updated).as_secs_f32());
        self.updated = now;
        Some(levels)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

#[derive(Clone, Copy)]
struct MixFormat {
    encoding: Encoding,
    sample_rate: u32,
    channels: usize,
    frame_bytes: usize,
}

impl MixFormat {
    unsafe fn from_wave_format(format: *const WAVEFORMATEX) -> Option<Self> {
        // The Windows bindings model both WAVEFORMAT structures as packed.
        let wave = unsafe { format.as_ref()? };
        let tag = if wave.wFormatTag == 0xfffe {
            if wave.cbSize < 22 {
                return None;
            }
            const PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
            const FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
            let subtype = unsafe { (*format.cast::<WAVEFORMATEXTENSIBLE>()).SubFormat };
            match subtype {
                PCM => 1,
                FLOAT => 3,
                _ => return None,
            }
        } else {
            wave.wFormatTag
        };
        let encoding = match (tag, wave.wBitsPerSample) {
            (3, 32) => Encoding::Float,
            (1, 16) => Encoding::Pcm16,
            (1, 24) => Encoding::Pcm24,
            (1, 32) => Encoding::Pcm32,
            _ => return None,
        };
        let sample_rate = wave.nSamplesPerSec;
        let channels = usize::from(wave.nChannels);
        let frame_bytes = usize::from(wave.nBlockAlign);
        if !(8_000..=384_000).contains(&sample_rate)
            || !(1..=32).contains(&channels)
            || frame_bytes != channels * encoding.bytes()
        {
            return None;
        }
        Some(Self {
            encoding,
            sample_rate,
            channels,
            frame_bytes,
        })
    }
}

#[derive(Clone, Copy)]
enum Encoding {
    Float,
    Pcm16,
    Pcm24,
    Pcm32,
}

impl Encoding {
    fn bytes(self) -> usize {
        match self {
            Self::Pcm16 => 2,
            Self::Pcm24 => 3,
            Self::Float | Self::Pcm32 => 4,
        }
    }

    fn decode(self, bytes: &[u8]) -> f32 {
        match self {
            Self::Float => f32::from_le_bytes(bytes.try_into().unwrap()),
            Self::Pcm16 => f32::from(i16::from_le_bytes(bytes.try_into().unwrap())) / 32_768.0,
            Self::Pcm24 => i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) as f32 / 2_147_483_648.0,
            Self::Pcm32 => i32::from_le_bytes(bytes.try_into().unwrap()) as f32 / 2_147_483_648.0,
        }
    }
}

const CENTERS_HZ: [f32; 4] = [100.0, 400.0, 1_600.0, 6_400.0];

struct Bands {
    filters: [BandPass; 4],
    channels: Vec<[FilterHistory; 4]>,
    energy: [f32; 4],
    samples: usize,
    envelope: [f32; 4],
}

impl Bands {
    fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            filters: CENTERS_HZ.map(|center| BandPass::new(center, sample_rate as f32)),
            channels: vec![[FilterHistory::default(); 4]; channels],
            energy: [0.0; 4],
            samples: 0,
            envelope: [0.0; 4],
        }
    }

    fn push(&mut self, channel: usize, sample: f32) {
        let sample = if sample.is_finite() {
            sample.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        // Sum power after filtering each channel so opposite stereo phases don't cancel.
        for ((filter, history), energy) in self
            .filters
            .iter()
            .zip(&mut self.channels[channel])
            .zip(&mut self.energy)
        {
            let output = filter.apply(history, sample);
            *energy += output * output;
        }
        self.samples += 1;
    }

    fn reset_filters(&mut self) {
        self.channels.fill([FilterHistory::default(); 4]);
    }

    fn levels(&mut self, elapsed: f32) -> [f32; 4] {
        let attack = 1.0 - (-elapsed / 0.025).exp();
        let release = 1.0 - (-elapsed / 0.110).exp();
        for (level, energy) in self.envelope.iter_mut().zip(self.energy) {
            let rms = (energy / self.samples.max(1) as f32).sqrt();
            let magnitude = (rms - 0.0005).max(0.0) * 10.0;
            let target = magnitude / (1.0 + magnitude);
            let response = if target > *level { attack } else { release };
            *level += (target - *level) * response;
            if *level < 0.001 {
                *level = 0.0;
            }
        }
        self.energy = [0.0; 4];
        self.samples = 0;
        self.envelope
    }
}

struct BandPass {
    gain: f32,
    feedback: [f32; 2],
}

#[derive(Clone, Copy, Default)]
struct FilterHistory {
    delay: [f32; 2],
}

impl BandPass {
    fn new(center: f32, sample_rate: f32) -> Self {
        // A unity-gain bandpass biquad with overlapping bands. Keep the top
        // band below Nyquist even when the output uses a voice sample rate.
        let angle = std::f32::consts::TAU * (center / sample_rate).min(0.45);
        let (sin, cos) = angle.sin_cos();
        let alpha = sin / (2.0 * 0.85);
        let denominator = 1.0 + alpha;
        Self {
            gain: alpha / denominator,
            feedback: [2.0 * cos / denominator, (alpha - 1.0) / denominator],
        }
    }

    fn apply(&self, history: &mut FilterHistory, sample: f32) -> f32 {
        let input = self.gain * sample;
        let output = history.delay[0] + input;
        history.delay[0] = history.delay[1] + self.feedback[0] * output;
        history.delay[1] = self.feedback[1] * output - input;
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SAMPLE_RATE: u32 = 48_000;
    const FRAMES: usize = 960;
    const INTERVAL: f32 = FRAMES as f32 / SAMPLE_RATE as f32;

    fn tone(bands: &mut Bands, frequency: f32) -> [f32; 4] {
        let mut levels = [0.0; 4];
        for window in 0..6 {
            for frame in 0..FRAMES {
                let time = (window * FRAMES + frame) as f32 / SAMPLE_RATE as f32;
                let sample = 0.2 * (time * frequency * TAU).sin();
                bands.push(0, sample);
                bands.push(1, -sample);
            }
            levels = bands.levels(INTERVAL);
        }
        levels
    }

    #[test]
    fn different_tones_raise_different_bands_even_in_antiphase_stereo() {
        for (target, frequency) in [100.0, 400.0, 1_600.0, 6_400.0].into_iter().enumerate() {
            let levels = tone(&mut Bands::new(SAMPLE_RATE, 2), frequency);
            assert!(levels[target] > 0.4, "{frequency} Hz: {levels:?}");
            for (band, level) in levels.into_iter().enumerate() {
                assert!((0.0..=1.0).contains(&level));
                if band != target {
                    assert!(levels[target] - level > 0.15, "{frequency} Hz: {levels:?}");
                }
            }
        }
    }

    #[test]
    fn levels_settle_to_zero_when_the_stream_goes_silent() {
        let mut bands = Bands::new(SAMPLE_RATE, 2);
        let playing = tone(&mut bands, 100.0);
        let releasing = bands.levels(INTERVAL);
        assert!(releasing[0] > 0.0 && releasing[0] < playing[0]);
        for _ in 0..50 {
            bands.levels(INTERVAL);
        }
        assert_eq!(bands.levels(INTERVAL), [0.0; 4]);
    }
}
