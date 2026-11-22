//! Reports what you listen to back to YouTube Music, the same way the website does, so
//! your YT Music history and recommendations learn from plays in this app.
//!
//! Per song: one "playback" ping when it starts (puts it in history), then "watchtime"
//! pings describing which parts were actually heard: on YouTube's schedule (e.g. 10/20/30 s,
//! then every 40 s), and on pause, seek and when the song ends (`final=1`).
//! Modelled on Metrolist's YouTubePlaybackReporter.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use rand::RngExt;
use tokio::sync::mpsc;

use crate::yt;

const CPN_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_";
const MIN_SEGMENT: f64 = 0.5;

fn cpn() -> String {
    let mut rng = rand::rng();
    (0..16).map(|_| CPN_ALPHABET[rng.random_range(0..CPN_ALPHABET.len())] as char).collect()
}

/// Seconds with one decimal, as YouTube's player formats them.
fn secs(s: f64) -> String {
    format!("{:.1}", s.max(0.0))
}

enum Ping {
    Start { cmt: f64 },
    Watchtime { st: String, et: String, cmt: f64, rt: f64, last: bool },
}

struct Schedule {
    scheduled: Vec<f64>,
    interval: f64,
}

pub struct Session {
    tx: mpsc::UnboundedSender<Ping>,
    started: Instant,
    schedule: Arc<Mutex<Schedule>>,
    flushes: usize,
    last_flush_rt: f64,
    open: Option<f64>,
    last_pos: f64,
    pending: Vec<(f64, f64)>,
}

impl Session {
    pub fn start(yt: Arc<yt::Client>, video_id: String, itag: u64) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Ping>();
        let schedule = Arc::new(Mutex::new(Schedule { scheduled: vec![10.0, 20.0, 30.0], interval: 40.0 }));
        let (sched, id, cpn) = (schedule.clone(), video_id, cpn());
        tokio::spawn(async move {
            let tracking = match yt.tracking(&id).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[report] {id}: no tracking urls ({e}); this play won't be in YT history");
                    return;
                }
            };
            {
                let mut s = sched.lock().unwrap();
                s.scheduled = tracking.scheduled.clone();
                s.interval = tracking.interval;
            }
            let fmt = itag.to_string();
            // Sequential: pings must arrive in order. Ends when the Session is dropped.
            while let Some(p) = rx.recv().await {
                let (url, params) = match p {
                    Ping::Start { cmt } => (&tracking.playback, vec![("cmt", secs(cmt)), ("fmt", fmt.clone())]),
                    Ping::Watchtime { st, et, cmt, rt, last } => (&tracking.watchtime, vec![
                        ("st", st), ("et", et), ("cmt", secs(cmt)), ("rt", secs(rt)),
                        ("final", if last { "1" } else { "0" }.to_string()), ("fmt", fmt.clone()),
                    ]),
                };
                let kind = if url.contains("/playback") { "playback" } else { "watchtime" };
                match yt.stats(url, &cpn, &params).await {
                    Ok(()) => eprintln!("[report] {id}: {kind} ok {}", params.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ")),
                    Err(e) => eprintln!("[report] {id}: {kind} failed: {e}"),
                }
            }
        });
        let _ = tx.send(Ping::Start { cmt: 0.0 });
        Self {
            tx,
            started: Instant::now(),
            schedule,
            flushes: 0,
            last_flush_rt: 0.0,
            open: None,
            last_pos: 0.0,
            pending: Vec::new(),
        }
    }

    fn rt(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn close(&mut self, at: f64) {
        if let Some(start) = self.open.take() {
            let end = at.max(start);
            if end - start >= MIN_SEGMENT {
                self.pending.push((start, end));
            }
        }
    }

    fn flush(&mut self, cmt: f64, last: bool) {
        let segs = std::mem::take(&mut self.pending);
        if segs.is_empty() && !last {
            return;
        }
        let st = segs.iter().map(|s| secs(s.0)).collect::<Vec<_>>().join(",");
        let et = segs.iter().map(|s| secs(s.1)).collect::<Vec<_>>().join(",");
        let rt = self.rt();
        self.last_flush_rt = rt;
        let _ = self.tx.send(Ping::Watchtime { st, et, cmt, rt, last });
    }

    /// Called every tick with the current position while the song is loaded.
    pub fn progress(&mut self, pos: f64, playing: bool) {
        if !playing {
            if self.open.is_some() {
                self.pause(pos);
            }
            return;
        }
        if self.open.is_none() {
            self.open = Some(pos);
        }
        self.last_pos = pos;
        let due = {
            let s = self.schedule.lock().unwrap();
            match s.scheduled.get(self.flushes) {
                Some(t) => self.rt() >= *t,
                None => self.rt() - self.last_flush_rt >= s.interval,
            }
        };
        if due {
            self.flushes += 1;
            self.close(pos);
            self.open = Some(pos);
            self.flush(pos, false);
        }
    }

    pub fn pause(&mut self, pos: f64) {
        self.close(pos);
        self.last_pos = pos;
        self.flush(pos, false);
    }

    pub fn seek(&mut self, to: f64, playing: bool) {
        self.close(self.last_pos);
        self.flush(self.last_pos, false);
        self.last_pos = to;
        if playing {
            self.open = Some(to);
        }
    }

    /// Song ended or was replaced.
    pub fn finish(mut self, pos: f64) {
        let pos = if pos > 0.0 { pos } else { self.last_pos };
        self.close(pos);
        self.flush(pos, true);
    }
}
