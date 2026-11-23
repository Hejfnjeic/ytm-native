//! Audio engine: WebM demux (symphonia) → Opus decode (libopus) → resample → cpal output.
//!
//! A decoder thread keeps ~0.5 s of ready samples in a shared buffer; the cpal callback
//! drains it. Tracks are held fully in memory (a 6-minute Premium track is ~11 MiB) and can
//! start playing while they're still downloading (see [`Stream`]).

use std::{
    collections::VecDeque,
    io::{self, Read, Seek, SeekFrom},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use symphonia::core::{
    codecs::CodecParameters,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType},
    io::{MediaSourceStream, MediaSourceStreamOptions},
    units::Time,
};

const OPUS_RATE: u32 = 48_000;

/// The bytes of one track, possibly still downloading. The download appends to it; the decoder
/// reads it like a file and waits for bytes that haven't arrived yet.
pub struct Stream {
    len: u64,
    inner: Mutex<StreamInner>,
    more: Condvar,
}

struct StreamInner {
    data: Vec<u8>,
    failed: Option<String>,
}

impl Stream {
    /// An empty stream that will receive `len` bytes.
    pub fn new(len: u64) -> Arc<Self> {
        Arc::new(Self { len, inner: Mutex::new(StreamInner { data: Vec::with_capacity(len as usize), failed: None }), more: Condvar::new() })
    }

    pub fn push(&self, bytes: &[u8]) {
        self.inner.lock().unwrap().data.extend_from_slice(bytes);
        self.more.notify_all();
    }

    /// The download gave up; readers waiting for more get this error.
    pub fn fail(&self, msg: String) {
        self.inner.lock().unwrap().failed = Some(msg);
        self.more.notify_all();
    }

    pub fn failed(&self) -> Option<String> {
        self.inner.lock().unwrap().failed.clone()
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn downloaded(&self) -> u64 {
        self.inner.lock().unwrap().data.len() as u64
    }

    fn reader(self: &Arc<Self>, stop: Arc<AtomicBool>) -> StreamReader {
        StreamReader { stream: self.clone(), pos: 0, stop }
    }
}

/// Reads a [`Stream`], waiting for data that is still downloading. `stop` ends the wait (the
/// track was replaced or stopped).
struct StreamReader {
    stream: Arc<Stream>,
    pos: u64,
    stop: Arc<AtomicBool>,
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.stream.len || buf.is_empty() {
            return Ok(0);
        }
        let mut inner = self.stream.inner.lock().unwrap();
        loop {
            let have = inner.data.len() as u64;
            if self.pos < have {
                let n = ((have - self.pos) as usize).min(buf.len());
                let at = self.pos as usize;
                buf[..n].copy_from_slice(&inner.data[at..at + n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if let Some(e) = &inner.failed {
                return Err(io::Error::other(e.clone()));
            }
            if self.stop.load(Ordering::Relaxed) {
                return Err(io::Error::other("stopped"));
            }
            inner = self.stream.more.wait_timeout(inner, Duration::from_millis(100)).unwrap().0;
        }
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(d) => self.stream.len as i64 + d,
            SeekFrom::Current(d) => self.pos as i64 + d,
        };
        if pos < 0 {
            return Err(io::Error::other("seek before start"));
        }
        self.pos = pos as u64;
        Ok(self.pos)
    }
}

impl symphonia::core::io::MediaSource for StreamReader {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.stream.len)
    }
}

#[derive(Default)]
struct State {
    buffer: VecDeque<f32>,
    paused: bool,
    volume: f32,
    /// Media seconds played since `base_secs` (device time × speed).
    played: f64,
    base_secs: f64,
    /// Playback speed (pitch preserved), 1.0 = normal.
    speed: f32,
    loaded: bool,
    decoder_done: bool,
    /// Bumped on every load, so stale decoder output is dropped.
    generation: u64,
}

pub struct Status {
    pub position: f64,
    pub playing: bool,
    pub finished: bool,
    pub loaded: bool,
}

enum Cmd {
    Load(StreamReader, u64),
    Seek(f64),
    Speed(f32),
    Stop,
}

pub struct Audio {
    state: Arc<Mutex<State>>,
    cmd: mpsc::Sender<Cmd>,
    /// Stops the current track's reader from waiting for more data.
    reader_stop: Mutex<Arc<AtomicBool>>,
}

impl Audio {
    pub fn start() -> Result<Self> {
        let state = Arc::new(Mutex::new(State { volume: 0.8, speed: 1.0, ..Default::default() }));
        // cpal streams can't move between threads on every platform: own it on a dedicated thread.
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, usize)>>();
        let st = state.clone();
        thread::Builder::new().name("audio-out".into()).spawn(move || {
            match open_output(st) {
                Ok((stream, rate, ch)) => {
                    let _ = ready_tx.send(Ok((rate, ch)));
                    let _keep = stream;
                    loop {
                        thread::park();
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })?;
        let (rate, channels) = ready_rx.recv().map_err(|_| anyhow!("audio thread died"))??;

        let (cmd, cmd_rx) = mpsc::channel();
        let st = state.clone();
        thread::Builder::new().name("audio-decode".into()).spawn(move || decoder_thread(st, cmd_rx, rate, channels))?;
        Ok(Self { state, cmd, reader_stop: Mutex::new(Arc::new(AtomicBool::new(false))) })
    }

    /// Play a track (it may still be downloading).
    pub fn load(&self, stream: &Arc<Stream>) {
        let stop = Arc::new(AtomicBool::new(false));
        std::mem::replace(&mut *self.reader_stop.lock().unwrap(), stop.clone()).store(true, Ordering::Relaxed);
        let generation = {
            let mut s = self.state.lock().unwrap();
            s.generation += 1;
            s.buffer.clear();
            s.played = 0.0;
            s.base_secs = 0.0;
            s.loaded = true;
            s.decoder_done = false;
            s.paused = false;
            s.generation
        };
        let _ = self.cmd.send(Cmd::Load(stream.reader(stop), generation));
    }

    pub fn stop(&self) {
        self.reader_stop.lock().unwrap().store(true, Ordering::Relaxed);
        {
            let mut s = self.state.lock().unwrap();
            s.generation += 1;
            s.buffer.clear();
            s.loaded = false;
            s.played = 0.0;
            s.base_secs = 0.0;
        }
        let _ = self.cmd.send(Cmd::Stop);
    }

    pub fn seek(&self, secs: f64) {
        let _ = self.cmd.send(Cmd::Seek(secs.max(0.0)));
    }

    /// Change playback speed (0.1×–3×, pitch preserved). Restarts output at the current spot.
    pub fn set_speed(&self, speed: f32) {
        let speed = speed.clamp(0.1, 3.0);
        let pos = self.status().position;
        self.state.lock().unwrap().speed = speed;
        let _ = self.cmd.send(Cmd::Speed(speed));
        if self.status().loaded {
            self.seek(pos);
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.state.lock().unwrap().paused = paused;
    }

    pub fn set_volume(&self, v: f32) {
        self.state.lock().unwrap().volume = v.clamp(0.0, 1.0);
    }

    pub fn status(&self) -> Status {
        let s = self.state.lock().unwrap();
        Status {
            position: s.base_secs + s.played,
            playing: s.loaded && !s.paused,
            finished: s.loaded && s.decoder_done && s.buffer.is_empty(),
            loaded: s.loaded,
        }
    }
}

fn open_output(state: Arc<Mutex<State>>) -> Result<(cpal::Stream, u32, usize)> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or_else(|| anyhow!("no audio output device"))?;
    // Prefer 48 kHz float (no resampling needed); otherwise take the device default.
    let config = device
        .supported_output_configs()?
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32 && c.channels() >= 2)
        .find(|c| c.min_sample_rate() <= OPUS_RATE && c.max_sample_rate() >= OPUS_RATE)
        .map(|c| c.with_sample_rate(OPUS_RATE))
        .map_or_else(|| device.default_output_config(), Ok)?;
    if config.sample_format() != cpal::SampleFormat::F32 {
        bail!("audio device doesn't offer float output ({:?})", config.sample_format());
    }
    let rate = config.sample_rate();
    let channels = config.channels() as usize;
    let stream = device.build_output_stream::<f32, _, _>(
        config.config(),
        move |out: &mut [f32], _| {
            let mut s = state.lock().unwrap();
            if s.paused || !s.loaded {
                out.fill(0.0);
                return;
            }
            let vol = s.volume * s.volume; // perceptual-ish curve
            let mut frames = 0u64;
            let media_per_frame = s.speed as f64 / rate as f64;
            for frame in out.chunks_mut(channels) {
                if s.buffer.len() < channels {
                    frame.fill(0.0);
                    continue;
                }
                for x in frame.iter_mut() {
                    *x = s.buffer.pop_front().unwrap_or(0.0) * vol;
                }
                frames += 1;
            }
            s.played += frames as f64 * media_per_frame;
        },
        |e| eprintln!("audio output error: {e}"),
        None,
    )?;
    stream.play()?;
    Ok((stream, rate, channels))
}

/// Minimal safe wrapper over libopus' decoder (system library on Linux).
struct OpusDecoder(*mut opusic_sys::OpusDecoder);

impl OpusDecoder {
    fn new() -> Result<Self> {
        let mut err = 0;
        let ptr = unsafe { opusic_sys::opus_decoder_create(OPUS_RATE as i32, 2, &mut err) };
        if ptr.is_null() || err != 0 {
            bail!("opus_decoder_create failed ({err})");
        }
        Ok(Self(ptr))
    }
    /// Decode one packet into interleaved stereo; returns frames decoded.
    fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize> {
        let n = unsafe {
            opusic_sys::opus_decode_float(self.0, packet.as_ptr(), packet.len() as i32, out.as_mut_ptr(), (out.len() / 2) as i32, 0)
        };
        if n < 0 {
            bail!("opus decode error {n}");
        }
        Ok(n as usize)
    }
    fn reset(&mut self) {
        unsafe { opusic_sys::opus_decoder_ctl(self.0, opusic_sys::OPUS_RESET_STATE) };
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { opusic_sys::opus_decoder_destroy(self.0) };
    }
}

/// Linear resampler + channel mapper from 48 kHz stereo to the device format.
struct Resampler {
    step: f64,
    pos: f64,
    pending: Vec<f32>, // stereo frames not yet consumed
    out_channels: usize,
}

impl Resampler {
    fn new(out_rate: u32, out_channels: usize) -> Self {
        Self { step: OPUS_RATE as f64 / out_rate as f64, pos: 0.0, pending: Vec::new(), out_channels }
    }
    fn reset(&mut self) {
        self.pos = 0.0;
        self.pending.clear();
    }
    fn push_frame(&self, out: &mut Vec<f32>, l: f32, r: f32) {
        match self.out_channels {
            1 => out.push((l + r) * 0.5),
            n => {
                out.push(l);
                out.push(r);
                out.extend(std::iter::repeat_n(0.0, n - 2));
            }
        }
    }
    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < f64::EPSILON {
            for f in input.chunks_exact(2) {
                self.push_frame(out, f[0], f[1]);
            }
            return;
        }
        self.pending.extend_from_slice(input);
        let frames = self.pending.len() / 2;
        while self.pos + 1.0 < frames as f64 {
            let i = self.pos as usize;
            let t = (self.pos - i as f64) as f32;
            let l = self.pending[2 * i] * (1.0 - t) + self.pending[2 * i + 2] * t;
            let r = self.pending[2 * i + 1] * (1.0 - t) + self.pending[2 * i + 3] * t;
            self.push_frame(out, l, r);
            self.pos += self.step;
        }
        let used = (self.pos as usize).min(frames);
        self.pending.drain(..used * 2);
        self.pos -= used as f64;
    }
}

struct Track {
    reader: Box<dyn FormatReader>,
    track_id: u32,
    time_base: Option<symphonia::core::units::TimeBase>,
    decoder: OpusDecoder,
    pre_skip: usize,
    generation: u64,
}

fn open_track(source: StreamReader, generation: u64) -> Result<Track> {
    let mss = MediaSourceStream::new(Box::new(source), MediaSourceStreamOptions::default());
    let reader = symphonia::default::formats::MkvReader::try_new(mss, FormatOptions::default())?;
    let track = reader.default_track(TrackType::Audio).ok_or_else(|| anyhow!("no audio track"))?;
    let track_id = track.id;
    let time_base = track.time_base;
    let pre_skip = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p
            .extra_data
            .as_ref()
            .filter(|d| d.len() >= 12 && d.starts_with(b"OpusHead"))
            .map(|d| u16::from_le_bytes([d[10], d[11]]) as usize)
            .unwrap_or(0),
        _ => bail!("not an audio track"),
    };
    Ok(Track { reader: Box::new(reader), track_id, time_base, decoder: OpusDecoder::new()?, pre_skip, generation })
}

fn decoder_thread(state: Arc<Mutex<State>>, cmds: mpsc::Receiver<Cmd>, rate: u32, channels: usize) {
    let target = (rate as usize) * channels / 2; // ~0.5 s buffered
    let mut track: Option<Track> = None;
    let mut resampler = Resampler::new(rate, channels);
    let mut pcm = vec![0f32; 5760 * 2]; // max Opus frame (120 ms) stereo
    let mut out = Vec::with_capacity(8192);
    // Pitch-preserving time-stretch (WSOLA) when speed != 1; bypassed at normal speed.
    let mut stretch: Vec<wsola::TimeStretch> = Vec::new();
    let mut stretched = Vec::new();

    loop {
        // Handle commands; block only when there's nothing to decode.
        let cmd = if track.is_some() {
            cmds.try_recv().ok()
        } else {
            match cmds.recv() {
                Ok(c) => Some(c),
                Err(_) => return, // Audio was dropped
            }
        };
        match cmd {
            Some(Cmd::Speed(speed)) => {
                stretch = make_stretch(speed);
                stretched = vec![0f32; stretch.first().map_or(0, |s| s.min_output_buff_size() * 8)];
            }
            Some(Cmd::Load(data, generation)) => {
                resampler.reset();
                stretch.iter_mut().for_each(|s| s.reset());
                track = match open_track(data, generation) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        eprintln!("can't open track: {e}");
                        let mut s = state.lock().unwrap();
                        if s.generation == generation {
                            s.decoder_done = true;
                        }
                        None
                    }
                };
            }
            Some(Cmd::Seek(secs)) => {
                if let Some(t) = track.as_mut() {
                    let to = SeekTo::Time { time: Time::try_from_secs_f64(secs).unwrap_or_default(), track_id: Some(t.track_id) };
                    if let Ok(seeked) = t.reader.seek(SeekMode::Accurate, to) {
                        t.decoder.reset();
                        t.pre_skip = 0;
                        resampler.reset();
                        stretch.iter_mut().for_each(|s| s.reset());
                        let actual = t.time_base.and_then(|tb| tb.calc_time(seeked.actual_ts)).map(|t| t.as_secs_f64()).unwrap_or(secs);
                        let mut s = state.lock().unwrap();
                        if s.generation == t.generation {
                            s.buffer.clear();
                            s.played = 0.0;
                            s.base_secs = actual;
                            s.decoder_done = false;
                        }
                    }
                }
            }
            Some(Cmd::Stop) => {
                track = None;
                continue;
            }
            None => {}
        }

        let Some(t) = track.as_mut() else { continue };
        {
            let s = state.lock().unwrap();
            if s.generation != t.generation {
                track = None;
                continue;
            }
            if s.buffer.len() >= target {
                drop(s);
                thread::sleep(Duration::from_millis(15));
                continue;
            }
        }

        out.clear();
        let mut ended = false;
        // Decode a few packets per lock to keep the callback's lock contention low.
        for _ in 0..8 {
            match t.reader.next_packet() {
                Ok(Some(p)) if p.track_id == t.track_id => match t.decoder.decode(&p.data, &mut pcm) {
                    Ok(frames) => {
                        let skip = t.pre_skip.min(frames);
                        t.pre_skip -= skip;
                        feed(&mut stretch, &mut stretched, &mut resampler, &pcm[skip * 2..frames * 2], &mut out);
                    }
                    Err(e) => eprintln!("{e}"),
                },
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    ended = true;
                    // Flush what the stretcher stages still hold.
                    drain(&mut stretch, &mut stretched, &mut resampler, &mut out);
                    break;
                }
            }
        }
        let mut s = state.lock().unwrap();
        if s.generation == t.generation {
            s.buffer.extend(out.iter().copied());
            if ended {
                s.decoder_done = true;
            }
        }
        if ended {
            track = None;
        }
    }
}

/// One stage per factor; the stretcher handles 0.25×–4×, so slower speeds use two stages
/// (e.g. 0.1× = 0.316 × 0.316). Empty at normal speed.
fn make_stretch(speed: f32) -> Vec<wsola::TimeStretch> {
    if (speed - 1.0).abs() < 0.001 {
        return Vec::new();
    }
    let factors = if speed < 0.25 { vec![speed.sqrt(); 2] } else { vec![speed] };
    // Music wants longer windows than the speech-tuned defaults.
    let config = wsola::Config { hop_ms: 25.0, search_ms: 12.0 };
    factors
        .into_iter()
        .filter_map(|f| {
            let mut s = wsola::TimeStretch::with_config(
                std::num::NonZeroU32::new(OPUS_RATE).unwrap(),
                std::num::NonZeroU16::new(2).unwrap(),
                config,
            )
            .ok()?;
            s.set_tempo(f);
            Some(s)
        })
        .collect()
}

/// Run `input` through the stretch stages; returns the final output.
fn run_stages(stages: &mut [wsola::TimeStretch], buf: &mut [f32], input: &[f32], draining: bool) -> Vec<f32> {
    let mut data = input.to_vec();
    for s in stages.iter_mut() {
        s.push(&data);
        if draining {
            s.set_draining();
        }
        let mut next = Vec::new();
        while let Ok(n) = s.pull(buf) {
            next.extend_from_slice(&buf[..n]);
            if draining && !s.is_draining() {
                break;
            }
        }
        data = next;
    }
    data
}

/// Decoded 48 kHz stereo → (time-stretch) → resample/channel-map into `out`.
fn feed(stages: &mut [wsola::TimeStretch], buf: &mut [f32], resampler: &mut Resampler, input: &[f32], out: &mut Vec<f32>) {
    if stages.is_empty() {
        resampler.process(input, out);
    } else {
        let data = run_stages(stages, buf, input, false);
        resampler.process(&data, out);
    }
}

fn drain(stages: &mut [wsola::TimeStretch], buf: &mut [f32], resampler: &mut Resampler, out: &mut Vec<f32>) {
    if !stages.is_empty() {
        let data = run_stages(stages, buf, &[], true);
        resampler.process(&data, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that adds `chunk` bytes every 5 ms on another thread.
    fn slow_fill(len: usize, chunk: usize) -> (Arc<Stream>, Vec<u8>, thread::JoinHandle<()>) {
        let data: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
        let stream = Stream::new(len as u64);
        let (s, d) = (stream.clone(), data.clone());
        let h = thread::spawn(move || {
            for c in d.chunks(chunk) {
                thread::sleep(Duration::from_millis(5));
                s.push(c);
            }
        });
        (stream, data, h)
    }

    #[test]
    fn reads_everything_while_it_arrives() {
        let (stream, data, h) = slow_fill(100_000, 7_000);
        let mut r = stream.reader(Arc::new(AtomicBool::new(false)));
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        h.join().unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn seeking_ahead_waits_for_the_bytes() {
        let (stream, data, h) = slow_fill(100_000, 5_000);
        let mut r = stream.reader(Arc::new(AtomicBool::new(false)));
        assert_eq!(r.seek(SeekFrom::End(-10)).unwrap(), 99_990);
        let mut tail = [0u8; 10];
        r.read_exact(&mut tail).unwrap();
        assert_eq!(&tail[..], &data[99_990..]);
        assert_eq!(r.read(&mut tail).unwrap(), 0); // end of track
        h.join().unwrap();
    }

    #[test]
    fn stop_and_failure_end_the_wait() {
        let stream = Stream::new(1_000);
        let stop = Arc::new(AtomicBool::new(false));
        let mut r = stream.reader(stop.clone());
        let s = stop.clone();
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            s.store(true, Ordering::Relaxed);
        });
        assert!(r.read(&mut [0u8; 10]).is_err());
        h.join().unwrap();

        let stream = Stream::new(1_000);
        stream.push(&[1, 2, 3]);
        stream.fail("HTTP 403".into());
        let mut r = stream.reader(Arc::new(AtomicBool::new(false)));
        let mut buf = [0u8; 10];
        assert_eq!(r.read(&mut buf).unwrap(), 3); // what arrived is still readable
        assert_eq!(r.read(&mut buf).unwrap_err().to_string(), "HTTP 403");
    }
}
