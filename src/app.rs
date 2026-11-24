//! The running app: its config, its database, and its line to the core.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Bytes;
use ridgewarden_client::core::{Projection, Whoami};
use ridgewarden_client::util::human_duration;
use ridgewarden_client::{CoreClient, open_db};
use rusqlite::Connection;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::mule::{Mule, View};
use crate::plan::Plan;
use crate::trips::Trips;

pub const PROJECTIONS: &[&dyn Projection] = &[&Mule, &Trips];

pub const DOWNLOADS: usize = 4;

pub struct Address {
    pub last: Option<Instant>,
    pub open: usize,
}

/// How long a post may take: in all before a file part has shown the key (which is all of
/// a small form), and between pieces after that.
#[derive(Clone, Copy)]
pub struct PostWaits {
    pub before_key: Duration,
    pub idle: Duration,
}

/// With no good read of the core for this long, what the pages show is out of date.
const STALE_MS: i64 = 2 * 60_000;

/// How the app's reads of its core's feed have gone: when one last worked, and why the
/// latest failed, if it did.
#[derive(Default)]
struct Reads {
    ok_ms: Option<i64>,
    err: Option<String>,
}

/// This site's own state, never rebuilt from the feed.
const LOCAL: &str = "
CREATE TABLE IF NOT EXISTS car_trip (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    trip      TEXT NOT NULL,
    depart_ms INTEGER NOT NULL,
    chosen_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS wanted (
    root TEXT PRIMARY KEY,
    ms   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS bundles (
    event TEXT NOT NULL,
    dir   TEXT NOT NULL,
    eid   TEXT NOT NULL,
    body  TEXT,
    ms    INTEGER NOT NULL,
    PRIMARY KEY (event, dir)
);
CREATE TABLE IF NOT EXISTS inbox (
    id   INTEGER PRIMARY KEY,
    body BLOB NOT NULL,
    ms   INTEGER NOT NULL
);";

pub struct App {
    pub cfg: Config,
    pub core: CoreClient,
    pub db: Mutex<Connection>,
    /// for asking cores (ours and, on a car, the islands') what they hold
    pub http: reqwest::Client,
    /// what the send form asks for; None if the key file couldn't be read or made
    pub send_key: Option<String>,
    /// each address's last post and how many it has open, to slow down key guessing
    pub posts: Mutex<HashMap<IpAddr, Address>>,
    pub post_waits: PostWaits,
    /// a download that takes nothing for this long gives its slot back
    pub download_stall: Duration,
    /// send-form posts that may carry a file, at once; each holds a disk write open
    pub uploads: Semaphore,
    /// the other forms' posts at once
    pub forms: Semaphore,
    /// file downloads streaming at once, so a few slow clients can't use every disk read
    pub downloads: Arc<Semaphore>,
    /// whether dtnd has our endpoint registered since this app started
    pub dtnd_registered: AtomicBool,
    /// the latest /api/mule answer and when it was made, so a burst of readers costs one
    pub last_json: tokio::sync::Mutex<Option<(Instant, Bytes)>>,
    who: Mutex<Option<Whoami>>,
    reads: Mutex<Reads>,
}

impl App {
    pub fn new(cfg: Config) -> Result<Self> {
        let token = cfg
            .core
            .token_file
            .exists()
            .then_some(cfg.core.token_file.as_path());
        if token.is_none() {
            tracing::warn!(
                "core token {} not found; this app can show messages but not send, carry or receipt them",
                cfg.core.token_file.display()
            );
        }
        let core = CoreClient::new(&cfg.core.url, token)?;
        let db = open_db(&cfg.db_path(), PROJECTIONS)?;
        db.execute_batch(LOCAL)?;
        crate::mule::set_pins(&db, &cfg.pins(None), &cfg.car_keys(None))?;
        let send_key = match load_or_make_key(&cfg) {
            Ok(k) => Some(k),
            Err(e) => {
                tracing::warn!("the send form is off: {e:#}");
                None
            }
        };
        Ok(Self {
            core,
            db: Mutex::new(db),
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .user_agent(concat!("ridgewarden-mule/", env!("CARGO_PKG_VERSION")))
                .build()?,
            send_key,
            posts: Mutex::new(HashMap::new()),
            download_stall: Duration::from_secs(120),
            post_waits: PostWaits {
                before_key: Duration::from_secs(30),
                idle: Duration::from_secs(60),
            },
            uploads: Semaphore::new(2),
            forms: Semaphore::new(8),
            downloads: Arc::new(Semaphore::new(DOWNLOADS)),
            dtnd_registered: AtomicBool::new(false),
            last_json: tokio::sync::Mutex::new(None),
            who: Mutex::new(None),
            reads: Mutex::new(Reads::default()),
            cfg,
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The core's callsign and key, cached once known.
    pub async fn whoami(&self) -> Result<Whoami> {
        if let Some(w) = self.who.lock().unwrap().clone() {
            return Ok(w);
        }
        let w = self.core.whoami().await?;
        *self.who.lock().unwrap() = Some(w.clone());
        Ok(w)
    }

    pub fn whoami_cached(&self) -> Option<Whoami> {
        self.who.lock().unwrap().clone()
    }

    /// Pin the islands' keys, this core's own included once it is known.
    pub async fn pin(&self) -> Result<Whoami> {
        let me = self.whoami().await?;
        crate::mule::set_pins(
            &self.db(),
            &self.cfg.pins(Some(&me.key)),
            &self.cfg.car_keys(Some(&me.key)),
        )?;
        Ok(me)
    }

    /// Note how a read of the core's feed went: None when it worked, else why not.
    pub fn read_core(&self, failed: Option<String>, now: i64) {
        let mut reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
        if failed.is_none() {
            reads.ok_ms = Some(now);
        }
        reads.err = failed;
    }

    /// What is wrong with the app's line to its core, as a sentence: the latest read of its
    /// feed failed, or none has worked for a while. None when all is well, or before the
    /// first read.
    pub fn core_trouble(&self, now: i64) -> Option<String> {
        let reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
        match (&reads.err, reads.ok_ms) {
            (Some(e), Some(t)) => Some(format!(
                "Can't read the core: {e}. The last good read was {} ago.",
                human_duration(now - t)
            )),
            (Some(e), None) => Some(format!(
                "Can't read the core: {e}. No read has worked since the app started."
            )),
            (None, Some(t)) if now - t > STALE_MS => Some(format!(
                "The core was last read {} ago.",
                human_duration(now - t)
            )),
            _ => None,
        }
    }

    /// Messages still under way.
    pub fn active(&self) -> Result<Vec<View>> {
        crate::mule::active(&self.db(), self.cfg.third_party)
    }

    /// What goes with whom, as of `now`.
    pub fn plan(&self, now: i64) -> Result<Plan> {
        let db = self.db();
        let views = crate::mule::active(&db, self.cfg.third_party)?;
        let cargo = crate::plan::cargo(&views);
        Ok(crate::plan::plan(cargo, &crate::trips::trips(&db)?, now))
    }
}

fn load_or_make_key(cfg: &Config) -> Result<String> {
    let p = cfg.key_path();
    if let Ok(k) = std::fs::read_to_string(&p)
        && k.trim().len() >= 12
    {
        return Ok(k.trim().to_string());
    }
    let mut b = [0u8; 9];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    let k = hex::encode(b);
    std::fs::create_dir_all(&cfg.data_dir)?;
    write_private(&p, &k).with_context(|| format!("writing {}", p.display()))?;
    tracing::info!("made a send key in {}", p.display());
    Ok(k)
}

#[cfg(unix)]
fn write_private(p: &std::path::Path, s: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(p)?;
    f.write_all(s.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(p: &std::path::Path, s: &str) -> Result<()> {
    std::fs::write(p, s)?;
    Ok(())
}

/// A failed request in a few words for pages and logs, without its URL.
pub fn short_err(e: anyhow::Error) -> String {
    let e = match e.downcast::<reqwest::Error>() {
        Ok(r) if r.is_timeout() => return "timed out".into(),
        Ok(r) if r.is_connect() => return "could not connect".into(),
        Ok(r) => anyhow::Error::new(r.without_url()),
        Err(e) => e,
    };
    format!("{e:#}").chars().take(200).collect()
}
