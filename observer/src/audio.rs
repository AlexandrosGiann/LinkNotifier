//! Audio capture through WASAPI shared mode.
//!
//! Two streams run side by side:
//! - `Source::Others`: loopback of the default render device, i.e. everything
//!   Teams plays (the other participants), with no Teams permission needed.
//! - `Source::Me`: the default capture device (the microphone).
//!
//! Each stream is cut into utterances by a simple energy detector with an
//! adaptive noise floor and handed to the recognizer as 16 kHz mono chunks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::GUID;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Me,
    Others,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Me => "Εγώ",
            Source::Others => "Συμμετέχοντες",
        }
    }
}

/// One utterance, ready for speech recognition.
pub struct Chunk {
    pub source: Source,
    /// Seconds since the session started.
    pub start: f64,
    pub end: f64,
    /// 16 kHz mono in [-1, 1].
    pub pcm: Vec<f32>,
}

const FRAME_MS: usize = 30;
/// Silence that closes an utterance.
const HANG_MS: usize = 700;
/// Utterances are cut here even mid-speech; Whistle covers 30 s per pass.
const MAX_CHUNK_S: f64 = 24.0;
/// Shorter speech is dropped (coughs, clicks).
const MIN_SPEECH_MS: usize = 500;
/// Audio kept before the first loud frame so word onsets are not clipped.
const PRE_ROLL_MS: usize = 300;

const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

#[derive(Clone, Copy)]
enum SampleKind {
    F32,
    I16,
    I24,
    I32,
}

/// Cuts a mono stream at the device rate into utterances. Time is counted in
/// samples from `t0`, so a WAV file fed faster than real time gets the right
/// timestamps too.
pub struct Segmenter {
    source: Source,
    rate: usize,
    frame: usize,
    t0: f64,
    samples: u64,
    pending: Vec<f32>,
    /// Pre-roll while idle, the utterance while active.
    buf: Vec<f32>,
    active: bool,
    speech_frames: usize,
    silent_frames: usize,
    started_at: f64,
    noise_floor: f32,
}

impl Segmenter {
    /// `t0`: seconds since the session started at the first sample.
    pub fn new(source: Source, rate: usize, t0: f64) -> Segmenter {
        Segmenter {
            source,
            rate,
            frame: rate * FRAME_MS / 1000,
            t0,
            samples: 0,
            pending: Vec::new(),
            buf: Vec::new(),
            active: false,
            speech_frames: 0,
            silent_frames: 0,
            started_at: 0.0,
            noise_floor: 0.003,
        }
    }

    fn now(&self) -> f64 {
        self.t0 + self.samples as f64 / self.rate as f64
    }

    pub fn push(&mut self, mono: &[f32], out: &Sender<Chunk>) {
        self.pending.extend_from_slice(mono);
        let frame = self.frame.max(1);
        while self.pending.len() >= frame {
            let f: Vec<f32> = self.pending.drain(..frame).collect();
            self.frame_in(&f, out);
        }
    }

    fn frame_in(&mut self, f: &[f32], out: &Sender<Chunk>) {
        self.samples += f.len() as u64;
        let rms = (f.iter().map(|x| x * x).sum::<f32>() / f.len() as f32).sqrt();
        let loud = rms > (self.noise_floor * 3.0).max(0.008);
        if !loud {
            // Track the floor slowly upwards and quickly downwards.
            let k = if rms < self.noise_floor { 0.2 } else { 0.01 };
            self.noise_floor += (rms - self.noise_floor) * k;
        }
        if !self.active {
            self.buf.extend_from_slice(f);
            let keep = self.rate * PRE_ROLL_MS / 1000;
            if self.buf.len() > keep {
                let cut = self.buf.len() - keep;
                self.buf.drain(..cut);
            }
            if loud {
                self.active = true;
                self.speech_frames = 1;
                self.silent_frames = 0;
                self.started_at = (self.now() - self.buf.len() as f64 / self.rate as f64).max(0.0);
            }
            return;
        }
        self.buf.extend_from_slice(f);
        if loud {
            self.speech_frames += 1;
            self.silent_frames = 0;
        } else {
            self.silent_frames += 1;
        }
        let long = self.buf.len() as f64 / self.rate as f64 >= MAX_CHUNK_S;
        if self.silent_frames * FRAME_MS >= HANG_MS || long {
            self.flush(out);
        }
    }

    pub fn flush(&mut self, out: &Sender<Chunk>) {
        if self.active && self.speech_frames * FRAME_MS >= MIN_SPEECH_MS {
            let pcm = whistle::audio::resample(&self.buf, self.rate as u32, 16000);
            let start = self.started_at;
            let end = start + self.buf.len() as f64 / self.rate as f64;
            let _ = out.send(Chunk { source: self.source, start, end, pcm });
        }
        self.buf.clear();
        self.active = false;
        self.speech_frames = 0;
        self.silent_frames = 0;
    }
}

/// Captures one endpoint until `stop` is set. Runs on its own thread.
pub fn capture(source: Source, session_start: Instant, stop: Arc<AtomicBool>, out: Sender<Chunk>) -> Result<(), String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().map_err(|e| format!("COM: {e}"))?;
        let r = capture_inner(source, session_start, &stop, &out);
        CoUninitialize();
        r
    }
}

unsafe fn capture_inner(source: Source, session_start: Instant, stop: &AtomicBool, out: &Sender<Chunk>) -> Result<(), String> {
    let e = |what: &str| {
        let what = what.to_string();
        move |err: windows::core::Error| format!("{:?} {}: {}", source, what, err)
    };
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(e("device enumerator"))?;
    let flow = if source == Source::Others { eRender } else { eCapture };
    let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole).map_err(e("default endpoint"))?;
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(e("activate"))?;
    let fmt_ptr = client.GetMixFormat().map_err(e("mix format"))?;
    let fmt: WAVEFORMATEX = std::ptr::read_unaligned(fmt_ptr);
    let channels = fmt.nChannels.max(1) as usize;
    let rate = fmt.nSamplesPerSec as usize;
    let bits = fmt.wBitsPerSample;
    let tag = fmt.wFormatTag;
    let is_float = tag == WAVE_FORMAT_IEEE_FLOAT
        || (tag == WAVE_FORMAT_EXTENSIBLE && {
            let ext: WAVEFORMATEXTENSIBLE = std::ptr::read_unaligned(fmt_ptr as *const WAVEFORMATEXTENSIBLE);
            let sub = ext.SubFormat;
            sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        });
    let kind = match (is_float, bits) {
        (true, 32) => SampleKind::F32,
        (false, 16) => SampleKind::I16,
        (false, 24) => SampleKind::I24,
        (false, 32) => SampleKind::I32,
        _ => {
            CoTaskMemFree(Some(fmt_ptr as _));
            return Err(format!("{:?}: unsupported mix format ({} bit, tag {:#x})", source, bits, tag));
        }
    };
    let block = fmt.nBlockAlign as usize;
    let flags = if source == Source::Others { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
    // 1 s buffer in 100 ns units.
    let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 10_000_000, 0, fmt_ptr, None);
    CoTaskMemFree(Some(fmt_ptr as _));
    init.map_err(e("initialize"))?;
    let capture: IAudioCaptureClient = client.GetService().map_err(e("capture service"))?;
    client.Start().map_err(e("start"))?;

    let mut seg = Segmenter::new(source, rate, session_start.elapsed().as_secs_f64());
    let mut mono: Vec<f32> = Vec::new();
    let mut last_packet = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));
        loop {
            let n = capture.GetNextPacketSize().map_err(e("packet size"))?;
            if n == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut bflags: u32 = 0;
            capture.GetBuffer(&mut data, &mut frames, &mut bflags, None, None).map_err(e("get buffer"))?;
            let frames_n = frames as usize;
            mono.clear();
            mono.reserve(frames_n);
            if bflags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                mono.resize(frames_n, 0.0);
            } else {
                let bytes = std::slice::from_raw_parts(data, frames_n * block);
                for fr in bytes.chunks_exact(block) {
                    let mut acc = 0.0f32;
                    for c in 0..channels {
                        acc += read_sample(fr, c, kind);
                    }
                    mono.push(acc / channels as f32);
                }
            }
            capture.ReleaseBuffer(frames).map_err(e("release buffer"))?;
            seg.push(&mono, out);
            last_packet = Instant::now();
        }
        // Loopback delivers no packets while nothing plays: treat that as silence
        // so an utterance in progress still gets closed.
        if last_packet.elapsed() > Duration::from_millis(100) {
            let gap = rate * 100 / 1000;
            mono.clear();
            mono.resize(gap, 0.0);
            seg.push(&mono, out);
            last_packet = Instant::now();
        }
    }
    seg.flush(out);
    let _ = client.Stop();
    Ok(())
}

fn read_sample(frame: &[u8], channel: usize, kind: SampleKind) -> f32 {
    match kind {
        SampleKind::F32 => {
            let o = channel * 4;
            f32::from_le_bytes([frame[o], frame[o + 1], frame[o + 2], frame[o + 3]])
        }
        SampleKind::I16 => {
            let o = channel * 2;
            i16::from_le_bytes([frame[o], frame[o + 1]]) as f32 / 32768.0
        }
        SampleKind::I24 => {
            let o = channel * 3;
            let v = i32::from_le_bytes([0, frame[o], frame[o + 1], frame[o + 2]]) >> 8;
            v as f32 / 8_388_608.0
        }
        SampleKind::I32 => {
            let o = channel * 4;
            i32::from_le_bytes([frame[o], frame[o + 1], frame[o + 2], frame[o + 3]]) as f32 / 2_147_483_648.0
        }
    }
}
