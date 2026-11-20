//! An optional bridge to a Bundle Protocol 7 network, through dtnd from dtn7-rs running on
//! the same box. BPv7 is only ever the wire format across the bridge; inside the mesh,
//! everything stays ordinary signed events.
//!
//! dtnd's HTTP interface (localhost only, port 3000 by default):
//!   GET  /register?<endpoint>               start receiving on dtn://<node>/<endpoint>
//!   POST /send?dst=<eid>&lifetime=<dur>     the body becomes the bundle's payload
//!   GET  /endpoint?<endpoint>               the next bundle, CBOR-encoded, or "Nothing to receive"
//!
//! Out: a text message for a routed island leaves as a bundle whose payload is the signed
//! `mule.msg` event, untouched. In: a bundle's payload must be a signed event whose
//! signature checks and whose author is pinned for the island it claims; the gateway's
//! core then re-publishes it here, naming the original. Receipts come back the same way.
//! Files stay on the mesh: a bundle would carry a blob id the far side can't fetch.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use ridgewarden_client::Event;
use ridgewarden_client::util::now_ms;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::app::App;
use crate::cbor::bundle_payload;
use crate::config::Bpv7Cfg;
use crate::mule::{self, State, id_field, island, parse_msg};

/// Bundles read from dtnd per round, so a flood can't hold up the rest of the round.
const MAX_IN: usize = 50;
/// A signed message of at most 4000 characters is a few kilobytes; nothing real is bigger.
const MAX_BUNDLE: usize = 64 * 1024;

pub struct Dtnd {
    http: reqwest::Client,
    base: String,
    endpoint: String,
    lifetime: String,
}

impl Dtnd {
    pub fn new(cfg: &Bpv7Cfg) -> Result<Dtnd> {
        Ok(Dtnd {
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(30))
                .build()?,
            base: cfg.url.trim_end_matches('/').to_string(),
            endpoint: cfg.endpoint.clone(),
            lifetime: cfg.lifetime.clone(),
        })
    }

    pub async fn register(&self) -> Result<()> {
        let r = self
            .http
            .get(format!("{}/register?{}", self.base, self.endpoint))
            .send()
            .await?;
        ensure!(
            r.status().is_success(),
            "dtnd refused to register {}: {}",
            self.endpoint,
            r.text().await.unwrap_or_default().trim()
        );
        Ok(())
    }

    pub async fn send(&self, eid: &str, payload: Vec<u8>) -> Result<()> {
        let r = self
            .http
            .post(format!("{}/send", self.base))
            .query(&[("dst", eid), ("lifetime", self.lifetime.as_str())])
            .body(payload)
            .send()
            .await?;
        ensure!(
            r.status().is_success(),
            "dtnd refused a bundle for {eid}: {}",
            r.text().await.unwrap_or_default().trim()
        );
        Ok(())
    }

    /// The next bundle for our endpoint, CBOR-encoded.
    pub async fn next(&self) -> Result<Option<Vec<u8>>> {
        let r = self
            .http
            .get(format!("{}/endpoint?{}", self.base, self.endpoint))
            .send()
            .await?
            .error_for_status()?;
        let b = r.bytes().await?;
        Ok((b.as_ref() != b"Nothing to receive").then(|| b.to_vec()))
    }
}

pub async fn round(app: &App, here: &str, d: &Dtnd) -> Result<()> {
    if !app.dtnd_registered.load(Ordering::Relaxed) {
        d.register().await?;
        app.dtnd_registered.store(true, Ordering::Relaxed);
    }
    for _ in 0..MAX_IN {
        let b = match d.next().await {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(e) => {
                // dtnd restarted and forgot our endpoint, most likely
                app.dtnd_registered.store(false, Ordering::Relaxed);
                return Err(e);
            }
        };
        if b.len() > MAX_BUNDLE {
            tracing::warn!(
                "bundle from dtnd refused: {} bytes is too big for a message",
                b.len()
            );
            continue;
        }
        // dtnd hands a bundle over once; keep it until it has been dealt with
        app.db().execute(
            "INSERT INTO inbox (body, ms) VALUES (?1, ?2)",
            params![b, now_ms()],
        )?;
    }
    take_in_all(app, here).await?;
    send_messages(app, d).await?;
    send_receipts(app, here, d).await
}

/// Check every bundle waiting in the inbox and re-publish what passes. A bundle stays
/// only if publishing it failed, so it is tried again next round.
async fn take_in_all(app: &App, here: &str) -> Result<()> {
    let waiting: Vec<(i64, Vec<u8>)> = app
        .db()
        .prepare("SELECT id, body FROM inbox ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (id, body) in waiting {
        match check(app, here, &body) {
            Err(e) => tracing::warn!("bundle from dtnd refused: {e:#}"),
            Ok(None) => {}
            Ok(Some(t)) => {
                let local = app.core.append(t.kind, t.payload).await?;
                note(app, &t.orig, "in", "")?;
                tracing::info!(
                    "{} {} came in as a bundle; here it is {}",
                    t.kind,
                    &t.orig[..12],
                    &local.id[..12]
                );
            }
        }
        app.db()
            .execute("DELETE FROM inbox WHERE id = ?1", params![id])?;
    }
    Ok(())
}

fn seen(app: &App, event: &str, dir: &str) -> Result<bool> {
    Ok(app
        .db()
        .query_row(
            "SELECT 1 FROM bundles WHERE event = ?1 AND dir = ?2",
            params![event, dir],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn note(app: &App, event: &str, dir: &str, eid: &str) -> Result<()> {
    app.db().execute(
        "INSERT OR IGNORE INTO bundles (event, dir, eid, body, ms) VALUES (?1, ?2, ?3, NULL, ?4)",
        params![event, dir, eid, now_ms()],
    )?;
    Ok(())
}

/// When and where a message left as a bundle, for its page.
pub fn sent_as(app: &App, event: &str) -> Result<Option<(String, i64)>> {
    Ok(app
        .db()
        .query_row(
            "SELECT eid, ms FROM bundles WHERE event = ?1 AND dir = 'out'",
            params![event],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// What a bundle that passed every check asks this site to publish.
pub struct Taken {
    pub orig: String,
    pub kind: &'static str,
    pub payload: Value,
}

/// Check one bundle: a signed event of a kind that crosses, from a key pinned for the
/// island it claims. None if it was already taken in.
pub fn check(app: &App, here: &str, bundle: &[u8]) -> Result<Option<Taken>> {
    let ev: Event = serde_json::from_slice(&bundle_payload(bundle)?)?;
    ev.verify()?;
    ensure!(ev.log.is_none(), "telemetry doesn't cross the bridge");
    if seen(app, &ev.id, "in")? {
        return Ok(None);
    }
    let relay = json!({"id": ev.id, "call": ev.author.call});
    let (kind, payload) = match ev.kind.as_str() {
        "mule.msg" => {
            let Some(m) = parse_msg(&ev, now_ms()) else {
                bail!("malformed message {}", &ev.id[..12]);
            };
            ensure!(
                m.to_island == here,
                "message is for {}, not {here}",
                m.to_island
            );
            ensure!(
                app.cfg
                    .island_keys(&m.from_island, None)
                    .contains(&ev.author.key),
                "{} is not pinned for {}",
                ev.author.call,
                m.from_island
            );
            ensure!(m.file.is_none(), "files don't cross the bridge");
            ensure!(
                m.on_behalf.is_none() || app.cfg.third_party,
                "third-party traffic is off here"
            );
            if mule::relayed(&app.db(), &ev.id)?.is_some() {
                return Ok(None);
            }
            let mut p = ev.payload.clone();
            p["relay"] = relay;
            ("mule.msg", p)
        }
        "mule.receipt" => {
            let p = &ev.payload;
            let (Some(msg), Some(at)) = (id_field(p, "msg"), island(p, "island")) else {
                bail!("malformed receipt {}", &ev.id[..12]);
            };
            ensure!(
                app.cfg.island_keys(&at, None).contains(&ev.author.key),
                "{} is not pinned for {at}",
                ev.author.call
            );
            (
                "mule.receipt",
                json!({"msg": msg, "island": at, "relay": relay}),
            )
        }
        k => bail!("{k} events don't cross the bridge"),
    };
    Ok(Some(Taken {
        orig: ev.id,
        kind,
        payload,
    }))
}

/// Text messages for routed islands that haven't left yet.
async fn send_messages(app: &App, d: &Dtnd) -> Result<()> {
    for v in app.active()? {
        let m = &v.msg;
        let Some(eid) = app.cfg.route(&m.to_island) else {
            continue;
        };
        if m.relay.is_some() || m.file.is_some() || seen(app, &m.id, "out")? {
            continue;
        }
        let Some(raw) = mule::raw(&app.db(), &m.id)? else {
            continue;
        };
        d.send(eid, raw.into_bytes()).await?;
        note(app, &m.id, "out", eid)?;
        tracing::info!(
            "sent {} to {} as a bundle for {eid}",
            &m.id[..12],
            m.to_island
        );
    }
    Ok(())
}

/// Receipts for messages that came in over the bridge and have been delivered here. Each
/// is signed once and kept until dtnd takes it.
async fn send_receipts(app: &App, here: &str, d: &Dtnd) -> Result<()> {
    let ids: Vec<String> = app
        .db()
        .prepare(
            "SELECT id FROM msgs WHERE relay_id IS NOT NULL
             AND relay_id NOT IN (SELECT event FROM bundles WHERE dir = 'receipt')",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for id in ids {
        let Some(v) = mule::find(&app.db(), app.cfg.third_party, &id)? else {
            continue;
        };
        let (Some(orig), Some(eid)) = (&v.msg.relay, app.cfg.route(&v.msg.from_island)) else {
            continue;
        };
        if v.state != State::Delivered {
            continue;
        }
        let r = app
            .core
            .append("mule.receipt", json!({"msg": orig.id, "island": here}))
            .await?;
        app.db().execute(
            "INSERT OR IGNORE INTO bundles (event, dir, eid, body, ms) VALUES (?1, 'receipt', ?2, ?3, 0)",
            params![orig.id, eid, serde_json::to_string(&r)?],
        )?;
    }
    let unsent: Vec<(String, String, String)> = app
        .db()
        .prepare("SELECT event, eid, body FROM bundles WHERE dir = 'receipt' AND ms = 0")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    for (orig, eid, body) in unsent {
        d.send(&eid, body.into_bytes()).await?;
        app.db().execute(
            "UPDATE bundles SET ms = ?2 WHERE event = ?1 AND dir = 'receipt'",
            params![orig, now_ms()],
        )?;
        tracing::info!("receipt for {} sent back as a bundle", &orig[..12]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::cbor::testing::bundle;
    use crate::testing::{FakeCore, author, event};
    use axum::Router;
    use axum::extract::{Query, RawQuery, State};
    use axum::routing::{get, post};
    use ridgewarden_client::mesh::testing::Fake;

    #[derive(Default)]
    struct Daemon {
        registered: Vec<String>,
        sent: Vec<(String, Vec<u8>)>,
        inbox: VecDeque<Vec<u8>>,
    }

    type D = State<Arc<Mutex<Daemon>>>;

    async fn register(State(d): D, RawQuery(q): RawQuery) -> String {
        let q = q.unwrap_or_default();
        d.lock().unwrap().registered.push(q.clone());
        format!("Registered dtn://tst-north-gw/{q}")
    }

    async fn send(
        State(d): D,
        Query(q): Query<HashMap<String, String>>,
        body: axum::body::Bytes,
    ) -> String {
        let n = body.len();
        d.lock()
            .unwrap()
            .sent
            .push((q["dst"].clone(), body.to_vec()));
        format!("Sent ADU in bundle dtn://tst-north-gw/-1-0 with {n} bytes")
    }

    async fn endpoint(State(d): D) -> Vec<u8> {
        d.lock()
            .unwrap()
            .inbox
            .pop_front()
            .unwrap_or_else(|| b"Nothing to receive".to_vec())
    }

    async fn dtnd() -> (String, Arc<Mutex<Daemon>>) {
        let d = Arc::new(Mutex::new(Daemon::default()));
        let app = Router::new()
            .route("/register", get(register))
            .route("/send", post(send))
            .route("/endpoint", get(endpoint))
            .with_state(d.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (url, d)
    }

    async fn step(app: &App, gw: &Dtnd) {
        app.pin().await.unwrap();
        ridgewarden_client::follow(&app.core, &app.db, crate::app::PROJECTIONS)
            .await
            .unwrap();
        round(app, "north", gw).await.unwrap();
        ridgewarden_client::follow(&app.core, &app.db, crate::app::PROJECTIONS)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn messages_and_receipts_cross_the_bridge_and_forgeries_do_not() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0gw", d.path()).await;
        let (url, daemon) = dtnd().await;
        let mut far = Fake::new(vec![]);
        let good = far.add(
            "mule.msg",
            json!({"from_island": "east", "to": "north", "from": "N0EEE", "class": "routine", "text": "all well"}),
            now_ms(),
        );
        let mut forged = good.clone();
        forged.payload["text"] = json!("send money");
        let local = author("n0north");
        let cfg = format!(
            "island = \"north\"\n[[islands]]\nname = \"north\"\nkeys = [\"{}\"]\n\
             [[islands]]\nname = \"east\"\nkeys = [\"{}\"]\n\
             [bpv7]\nurl = \"{url}\"\n[[bpv7.route]]\nisland = \"east\"\neid = \"dtn://tst-east-gw/mule\"",
            local.key, good.author.key
        );
        let app = crate::testing::app(d.path(), &core, &cfg);
        let out = event(
            &local,
            1,
            "mule.msg",
            json!({"from_island": "north", "to": "east", "from": "N0AAA", "class": "urgent", "text": "need a generator"}),
            now_ms(),
        );
        core.add(out.clone());
        for e in [&forged, &good, &good] {
            daemon
                .lock()
                .unwrap()
                .inbox
                .push_back(bundle(&serde_json::to_vec(e).unwrap()));
        }
        let gw = Dtnd::new(app.cfg.bpv7.as_ref().unwrap()).unwrap();
        step(&app, &gw).await;
        assert_eq!(daemon.lock().unwrap().registered, ["mule"]);
        let relayed: Vec<Value> = core
            .state
            .lock()
            .unwrap()
            .appended("mule.msg")
            .iter()
            .map(|e| e.payload.clone())
            .collect();
        assert_eq!(
            relayed.len(),
            1,
            "the forgery is refused and the repeat ignored"
        );
        assert_eq!(relayed[0]["relay"]["id"], json!(good.id));
        assert_eq!(relayed[0]["text"], json!("all well"));
        {
            let sent = &daemon.lock().unwrap().sent;
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].0, "dtn://tst-east-gw/mule");
            let ev: Event = serde_json::from_slice(&sent[0].1).unwrap();
            assert_eq!(ev, out, "the signed event leaves untouched");
        }
        // a site here receipts the relayed copy; a receipt goes back for the original
        let copy = core.state.lock().unwrap().appended("mule.msg")[0]
            .id
            .clone();
        core.add(event(
            &local,
            2,
            "mule.receipt",
            json!({"msg": copy, "island": "north"}),
            now_ms(),
        ));
        step(&app, &gw).await;
        let sent = &daemon.lock().unwrap().sent;
        assert_eq!(sent.len(), 2);
        let r: Event = serde_json::from_slice(&sent[1].1).unwrap();
        assert_eq!(
            (r.kind.as_str(), &r.payload["msg"]),
            ("mule.receipt", &json!(good.id))
        );
    }
}
