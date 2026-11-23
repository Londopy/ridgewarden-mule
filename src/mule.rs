//! Messages, their trip, and receipts, built from the core's event feed.
//!
//!   mule.msg       a message for another island, maybe with a file (signed by the sending site)
//!   mule.withdraw  the sending site takes a message back before it is delivered
//!   mule.pickup    a car has the message (and its file) aboard
//!   mule.dropoff   a pinned core on the destination island holds what the car carried
//!   mule.receipt   the destination island has the message, its file checked against the blob id

use anyhow::Result;
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::event::valid_call;
use ridgewarden_client::ui::Color;
use ridgewarden_client::util::{median, valid_node};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

pub const BLOB_PREFIX: &str = "ridgewarden-mule/";
/// ridgewarden-library's updates: the library app takes the file from the core itself.
pub const LIBRARY_APP: &str = "ridgewarden-library";
pub const MAX_TEXT: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    Urgent,
    Routine,
    Bulk,
}

impl Class {
    pub const ALL: [Class; 3] = [Class::Urgent, Class::Routine, Class::Bulk];

    pub fn parse(s: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|c| c.name() == s)
    }

    pub fn name(self) -> &'static str {
        match self {
            Class::Urgent => "urgent",
            Class::Routine => "routine",
            Class::Bulk => "bulk",
        }
    }

    /// The core fetches blobs lowest number first, so an urgent file goes before bulk.
    pub fn blob_priority(self) -> u8 {
        match self {
            Class::Urgent => 0,
            Class::Routine => 4,
            Class::Bulk => 8,
        }
    }

    /// Urgent stays short so it never waits behind a big file; bulk is for map tiles and the like.
    pub fn max_file(self) -> u64 {
        match self {
            Class::Urgent => 1_000_000,
            Class::Routine => 50_000_000,
            Class::Bulk => 1_000_000_000_000,
        }
    }

    pub fn color(self) -> Color {
        match self {
            Class::Urgent => Color::Red,
            Class::Routine => Color::Yellow,
            Class::Bulk => Color::Unknown,
        }
    }

    pub fn from_i64(n: i64) -> Class {
        Class::ALL[n.clamp(0, 2) as usize]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileRef {
    pub root: String,
    pub name: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct Msg {
    pub id: String,
    pub author: String,
    pub author_call: String,
    pub seq: u64,
    pub from_island: String,
    pub to_island: String,
    pub from_call: String,
    pub to_call: Option<String>,
    pub class: Class,
    pub text: String,
    pub file: Option<FileRef>,
    /// third-party traffic: the person without a licence it was written for
    pub on_behalf: Option<String>,
    /// set when a BPv7 gateway re-published a message that arrived as a bundle
    pub relay: Option<Relay>,
    /// the app that wrote it, when not a person through this one
    pub app: Option<String>,
    pub sent_ms: i64,
}

impl Msg {
    pub fn is_library(&self) -> bool {
        self.app.as_deref() == Some(LIBRARY_APP)
    }
}

/// The original message behind a gateway's copy.
#[derive(Clone, Debug, PartialEq)]
pub struct Relay {
    pub id: String,
    pub call: String,
}

#[derive(Clone, Debug)]
pub struct Hop {
    pub car: String,
    pub car_call: String,
    pub operator: String,
    pub island: Option<String>,
    pub trip: Option<String>,
    pub ms: i64,
}

#[derive(Clone, Debug)]
pub struct Receipt {
    pub site_call: String,
    pub island: String,
    pub ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Waiting,
    InTransit,
    HandedOver,
    Delivered,
    Withdrawn,
}

impl State {
    fn code(self) -> i64 {
        match self {
            State::Waiting => 0,
            State::InTransit => 1,
            State::HandedOver => 2,
            State::Delivered => 3,
            State::Withdrawn => 4,
        }
    }

    fn from_code(n: i64) -> State {
        match n {
            1 => State::InTransit,
            2 => State::HandedOver,
            3 => State::Delivered,
            4 => State::Withdrawn,
            _ => State::Waiting,
        }
    }
}

#[derive(Clone, Debug)]
pub struct View {
    pub msg: Msg,
    pub state: State,
    pub pickups: Vec<Hop>,
    pub dropoffs: Vec<Hop>,
    /// the first receipt from a key pinned for the destination island
    pub receipt: Option<Receipt>,
    /// signed by a key pinned for the island it says it comes from; nothing else is
    /// carried or delivered
    pub pinned: bool,
}

impl View {
    pub fn corridor_ms(&self) -> Option<i64> {
        self.receipt.as_ref().map(|r| r.ms - self.msg.sent_ms)
    }

    /// Pickup to drop-off by one car, both by that car's own clock.
    pub fn car_leg_ms(&self) -> Option<i64> {
        self.pickups.iter().find_map(|p| {
            self.dropoffs
                .iter()
                .find(|d| d.car == p.car)
                .map(|d| d.ms - p.ms)
        })
    }
}

pub fn is_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A trimmed, non-empty string field of at most `max` characters.
pub fn field(p: &Value, k: &str, max: usize) -> Option<String> {
    let s = p.get(k)?.as_str()?.trim();
    (!s.is_empty() && s.chars().count() <= max).then(|| s.to_string())
}

pub fn call(p: &Value, k: &str) -> Option<String> {
    field(p, k, 20).filter(|c| valid_call(c))
}

pub fn island(p: &Value, k: &str) -> Option<String> {
    field(p, k, 63).filter(|i| valid_node(i))
}

pub fn id_field(p: &Value, k: &str) -> Option<String> {
    field(p, k, 64).filter(|m| is_id(m))
}

/// `Some(None)` when the field is absent, `None` when it is present but malformed.
fn optional<T>(p: &Value, k: &str, f: impl FnOnce(&Value) -> Option<T>) -> Option<Option<T>> {
    match p.get(k) {
        None => Some(None),
        Some(_) => f(p).map(Some),
    }
}

fn file(p: &Value) -> Option<FileRef> {
    let f = p.get("file")?;
    Some(FileRef {
        root: id_field(f, "root")?,
        name: field(f, "name", 200)?,
        size: f.get("size")?.as_u64().filter(|n| *n > 0)?,
    })
}

fn relay(p: &Value) -> Option<Relay> {
    let r = p.get("relay")?;
    Some(Relay {
        id: id_field(r, "id")?,
        call: call(r, "call")?,
    })
}

/// A message as it arrived from the mesh; None if anything in it is malformed or breaks the
/// rules a sending site enforces (class file caps, text length, a real destination).
pub fn parse_msg(e: &Event, ts: i64) -> Option<Msg> {
    let p = &e.payload;
    let from_island = island(p, "from_island")?;
    let to_island = island(p, "to")?;
    let class = Class::parse(p.get("class")?.as_str()?)?;
    let file = optional(p, "file", file)?;
    let text = optional(p, "text", |p| field(p, "text", MAX_TEXT))?.unwrap_or_default();
    if from_island == to_island
        || (text.is_empty() && file.is_none())
        || file.as_ref().is_some_and(|f| f.size > class.max_file())
    {
        return None;
    }
    Some(Msg {
        id: e.id.clone(),
        author: e.author.key.clone(),
        author_call: e.author.call.clone(),
        seq: e.seq,
        from_island,
        to_island,
        from_call: call(p, "from")?,
        to_call: optional(p, "to_call", |p| call(p, "to_call"))?,
        class,
        text,
        file,
        on_behalf: optional(p, "for", |p| field(p, "for", 100))?,
        relay: optional(p, "relay", relay)?,
        app: optional(p, "app", |p| field(p, "app", 40))?,
        sent_ms: ts,
    })
}

pub struct Mule;

impl Projection for Mule {
    fn schema(&self) -> &'static str {
        "CREATE TABLE IF NOT EXISTS msgs (
            id          TEXT PRIMARY KEY,
            author      TEXT NOT NULL,
            author_call TEXT NOT NULL,
            seq         INTEGER NOT NULL,
            from_island TEXT NOT NULL,
            to_island   TEXT NOT NULL,
            from_call   TEXT NOT NULL,
            to_call     TEXT,
            class       INTEGER NOT NULL,
            text        TEXT NOT NULL,
            file_root   TEXT,
            file_name   TEXT,
            file_size   INTEGER,
            on_behalf   TEXT,
            relay_id    TEXT,
            relay_call  TEXT,
            app         TEXT,
            sent_ms     INTEGER NOT NULL,
            raw         TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS msgs_order ON msgs(class, sent_ms);
        CREATE INDEX IF NOT EXISTS msgs_relay ON msgs(relay_id);
        CREATE TABLE IF NOT EXISTS hops (
            msg      TEXT NOT NULL,
            kind     TEXT NOT NULL,
            car      TEXT NOT NULL,
            car_call TEXT NOT NULL,
            operator TEXT NOT NULL,
            island   TEXT,
            trip     TEXT,
            ms       INTEGER NOT NULL,
            PRIMARY KEY (msg, kind, car)
        );
        CREATE TABLE IF NOT EXISTS receipts (
            msg         TEXT NOT NULL,
            author      TEXT NOT NULL,
            author_call TEXT NOT NULL,
            island      TEXT NOT NULL,
            ms          INTEGER NOT NULL,
            PRIMARY KEY (msg, author)
        );
        CREATE TABLE IF NOT EXISTS withdrawals (
            msg    TEXT NOT NULL,
            author TEXT NOT NULL,
            ms     INTEGER NOT NULL,
            PRIMARY KEY (msg, author)
        );
        CREATE TABLE IF NOT EXISTS manifests (
            root   TEXT NOT NULL,
            author TEXT NOT NULL,
            size   INTEGER NOT NULL,
            PRIMARY KEY (root, author)
        );
        CREATE TABLE IF NOT EXISTS pins (
            island TEXT NOT NULL,
            key    TEXT NOT NULL,
            PRIMARY KEY (key, island)
        );
        CREATE TABLE IF NOT EXISTS cars (
            key TEXT PRIMARY KEY
        );
        CREATE TABLE IF NOT EXISTS saved (
            msg      TEXT PRIMARY KEY,
            path     TEXT,
            error    TEXT,
            attempts INTEGER NOT NULL DEFAULT 0,
            next_ms  INTEGER NOT NULL DEFAULT 0,
            ms       INTEGER NOT NULL
        );"
    }

    // `pins` and `cars` come from the config and `saved` is this site's own work; none is
    // built from the feed, so they survive a reset
    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch(
            "DELETE FROM msgs; DELETE FROM hops; DELETE FROM receipts;
             DELETE FROM withdrawals; DELETE FROM manifests;",
        )?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.log.is_some() {
            return Ok(());
        }
        let p = &e.payload;
        match e.kind.as_str() {
            "mule.msg" => {
                if let Some(m) = parse_msg(e, ts) {
                    insert_msg(tx, &m, &serde_json::to_string(e)?)?;
                }
            }
            "mule.pickup" | "mule.dropoff" => {
                let kind = &e.kind[5..];
                let at = island(p, "island");
                if let (Some(msg), Some(op)) = (id_field(p, "msg"), call(p, "operator"))
                    && (kind == "pickup" || at.is_some())
                {
                    let trip = field(p, "trip", 40);
                    tx.execute(
                        "INSERT OR IGNORE INTO hops (msg, kind, car, car_call, operator, island, trip, ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![msg, kind, e.author.key, e.author.call, op, at, trip, ts],
                    )?;
                }
            }
            "mule.receipt" => {
                if let (Some(msg), Some(at)) = (id_field(p, "msg"), island(p, "island")) {
                    tx.execute(
                        "INSERT OR IGNORE INTO receipts (msg, author, author_call, island, ms)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![msg, e.author.key, e.author.call, at, ts],
                    )?;
                }
            }
            "mule.withdraw" => {
                if let Some(msg) = id_field(p, "msg") {
                    tx.execute(
                        "INSERT OR IGNORE INTO withdrawals (msg, author, ms) VALUES (?1, ?2, ?3)",
                        params![msg, e.author.key, ts],
                    )?;
                }
            }
            "blob.manifest" => {
                if let (Some(root), Some(size)) =
                    (id_field(p, "root"), p.get("size").and_then(Value::as_u64))
                {
                    tx.execute(
                        "INSERT OR IGNORE INTO manifests (root, author, size) VALUES (?1, ?2, ?3)",
                        params![root, e.author.key, size as i64],
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn insert_msg(tx: &Transaction, m: &Msg, raw: &str) -> Result<()> {
    let f = m.file.as_ref();
    let r = m.relay.as_ref();
    tx.execute(
        "INSERT OR IGNORE INTO msgs (id, author, author_call, seq, from_island, to_island,
             from_call, to_call, class, text, file_root, file_name, file_size, on_behalf,
             relay_id, relay_call, sent_ms, raw, app)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            m.id,
            m.author,
            m.author_call,
            m.seq as i64,
            m.from_island,
            m.to_island,
            m.from_call,
            m.to_call,
            m.class as i64,
            m.text,
            f.map(|f| &f.root),
            f.map(|f| &f.name),
            f.map(|f| f.size as i64),
            m.on_behalf,
            r.map(|r| &r.id),
            r.map(|r| &r.call),
            m.sent_ms,
            raw,
            m.app
        ],
    )?;
    Ok(())
}

/// Replace the island and car pins with the config's (and this site's own key).
pub fn set_pins(conn: &Connection, pins: &[(String, String)], cars: &[String]) -> Result<()> {
    conn.execute_batch("DELETE FROM pins; DELETE FROM cars;")?;
    for (island, key) in pins {
        conn.execute(
            "INSERT OR IGNORE INTO pins (island, key) VALUES (?1, ?2)",
            params![island, key],
        )?;
    }
    for key in cars {
        conn.execute("INSERT OR IGNORE INTO cars (key) VALUES (?1)", params![key])?;
    }
    Ok(())
}

const VIEW_SQL: &str = "
WITH v AS (
  SELECT m.*,
    (SELECT r.ms FROM receipts r JOIN pins p ON p.key = r.author AND p.island = m.to_island
      WHERE r.msg = m.id AND r.island = m.to_island ORDER BY r.ms LIMIT 1) AS r_ms,
    (SELECT r.author_call FROM receipts r JOIN pins p ON p.key = r.author AND p.island = m.to_island
      WHERE r.msg = m.id AND r.island = m.to_island ORDER BY r.ms LIMIT 1) AS r_call,
    EXISTS (SELECT 1 FROM hops h JOIN cars c ON c.key = h.car
            WHERE h.msg = m.id AND h.kind = 'dropoff') AS dropped,
    EXISTS (SELECT 1 FROM hops h JOIN cars c ON c.key = h.car
            WHERE h.msg = m.id AND h.kind = 'pickup') AS picked,
    EXISTS (SELECT 1 FROM withdrawals w WHERE w.msg = m.id AND w.author = m.author) AS gone,
    EXISTS (SELECT 1 FROM pins p WHERE p.key = m.author AND p.island = m.from_island) AS pinned,
    (SELECT min(d.ms - p.ms) FROM hops p JOIN hops d ON d.msg = p.msg AND d.car = p.car
       AND d.kind = 'dropoff' JOIN cars c ON c.key = p.car
     WHERE p.msg = m.id AND p.kind = 'pickup') AS leg
  FROM msgs m WHERE (?1 OR m.on_behalf IS NULL)
), s AS (
  SELECT v.*, CASE WHEN r_ms IS NOT NULL THEN 3 WHEN gone THEN 4 WHEN dropped THEN 2
                   WHEN picked THEN 1 ELSE 0 END AS st
  FROM v
)
SELECT id, author, author_call, seq, from_island, to_island, from_call, to_call, class, text,
       file_root, file_name, file_size, on_behalf, relay_id, relay_call, sent_ms, r_ms, r_call, st,
       pinned, app
FROM s";

fn row(r: &rusqlite::Row) -> rusqlite::Result<(Msg, State, Option<Receipt>, bool)> {
    let root: Option<String> = r.get(10)?;
    let file = match root {
        Some(root) => Some(FileRef {
            root,
            name: r.get(11)?,
            size: r.get::<_, i64>(12)? as u64,
        }),
        None => None,
    };
    let relay = match r.get::<_, Option<String>>(14)? {
        Some(id) => Some(Relay {
            id,
            call: r.get(15)?,
        }),
        None => None,
    };
    let to_island: String = r.get(5)?;
    let receipt = match r.get::<_, Option<i64>>(17)? {
        Some(ms) => Some(Receipt {
            site_call: r.get(18)?,
            island: to_island.clone(),
            ms,
        }),
        None => None,
    };
    let msg = Msg {
        id: r.get(0)?,
        author: r.get(1)?,
        author_call: r.get(2)?,
        seq: r.get::<_, i64>(3)? as u64,
        from_island: r.get(4)?,
        to_island,
        from_call: r.get(6)?,
        to_call: r.get(7)?,
        class: Class::from_i64(r.get(8)?),
        text: r.get(9)?,
        file,
        on_behalf: r.get(13)?,
        relay,
        app: r.get(21)?,
        sent_ms: r.get(16)?,
    };
    Ok((msg, State::from_code(r.get(19)?), receipt, r.get(20)?))
}

fn hops(conn: &Connection, msg: &str, kind: &str) -> Result<Vec<Hop>> {
    let mut st = conn.prepare_cached(
        "SELECT h.car, h.car_call, h.operator, h.island, h.trip, h.ms FROM hops h
         JOIN cars c ON c.key = h.car
         WHERE h.msg = ?1 AND h.kind = ?2 ORDER BY h.ms",
    )?;
    let rows = st.query_map(params![msg, kind], |r| {
        Ok(Hop {
            car: r.get(0)?,
            car_call: r.get(1)?,
            operator: r.get(2)?,
            island: r.get(3)?,
            trip: r.get(4)?,
            ms: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn views(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<View>> {
    let mut st = conn.prepare(sql)?;
    let rows: Vec<(Msg, State, Option<Receipt>, bool)> =
        st.query_map(args, row)?.collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(|(msg, state, receipt, pinned)| {
            Ok(View {
                pickups: hops(conn, &msg.id, "pickup")?,
                dropoffs: hops(conn, &msg.id, "dropoff")?,
                msg,
                state,
                receipt,
                pinned,
            })
        })
        .collect()
}

fn codes(states: &[State]) -> String {
    states
        .iter()
        .map(|s| s.code().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Messages in any of `states`, a page at a time: urgent and oldest first, except
/// delivered ones, newest receipt first.
pub fn list(
    conn: &Connection,
    third_party: bool,
    states: &[State],
    limit: usize,
    offset: usize,
) -> Result<Vec<View>> {
    let order = if states == [State::Delivered] {
        "r_ms DESC"
    } else {
        "class, sent_ms"
    };
    let sql = format!(
        "{VIEW_SQL} WHERE st IN ({}) ORDER BY {order} LIMIT ?2 OFFSET ?3",
        codes(states)
    );
    views(
        conn,
        &sql,
        &[&third_party, &(limit as i64), &(offset as i64)],
    )
}

pub fn count(conn: &Connection, third_party: bool, states: &[State]) -> Result<usize> {
    let sql = format!(
        "SELECT count(*) FROM ({VIEW_SQL}) WHERE st IN ({})",
        codes(states)
    );
    Ok(conn.query_row(&sql, params![third_party], |r| r.get::<_, i64>(0))? as usize)
}

/// Everything still under way: what the car and destination loops and the planner work on.
pub fn active(conn: &Connection, third_party: bool) -> Result<Vec<View>> {
    list(
        conn,
        third_party,
        &[State::Waiting, State::InTransit, State::HandedOver],
        usize::MAX >> 1,
        0,
    )
}

pub fn find(conn: &Connection, third_party: bool, id: &str) -> Result<Option<View>> {
    let sql = format!("{VIEW_SQL} WHERE id = ?2");
    Ok(views(conn, &sql, &[&third_party, &id])?.pop())
}

/// Every receipt for a message, pinned or not, for its page.
pub fn receipts(conn: &Connection, msg: &str) -> Result<Vec<(Receipt, bool)>> {
    let mut st = conn.prepare(
        "SELECT r.author_call, r.island, r.ms,
                EXISTS (SELECT 1 FROM pins p WHERE p.key = r.author AND p.island = r.island)
         FROM receipts r WHERE r.msg = ?1 ORDER BY r.ms",
    )?;
    let rows = st.query_map(params![msg], |r| {
        Ok((
            Receipt {
                site_call: r.get(0)?,
                island: r.get(1)?,
                ms: r.get(2)?,
            },
            r.get(3)?,
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The signed event behind a message, as JSON, for sending it on as a bundle.
pub fn raw(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT raw FROM msgs WHERE id = ?1", params![id], |r| {
            r.get(0)
        })
        .optional()?)
}

/// The message a gateway re-published from this original, if any.
pub fn relayed(conn: &Connection, orig: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM msgs WHERE relay_id = ?1 LIMIT 1",
            params![orig],
            |r| r.get(0),
        )
        .optional()?)
}

/// Whether `author` published a manifest for `root` with exactly `size` bytes.
pub fn manifest_matches(
    conn: &Connection,
    root: &str,
    author: &str,
    size: u64,
) -> Result<Option<bool>> {
    let got: Option<i64> = conn
        .query_row(
            "SELECT size FROM manifests WHERE root = ?1 AND author = ?2",
            params![root, author],
            |r| r.get(0),
        )
        .optional()?;
    Ok(got.map(|n| n as u64 == size))
}

/// On a car: messages aboard that it hasn't logged as picked up yet.
pub fn to_pick_up<'a>(views: &'a [View], me: &str) -> Vec<&'a View> {
    views
        .iter()
        .filter(|v| v.pinned && !v.pickups.iter().any(|h| h.car == me))
        .collect()
}

/// On a car: what it picked up and the destination hasn't receipted yet. A file stays
/// aboard until then, even after a drop-off, in case the island's copy didn't take.
pub fn carrying<'a>(views: &'a [View], me: &str) -> Vec<&'a View> {
    views
        .iter()
        .filter(|v| v.receipt.is_none() && v.pickups.iter().any(|h| h.car == me))
        .collect()
}

/// On a car: what it picked up and hasn't seen at the destination yet.
pub fn to_drop_off<'a>(views: &'a [View], me: &str) -> Vec<&'a View> {
    views
        .iter()
        .filter(|v| {
            v.pickups.iter().any(|h| h.car == me) && !v.dropoffs.iter().any(|h| h.car == me)
        })
        .collect()
}

/// On an island: messages for it that no pinned site on it has receipted yet.
pub fn to_deliver<'a>(views: &'a [View], island: &str) -> Vec<&'a View> {
    views
        .iter()
        .filter(|v| v.pinned && v.msg.to_island == island && v.receipt.is_none())
        .collect()
}

/// What this site did with a delivered file: where it is, or why it isn't yet.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Saved {
    pub path: Option<String>,
    pub error: Option<String>,
    pub attempts: u32,
    pub next_ms: i64,
}

pub fn saved(conn: &Connection, msg: &str) -> Result<Option<Saved>> {
    Ok(conn
        .query_row(
            "SELECT path, error, attempts, next_ms FROM saved WHERE msg = ?1",
            params![msg],
            |r| {
                Ok(Saved {
                    path: r.get(0)?,
                    error: r.get(1)?,
                    attempts: r.get(2)?,
                    next_ms: r.get(3)?,
                })
            },
        )
        .optional()?)
}

pub fn note_path(conn: &Connection, msg: &str, path: &str, ms: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO saved (msg, path, error, attempts, next_ms, ms) VALUES (?1, ?2, NULL, 0, 0, ?3)
         ON CONFLICT(msg) DO UPDATE SET path = excluded.path, error = NULL, attempts = 0,
             next_ms = 0, ms = excluded.ms",
        params![msg, path, ms],
    )?;
    Ok(())
}

/// Record a failed attempt and when to try again: 1, 2, 4 ... minutes, at most 6 hours.
/// A saved path stays saved; only a receipt that failed to append is retried.
pub fn note_failure(conn: &Connection, msg: &str, error: &str, now: i64) -> Result<Saved> {
    let attempts = saved(conn, msg)?.map_or(0, |s| s.attempts) + 1;
    let wait = (60_000i64 << (attempts - 1).min(9)).min(6 * 3_600_000);
    conn.execute(
        "INSERT INTO saved (msg, path, error, attempts, next_ms, ms) VALUES (?1, NULL, ?2, ?3, ?4, ?5)
         ON CONFLICT(msg) DO UPDATE SET error = excluded.error, attempts = excluded.attempts,
             next_ms = excluded.next_ms, ms = excluded.ms",
        params![msg, error, attempts, now + wait, now],
    )?;
    Ok(saved(conn, msg)?.unwrap_or_default())
}

#[derive(Debug, PartialEq)]
pub struct Corridor {
    pub from: String,
    pub to: String,
    pub class: Class,
    pub count: usize,
    pub median_ms: i64,
    /// receipts that claim to come before the message: someone's clock is off
    pub skewed: usize,
    /// pickup to drop-off by the car's own clock
    pub car_count: usize,
    pub car_median_ms: i64,
}

/// Send-to-receipt times, how many receipts predate their message, and car drive times.
type Times = (Vec<f64>, usize, Vec<f64>);

/// Measured delivery time per island pair and class, from every delivered message.
pub fn corridors(conn: &Connection, third_party: bool) -> Result<Vec<Corridor>> {
    let mut by: std::collections::BTreeMap<(String, String, Class), Times> = Default::default();
    let sql = VIEW_SQL.replace(
        "SELECT id, author,",
        "SELECT from_island, to_island, class, r_ms - sent_ms, leg, st, id, author,",
    );
    let mut st = conn.prepare(&format!("{sql} WHERE st = 3"))?;
    let mut rows = st.query(params![third_party])?;
    while let Some(r) = rows.next()? {
        let key = (r.get(0)?, r.get(1)?, Class::from_i64(r.get(2)?));
        let trip: i64 = r.get(3)?;
        let leg: Option<i64> = r.get(4)?;
        let e = by.entry(key).or_default();
        if trip >= 0 {
            e.0.push(trip as f64);
        } else {
            e.1 += 1;
        }
        if let Some(ms) = leg.filter(|ms| *ms >= 0) {
            e.2.push(ms as f64);
        }
    }
    Ok(by
        .into_iter()
        .map(|((from, to, class), (mut t, skewed, mut car))| Corridor {
            from,
            to,
            class,
            count: t.len(),
            median_ms: median(&mut t).unwrap_or(0.0) as i64,
            skewed,
            car_count: car.len(),
            car_median_ms: median(&mut car).unwrap_or(0.0) as i64,
        })
        .collect())
}

/// A courier as the cars' records show them.
#[derive(Debug, PartialEq, Default)]
pub struct Seen {
    pub call: String,
    pub last_pickup: Option<i64>,
    pub last_dropoff: Option<i64>,
    /// days since `since_ms` on which they picked anything up
    pub drive_days: usize,
}

/// Every control operator in the pinned cars' pickups and drop-offs.
pub fn couriers(conn: &Connection, since_ms: i64) -> Result<Vec<Seen>> {
    let mut st = conn.prepare(
        "SELECT h.operator,
                max(CASE WHEN h.kind = 'pickup' THEN h.ms END),
                max(CASE WHEN h.kind = 'dropoff' THEN h.ms END),
                count(DISTINCT CASE WHEN h.kind = 'pickup' AND h.ms >= ?1 THEN h.ms / 86400000 END)
         FROM hops h JOIN cars c ON c.key = h.car
         GROUP BY h.operator ORDER BY h.operator",
    )?;
    let rows = st.query_map(params![since_ms], |r| {
        Ok(Seen {
            call: r.get(0)?,
            last_pickup: r.get(1)?,
            last_dropoff: r.get(2)?,
            drive_days: r.get::<_, i64>(3)? as usize,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Island names seen in messages and pins, for the send form's suggestions.
pub fn islands(conn: &Connection) -> Result<Vec<String>> {
    Ok(conn
        .prepare(
            "SELECT from_island FROM msgs UNION SELECT to_island FROM msgs
             UNION SELECT island FROM pins ORDER BY 1",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use ridgewarden_client::Author;
    use ridgewarden_client::core::init_db;
    use ridgewarden_client::util::ms_to_rfc3339;
    use serde_json::json;

    /// Events from several stations, applied the way `follow` would. The core has already
    /// checked signatures by the time the app sees an event, so these carry none. Stations
    /// named after an island ("n0north", "n0south") are pinned to it.
    pub struct Sites {
        pub conn: Connection,
        seq: u64,
    }

    pub fn key(name: &str) -> String {
        blake3::hash(name.as_bytes()).to_hex().to_string()
    }

    impl Default for Sites {
        fn default() -> Self {
            let conn = Connection::open_in_memory().unwrap();
            init_db(&conn, crate::app::PROJECTIONS).unwrap();
            let pins: Vec<(String, String)> = ["north", "south"]
                .iter()
                .map(|i| (i.to_string(), key(&format!("n0{i}"))))
                .collect();
            set_pins(&conn, &pins, &[key("n0car"), key("n0other")]).unwrap();
            Self { conn, seq: 0 }
        }
    }

    impl Sites {
        pub fn add(&mut self, station: &str, kind: &str, payload: Value, ts: i64) -> Event {
            self.seq += 1;
            let e = Event {
                id: blake3::hash(format!("{station}{}", self.seq).as_bytes())
                    .to_hex()
                    .to_string(),
                author: Author {
                    call: station.to_ascii_uppercase(),
                    key: key(station),
                },
                seq: self.seq,
                prev: None,
                lamport: self.seq,
                wall_time: ms_to_rfc3339(ts),
                mono_time: 0,
                kind: kind.into(),
                payload,
                sig: String::new(),
                log: None,
            };
            let tx = self.conn.transaction().unwrap();
            for p in crate::app::PROJECTIONS {
                p.apply(&tx, &e, ts).unwrap();
            }
            tx.commit().unwrap();
            e
        }

        pub fn msg(&mut self, class: &str, to: &str, ts: i64) -> String {
            let p = json!({"from_island": "north", "to": to, "from": "N0AAA",
                           "class": class, "text": "water at the shelter"});
            self.add("n0north", "mule.msg", p, ts).id
        }

        pub fn file_msg(&mut self, class: &str, size: u64, ts: i64) -> String {
            let root = key(&format!("file{}", self.seq));
            self.add(
                "n0north",
                "blob.manifest",
                json!({"root": root, "size": size}),
                ts,
            );
            let p = json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": class,
                           "file": {"root": root, "name": "tiles.bin", "size": size}});
            self.add("n0north", "mule.msg", p, ts).id
        }

        pub fn views(&self) -> Vec<View> {
            list(
                &self.conn,
                false,
                &[
                    State::Waiting,
                    State::InTransit,
                    State::HandedOver,
                    State::Delivered,
                    State::Withdrawn,
                ],
                1000,
                0,
            )
            .unwrap()
        }

        pub fn one(&self, id: &str) -> View {
            find(&self.conn, false, id).unwrap().unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Sites, key};
    use super::*;
    use serde_json::json;

    const MIN: i64 = 60_000;

    #[test]
    fn a_message_goes_from_waiting_to_delivered() {
        let mut s = Sites::default();
        let id = s.msg("urgent", "south", 0);
        assert_eq!(s.one(&id).state, State::Waiting);
        s.add(
            "n0car",
            "mule.pickup",
            json!({"msg": id, "operator": "N0DRV"}),
            10 * MIN,
        );
        let v = s.one(&id);
        assert_eq!(v.state, State::InTransit);
        assert_eq!(v.pickups[0].operator, "N0DRV");
        assert_eq!(v.pickups[0].car_call, "N0CAR");
        s.add(
            "n0car",
            "mule.dropoff",
            json!({"msg": id, "operator": "N0DRV", "island": "south"}),
            90 * MIN,
        );
        assert_eq!(s.one(&id).state, State::HandedOver);
        s.add(
            "n0south",
            "mule.receipt",
            json!({"msg": id, "island": "south"}),
            95 * MIN,
        );
        let v = s.one(&id);
        assert_eq!(v.state, State::Delivered);
        assert_eq!(v.corridor_ms(), Some(95 * MIN));
        assert_eq!(v.car_leg_ms(), Some(80 * MIN));
    }

    #[test]
    fn only_a_pinned_receipt_from_the_destination_counts() {
        let mut s = Sites::default();
        let id = s.msg("routine", "south", 0);
        s.add(
            "n0east",
            "mule.receipt",
            json!({"msg": id, "island": "east"}),
            MIN,
        );
        s.add(
            "n0rogue",
            "mule.receipt",
            json!({"msg": id, "island": "south"}),
            MIN,
        );
        s.add(
            "n0north",
            "mule.receipt",
            json!({"msg": id, "island": "south"}),
            MIN,
        );
        assert_eq!(s.one(&id).state, State::Waiting);
        assert_eq!(to_deliver(&s.views(), "south").len(), 1);
        assert_eq!(receipts(&s.conn, &id).unwrap().len(), 3);
        s.add(
            "n0south",
            "mule.receipt",
            json!({"msg": id, "island": "south"}),
            2 * MIN,
        );
        assert!(to_deliver(&s.views(), "south").is_empty());
        assert_eq!(s.one(&id).receipt.unwrap().site_call, "N0SOUTH");
    }

    #[test]
    fn corridor_times_are_per_class_medians() {
        let mut s = Sites::default();
        for (class, sent, got) in [
            ("urgent", 0, 60),
            ("urgent", 0, 100),
            ("urgent", 0, 300),
            ("bulk", 0, 600),
            ("bulk", 10, 1000),
            ("bulk", 50, 20),
        ] {
            let id = s.msg(class, "south", sent * MIN);
            s.add(
                "n0south",
                "mule.receipt",
                json!({"msg": id, "island": "south"}),
                got * MIN,
            );
        }
        s.msg("urgent", "south", 0);
        let c = corridors(&s.conn, false).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(
            (c[0].class, c[0].count, c[0].median_ms),
            (Class::Urgent, 3, 100 * MIN)
        );
        assert_eq!(
            (c[1].class, c[1].count, c[1].median_ms),
            (Class::Bulk, 2, 795 * MIN)
        );
        assert_eq!(
            c[1].skewed, 1,
            "a receipt from before the send is clock skew, not a time"
        );
        assert_eq!((c[0].from.as_str(), c[0].to.as_str()), ("north", "south"));
    }

    #[test]
    fn urgent_messages_list_before_bulk() {
        let mut s = Sites::default();
        s.msg("bulk", "south", 0);
        s.msg("urgent", "south", 5 * MIN);
        s.msg("routine", "south", MIN);
        let order: Vec<Class> = s.views().iter().map(|v| v.msg.class).collect();
        assert_eq!(order, [Class::Urgent, Class::Routine, Class::Bulk]);
    }

    #[test]
    fn lists_page_and_count() {
        let mut s = Sites::default();
        for i in 0..5 {
            s.msg("routine", "south", i * MIN);
        }
        let first = list(&s.conn, false, &[State::Waiting], 2, 0).unwrap();
        let third = list(&s.conn, false, &[State::Waiting], 2, 4).unwrap();
        assert_eq!((first.len(), third.len()), (2, 1));
        assert_eq!(first[0].msg.sent_ms, 0);
        assert_eq!(count(&s.conn, false, &[State::Waiting]).unwrap(), 5);
    }

    #[test]
    fn malformed_or_oversized_messages_are_ignored() {
        let mut s = Sites::default();
        let root = "ab".repeat(32);
        let bad = [
            json!({"from_island": "north", "to": "north", "from": "N0AAA", "class": "urgent", "text": "x"}),
            json!({"from_island": "north", "to": "south", "from": "not a call", "class": "urgent", "text": "x"}),
            json!({"from_island": "north", "to": "South Bay", "from": "N0AAA", "class": "urgent", "text": "x"}),
            json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "whenever", "text": "x"}),
            json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "bulk"}),
            json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "bulk",
                   "text": "x", "file": {"root": "../../etc", "name": "a", "size": 1}}),
            json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "bulk",
                   "text": "x".repeat(MAX_TEXT + 1)}),
            json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "urgent",
                   "file": {"root": root, "name": "big", "size": (1 << 20) + 1}}),
            json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "bulk",
                   "file": {"root": root, "name": "huge", "size": 1u64 << 41}}),
            json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "bulk",
                   "text": "x", "to_call": "lower case"}),
        ];
        for p in bad {
            s.add("n0north", "mule.msg", p, 0);
        }
        s.add(
            "n0car",
            "mule.pickup",
            json!({"msg": "nope", "operator": "N0DRV"}),
            0,
        );
        assert!(s.views().is_empty());
        let n: i64 = s
            .conn
            .query_row("SELECT count(*) FROM hops", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn third_party_traffic_is_left_out_unless_allowed() {
        let mut s = Sites::default();
        let p = json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "routine",
                       "text": "tell my sister I'm fine", "for": "Pat"});
        s.add("n0north", "mule.msg", p, 0);
        assert!(s.views().is_empty());
        let v = active(&s.conn, true).unwrap();
        assert_eq!(v[0].msg.on_behalf.as_deref(), Some("Pat"));
    }

    #[test]
    fn a_car_picks_up_once_and_drops_off_once() {
        let mut s = Sites::default();
        let a = s.msg("urgent", "south", 0);
        let b = s.msg("bulk", "south", 0);
        let car = key("n0car");
        assert_eq!(to_pick_up(&active(&s.conn, false).unwrap(), &car).len(), 2);
        s.add(
            "n0car",
            "mule.pickup",
            json!({"msg": a, "operator": "N0DRV"}),
            MIN,
        );
        s.add(
            "n0other",
            "mule.pickup",
            json!({"msg": b, "operator": "N0XYZ"}),
            MIN,
        );
        let v = active(&s.conn, false).unwrap();
        let left: Vec<&str> = to_pick_up(&v, &car)
            .iter()
            .map(|v| v.msg.id.as_str())
            .collect();
        assert_eq!(left, [b.as_str()], "another car's pickup is not ours");
        let drop: Vec<&str> = to_drop_off(&v, &car)
            .iter()
            .map(|v| v.msg.id.as_str())
            .collect();
        assert_eq!(drop, [a.as_str()]);
        s.add(
            "n0car",
            "mule.dropoff",
            json!({"msg": a, "operator": "N0DRV", "island": "south"}),
            2 * MIN,
        );
        assert!(to_drop_off(&active(&s.conn, false).unwrap(), &car).is_empty());
    }

    #[test]
    fn records_count_only_from_pinned_cars_and_senders() {
        let mut s = Sites::default();
        let id = s.msg("urgent", "south", 0);
        s.add(
            "n0rogue",
            "mule.pickup",
            json!({"msg": id, "operator": "N0BAD"}),
            MIN,
        );
        assert_eq!(s.one(&id).state, State::Waiting, "not a pinned car");
        let p = json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "bulk",
                       "text": "says it's from north"});
        let fake = s.add("n0rogue", "mule.msg", p, 0).id;
        let v = active(&s.conn, false).unwrap();
        assert!(!s.one(&fake).pinned);
        assert_eq!(
            to_deliver(&v, "south").len(),
            1,
            "only the pinned sender's message"
        );
        assert_eq!(to_pick_up(&v, &key("n0car")).len(), 1);
    }

    #[test]
    fn couriers_are_seen_through_their_cars_records() {
        let mut s = Sites::default();
        const DAY: i64 = 86_400_000;
        for (day, op) in [(1, "N0DRV"), (1, "N0DRV"), (3, "N0DRV"), (2, "N0XYZ")] {
            let id = s.msg("urgent", "south", day * DAY);
            s.add(
                "n0car",
                "mule.pickup",
                json!({"msg": id, "operator": op}),
                day * DAY + 1,
            );
        }
        let id = s.msg("urgent", "south", 0);
        s.add(
            "n0car",
            "mule.dropoff",
            json!({"msg": id, "operator": "N0DRV", "island": "south"}),
            4 * DAY,
        );
        s.add(
            "n0rogue",
            "mule.pickup",
            json!({"msg": id, "operator": "N0FAKE"}),
            5 * DAY,
        );
        let c = couriers(&s.conn, 2 * DAY).unwrap();
        assert_eq!(c.len(), 2, "only pinned cars' records");
        assert_eq!(
            c[0],
            Seen {
                call: "N0DRV".into(),
                last_pickup: Some(3 * DAY + 1),
                last_dropoff: Some(4 * DAY),
                drive_days: 1,
            }
        );
        assert_eq!((c[1].call.as_str(), c[1].drive_days), ("N0XYZ", 1));
    }

    #[test]
    fn delivered_and_withdrawn_messages_are_not_cargo() {
        let mut s = Sites::default();
        let a = s.msg("urgent", "south", 0);
        let b = s.msg("urgent", "south", 0);
        s.add(
            "n0south",
            "mule.receipt",
            json!({"msg": a, "island": "south"}),
            MIN,
        );
        s.add("n0rogue", "mule.withdraw", json!({"msg": b}), MIN);
        assert_eq!(
            active(&s.conn, false).unwrap().len(),
            1,
            "only the sender can withdraw"
        );
        s.add("n0north", "mule.withdraw", json!({"msg": b}), MIN);
        assert!(active(&s.conn, false).unwrap().is_empty());
        assert_eq!(s.one(&b).state, State::Withdrawn);
    }

    #[test]
    fn manifests_are_checked_against_the_message() {
        let mut s = Sites::default();
        let id = s.file_msg("bulk", 5000, 0);
        let f = s.one(&id).msg.file.unwrap();
        assert_eq!(
            manifest_matches(&s.conn, &f.root, &key("n0north"), 5000).unwrap(),
            Some(true)
        );
        assert_eq!(
            manifest_matches(&s.conn, &f.root, &key("n0north"), 4999).unwrap(),
            Some(false)
        );
        assert_eq!(
            manifest_matches(&s.conn, &f.root, &key("n0car"), 5000).unwrap(),
            None
        );
    }

    #[test]
    fn failures_back_off_and_keep_a_saved_path() {
        let s = Sites::default();
        let id = "ab".repeat(32);
        let a = note_failure(&s.conn, &id, "core down", 0).unwrap();
        let b = note_failure(&s.conn, &id, "core down", 0).unwrap();
        assert_eq!(
            (a.attempts, a.next_ms, b.attempts, b.next_ms),
            (1, MIN, 2, 2 * MIN)
        );
        note_path(&s.conn, &id, "/x/y", 0).unwrap();
        let c = note_failure(&s.conn, &id, "receipt refused", 0).unwrap();
        assert_eq!(
            c.path.as_deref(),
            Some("/x/y"),
            "a failed receipt must not lose the file"
        );
        assert_eq!(c.attempts, 1);
    }

    #[test]
    fn reset_keeps_what_this_site_saved_and_its_pins() {
        let mut s = Sites::default();
        let id = s.msg("urgent", "south", 0);
        note_path(&s.conn, &id, "/x/y", 0).unwrap();
        let tx = s.conn.transaction().unwrap();
        Mule.reset(&tx).unwrap();
        tx.commit().unwrap();
        assert!(s.views().is_empty());
        assert_eq!(
            saved(&s.conn, &id).unwrap().unwrap().path.as_deref(),
            Some("/x/y")
        );
        assert_eq!(islands(&s.conn).unwrap(), ["north", "south"]);
    }
}
