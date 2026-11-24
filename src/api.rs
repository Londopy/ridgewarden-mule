//! `GET /api/mule`: what the pages show, as JSON, and what `ridgewarden-mule top` reads.
//! Each message and where it is, the next departures and what goes on them, the couriers,
//! and on a car tonight's manifest.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::State as Axum;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use ridgewarden_client::util::now_ms;
use serde::{Deserialize, Serialize};

use crate::app::{App, short_err};
use crate::car::{self, Drive};
use crate::mule::{self, Class, FileRef, Hop, Receipt, State, View};
use crate::plan::{self, Cargo, Plan, Why};
use crate::trips::{self, Trip};
use crate::web::Fail;

/// The most of anything listed: messages per state, departures, cargo per departure,
/// couriers, islands. The counts beside each list say how many there are in all.
pub const LIST: usize = 100;
/// How long one answer serves every reader, so a burst of them costs one snapshot.
const FRESH: Duration = Duration::from_millis(1500);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// when the app put this together, by its clock
    pub now_ms: i64,
    /// None on a car
    pub island: Option<String>,
    /// the sender this site defaults to; on a car, the control operator
    pub operator: String,
    /// the core's callsign, once it has answered
    pub call: Option<String>,
    /// what is wrong with the app's line to its core, as a sentence: it can't read the
    /// core's feed, hasn't for a while, or couldn't ask it something just now
    pub core_error: Option<String>,
    pub waiting: usize,
    /// in transit or handed over
    pub moving: usize,
    pub delivered: usize,
    /// waiting, then moving, then the latest deliveries
    pub messages: Vec<Message>,
    /// islands messages here go to whose receipts can't count, for lack of pinned keys
    pub unpinned: Vec<String>,
    // the totals are missing from an app older than them, and 0 then means "as listed"
    #[serde(default)]
    pub unpinned_total: usize,
    /// each trip's next departure and any later one with cargo, soonest first
    pub departures: Vec<Departure>,
    #[serde(default)]
    pub departures_total: usize,
    /// cargo no departure in the next two weeks takes
    pub unplanned: Vec<Unplanned>,
    #[serde(default)]
    pub unplanned_total: usize,
    pub manifest: Option<Manifest>,
    pub couriers: Vec<Courier>,
    #[serde(default)]
    pub couriers_total: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Message {
    pub id: String,
    pub class: Class,
    pub state: State,
    pub from_island: String,
    /// the operator who wrote it
    pub from: String,
    pub to: String,
    pub to_call: Option<String>,
    /// the station whose core signed it
    pub signed_by: String,
    /// third-party traffic: who it was written for
    pub on_behalf: Option<String>,
    pub text: String,
    pub file: Option<FileRef>,
    pub library: bool,
    pub sent_ms: i64,
    pub pickups: Vec<Hop>,
    pub dropoffs: Vec<Hop>,
    /// the first receipt from a key pinned for the destination
    pub receipt: Option<Receipt>,
    /// in transit on a registered trip: when that drive should reach the island
    pub expected_ms: Option<i64>,
}

/// One departure of a registered trip.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Run {
    /// the trip's key, as pickups name it
    pub trip: String,
    pub name: String,
    pub courier: String,
    pub from: String,
    pub to: String,
    pub depart_ms: i64,
    pub arrives_ms: i64,
    /// room for files, in bytes
    pub capacity: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Departure {
    #[serde(flatten)]
    pub run: Run,
    pub items: usize,
    pub bytes: u64,
    /// in loading order
    pub load: Vec<Cargo>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Unplanned {
    #[serde(flatten)]
    pub cargo: Cargo,
    pub why: Why,
}

/// The car's drive and what goes on it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Manifest {
    /// None on a drive that isn't a registered trip
    pub drive: Option<Run>,
    /// picked on the manifest page rather than worked out
    pub chosen: bool,
    /// the drive's room for files, in bytes
    pub room: u64,
    /// what this drive still takes, in loading order
    pub load: Vec<Item>,
    /// picked up and not receipted yet, so still aboard
    pub carried: Vec<Carried>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Item {
    #[serde(flatten)]
    pub cargo: Cargo,
    /// None when the core couldn't be asked
    pub aboard: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Carried {
    #[serde(flatten)]
    pub cargo: Cargo,
    /// a pinned core on the destination island holds it; the receipt isn't back yet
    pub handed_over: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Courier {
    pub call: String,
    pub trips: Vec<TripInfo>,
    /// registered departures due in the last 30 days
    pub due: usize,
    /// days in the last 30 on which a pinned car logged a pickup naming them
    pub drive_days: usize,
    pub last_pickup_ms: Option<i64>,
    pub last_dropoff_ms: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TripInfo {
    pub name: String,
    pub from: String,
    pub to: String,
    /// "daily at 17:30 UTC, about 2 h" or "Mon, Fri at 07:05 UTC, about 2.5 h"
    pub schedule: String,
    pub next_ms: Option<i64>,
    pub capacity: u64,
    pub note: String,
    /// the station that registered it
    pub by: String,
}

pub async fn mule_json(Axum(app): Axum<Arc<App>>) -> Result<Response, Fail> {
    // readers queue here for the snapshot being made rather than each making their own
    let mut last = app.last_json.lock().await;
    let body = match &*last {
        Some((at, body)) if at.elapsed() < FRESH => body.clone(),
        _ => {
            let body = Bytes::from(serde_json::to_vec(&snapshot(&app, now_ms()).await?)?);
            *last = Some((Instant::now(), body.clone()));
            body
        }
    };
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

/// Everything `top` shows, as of `now`.
pub async fn snapshot(app: &App, now: i64) -> Result<Snapshot> {
    let me = app.whoami().await.map_err(short_err);
    // the app's own reads of the feed say most; a core that never answered shows up here too
    let mut core_error = app.core_trouble(now).or_else(|| {
        me.as_ref()
            .err()
            .map(|e| format!("Can't reach the core: {e}."))
    });
    let me = me.ok();
    // the car's core is asked about its files before the database is locked
    let mut tonight = None;
    if let (Some(c), Some(w)) = (&app.cfg.car, &me) {
        let m = car::manifest(app, &w.key, now)?;
        let aboard = match car::aboard(app, &m).await {
            Ok(a) => a.into_iter().map(Some).collect(),
            Err(e) => {
                core_error.get_or_insert_with(|| {
                    format!("Can't ask the core what is aboard: {}.", short_err(e))
                });
                vec![None; m.load.len()]
            }
        };
        tonight = Some((m, aboard, c.capacity_mb.saturating_mul(1_000_000)));
    }
    let tp = app.cfg.third_party;
    let (active, all, delivered, n_delivered, seen) = {
        let db = app.db();
        (
            mule::active(&db, tp)?,
            trips::trips(&db)?,
            mule::list(&db, tp, &[State::Delivered], LIST, 0)?,
            mule::count(&db, tp, &[State::Delivered])?,
            mule::couriers(&db, now - trips::MONTH)?,
        )
    };
    let plan = plan::plan(plan::cargo(&active), &all, now);
    let waiting: Vec<&View> = active
        .iter()
        .filter(|v| v.state == State::Waiting)
        .collect();
    let moving: Vec<&View> = active
        .iter()
        .filter(|v| v.state != State::Waiting)
        .collect();
    let messages = waiting
        .iter()
        .take(LIST)
        .chain(moving.iter().take(LIST))
        .copied()
        .chain(&delivered)
        .map(|v| message(v, &all))
        .collect();
    let manifest = match (tonight, &me) {
        (Some((m, aboard, car_room)), Some(w)) => {
            let carried = mule::carrying(&active, &w.key)
                .into_iter()
                .map(|v| Carried {
                    cargo: plan::item(v),
                    handed_over: v.state == State::HandedOver,
                })
                .collect();
            Some(manifest(&m, aboard, carried, car_room))
        }
        _ => None,
    };
    let unpinned = crate::web::unpinned(&app.cfg, me.as_ref().map(|w| w.key.as_str()), &active);
    let (departures, departures_total) = departures(&plan);
    let (couriers, couriers_total) = couriers(&all, &seen, now);
    Ok(Snapshot {
        now_ms: now,
        island: app.cfg.island.clone(),
        operator: app.cfg.operator.clone(),
        call: me.as_ref().map(|w| w.call.clone()),
        core_error,
        waiting: waiting.len(),
        moving: moving.len(),
        delivered: n_delivered,
        messages,
        unpinned_total: unpinned.len(),
        unpinned: unpinned.into_iter().take(LIST).collect(),
        departures,
        departures_total,
        unplanned_total: plan.unplanned.len(),
        unplanned: plan
            .unplanned
            .iter()
            .take(LIST)
            .map(|(i, why)| Unplanned {
                cargo: plan.cargo[*i].clone(),
                why: *why,
            })
            .collect(),
        manifest,
        couriers,
        couriers_total,
    })
}

fn message(v: &View, trips: &[Trip]) -> Message {
    let m = &v.msg;
    let expected_ms = match (v.state, v.pickups.first()) {
        (State::InTransit, Some(h)) => h
            .trip
            .as_deref()
            .and_then(|key| trips::arrival(trips, key, h.ms)),
        _ => None,
    };
    Message {
        id: m.id.clone(),
        class: m.class,
        state: v.state,
        from_island: m.from_island.clone(),
        from: m.from_call.clone(),
        to: m.to_island.clone(),
        to_call: m.to_call.clone(),
        signed_by: m.author_call.clone(),
        on_behalf: m.on_behalf.clone(),
        text: m.text.clone(),
        file: m.file.clone(),
        library: m.is_library(),
        sent_ms: m.sent_ms,
        pickups: v.pickups.clone(),
        dropoffs: v.dropoffs.clone(),
        receipt: v.receipt.clone(),
        expected_ms,
    }
}

fn run(t: &Trip, depart_ms: i64) -> Run {
    Run {
        trip: t.key(),
        name: t.id.clone(),
        courier: t.courier.clone(),
        from: t.from.clone(),
        to: t.to.clone(),
        depart_ms,
        arrives_ms: depart_ms + t.length_ms(),
        capacity: t.capacity_bytes(),
    }
}

/// The plan's runs worth showing, the first LIST of them, and how many there are: each
/// trip's next one, and any later one that carries something because the earlier ones are
/// full.
fn departures(p: &Plan) -> (Vec<Departure>, usize) {
    let mut seen = HashSet::new();
    let runs: Vec<&plan::Run> = p
        .runs
        .iter()
        .filter(|r| seen.insert(r.trip.key()) || !r.load.is_empty())
        .collect();
    let listed = runs
        .iter()
        .take(LIST)
        .map(|r| Departure {
            run: run(&r.trip, r.depart_ms),
            items: r.load.len(),
            bytes: r.bytes,
            load: r
                .load
                .iter()
                .take(LIST)
                .map(|i| p.cargo[*i].clone())
                .collect(),
        })
        .collect();
    (listed, runs.len())
}

fn manifest(
    m: &car::Manifest,
    aboard: Vec<Option<bool>>,
    carried: Vec<Carried>,
    car_room: u64,
) -> Manifest {
    let (drive, chosen, room) = match &m.drive {
        Drive::Trip {
            trip,
            depart_ms,
            chosen,
        } => (Some(run(trip, *depart_ms)), *chosen, trip.capacity_bytes()),
        Drive::AdHoc => (None, false, car_room),
    };
    Manifest {
        drive,
        chosen,
        room,
        load: m
            .load
            .iter()
            .zip(aboard)
            .map(|(i, aboard)| Item {
                cargo: m.plan.cargo[*i].clone(),
                aboard,
            })
            .collect(),
        carried,
    }
}

/// The first LIST couriers, and how many there are.
fn couriers(all: &[Trip], seen: &[mule::Seen], now: i64) -> (Vec<Courier>, usize) {
    let all = trips::couriers(all, seen, now);
    let total = all.len();
    let listed = all
        .into_iter()
        .take(LIST)
        .map(|c| Courier {
            call: c.call.to_string(),
            trips: c
                .trips
                .iter()
                .map(|t| TripInfo {
                    name: t.id.clone(),
                    from: t.from.clone(),
                    to: t.to.clone(),
                    schedule: t.schedule_text(),
                    next_ms: t.next(now),
                    capacity: t.capacity_bytes(),
                    note: t.note.clone(),
                    by: t.author_call.clone(),
                })
                .collect(),
            due: c.due,
            drive_days: c.seen.map_or(0, |s| s.drive_days),
            last_pickup_ms: c.seen.and_then(|s| s.last_pickup),
            last_dropoff_ms: c.seen.and_then(|s| s.last_dropoff),
        })
        .collect();
    (listed, total)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeCore, author, event, file_msg, text_msg};
    use ridgewarden_client::{Author, Event};
    use serde_json::json;

    const MIN: i64 = 60_000;
    const H: i64 = 60 * MIN;

    /// A daily trip for N0DRV from north to south, leaving at the time of day of `depart`.
    fn trip(by: &Author, seq: u64, name: &str, depart: i64, hours: f64, mb: u64, ts: i64) -> Event {
        let m = depart.rem_euclid(86_400_000) / MIN;
        let p = json!({"trip": name, "courier": "N0DRV", "from": "north", "to": "south",
                       "days": "daily", "depart": format!("{:02}:{:02}", m / 60, m % 60),
                       "hours": hours, "capacity_mb": mb});
        event(by, seq, "mule.trip", p, ts)
    }

    async fn caught_up(app: &App) {
        app.pin().await.unwrap();
        ridgewarden_client::follow(&app.core, &app.db, crate::app::PROJECTIONS)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_island_sees_where_each_message_is_and_what_comes_next() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0north", d.path()).await;
        let (north, car, south) = (author("n0north"), author("n0car"), author("n0south"));
        let now = now_ms();
        let pick = |seq, m: &Event, trip: &str, ts| {
            let p = json!({"msg": m.id, "operator": "N0DRV", "trip": format!("{}:{trip}", &north.key[..8])});
            event(&car, seq, "mule.pickup", p, ts)
        };
        // early-south's drive ended two hours ago; late-south's ends in half an hour
        let early = trip(&north, 1, "early-south", now - 3 * H, 1.0, 10, now - 5 * H);
        let late = trip(
            &north,
            2,
            "late-south",
            now - 30 * MIN,
            1.0,
            10,
            now - 5 * H,
        );
        let overdue = text_msg(&north, 3, "urgent", now - 4 * H);
        let moving = text_msg(&north, 4, "routine", now - 40 * MIN);
        let waiting = text_msg(&north, 5, "routine", now - 10 * MIN);
        let far = event(
            &north,
            6,
            "mule.msg",
            json!({"from_island": "north", "to": "east", "from": "N0AAA", "class": "routine",
                   "text": "radio check"}),
            now - 5 * MIN,
        );
        let done = text_msg(&north, 7, "bulk", now - 6 * H);
        let events = [
            early,
            late,
            overdue.clone(),
            moving.clone(),
            waiting.clone(),
            far.clone(),
            done.clone(),
            pick(1, &overdue, "early-south", now - 3 * H - 30 * MIN),
            pick(2, &moving, "late-south", now - 35 * MIN),
            event(
                &car,
                3,
                "mule.pickup",
                json!({"msg": done.id, "operator": "N0DRV"}),
                now - 5 * H,
            ),
            event(
                &south,
                1,
                "mule.receipt",
                json!({"msg": done.id, "island": "south"}),
                now - 2 * H,
            ),
        ];
        for e in events {
            core.add(e);
        }
        let cfg = format!(
            "island = \"north\"\ncars = [\"{}\"]\n[[islands]]\nname = \"south\"\nkeys = [\"{}\"]",
            car.key, south.key
        );
        let app = crate::testing::app(d.path(), &core, &cfg);
        caught_up(&app).await;
        let s = snapshot(&app, now).await.unwrap();

        assert_eq!((s.waiting, s.moving, s.delivered), (2, 2, 1));
        assert_eq!(
            (s.call.as_deref(), s.manifest.is_none()),
            (Some("N0NORTH"), true)
        );
        assert_eq!(s.unpinned, ["east"], "no keys are pinned for east");
        let one = |m: &Event| s.messages.iter().find(|x| x.id == m.id).unwrap();
        let expected = |m: &Event| one(m).expected_ms.unwrap();
        assert!(expected(&overdue) < now, "early-south's drive is over");
        assert!(expected(&moving) > now && expected(&moving) <= now + 30 * MIN);
        assert_eq!(one(&waiting).expected_ms, None, "not on the road yet");
        assert_eq!(one(&done).receipt.as_ref().unwrap().site_call, "N0SOUTH");
        let order: Vec<State> = s.messages.iter().map(|m| m.state).collect();
        assert_eq!(
            order,
            [
                State::Waiting,
                State::Waiting,
                State::InTransit,
                State::InTransit,
                State::Delivered
            ]
        );

        let runs: Vec<(&str, usize)> = s
            .departures
            .iter()
            .map(|d| (d.run.name.as_str(), d.load.len()))
            .collect();
        assert_eq!(
            runs,
            [("early-south", 1), ("late-south", 0)],
            "each trip's next run, the waiting message on the first"
        );
        assert_eq!(s.departures[0].load[0].msg, waiting.id);
        assert!(s.departures[0].run.depart_ms > now);
        assert_eq!(
            (
                s.unplanned.len(),
                s.unplanned[0].why,
                s.unplanned[0].cargo.msg.as_str()
            ),
            (1, Why::NoTrip, far.id.as_str())
        );

        let c = &s.couriers[0];
        assert_eq!((c.call.as_str(), c.trips.len(), c.due), ("N0DRV", 2, 2));
        assert_eq!(c.last_pickup_ms, Some(now - 35 * MIN));
        assert!(c.trips.iter().all(|t| t.next_ms.is_some_and(|n| n > now)));

        // other tools read this too, so the names and shape are part of the page
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Snapshot>(&text).unwrap(), s);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["unplanned"][0]["why"], "no_trip");
        assert_eq!(v["unplanned"][0]["to"], "east");
        assert_eq!(v["departures"][0]["name"], "early-south");
        assert_eq!(v["departures"][0]["load"][0]["class"], "routine");
        assert_eq!(v["messages"][2]["state"], "in_transit");
        assert_eq!(v["messages"][2]["class"], "urgent");
        assert_eq!(v["messages"][4]["receipt"]["site_call"], "N0SOUTH");
    }

    #[test]
    fn departures_are_each_trips_next_run_and_later_ones_with_cargo() {
        // 2026-11-20 00:00 UTC, a Friday
        const FRI: i64 = 1_795_132_800_000;
        let trip = |id: &str, days: &str, depart: &str| Trip {
            author: "ab".repeat(32),
            author_call: "N0AAA".into(),
            id: id.into(),
            courier: "N0DRV".into(),
            from: "north".into(),
            to: "south".into(),
            days: trips::parse_days(days).unwrap(),
            depart_min: trips::parse_hhmm(depart).unwrap(),
            hours: 2.0,
            capacity_mb: 1,
            note: String::new(),
            ts: 0,
        };
        let cargo = |id: &str| Cargo {
            msg: id.into(),
            author: "k".into(),
            label: id.into(),
            from: "north".into(),
            to: "south".into(),
            class: Class::Bulk,
            since_ms: 0,
            bytes: 600_000,
            root: Some("cd".repeat(32)),
            library: false,
        };
        let trips = [trip("daily", "daily", "17:00"), trip("sat", "sat", "09:00")];
        let p = plan::plan(vec![cargo("a"), cargo("b"), cargo("c")], &trips, FRI);
        let (listed, total) = departures(&p);
        assert_eq!(total, 3);
        let got: Vec<(String, i64, Vec<String>)> = listed
            .into_iter()
            .map(|d| {
                let load = d.load.into_iter().map(|c| c.msg).collect();
                (d.run.name, (d.run.depart_ms - FRI) / H, load)
            })
            .collect();
        let want =
            |n: &str, h, l: &[&str]| (n.to_string(), h, l.iter().map(|s| s.to_string()).collect());
        assert_eq!(
            got,
            [
                want("daily", 17, &["a"]),
                want("sat", 24 + 9, &["b"]),
                want("daily", 24 + 17, &["c"]),
            ],
            "0.6 MB files take the first departure with 1 MB free; the empty ones after are left out"
        );
    }

    #[tokio::test]
    async fn a_car_shows_its_drive_and_what_is_aboard() {
        let d = tempfile::tempdir().unwrap();
        let car = FakeCore::start("n0car", d.path()).await;
        let north = author("n0north");
        let now = now_ms();
        let (man, file, root) = file_msg(&north, 1, "bulk", b"tiles", now - 30 * MIN);
        let text = text_msg(&north, 3, "urgent", now - 20 * MIN);
        let carried = text_msg(&north, 4, "routine", now - 3 * H);
        let evening = trip(&north, 5, "evening-south", now + H, 2.0, 100, now - 5 * H);
        for e in [man, file.clone(), text.clone(), carried.clone(), evening] {
            car.add(e);
        }
        let p = json!({"msg": carried.id, "operator": "N0DRV"});
        car.add(event(&author("n0car"), 1, "mule.pickup", p, now - 2 * H));
        car.blob(&root, 5, false);
        let cfg = format!(
            "operator = \"N0DRV\"\n[car]\n[[islands]]\nname = \"north\"\nkeys = [\"{}\"]",
            north.key
        );
        let app = crate::testing::app(d.path(), &car, &cfg);
        caught_up(&app).await;
        let s = snapshot(&app, now).await.unwrap();
        let m = s.manifest.as_ref().unwrap();
        assert_eq!(m.drive.as_ref().unwrap().name, "evening-south");
        assert_eq!((m.room, m.chosen), (100_000_000, false));
        let load: Vec<(&str, Option<bool>)> = m
            .load
            .iter()
            .map(|i| (i.cargo.msg.as_str(), i.aboard))
            .collect();
        assert_eq!(
            load,
            [
                (text.id.as_str(), Some(true)),
                (file.id.as_str(), Some(false))
            ],
            "text is aboard at once, the file is still loading"
        );
        let held: Vec<&str> = m.carried.iter().map(|c| c.cargo.msg.as_str()).collect();
        assert_eq!(held, [carried.id.as_str()]);
        assert_eq!(s.core_error, None);

        car.state.lock().unwrap().broken = true;
        let s = snapshot(&app, now).await.unwrap();
        assert!(s.manifest.unwrap().load.iter().all(|i| i.aboard.is_none()));
        let e = s.core_error.unwrap();
        assert!(e.starts_with("Can't ask the core what is aboard: "), "{e}");
        assert!(
            e.contains("503") && !e.contains("http://"),
            "short, and no URL: {e}"
        );
    }

    #[tokio::test]
    async fn a_car_whose_core_never_answered_has_no_manifest() {
        let d = tempfile::tempdir().unwrap();
        let app = crate::testing::app(d.path(), &FakeCore::closed(), "operator = \"N0DRV\"\n[car]");
        let s = snapshot(&app, now_ms()).await.unwrap();
        assert_eq!((s.call, s.manifest), (None, None));
        assert_eq!(
            s.core_error.as_deref(),
            Some("Can't reach the core: could not connect.")
        );
    }

    #[tokio::test]
    async fn a_core_that_goes_away_shows_in_the_json_and_on_the_pages() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0north", d.path()).await;
        core.add(text_msg(&author("n0north"), 1, "urgent", now_ms()));
        let app = Arc::new(crate::testing::app(d.path(), &core, "island = \"north\""));
        crate::round(app.clone()).await.unwrap();
        let s = snapshot(&app, now_ms()).await.unwrap();
        assert_eq!((s.core_error, s.waiting), (None, 1));

        core.stop();
        let until = Instant::now() + Duration::from_secs(30);
        while crate::round(app.clone()).await.is_ok() {
            assert!(
                Instant::now() < until,
                "the app never noticed the core had gone"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let s = snapshot(&app, now_ms()).await.unwrap();
        let e = s.core_error.unwrap();
        assert!(
            e.starts_with("Can't read the core: ") && e.contains("The last good read was"),
            "{e}"
        );
        assert_eq!(s.waiting, 1, "what the app had read is still shown");
        let page = crate::web::page(&app, "/", "Mule", "", false).await.0;
        // the page escapes the apostrophe; a browser shows "Can't"
        assert!(page.contains("Can&#39;t read the core: "), "{page}");
        assert!(page.contains("What this page shows may be out of date."));
    }

    #[test]
    fn a_core_read_long_ago_is_called_stale() {
        let d = tempfile::tempdir().unwrap();
        let app = crate::testing::app(d.path(), &FakeCore::closed(), "island = \"north\"");
        assert_eq!(
            app.core_trouble(0),
            None,
            "nothing read yet, nothing to say"
        );
        app.read_core(None, 0);
        assert_eq!(app.core_trouble(2 * 60_000), None);
        assert_eq!(
            app.core_trouble(2 * 60_000 + 1).as_deref(),
            Some("The core was last read 2 min ago.")
        );
        app.read_core(Some("could not connect".into()), 3 * 60_000);
        assert_eq!(
            app.core_trouble(3 * 60_000).as_deref(),
            Some("Can't read the core: could not connect. The last good read was 3 min ago.")
        );
        app.read_core(None, 4 * 60_000);
        assert_eq!(app.core_trouble(4 * 60_000), None, "a good read clears it");
    }

    #[tokio::test]
    async fn long_lists_stop_at_list_and_say_how_many_there_are() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0north", d.path()).await;
        let north = author("n0north");
        let now = now_ms();
        let mut seq = 0;
        // each for an island with no courier and no pinned keys
        for i in 0..LIST + 5 {
            seq += 1;
            let p = json!({"from_island": "north", "to": format!("i{i}"), "from": "N0AAA",
                           "class": "routine", "text": "radio check"});
            core.add(event(&north, seq, "mule.msg", p, now - 10 * MIN));
        }
        for i in 0..=LIST {
            seq += 1;
            let p = json!({"trip": format!("t{i}"), "courier": format!("N0D{i}"), "from": "north",
                           "to": "south", "days": "daily", "depart": "12:00", "hours": 1,
                           "capacity_mb": 10});
            core.add(event(&north, seq, "mule.trip", p, now - H));
        }
        let app = crate::testing::app(d.path(), &core, "island = \"north\"");
        caught_up(&app).await;
        let s = snapshot(&app, now).await.unwrap();
        assert_eq!((s.waiting, s.messages.len()), (LIST + 5, LIST));
        assert_eq!((s.unplanned_total, s.unplanned.len()), (LIST + 5, LIST));
        assert_eq!((s.unpinned_total, s.unpinned.len()), (LIST + 5, LIST));
        assert_eq!((s.departures_total, s.departures.len()), (LIST + 1, LIST));
        assert_eq!((s.couriers_total, s.couriers.len()), (LIST + 1, LIST));
    }

    #[tokio::test]
    async fn a_car_with_no_trip_takes_what_has_no_courier_soon() {
        let d = tempfile::tempdir().unwrap();
        let car = FakeCore::start("n0car", d.path()).await;
        let north = author("n0north");
        let now = now_ms();
        let (man, file, root) = file_msg(&north, 1, "bulk", b"tiles", now - 30 * MIN);
        let text = text_msg(&north, 3, "urgent", now - 20 * MIN);
        for e in [man, file.clone(), text.clone()] {
            car.add(e);
        }
        car.blob(&root, 5, true);
        let cfg = format!(
            "operator = \"N0DRV\"\n[car]\ncapacity_mb = 3\n[[islands]]\nname = \"north\"\nkeys = [\"{}\"]",
            north.key
        );
        let app = crate::testing::app(d.path(), &car, &cfg);
        caught_up(&app).await;
        let m = snapshot(&app, now).await.unwrap().manifest.unwrap();
        assert_eq!((m.drive, m.chosen, m.room), (None, false, 3_000_000));
        let load: Vec<(&str, Option<bool>)> = m
            .load
            .iter()
            .map(|i| (i.cargo.msg.as_str(), i.aboard))
            .collect();
        assert_eq!(
            load,
            [
                (text.id.as_str(), Some(true)),
                (file.id.as_str(), Some(true))
            ]
        );
    }

    #[tokio::test]
    async fn third_party_traffic_is_left_out_unless_the_site_carries_it() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0north", d.path()).await;
        let p = json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "routine",
                       "text": "tell my sister I'm fine", "for": "Pat"});
        core.add(event(&author("n0north"), 1, "mule.msg", p, now_ms()));
        for (third_party, waiting) in [(false, 0), (true, 1)] {
            let dir = tempfile::tempdir().unwrap();
            let cfg = format!("island = \"north\"\nthird_party = {third_party}");
            let app = crate::testing::app(dir.path(), &core, &cfg);
            caught_up(&app).await;
            let s = snapshot(&app, now_ms()).await.unwrap();
            assert_eq!((s.waiting, s.messages.len()), (waiting, waiting), "{cfg}");
        }
    }

    #[tokio::test]
    async fn readers_close_together_share_one_snapshot() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0north", d.path()).await;
        let app = Arc::new(crate::testing::app(d.path(), &core, "island = \"north\""));
        let read = || async {
            let Ok(r) = mule_json(Axum(app.clone())).await else {
                panic!("/api/mule failed")
            };
            assert_eq!(r.headers()[header::CONTENT_TYPE], "application/json");
            axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap()
        };
        let first = read().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(read().await, first, "the same answer, made once");
        let s: Snapshot = serde_json::from_slice(&first).unwrap();
        assert_eq!(s.island.as_deref(), Some("north"));
    }
}
