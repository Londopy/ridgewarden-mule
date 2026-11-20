//! The running app: its config, its database, and its line to the core.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ridgewarden_client::core::{Projection, Whoami};
use ridgewarden_client::{CoreClient, open_db};
use rusqlite::Connection;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::mule::{Mule, View};
use crate::plan::Plan;
use crate::trips::Trips;

pub const PROJECTIONS: &[&dyn Projection] = &[&Mule, &Trips];

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
    /// last send-form post per address, to slow down key guessing
    pub posts: Mutex<HashMap<IpAddr, Instant>>,
    /// uploads read into memory at once; each can be the form's whole size limit
    pub uploads: Semaphore,
    /// file downloads read into memory at once
    pub downloads: Semaphore,
    /// whether dtnd has our endpoint registered since this app started
    pub dtnd_registered: AtomicBool,
    who: Mutex<Option<Whoami>>,
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
            uploads: Semaphore::new(2),
            downloads: Semaphore::new(2),
            dtnd_registered: AtomicBool::new(false),
            who: Mutex::new(None),
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

pub fn short_err(e: &anyhow::Error) -> String {
    if let Some(r) = e.downcast_ref::<reqwest::Error>() {
        if r.is_timeout() {
            return "timed out".into();
        }
        if r.is_connect() {
            return "could not connect".into();
        }
    }
    format!("{e:#}").chars().take(200).collect()
}
