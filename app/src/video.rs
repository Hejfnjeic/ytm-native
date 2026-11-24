//! Music videos as moving pictures: YouTube's H.264 in fragmented MP4, decoded on the CPU
//! with rusty_h264 (pure Rust) and handed to the UI in time with the audio. Ported from
//! ytm-rs by grapedjuice, which decodes with OpenH264 (C++).
//!
//! The audio engine stays the clock: a thread here fetches one fragment at a time from
//! where the song is, decodes it, and keeps a few frames ready. The UI takes whichever frame
//! is due. Nothing runs unless the listener turned video on.

use std::collections::{BinaryHeap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusty_h264::{Decoder, YuvFrame};

use crate::audio::AudioStatus;
use crate::innertube::VideoStream;

/// Frames kept ready: under half a second, about 30 MB at 720p.
const READY: usize = 12;
/// Pictures held back to put them in display order: rusty_h264 returns them in decode
/// order, and YouTube's B-frames reach at most a few ahead.
const REORDER: usize = 4;
/// Decoded this far behind the clock, a frame isn't worth converting: it would never show.
const LATE: f64 = 0.05;
/// The clock this far before what's waiting to show is a seek back.
const SEEK_BACK: f64 = 0.5;

// ---------------------------------------------------------------------------- MP4

use crate::mp4::{be32, be64, boxes, find};

/// What the start of the file says about all of it.
#[derive(Debug, PartialEq)]
pub struct Track {
    /// The decoder's setup, as length-prefixed-free NAL units with start codes.
    params: Vec<u8>,
    /// Bytes in each NAL unit's length prefix.
    nal_len: usize,
    timescale: u32,
    default_duration: u32,
    /// The presentation time of the first sample, in timescale units.
    start: i64,
    colors: Colors,
}

/// How a picture's numbers turn into colours, as its stream's header says: the range black
/// and white are at, and the matrix. A stream that doesn't say gets YouTube's own: TV range
/// (black at 16, white at 235) and BT.709, which its H.264 is at every size.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Colors {
    full_range: bool,
    /// BT.601's matrix, as SD broadcasts have, rather than BT.709's.
    bt601: bool,
}

/// [`Colors`] as sums, in 65536ths: light from `y` less `black`, red from v, green from both,
/// blue from u.
struct ToRgb {
    black: i32,
    y: i32,
    rv: i32,
    gu: i32,
    gv: i32,
    bu: i32,
}

impl Colors {
    fn to_rgb(self) -> ToRgb {
        let (kr, kb) = if self.bt601 { (0.299, 0.114) } else { (0.2126, 0.0722) };
        let kg = 1.0 - kr - kb;
        // TV range spreads 16..=235 (and colour's ±112) out over all of 0..=255.
        let (black, y, c) = if self.full_range { (0, 1.0, 1.0) } else { (16, 255.0 / 219.0, 255.0 / 224.0) };
        let f = |x: f64| (x * 65536.0).round() as i32;
        ToRgb { black, y: f(y), rv: f(2.0 * (1.0 - kr) * c), gu: f(2.0 * (1.0 - kb) * kb / kg * c), gv: f(2.0 * (1.0 - kr) * kr / kg * c), bu: f(2.0 * (1.0 - kb) * c) }
    }
}

impl ToRgb {
    #[inline]
    fn rgb(&self, y: i32, u: u8, v: u8) -> egui::Color32 {
        let l = (y - self.black) * self.y + 32768;
        let (u, v) = (u as i32 - 128, v as i32 - 128);
        let c = |x: i32| (x >> 16).clamp(0, 255) as u8;
        egui::Color32::from_rgb(c(l + self.rv * v), c(l - self.gu * u - self.gv * v), c(l + self.bu * u))
    }
}

/// What a sequence parameter set (an SPS NAL unit, header byte and all) says of colours, from
/// its video usability information; `None` if it's cut short.
fn sps_colors(nal: &[u8]) -> Option<Colors> {
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    // Without the bytes that keep start codes out of it: a 3 after two zeros.
    let mut rbsp = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal.get(1..)? {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    let mut r = Bits { b: &rbsp, at: 0 };
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags, level
    r.ue()?; // id
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        r.ue()?; // bit depths
        r.ue()?;
        r.bit()?;
        if r.bit()? == 1 {
            // Scaling lists: read through for their length.
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.bit()? == 1 {
                    let (mut last, mut next) = (8, 8);
                    for _ in 0..if i < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?; // frame number size
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // reference frames
    r.bit()?;
    r.ue()?; // size in macroblocks
    r.ue()?;
    if r.bit()? == 0 {
        r.bit()?;
    }
    r.bit()?;
    if r.bit()? == 1 {
        // Cropping.
        for _ in 0..4 {
            r.ue()?;
        }
    }
    // No VUI, or no video signal type in it: nothing said.
    if r.bit()? == 0 {
        return Some(Colors::default());
    }
    if r.bit()? == 1 && r.bits(8)? == 255 {
        r.bits(32)?; // the pixels' own aspect ratio
    }
    if r.bit()? == 1 {
        r.bit()?; // overscan
    }
    if r.bit()? == 0 {
        return Some(Colors::default());
    }
    r.bits(3)?; // video format
    let full_range = r.bit()? == 1;
    // Primaries and transfer, then the matrix: 5 and 6 are BT.601's, 1 (and unsaid) BT.709's.
    let matrix = if r.bit()? == 1 { r.bits(24)? & 0xff } else { 2 };
    Some(Colors { full_range, bt601: matches!(matrix, 5 | 6) })
}

/// Reads an SPS bit by bit.
struct Bits<'a> {
    b: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.b.get(self.at / 8)?;
        let bit = byte >> (7 - self.at % 8) & 1;
        self.at += 1;
        Some(bit as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0, |v, _| Some(v << 1 | self.bit()?))
    }

    /// Exp-Golomb: as many zeros as bits to follow, a one, then those bits.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.bits(zeros)? as u64).min(u32::MAX as u64) as u32)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

fn parse_track(moov: &[u8]) -> Option<Track> {
    let mdia = ["trak", "mdia"];
    let mdhd = find(moov, &[mdia[0], mdia[1], "mdhd"])?;
    let timescale = if mdhd[0] == 1 { be32(mdhd, 20)? } else { be32(mdhd, 12)? };
    let stsd = find(moov, &["trak", "mdia", "minf", "stbl", "stsd"])?;
    let at = stsd.windows(4).position(|w| w == b"avcC")? + 4;
    let c = stsd.get(at..)?;
    let nal_len = (*c.get(4)? & 3) as usize + 1;
    // The first SPS's: the one a stream starts with.
    let colors = c.get(6..8).and_then(|n| c.get(8..8 + u16::from_be_bytes([n[0], n[1]]) as usize)).and_then(sps_colors).unwrap_or_default();
    let mut params = Vec::new();
    let mut o = 6;
    let mut copy = |count: usize, o: &mut usize| -> Option<()> {
        for _ in 0..count {
            let n = u16::from_be_bytes([*c.get(*o)?, *c.get(*o + 1)?]) as usize;
            params.extend_from_slice(&[0, 0, 0, 1]);
            params.extend_from_slice(c.get(*o + 2..*o + 2 + n)?);
            *o += 2 + n;
        }
        Some(())
    };
    copy((*c.get(5)? & 31) as usize, &mut o)?;
    let pps = *c.get(o)? as usize;
    o += 1;
    copy(pps, &mut o)?;
    let default_duration = find(moov, &["mvex", "trex"]).and_then(|t| be32(t, 12)).unwrap_or(0);
    // With B-frames the first frame shows late; the edit list takes that back out.
    let start = find(moov, &["trak", "edts", "elst"]).and_then(|e| if e[0] == 1 { be64(e, 16).map(|v| v as i64) } else { be32(e, 12).map(|v| v as i32 as i64) }).filter(|v| *v > 0).unwrap_or(0);
    Some(Track { params, nal_len, timescale, default_duration, start, colors })
}

/// One piece of the file, from its index.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fragment {
    offset: u64,
    size: u32,
    /// Seconds.
    start: f64,
}

/// The `sidx` box's fragments. `after`: the byte right after the box, where offsets count from.
fn parse_index(sidx: &[u8], after: u64) -> Option<Vec<Fragment>> {
    let v1 = *sidx.first()? == 1;
    let timescale = be32(sidx, 8)? as f64;
    let (mut time, first, mut at) = if v1 { (be64(sidx, 12)?, be64(sidx, 20)?, 28) } else { (be32(sidx, 12)? as u64, be32(sidx, 16)? as u64, 20) };
    let count = u16::from_be_bytes(sidx.get(at + 2..at + 4)?.try_into().ok()?) as usize;
    at += 4;
    let mut offset = after + first;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let size = be32(sidx, at)? & 0x7fff_ffff;
        out.push(Fragment { offset, size, start: time as f64 / timescale });
        offset += size as u64;
        time += be32(sidx, at + 4)? as u64;
        at += 12;
    }
    Some(out)
}

/// The fragment holding `secs`.
fn fragment_at(fragments: &[Fragment], secs: f64) -> usize {
    fragments.partition_point(|f| f.start <= secs).saturating_sub(1)
}

/// A frame's bytes in the fragment, and when it shows.
#[derive(Debug, PartialEq)]
struct Sample {
    at: usize,
    size: usize,
    /// Seconds.
    pts: f64,
}

/// The samples of a whole fragment (`moof` and `mdat` pairs), in decode order.
fn parse_fragment(b: &[u8], track: &Track) -> Vec<Sample> {
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(size) = be32(b, start).map(|s| s as usize) {
        if size < 8 || start + size > b.len() {
            break;
        }
        if &b[start + 4..start + 8] == b"moof" {
            parse_moof(&b[start + 8..start + size], start, track, &mut out);
        }
        start += size;
    }
    out
}

fn parse_moof(moof: &[u8], moof_at: usize, track: &Track, out: &mut Vec<Sample>) -> Option<()> {
    let traf = find(moof, &["traf"])?;
    let tfhd = find(traf, &["tfhd"])?;
    let flags = be32(tfhd, 0)? & 0xff_ffff;
    let mut o = 8;
    if flags & 0x1 != 0 {
        o += 8;
    }
    if flags & 0x2 != 0 {
        o += 4;
    }
    let default_duration = if flags & 0x8 != 0 {
        o += 4;
        be32(tfhd, o - 4)?
    } else {
        track.default_duration
    };
    let default_size = if flags & 0x10 != 0 { be32(tfhd, o)? } else { 0 };
    let tfdt = find(traf, &["tfdt"])?;
    let mut time = if tfdt[0] == 1 { be64(tfdt, 4)? as i64 } else { be32(tfdt, 4)? as i64 };
    let scale = track.timescale.max(1) as f64;
    // Counted from the start of the `moof`, as YouTube's and Apple's files say to.
    let mut at = moof_at;
    // Every run of samples: Apple's clips have two in each fragment, and a run without
    // an offset of its own carries on from the one before.
    for trun in boxes(traf).filter(|(kind, _)| *kind == b"trun").map(|(_, body)| body) {
        let flags = be32(trun, 0)? & 0xff_ffff;
        let count = be32(trun, 4)? as usize;
        let mut o = 8;
        if flags & 0x1 != 0 {
            at = (moof_at as i64 + be32(trun, o)? as i32 as i64) as usize;
            o += 4;
        }
        if flags & 0x4 != 0 {
            o += 4;
        }
        for _ in 0..count {
            let duration = if flags & 0x100 != 0 {
                o += 4;
                be32(trun, o - 4)?
            } else {
                default_duration
            };
            let size = if flags & 0x200 != 0 {
                o += 4;
                be32(trun, o - 4)?
            } else {
                default_size
            } as usize;
            if flags & 0x400 != 0 {
                o += 4;
            }
            let offset = if flags & 0x800 != 0 {
                o += 4;
                be32(trun, o - 4)? as i32 as i64
            } else {
                0
            };
            out.push(Sample { at, size, pts: (time + offset - track.start) as f64 / scale });
            at += size;
            time += duration as i64;
        }
    }
    Some(())
}

/// A sample's length-prefixed NAL units with start codes, as the decoder takes them.
fn annex_b(sample: &[u8], nal_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(sample.len() + 16);
    let mut i = 0;
    while i + nal_len <= sample.len() {
        let len = sample[i..i + nal_len].iter().fold(0usize, |n, b| n << 8 | *b as usize);
        i += nal_len;
        let Some(nal) = sample.get(i..i + len) else { break };
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        i += len;
    }
    out
}

// ---------------------------------------------------------------------------- player

pub struct Frame {
    pub pts: f64,
    pub image: egui::ColorImage,
}

#[derive(Default)]
struct Shared {
    frames: Mutex<VecDeque<Frame>>,
    stop: AtomicBool,
    failed: AtomicBool,
}

/// A video being decoded along with the audio. Stops when dropped.
pub struct Player {
    shared: Arc<Shared>,
    status: Arc<AudioStatus>,
}

impl Player {
    pub fn start(stream: VideoStream, http: reqwest::Client, status: Arc<AudioStatus>) -> Self {
        let shared = Arc::new(Shared::default());
        let (s, clock) = (shared.clone(), status.clone());
        std::thread::Builder::new()
            .name("video".into())
            .spawn(move || {
                if let Err(e) = run(&stream, &http, &clock, &s) {
                    log::warn!("video: {e:#}");
                    s.failed.store(true, Ordering::Relaxed);
                }
            })
            .expect("a thread for the video");
        Self { shared, status }
    }

    /// The frame to show now, if a newer one than the last is due.
    pub fn take_due(&self) -> Option<Frame> {
        let now = self.status.position_smooth().as_secs_f64();
        let mut frames = self.shared.frames.lock().unwrap();
        let mut due = None;
        while frames.front().is_some_and(|f| f.pts <= now) {
            due = frames.pop_front();
        }
        due
    }

    /// It can't play: the picture stays.
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Relaxed)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

fn new_decoder(track: &Track) -> anyhow::Result<Decoder> {
    let mut decoder = Decoder::new();
    decoder.decode(&track.params).map_err(|e| anyhow::anyhow!("the video's parameters: {e}"))?;
    Ok(decoder)
}

/// A decoded picture and its time, sorted to come out of the heap earliest first.
struct Pending(f64, YuvFrame);
impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Pending {
    // Smallest first out of the heap.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.0.total_cmp(&self.0)
    }
}

/// The picture in `colors`: YouTube's is TV range and BT.709, which a plain conversion
/// (full range, BT.601) would show with black lifted to grey.
fn to_image(yuv: &YuvFrame, colors: Colors) -> egui::ColorImage {
    let (w, h) = (yuv.width, yuv.height);
    let (ys, us, vs) = (w, w / 2, w / 2);
    let (y, u, v) = (&yuv.y[..], &yuv.u[..], &yuv.v[..]);
    let m = colors.to_rgb();
    let mut pixels = Vec::with_capacity(w * h);
    for row in 0..h {
        let (yr, ur, vr) = (&y[row * ys..], &u[row / 2 * us..], &v[row / 2 * vs..]);
        pixels.extend((0..w).map(|x| m.rgb(yr[x] as i32, ur[x / 2], vr[x / 2])));
    }
    egui::ColorImage { size: [w, h], source_size: egui::vec2(w as f32, h as f32), pixels }
}

fn run(stream: &VideoStream, http: &reqwest::Client, status: &AudioStatus, shared: &Shared) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let download = |from: u64, to: u64| Download { rt: &rt, http, url: &stream.url, from, to, resp: None, tries: 0 };
    // The header and the index after it, in one request.
    let start = download(0, stream.index.1).all()?;
    let head = start.get(..=stream.init_end as usize).ok_or_else(|| anyhow::anyhow!("short header"))?;
    let index = start.get(stream.index.0 as usize..).ok_or_else(|| anyhow::anyhow!("short index"))?;
    let track = parse_track(find(head, &["moov"]).ok_or_else(|| anyhow::anyhow!("no moov"))?).ok_or_else(|| anyhow::anyhow!("not H.264 in MP4"))?;
    let fragments = find(index, &["sidx"]).and_then(|s| parse_index(s, stream.index.1 + 1)).filter(|f| !f.is_empty()).ok_or_else(|| anyhow::anyhow!("no index"))?;
    log::info!("video: {}x{}, {} fragments", stream.width, stream.height, fragments.len());

    let mut decoder = new_decoder(&track)?;
    let mut next = fragment_at(&fragments, status.position_smooth().as_secs_f64());
    // Decoded pictures waiting to go out in display order.
    let mut pending: BinaryHeap<Pending> = BinaryHeap::new();
    // The last frame out of the decoder: with none waiting to show, where the picture is.
    let mut last_out = fragments[next].start;
    // Where to start over, from deeper in.
    let mut restart = None;
    // Since starting or starting over, until there's a picture: for the log.
    let mut waiting_since = Some((std::time::Instant::now(), fragments[next].start));
    // The earliest frame still to show.
    let showing_from = |last_out: f64| shared.frames.lock().unwrap().front().map_or(last_out, |f| f.pts);
    'play: while !shared.stop.load(Ordering::Relaxed) {
        let clock = status.position_smooth().as_secs_f64();
        if let Some(at) = restart.take().or_else(|| seek_target(&fragments, next, clock, showing_from(last_out))) {
            next = at;
            last_out = fragments[at].start;
            waiting_since = Some((std::time::Instant::now(), clock));
            decoder = new_decoder(&track)?;
            pending.clear();
            shared.frames.lock().unwrap().clear();
        }
        if shared.frames.lock().unwrap().len() >= READY || next >= fragments.len() {
            std::thread::sleep(Duration::from_millis(15));
            continue;
        }
        let f = fragments[next];
        next += 1;
        // Decoded as it arrives: each picture once its bytes are in, not after all of them.
        let mut dl = download(f.offset, f.offset + f.size as u64 - 1);
        let (mut bytes, mut done) = (Vec::with_capacity(f.size as usize), 0);
        loop {
            let more = dl.more(&mut bytes)?;
            let samples = parse_fragment(&bytes, &track);
            while let Some(sample) = samples.get(done).filter(|s| s.at + s.size <= bytes.len()) {
                done += 1;
                let data = &bytes[sample.at..sample.at + sample.size];
                let clock = status.position_smooth().as_secs_f64();
                // Skipped through: on from the fragment the clock is in.
                if let Some(at) = seek_target(&fragments, next - 1, clock, showing_from(last_out)) {
                    restart = Some(at);
                    continue 'play;
                }
                // Catching up (starting, or after a seek): a picture nothing else is made
                // from needn't be decoded to get to the ones that show. About half the work.
                if sample.pts < clock - LATE && disposable(data, track.nal_len) {
                    continue;
                }
                let Ok(Some(yuv)) = decoder.decode(&annex_b(data, track.nal_len)) else { continue };
                pending.push(Pending(sample.pts, yuv));
                if pending.len() <= REORDER {
                    continue;
                }
                let Some(Pending(pts, yuv)) = pending.pop() else { continue };
                last_out = pts;
                if pts < clock - LATE {
                    continue;
                }
                let frame = Frame { pts, image: to_image(&yuv, track.colors) };
                // Paused, or decoding ahead: wait with this frame for room.
                loop {
                    if shared.stop.load(Ordering::Relaxed) {
                        break 'play;
                    }
                    let now = status.position_smooth().as_secs_f64();
                    if let Some(at) = seek_target(&fragments, next - 1, now, showing_from(pts).min(pts)) {
                        restart = Some(at);
                        continue 'play;
                    }
                    if shared.frames.lock().unwrap().len() < READY {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                shared.frames.lock().unwrap().push_back(frame);
                if let Some((since, from)) = waiting_since.take() {
                    log::info!("video: picture {:.0} ms after starting at {from:.1} s", since.elapsed().as_secs_f64() * 1000.0);
                }
            }
            if !more {
                break;
            }
        }
    }
    Ok(())
}

/// Where to start decoding over when the clock has jumped: back before what's waiting to
/// show, or on past the fragment being decoded, `current` (decoding the rest of it would
/// only make pictures too late to show, and each fragment starts with one of its own).
fn seek_target(fragments: &[Fragment], current: usize, clock: f64, showing_from: f64) -> Option<usize> {
    let at = fragment_at(fragments, clock);
    (clock < showing_from - SEEK_BACK || (at > current && current < fragments.len())).then_some(at)
}

/// Whether nothing is decoded from this sample's picture (`nal_ref_idc` 0 on all of its
/// slices): it can be left out when it wouldn't show anyway.
fn disposable(sample: &[u8], nal_len: usize) -> bool {
    let (mut i, mut slices) = (0, 0);
    while i + nal_len <= sample.len() {
        let len = sample[i..i + nal_len].iter().fold(0usize, |n, b| n << 8 | *b as usize);
        i += nal_len;
        let Some(&header) = sample.get(i) else { break };
        // Coded slices: 1, and 5 for the pictures everything after starts from.
        if matches!(header & 0x1f, 1 | 5) {
            if header >> 5 & 3 != 0 {
                return false;
            }
            slices += 1;
        }
        i += len;
    }
    slices > 0
}

/// A range of the video, read as it arrives. One that breaks off goes on from where it got
/// to, three times.
struct Download<'a> {
    rt: &'a tokio::runtime::Runtime,
    http: &'a reqwest::Client,
    url: &'a str,
    from: u64,
    to: u64,
    resp: Option<reqwest::Response>,
    tries: u32,
}

impl Download<'_> {
    /// Add what has arrived since to `buf`: false once all of it has.
    fn more(&mut self, buf: &mut Vec<u8>) -> anyhow::Result<bool> {
        loop {
            let at = self.from + buf.len() as u64;
            if at > self.to {
                return Ok(false);
            }
            let got = self.rt.block_on(async {
                if self.resp.is_none() {
                    let url = format!("{}&range={at}-{}", self.url, self.to);
                    self.resp = Some(self.http.get(&url).header("User-Agent", crate::innertube::UA).timeout(Duration::from_secs(30)).send().await?.error_for_status()?);
                }
                anyhow::Ok(self.resp.as_mut().expect("just set").chunk().await?)
            });
            match got {
                Ok(Some(chunk)) => {
                    buf.extend_from_slice(&chunk);
                    return Ok(true);
                }
                // Ended short: ask for the rest.
                Ok(None) => self.resp = None,
                Err(e) => {
                    self.resp = None;
                    self.tries += 1;
                    if self.tries >= 3 {
                        return Err(e);
                    }
                }
            }
        }
    }

    fn all(mut self) -> anyhow::Result<Vec<u8>> {
        let mut buf = Vec::new();
        while self.more(&mut buf)? {}
        Ok(buf)
    }
}

