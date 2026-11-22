//! Solves YouTube's `sig` and `n` URL challenges with yt-dlp's EJS solver
//! (vendor/ejs, Unlicense/MIT/ISC) running in an embedded QuickJS engine.
//!
//! One long-lived JS engine on its own thread. The first solve for a new player version
//! preprocesses the 3 MB player script (slow, ~10 s) and caches the result on disk; later
//! solves reuse it (~0.5 s).

use std::{fs, path::PathBuf, sync::Arc, thread};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

const EJS_LIB: &str = include_str!("../../vendor/ejs/lib.min.js");
const EJS_CORE: &str = include_str!("../../vendor/ejs/core.min.js");

pub struct Solved {
    pub sig: Option<String>,
    pub n: Option<String>,
}

struct Job {
    player_id: String,
    player_js: Arc<String>,
    sig: Option<String>,
    n: Option<String>,
    reply: oneshot::Sender<Result<Solved>>,
}

#[derive(Clone)]
pub struct Solver {
    tx: mpsc::UnboundedSender<Job>,
}

impl Solver {
    pub fn start(cache_dir: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        thread::Builder::new()
            .name("js-solver".into())
            .spawn(move || {
                let engine = Engine::new();
                while let Some(job) = rx.blocking_recv() {
                    let result = match &engine {
                        Ok(e) => {
                            let r = e.solve(&cache_dir, &job);
                            // A solve parses the 4 MB player; free that right away and give the
                            // memory back to the OS (glibc keeps it otherwise: ~70 MB per solve).
                            e._rt.run_gc();
                            crate::paths::release_memory();
                            r
                        }
                        Err(err) => Err(anyhow!("JS engine failed to start: {err}")),
                    };
                    let _ = job.reply.send(result);
                }
            })
            .expect("spawn solver thread");
        Self { tx }
    }

    pub async fn solve(&self, player_id: String, player_js: Arc<String>, sig: Option<String>, n: Option<String>) -> Result<Solved> {
        if sig.is_none() && n.is_none() {
            return Ok(Solved { sig: None, n: None });
        }
        let (reply, rx) = oneshot::channel();
        self.tx.send(Job { player_id, player_js, sig, n, reply }).map_err(|_| anyhow!("solver thread stopped"))?;
        rx.await.map_err(|_| anyhow!("solver thread stopped"))?
    }
}

struct Engine {
    _rt: rquickjs::Runtime,
    ctx: rquickjs::Context,
}

impl Engine {
    fn new() -> Result<Self> {
        let rt = rquickjs::Runtime::new()?;
        rt.set_memory_limit(768 * 1024 * 1024);
        rt.set_max_stack_size(16 * 1024 * 1024);
        let ctx = rquickjs::Context::full(&rt)?;
        ctx.with(|ctx| -> Result<()> {
            ctx.eval::<(), _>(format!("{EJS_LIB}\nObject.assign(globalThis, lib);\n{EJS_CORE}\nglobalThis.jsc = jsc;"))
                .map_err(|e| anyhow!("loading solver: {e}"))
        })?;
        Ok(Self { _rt: rt, ctx })
    }

    fn solve(&self, cache_dir: &std::path::Path, job: &Job) -> Result<Solved> {
        if !job.player_id.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("unexpected player id");
        }
        let cache = cache_dir.join(format!("player-{}.pre.js", job.player_id));
        let mut requests = Vec::new();
        if let Some(s) = &job.sig {
            requests.push(json!({ "type": "sig", "challenges": [s] }));
        }
        if let Some(n) = &job.n {
            requests.push(json!({ "type": "n", "challenges": [n] }));
        }
        let input = match fs::read_to_string(&cache) {
            Ok(pre) => json!({ "type": "preprocessed", "preprocessed_player": pre, "requests": requests }),
            Err(_) => json!({ "type": "player", "player": job.player_js.as_str(), "requests": requests, "output_preprocessed": true }),
        };
        let out: String = self.ctx.with(|ctx| {
            // Pass the input as a JS value (not spliced into source) so nothing in it is executed as code.
            let globals = ctx.globals();
            let parsed = ctx.json_parse(input.to_string()).map_err(|e| anyhow!("json_parse: {e}"))?;
            globals.set("__jscInput", parsed).map_err(|e| anyhow!("{e}"))?;
            let r = ctx.eval::<String, _>("JSON.stringify(jsc(globalThis.__jscInput))").map_err(|e| {
                let msg = ctx.catch().as_exception().and_then(|x| x.message()).unwrap_or_default();
                anyhow!("solver failed: {e} {msg}")
            });
            let _ = globals.remove("__jscInput");
            r
        })?;
        let v: Value = serde_json::from_str(&out)?;
        if v["type"] == "error" {
            let _ = fs::remove_file(&cache); // a stale cache could be the reason
            bail!("solver error: {}", v["error"]);
        }
        if let Some(pre) = v["preprocessed_player"].as_str() {
            let _ = fs::create_dir_all(cache_dir);
            let _ = fs::write(&cache, pre);
        }
        let responses = v["responses"].as_array().cloned().unwrap_or_default();
        let mut it = responses.iter();
        let mut answer = |challenge: &Option<String>| -> Result<Option<String>> {
            let Some(c) = challenge else { return Ok(None) };
            let r = it.next().ok_or_else(|| anyhow!("solver returned too few answers"))?;
            if r["type"] == "error" {
                bail!("solver: {}", r["error"]);
            }
            Ok(Some(r["data"][c].as_str().ok_or_else(|| anyhow!("no answer for challenge"))?.to_string()))
        };
        let sig = answer(&job.sig)?;
        let n = answer(&job.n)?;
        Ok(Solved { sig, n })
    }
}

#[cfg(test)]
mod mem {
    use super::*;

    fn rss_mb() -> f64 {
        let s = std::fs::read_to_string("/proc/self/status").unwrap();
        s.lines().find(|l| l.starts_with("VmRSS")).unwrap().split_whitespace().nth(1).unwrap().parse::<f64>().unwrap() / 1024.0
    }

    // Measures solver memory using the cached preprocessed player: cargo test -p ytm-native --release -- --ignored solver_memory --nocapture
    #[test]
    #[ignore]
    fn solver_memory() {
        let dir = crate::paths::cache_dir();
        let pre = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path())
            .find(|p| p.to_string_lossy().ends_with(".pre.js")).expect("no cached player");
        let id = pre.file_name().unwrap().to_string_lossy().trim_start_matches("player-").trim_end_matches(".pre.js").to_string();
        println!("start: {:.0} MB", rss_mb());
        let engine = Engine::new().unwrap();
        println!("engine loaded: {:.0} MB", rss_mb());
        for i in 0..6 {
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let job = Job { player_id: id.clone(), player_js: Arc::new(String::new()), sig: Some("AOq0QJ8wRAIgXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX".into()),
                            n: Some("abcdefghijklmnop".into()), reply };
            let t = std::time::Instant::now();
            let r = engine.solve(&dir, &job);
            let u = engine._rt.memory_usage();
            println!("solve {i}: {:?} in {:?}; RSS {:.0} MB; QuickJS heap {:.0} MB", r.is_ok(), t.elapsed(), rss_mb(), u.memory_used_size as f64 / 1048576.0);
        }
        engine._rt.run_gc();
        println!("after GC: RSS {:.0} MB; QuickJS heap {:.0} MB", rss_mb(), engine._rt.memory_usage().memory_used_size as f64 / 1048576.0);
        crate::paths::release_memory();
        println!("after malloc_trim: RSS {:.0} MB", rss_mb());
    }
}
