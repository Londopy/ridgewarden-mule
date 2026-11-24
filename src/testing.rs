//! A fake core on a local port, for tests of the loops that talk to one: it serves the
//! feed, takes appends, and keeps a blob table the test can change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ridgewarden_client::core::{BlobStatus, FeedItem};
use ridgewarden_client::util::{ms_to_rfc3339, now_ms};
use ridgewarden_client::{Author, Event};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::App;
use crate::config::Config;

pub fn key(name: &str) -> String {
    blake3::hash(name.as_bytes()).to_hex().to_string()
}

pub fn author(name: &str) -> Author {
    Author {
        call: name.to_ascii_uppercase(),
        key: key(name),
    }
}

/// An event as the core would hand it to an app, minus the signature the core checked.
pub fn event(by: &Author, seq: u64, kind: &str, payload: Value, ts: i64) -> Event {
    Event {
        id: blake3::hash(format!("{}{seq}{kind}{payload}", by.key).as_bytes())
            .to_hex()
            .to_string(),
        author: by.clone(),
        seq,
        prev: None,
        lamport: seq,
        wall_time: ms_to_rfc3339(ts),
        mono_time: 0,
        kind: kind.into(),
        payload,
        sig: String::new(),
        log: None,
    }
}

#[derive(Default)]
pub struct Core {
    pub me: Option<Author>,
    pub events: Vec<Event>,
    pub blobs: HashMap<String, BlobStatus>,
    /// what an export of a root writes
    pub files: HashMap<String, Vec<u8>>,
    pub wants: Vec<String>,
    pub unwants: Vec<String>,
    /// files published with blob/add: name, size and BLAKE3 of the content
    pub added: Vec<(String, u64, String)>,
    pub export_dir: PathBuf,
    /// answer blob lookups with a 503, like a core that is restarting
    pub broken: bool,
    seq: u64,
}

impl Core {
    pub fn appended(&self, kind: &str) -> Vec<&Event> {
        let me = self.me.as_ref().map(|a| a.key.clone());
        self.events
            .iter()
            .filter(|e| e.kind == kind && Some(&e.author.key) == me.as_ref())
            .collect()
    }
}

#[derive(Clone)]
pub struct FakeCore {
    pub url: String,
    pub state: Arc<Mutex<Core>>,
    /// shuts the server, as a core that is stopped closes its port
    stop: Arc<tokio::sync::Notify>,
}

type S = State<Arc<Mutex<Core>>>;

async fn whoami(State(s): S) -> Json<Value> {
    let me = s.lock().unwrap().me.clone().unwrap();
    Json(json!({"call": me.call, "key": me.key, "node": "tst-node"}))
}

#[derive(Deserialize)]
struct FeedQ {
    after: i64,
}

async fn feed(State(s): S, Query(q): Query<FeedQ>) -> Json<Value> {
    let s = s.lock().unwrap();
    let items: Vec<FeedItem> = s
        .events
        .iter()
        .enumerate()
        .filter(|(i, _)| *i as i64 > q.after)
        .map(|(i, e)| FeedItem {
            seq: i as i64,
            event: e.clone(),
            trusted: true,
            seen_ms: None,
        })
        .collect();
    Json(json!({"instance": "fake", "events": items}))
}

#[derive(Deserialize)]
struct AppendReq {
    kind: String,
    payload: Value,
}

async fn append(State(s): S, Json(r): Json<AppendReq>) -> Json<Event> {
    let mut s = s.lock().unwrap();
    s.seq += 1;
    let me = s.me.clone().unwrap();
    let e = event(&me, 1000 + s.seq, &r.kind, r.payload, now_ms());
    s.events.push(e.clone());
    Json(e)
}

#[derive(Deserialize)]
struct BlobsQ {
    root: String,
}

async fn blob(State(s): S, Query(q): Query<BlobsQ>) -> Response {
    let s = s.lock().unwrap();
    if s.broken {
        return (StatusCode::SERVICE_UNAVAILABLE, "restarting").into_response();
    }
    match s.blobs.get(&q.root) {
        Some(b) => Json(b.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, "no manifest for that blob").into_response(),
    }
}

#[derive(Deserialize)]
struct RootReq {
    root: String,
    name: Option<String>,
}

async fn want(State(s): S, Json(r): Json<RootReq>) -> Response {
    let mut s = s.lock().unwrap();
    let Some(b) = s.blobs.get_mut(&r.root) else {
        return (StatusCode::NOT_FOUND, "no manifest for that blob yet").into_response();
    };
    b.wanted = true;
    s.wants.push(r.root.clone());
    Json(json!({"wanted": r.root})).into_response()
}

async fn unwant(State(s): S, Json(r): Json<RootReq>) -> Json<Value> {
    let mut s = s.lock().unwrap();
    s.blobs.remove(&r.root);
    s.unwants.push(r.root);
    Json(json!({"freed": 1}))
}

#[derive(Deserialize)]
struct AddReq {
    file: String,
    name: String,
}

/// Publish a file from the import folder, which `app` puts next to the export folder.
async fn add(State(s): S, Json(r): Json<AddReq>) -> Response {
    let dir = s.lock().unwrap().export_dir.join("imports");
    let Ok(data) = std::fs::read(dir.join(&r.file)) else {
        return (StatusCode::NOT_FOUND, "no such file in the import folder").into_response();
    };
    let hash = blake3::hash(&data).to_hex().to_string();
    s.lock()
        .unwrap()
        .added
        .push((r.name, data.len() as u64, hash.clone()));
    Json(json!({"root": hash})).into_response()
}

async fn export(State(s): S, Json(r): Json<RootReq>) -> Response {
    let s = s.lock().unwrap();
    let (Some(data), Some(name)) = (s.files.get(&r.root), r.name) else {
        return (StatusCode::CONFLICT, "blob is not complete").into_response();
    };
    let p = s.export_dir.join(name);
    std::fs::write(&p, data).unwrap();
    Json(json!({"exported": p})).into_response()
}

#[derive(Deserialize)]
struct EventsQ {
    author: String,
    seq: u64,
}

async fn events(State(s): S, Query(q): Query<EventsQ>) -> Json<Vec<Event>> {
    let s = s.lock().unwrap();
    Json(
        s.events
            .iter()
            .filter(|e| e.author.key == q.author && e.seq == q.seq)
            .cloned()
            .collect(),
    )
}

impl FakeCore {
    pub async fn start(name: &str, export_dir: &Path) -> FakeCore {
        let state = Arc::new(Mutex::new(Core {
            me: Some(author(name)),
            export_dir: export_dir.to_path_buf(),
            ..Default::default()
        }));
        let app = Router::new()
            .route("/v1/whoami", get(whoami))
            .route("/v1/feed", get(feed))
            .route("/v1/blobs", get(blob))
            .route("/v1/events", get(events))
            .route("/local/v1/append", post(append))
            .route("/local/v1/blob/want", post(want))
            .route("/local/v1/blob/unwant", post(unwant))
            .route("/local/v1/blob/export", post(export))
            .route("/local/v1/blob/add", post(add))
            .with_state(state.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let stop = Arc::new(tokio::sync::Notify::new());
        let quit = stop.clone();
        tokio::spawn(async move {
            axum::serve(l, app)
                .with_graceful_shutdown(async move { quit.notified().await })
                .await
                .unwrap()
        });
        FakeCore { url, state, stop }
    }

    /// The address of a core that isn't running: nothing listens there.
    pub fn closed() -> FakeCore {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        FakeCore {
            url: format!("http://{}", l.local_addr().unwrap()),
            state: Default::default(),
            stop: Default::default(),
        }
    }

    /// Stop answering for good, the way a core that was shut down does.
    pub fn stop(&self) {
        self.stop.notify_one();
    }

    pub fn key(&self) -> String {
        self.state.lock().unwrap().me.as_ref().unwrap().key.clone()
    }

    pub fn add(&self, e: Event) {
        self.state.lock().unwrap().events.push(e);
    }

    /// A blob the core knows of, complete or not.
    pub fn blob(&self, root: &str, size: u64, complete: bool) {
        self.state.lock().unwrap().blobs.insert(
            root.to_string(),
            BlobStatus {
                root: root.to_string(),
                name: "x".into(),
                size,
                complete,
                wanted: false,
                priority: 5,
                author: String::new(),
                trusted: true,
            },
        );
    }
}

/// An app on a temp dir talking to `core`, with `extra` appended to its config.
pub fn app(dir: &Path, core: &FakeCore, extra: &str) -> App {
    let token = dir.join("local.token");
    std::fs::write(&token, "t".repeat(64)).unwrap();
    let text = format!(
        "data_dir = {:?}\n{extra}\n[core]\nurl = {:?}\ntoken_file = {:?}\nimports_dir = {:?}\n",
        dir.join("app").display().to_string(),
        core.url,
        token.display().to_string(),
        dir.join("imports").display().to_string(),
    );
    let mut cfg: Config = toml::from_str(&text).unwrap();
    cfg.operator = cfg.operator.to_ascii_uppercase();
    cfg.validate().unwrap();
    App::new(cfg).unwrap()
}

/// A message from `by` and the manifest for its file, as the sending core logs them.
pub fn file_msg(
    by: &Author,
    seq: u64,
    class: &str,
    data: &[u8],
    ts: i64,
) -> (Event, Event, String) {
    let mut ids = blake3::Hasher::new();
    for c in data.chunks(1 << 20) {
        ids.update(blake3::hash(c).as_bytes());
    }
    let root = ids.finalize().to_hex().to_string();
    let size = data.len() as u64;
    let man = event(
        by,
        seq,
        "blob.manifest",
        json!({"root": root, "size": size, "name": "ridgewarden-mule/tiles.bin"}),
        ts,
    );
    let msg = event(
        by,
        seq + 1,
        "mule.msg",
        json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": class,
               "text": "map tiles", "file": {"root": root, "name": "tiles.bin", "size": size}}),
        ts,
    );
    (man, msg, root)
}

pub fn text_msg(by: &Author, seq: u64, class: &str, ts: i64) -> Event {
    event(
        by,
        seq,
        "mule.msg",
        json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": class,
               "text": "cots and water needed"}),
        ts,
    )
}
