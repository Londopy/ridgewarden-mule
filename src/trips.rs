//! Couriers' regular trips, the contact plan: who drives from which island to which, on
//! which days, leaving when, with how much room for files.
//!
//!   mule.trip  {trip, courier, from, to, days, depart, hours, capacity_mb, note, retired}
//!
//! The newest event for a (station, trip) pair wins, so changing a trip is registering it
//! again, and `retired: true` takes it off the plan. Times are UTC, like every other time
//! in the suite; a Pi without a time zone database has no other choice that stays right.

use anyhow::{Result, bail};
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::util::valid_node;
use rusqlite::{Connection, Transaction, params};
use serde_json::{Value, json};

use crate::mule::{Seen, call, field, island};

const DAY: i64 = 86_400_000;
/// How far back the couriers' counts look.
pub const MONTH: i64 = 30 * DAY;
const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
/// Ten terabytes: more than any car carries, small enough that nothing overflows.
const MAX_CAPACITY_MB: u64 = 10_000_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Trip {
    pub author: String,
    pub author_call: String,
    pub id: String,
    pub courier: String,
    pub from: String,
    pub to: String,
    /// bit 0 is Monday
    pub days: u8,
    /// minutes after midnight UTC
    pub depart_min: u32,
    pub hours: f64,
    pub capacity_mb: u64,
    pub note: String,
    pub ts: i64,
}

/// 0 for Monday.
pub fn weekday(ms: i64) -> u32 {
    // 1970-01-01 was a Thursday
    (ms.div_euclid(DAY) + 3).rem_euclid(7) as u32
}

/// "fri", "mon,thu", "mon-fri", "sat-mon" or "daily"; day names may be spelled out.
pub fn parse_days(s: &str) -> Option<u8> {
    const FULL: [&str; 7] = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    let day = |d: &str| {
        let d = d.trim().to_ascii_lowercase();
        (d.len() >= 3).then(|| FULL.iter().position(|f| f.starts_with(&d)))?
    };
    let mut bits = 0u8;
    for part in s.split([',', ' ']).filter(|d| !d.is_empty()) {
        if part.eq_ignore_ascii_case("daily") {
            return Some(0x7f);
        }
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (day(a)?, day(b)?);
                let mut i = a;
                loop {
                    bits |= 1 << i;
                    if i == b {
                        break;
                    }
                    i = (i + 1) % 7;
                }
            }
            None => bits |= 1 << day(part)?,
        }
    }
    (bits != 0).then_some(bits)
}

/// "17:30" as minutes after midnight.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60 && s.trim().len() <= 5).then_some(h * 60 + m)
}

impl Trip {
    /// How pages and pickups name a trip: the courier's own id, unique per station.
    pub fn key(&self) -> String {
        format!("{}:{}", &self.author[..8], self.id)
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_mb.saturating_mul(1_000_000)
    }

    pub fn length_ms(&self) -> i64 {
        (self.hours * 3_600_000.0) as i64
    }

    /// Departures that are still under way at `from_ms` or start before `until_ms`.
    pub fn runs(&self, from_ms: i64, until_ms: i64) -> Vec<i64> {
        let mut out = vec![];
        let mut day = (from_ms - self.length_ms()).div_euclid(DAY);
        while day * DAY <= until_ms {
            let dep = day * DAY + self.depart_min as i64 * 60_000;
            if self.days & (1 << weekday(day * DAY)) != 0
                && dep + self.length_ms() > from_ms
                && dep <= until_ms
            {
                out.push(dep);
            }
            day += 1;
        }
        out
    }

    /// The next departure within the plan's two weeks.
    pub fn next(&self, now: i64) -> Option<i64> {
        self.runs(now, now + crate::plan::HORIZON_MS)
            .into_iter()
            .find(|d| *d >= now)
    }

    /// Departures in the last 30 days, counted from when it was registered, since only
    /// those can have been missed.
    pub fn due(&self, now: i64) -> usize {
        let from = (now - MONTH).max(self.ts);
        self.runs(from, now)
            .into_iter()
            .filter(|d| *d >= from && *d <= now)
            .count()
    }

    pub fn days_text(&self) -> String {
        if self.days == 0x7f {
            return "daily".into();
        }
        DAYS.iter()
            .enumerate()
            .filter(|(i, _)| self.days & (1 << i) != 0)
            .map(|(_, d)| {
                let mut c = d.chars();
                c.next().map_or(String::new(), |f| {
                    f.to_ascii_uppercase().to_string() + c.as_str()
                })
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn schedule_text(&self) -> String {
        format!(
            "{} at {:02}:{:02} UTC, about {} h",
            self.days_text(),
            self.depart_min / 60,
            self.depart_min % 60,
            self.hours
        )
    }
}

pub fn parse_trip(e: &Event, ts: i64) -> Option<(Trip, bool)> {
    let p = &e.payload;
    let id = field(p, "trip", 40).filter(|t| valid_node(t))?;
    let retired = p.get("retired").and_then(Value::as_bool).unwrap_or(false);
    let hours = p.get("hours").and_then(Value::as_f64).unwrap_or(4.0);
    let capacity_mb = p
        .get("capacity_mb")?
        .as_u64()
        .filter(|n| *n <= MAX_CAPACITY_MB)?;
    let trip = Trip {
        author: e.author.key.clone(),
        author_call: e.author.call.clone(),
        id,
        courier: call(p, "courier")?,
        from: island(p, "from")?,
        to: island(p, "to")?,
        days: parse_days(field(p, "days", 60)?.as_str())?,
        depart_min: parse_hhmm(&field(p, "depart", 5)?)?,
        hours: (hours > 0.0 && hours <= 72.0).then_some(hours)?,
        capacity_mb,
        note: field(p, "note", 200).unwrap_or_default(),
        ts,
    };
    (trip.from != trip.to).then_some((trip, retired))
}

/// Check a trip someone is registering here and build its payload.
pub fn payload(t: &Trip) -> Result<Value> {
    if t.from == t.to {
        bail!("a trip goes from one island to another");
    }
    if !(t.hours > 0.0 && t.hours <= 72.0) {
        bail!("hours is the length of the drive, more than 0 and at most 72");
    }
    if t.capacity_mb > MAX_CAPACITY_MB {
        bail!("room for files is at most {MAX_CAPACITY_MB} MB");
    }
    let days: Vec<&str> = DAYS
        .iter()
        .enumerate()
        .filter(|(i, _)| t.days & (1 << i) != 0)
        .map(|(_, d)| *d)
        .collect();
    let mut p = json!({
        "trip": t.id, "courier": t.courier, "from": t.from, "to": t.to,
        "days": days.join(","),
        "depart": format!("{:02}:{:02}", t.depart_min / 60, t.depart_min % 60),
        "hours": t.hours, "capacity_mb": t.capacity_mb,
    });
    if !t.note.is_empty() {
        p["note"] = json!(t.note);
    }
    Ok(p)
}

pub struct Trips;

impl Projection for Trips {
    fn schema(&self) -> &'static str {
        "CREATE TABLE IF NOT EXISTS trips (
            author      TEXT NOT NULL,
            author_call TEXT NOT NULL,
            trip        TEXT NOT NULL,
            courier     TEXT NOT NULL,
            from_island TEXT NOT NULL,
            to_island   TEXT NOT NULL,
            days        INTEGER NOT NULL,
            depart_min  INTEGER NOT NULL,
            hours       REAL NOT NULL,
            capacity_mb INTEGER NOT NULL,
            note        TEXT NOT NULL,
            retired     INTEGER NOT NULL,
            ts          INTEGER NOT NULL,
            PRIMARY KEY (author, trip)
        );"
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute("DELETE FROM trips", [])?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != "mule.trip" || e.log.is_some() {
            return Ok(());
        }
        let Some((t, retired)) = parse_trip(e, ts) else {
            return Ok(());
        };
        tx.execute(
            "INSERT INTO trips (author, author_call, trip, courier, from_island, to_island, days,
                 depart_min, hours, capacity_mb, note, retired, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(author, trip) DO UPDATE SET author_call = excluded.author_call,
                 courier = excluded.courier, from_island = excluded.from_island,
                 to_island = excluded.to_island, days = excluded.days,
                 depart_min = excluded.depart_min, hours = excluded.hours,
                 capacity_mb = excluded.capacity_mb, note = excluded.note,
                 retired = excluded.retired, ts = excluded.ts
             WHERE excluded.ts >= trips.ts",
            params![
                t.author,
                t.author_call,
                t.id,
                t.courier,
                t.from,
                t.to,
                t.days,
                t.depart_min,
                t.hours,
                t.capacity_mb as i64,
                t.note,
                retired,
                ts
            ],
        )?;
        Ok(())
    }
}

/// When a load picked up for trip `key` at `after_ms` should reach the far island: the end
/// of that trip's drive under way then, or of its next one.
pub fn arrival(trips: &[Trip], key: &str, after_ms: i64) -> Option<i64> {
    let t = trips.iter().find(|t| t.key() == key)?;
    let dep = t.runs(after_ms, after_ms + 8 * DAY).into_iter().next()?;
    Some(dep + t.length_ms())
}

/// A courier: the trips they registered next to what the pinned cars' records show.
pub struct Courier<'a> {
    pub call: &'a str,
    pub trips: Vec<&'a Trip>,
    /// their registered departures due in the last 30 days
    pub due: usize,
    pub seen: Option<&'a Seen>,
}

/// Everyone who registered a trip or drove as control operator, by callsign.
pub fn couriers<'a>(all: &'a [Trip], seen: &'a [Seen], now: i64) -> Vec<Courier<'a>> {
    let mut calls: Vec<&str> = all
        .iter()
        .map(|t| t.courier.as_str())
        .chain(seen.iter().map(|s| s.call.as_str()))
        .collect();
    calls.sort();
    calls.dedup();
    calls
        .into_iter()
        .map(|call| {
            let trips: Vec<&Trip> = all.iter().filter(|t| t.courier == call).collect();
            Courier {
                call,
                due: trips.iter().map(|t| t.due(now)).sum(),
                trips,
                seen: seen.iter().find(|s| s.call == call),
            }
        })
        .collect()
}

/// Every trip on the plan, by corridor.
pub fn trips(conn: &Connection) -> Result<Vec<Trip>> {
    let mut st = conn.prepare(
        "SELECT author, author_call, trip, courier, from_island, to_island, days, depart_min,
                hours, capacity_mb, note, ts
         FROM trips WHERE retired = 0 ORDER BY from_island, to_island, depart_min",
    )?;
    let rows = st.query_map([], |r| {
        Ok(Trip {
            author: r.get(0)?,
            author_call: r.get(1)?,
            id: r.get(2)?,
            courier: r.get(3)?,
            from: r.get(4)?,
            to: r.get(5)?,
            days: r.get(6)?,
            depart_min: r.get(7)?,
            hours: r.get(8)?,
            capacity_mb: r.get::<_, i64>(9)? as u64,
            note: r.get(10)?,
            ts: r.get(11)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mule::testing::Sites;

    // 2026-11-20 00:00 UTC, a Friday
    const FRI: i64 = 1_795_132_800_000;
    const H: i64 = 3_600_000;

    fn trip(days: &str, depart: &str, hours: f64) -> Trip {
        Trip {
            author: "ab".repeat(32),
            author_call: "N0AAA".into(),
            id: "friday-south".into(),
            courier: "N0DRV".into(),
            from: "north".into(),
            to: "south".into(),
            days: parse_days(days).unwrap(),
            depart_min: parse_hhmm(depart).unwrap(),
            hours,
            capacity_mb: 2000,
            note: String::new(),
            ts: 0,
        }
    }

    #[test]
    fn weekdays_line_up_with_the_calendar() {
        assert_eq!(weekday(FRI), 4);
        assert_eq!(weekday(FRI + 3 * 24 * H), 0);
        assert_eq!(weekday(0), 3, "1970-01-01 was a Thursday");
    }

    #[test]
    fn runs_cover_the_window_and_a_drive_under_way() {
        let t = trip("fri,sun", "17:30", 3.0);
        let runs = t.runs(FRI, FRI + 7 * 24 * H);
        assert_eq!(
            runs,
            [FRI + 17 * H + H / 2, FRI + 2 * 24 * H + 17 * H + H / 2]
        );
        // at 19:00 Friday the 17:30 drive is still on the road
        assert_eq!(t.runs(FRI + 19 * H, FRI + 20 * H), [FRI + 17 * H + H / 2]);
        // at 21:00 it has arrived
        assert!(t.runs(FRI + 21 * H, FRI + 22 * H).is_empty());
    }

    #[test]
    fn arrival_is_the_end_of_the_drive_the_load_went_with() {
        let t = [trip("fri", "17:30", 3.0)];
        let key = t[0].key();
        let fri_run_ends = FRI + 20 * H + H / 2;
        assert_eq!(
            arrival(&t, &key, FRI + 9 * H),
            Some(fri_run_ends),
            "loaded before it left"
        );
        assert_eq!(
            arrival(&t, &key, FRI + 19 * H),
            Some(fri_run_ends),
            "on the road"
        );
        assert_eq!(
            arrival(&t, &key, FRI + 21 * H),
            Some(fri_run_ends + 7 * 24 * H),
            "next week's"
        );
        assert_eq!(arrival(&t, "nobody:x", FRI), None);
    }

    #[test]
    fn days_and_times_parse() {
        assert_eq!(parse_days("Mon, fri"), Some(0b10001));
        assert_eq!(parse_days("daily"), Some(0x7f));
        assert_eq!(parse_days("someday"), None);
        assert_eq!(parse_days("mon-fri"), Some(0b11111));
        assert_eq!(parse_days("sat-mon"), Some(0b1100001));
        assert_eq!(parse_days("Friday"), Some(0b10000));
        assert_eq!(parse_days("mo"), None, "too short to tell");
        assert_eq!(parse_hhmm("07:05"), Some(425));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(
            trip("sat,mon", "07:05", 2.5).schedule_text(),
            "Mon, Sat at 07:05 UTC, about 2.5 h"
        );
    }

    #[test]
    fn the_newest_registration_wins_and_retiring_removes_it() {
        let mut s = Sites::default();
        let t = trip("fri", "17:30", 3.0);
        let mut p = payload(&t).unwrap();
        s.add("n0north", "mule.trip", p.clone(), 10);
        p["capacity_mb"] = json!(500);
        s.add("n0north", "mule.trip", p.clone(), 20);
        let mut old = p.clone();
        old["capacity_mb"] = json!(9);
        s.add("n0north", "mule.trip", old, 15);
        let got = trips(&s.conn).unwrap();
        assert_eq!(
            (got.len(), got[0].capacity_mb),
            (1, 500),
            "an older event doesn't undo a newer one"
        );
        p["retired"] = json!(true);
        s.add("n0north", "mule.trip", p, 30);
        assert!(trips(&s.conn).unwrap().is_empty());
    }

    #[test]
    fn bad_trips_are_ignored() {
        let mut s = Sites::default();
        let good = payload(&trip("fri", "17:30", 3.0)).unwrap();
        for (k, v) in [
            ("from", json!("south")),
            ("days", json!("someday")),
            ("depart", json!("5pm")),
            ("hours", json!(500)),
            ("courier", json!("nobody")),
            ("trip", json!("Friday Run")),
        ] {
            let mut p = good.clone();
            p[k] = v;
            s.add("n0north", "mule.trip", p, 0);
        }
        assert!(trips(&s.conn).unwrap().is_empty());
    }
}
