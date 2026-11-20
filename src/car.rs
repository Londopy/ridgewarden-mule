//! The car's side. It works out which drive it is on and loads only that drive's share of
//! the files (tonight's manifest), logs what it has aboard, and logs when a pinned core on
//! the destination island holds what it carried, then lets the files go. Each record is
//! signed by the car's core and names the control operator.

use std::collections::BTreeSet;

use anyhow::Result;
use ridgewarden_client::util::now_ms;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::app::{App, short_err};
use crate::held::{Remote, holds};
use crate::mule::{View, carrying, manifest_matches, to_drop_off, to_pick_up};
use crate::plan::{Plan, ad_hoc};
use crate::trips::{Trip, trips};

/// How long before a registered departure the car starts loading for it.
pub const LOAD_AHEAD_MS: i64 = 18 * 3_600_000;
/// A drive the driver picked by hand counts for this long.
const CHOICE_MS: i64 = 24 * 3_600_000;

#[derive(Clone, Debug, PartialEq)]
pub enum Drive {
    Trip {
        trip: Box<Trip>,
        depart_ms: i64,
        chosen: bool,
    },
    /// not a registered trip: take what has no courier soon, up to the car's room
    AdHoc,
}

/// The drive the driver picked on the car's page, if recent: Some("") means "not a
/// registered trip".
pub fn choice(conn: &Connection, now: i64) -> Result<Option<(String, i64)>> {
    Ok(conn
        .query_row(
            "SELECT trip, depart_ms FROM car_trip WHERE id = 1 AND chosen_ms > ?1",
            params![now - CHOICE_MS],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Record the driver's pick; None picks "not a registered trip".
pub fn choose(conn: &Connection, trip: Option<(&str, i64)>, now: i64) -> Result<()> {
    let (key, dep) = trip.unwrap_or(("", 0));
    conn.execute(
        "INSERT INTO car_trip (id, trip, depart_ms, chosen_ms) VALUES (1, ?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET trip = excluded.trip, depart_ms = excluded.depart_ms,
             chosen_ms = excluded.chosen_ms",
        params![key, dep, now],
    )?;
    Ok(())
}

pub fn forget_choice(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM car_trip", [])?;
    Ok(())
}

/// Which drive this car is on: the one picked on its page, else its operator's registered
/// trip that is on the road now or leaves soonest within the loading window.
pub fn drive(trips: &[Trip], operator: &str, choice: Option<(String, i64)>, now: i64) -> Drive {
    if let Some((key, dep)) = choice {
        return match trips.iter().find(|t| t.key() == key) {
            Some(t) if dep + t.length_ms() > now => Drive::Trip {
                trip: Box::new(t.clone()),
                depart_ms: dep,
                chosen: true,
            },
            _ => Drive::AdHoc,
        };
    }
    trips
        .iter()
        .filter(|t| t.courier == operator)
        .flat_map(|t| {
            t.runs(now, now + LOAD_AHEAD_MS)
                .into_iter()
                .map(move |d| (d, t))
        })
        .min_by_key(|(d, _)| *d)
        .map_or(Drive::AdHoc, |(d, t)| Drive::Trip {
            trip: Box::new(t.clone()),
            depart_ms: d,
            chosen: false,
        })
}

/// Tonight's manifest: the drive, the plan it was cut from, and what this car takes.
pub struct Manifest {
    pub drive: Drive,
    pub plan: Plan,
    pub load: Vec<usize>,
}

/// Bytes of files this car is still carrying.
pub fn aboard_bytes(views: &[View], me: &str) -> u64 {
    carrying(views, me)
        .iter()
        .map(|v| v.msg.file.as_ref().map_or(0, |f| f.size))
        .sum()
}

/// Keep what fits in `room`, in order.
fn fit(p: &Plan, load: Vec<usize>, mut room: u64) -> Vec<usize> {
    load.into_iter()
        .filter(|i| {
            let n = p.cargo[*i].bytes;
            let ok = n <= room;
            if ok {
                room -= n;
            }
            ok
        })
        .collect()
}

/// This car's drive and load, given what it is already carrying: never more than the
/// trip's room or the car's own, whichever is smaller.
pub fn manifest(app: &App, me: &str, now: i64) -> Result<Manifest> {
    let choice = choice(&app.db(), now)?;
    let all = trips(&app.db())?;
    let drive = drive(&all, &app.cfg.operator, choice, now);
    let car_room = app.cfg.car.as_ref().map_or(0, |c| c.capacity_mb) * 1_000_000;
    let held = aboard_bytes(&app.active()?, me);
    Ok(match &drive {
        Drive::Trip {
            trip, depart_ms, ..
        } => {
            // a drive already on the road keeps the load it left with
            let plan = app.plan((*depart_ms).min(now))?;
            let load = plan
                .runs
                .iter()
                .find(|r| r.trip.key() == trip.key() && r.depart_ms == *depart_ms)
                .map(|r| r.load.clone())
                .unwrap_or_default();
            let room = trip.capacity_bytes().min(car_room).saturating_sub(held);
            let load = fit(&plan, load, room);
            Manifest { drive, plan, load }
        }
        Drive::AdHoc => {
            let plan = app.plan(now)?;
            let load = ad_hoc(&plan, car_room.saturating_sub(held), now);
            Manifest { drive, plan, load }
        }
    })
}

/// Each step on its own, so a problem with one file or one island doesn't stop the rest.
pub async fn round(app: &App) -> Result<()> {
    let me = app.pin().await?;
    let views = app.active()?;
    let m = manifest(app, &me.key, now_ms())?;
    if let Err(e) = load(app, &views, &m, &me.key).await {
        tracing::warn!("loading: {e:#}");
    }
    if let Err(e) = pick_up(app, &views, &m, &me.key).await {
        tracing::warn!("logging pickups: {e:#}");
    }
    drop_off(app, &views, &me.key).await
}

/// The first characters of a key or id, for log lines; whatever a peer sends.
pub fn short(s: &str) -> &str {
    s.get(..12).unwrap_or(s)
}

/// Hold the files this car should: this drive's share, and whatever it has aboard that
/// the destination hasn't receipted yet. Let go of everything else.
async fn load(app: &App, views: &[View], m: &Manifest, me: &str) -> Result<()> {
    let mut keep: BTreeSet<String> = BTreeSet::new();
    let mut fetch: Vec<(String, String, u64)> = vec![];
    for i in &m.load {
        let c = &m.plan.cargo[*i];
        let Some(root) = &c.root else { continue };
        keep.insert(root.clone());
        fetch.push((root.clone(), c.author.clone(), c.bytes));
    }
    for v in carrying(views, me) {
        if let Some(f) = &v.msg.file {
            keep.insert(f.root.clone());
        }
    }
    let had: Vec<String> = app
        .db()
        .prepare("SELECT root FROM wanted")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for root in &had {
        if keep.contains(root) {
            continue;
        }
        let freed = app.core.blob_unwant(root).await?;
        app.db()
            .execute("DELETE FROM wanted WHERE root = ?1", params![root])?;
        tracing::info!("let go of {} ({freed} bytes freed)", short(root));
    }
    for (root, author, size) in fetch {
        if had.contains(&root) {
            continue;
        }
        // the event claims a size; only fetch what the publisher's own manifest agrees with
        match manifest_matches(&app.db(), &root, &author, size)? {
            Some(true) => {}
            Some(false) => {
                tracing::warn!(
                    "not loading {}: its manifest and the event naming it disagree on the size",
                    short(&root)
                );
                continue;
            }
            None => continue,
        }
        // the core refuses manifests it can't check; one bad file must not hold up the rest
        if let Err(e) = app.core.blob_want(&root).await {
            tracing::warn!("not loading {}: {e:#}", short(&root));
            continue;
        }
        app.db().execute(
            "INSERT OR IGNORE INTO wanted (root, ms) VALUES (?1, ?2)",
            params![root, now_ms()],
        )?;
        tracing::info!("loading {} ({size} bytes)", short(&root));
    }
    Ok(())
}

async fn pick_up(app: &App, views: &[View], m: &Manifest, me: &str) -> Result<()> {
    let trip = match &m.drive {
        Drive::Trip { trip, .. } => Some(trip.key()),
        Drive::AdHoc => None,
    };
    for v in to_pick_up(views, me) {
        if !holds(&app.http, &app.core, &v.msg).await? {
            continue;
        }
        let mut p = json!({"msg": v.msg.id, "operator": app.cfg.operator});
        if let Some(t) = &trip {
            p["trip"] = json!(t);
        }
        app.core.append("mule.pickup", p).await?;
        tracing::info!(
            "picked up {} ({}, {} to {})",
            &v.msg.id[..12],
            v.msg.class.name(),
            v.msg.from_island,
            v.msg.to_island
        );
    }
    Ok(())
}

async fn drop_off(app: &App, views: &[View], me: &str) -> Result<()> {
    let mut carried = to_drop_off(views, me);
    let islands: BTreeSet<String> = carried.iter().map(|v| v.msg.to_island.clone()).collect();
    for island in islands {
        let pinned = app.cfg.island_keys(&island, None);
        for url in app.cfg.island_cores(&island) {
            let there = match Remote::connect(url).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("{island} core {url}: {}", short_err(&e));
                    continue;
                }
            };
            if !pinned.contains(&there.key) {
                tracing::warn!(
                    "{url} answers as {}, which is not one of {island}'s keys under [[islands]]; not counting it",
                    short(&there.key)
                );
                continue;
            }
            let mut left = vec![];
            for v in carried {
                if v.msg.to_island != island || !holds(&app.http, &there.core, &v.msg).await? {
                    left.push(v);
                    continue;
                }
                app.core
                    .append(
                        "mule.dropoff",
                        json!({"msg": v.msg.id, "operator": app.cfg.operator, "island": island}),
                    )
                    .await?;
                tracing::info!(
                    "dropped off {} at {island} ({})",
                    &v.msg.id[..12],
                    there.url
                );
            }
            carried = left;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeCore, author, event, file_msg, text_msg};
    use crate::trips::{parse_days, parse_hhmm};

    async fn step(app: &App) {
        ridgewarden_client::follow(&app.core, &app.db, crate::app::PROJECTIONS)
            .await
            .unwrap();
        round(app).await.unwrap();
    }

    #[tokio::test]
    async fn the_car_loads_its_share_picks_up_drops_off_and_lets_go() {
        let d = tempfile::tempdir().unwrap();
        let car = FakeCore::start("n0car", d.path()).await;
        let south = FakeCore::start("n0south", d.path()).await;
        let north = author("n0north");
        let now = now_ms();
        let (man, msg, root) = file_msg(&north, 1, "bulk", b"tiles", now - 60_000);
        let text = text_msg(&north, 3, "urgent", now - 60_000);
        let leaves = (now + 3_600_000).rem_euclid(86_400_000) / 60_000;
        let trip = event(
            &north,
            4,
            "mule.trip",
            json!({"trip": "daily-south", "courier": "N0DRV", "from": "north", "to": "south",
                   "days": "daily", "depart": format!("{:02}:{:02}", leaves / 60, leaves % 60),
                   "hours": 3, "capacity_mb": 100}),
            now - 60_000,
        );
        for e in [man, msg.clone(), text.clone(), trip] {
            car.add(e);
        }
        car.blob(&root, 5, false);
        let cfg = format!(
            "operator = \"N0DRV\"
[car]
[[islands]]
name = \"north\"
keys = [\"{}\"]
             [[islands]]
name = \"south\"
keys = [\"{}\"]
cores = [\"{}\"]",
            north.key,
            south.key(),
            south.url
        );
        let app = crate::testing::app(d.path(), &car, &cfg);
        step(&app).await;
        let pickups = |c: &FakeCore| -> Vec<serde_json::Value> {
            c.state
                .lock()
                .unwrap()
                .appended("mule.pickup")
                .iter()
                .map(|e| e.payload.clone())
                .collect()
        };
        assert_eq!(
            car.state.lock().unwrap().wants,
            [root.as_str()],
            "the file on this drive's manifest"
        );
        let trip_key = format!("{}:daily-south", &north.key[..8]);
        assert_eq!(
            pickups(&car),
            [json!({"msg": text.id, "operator": "N0DRV", "trip": trip_key})],
            "text is aboard at once; the file isn't yet"
        );
        car.blob(&root, 5, true);
        step(&app).await;
        assert_eq!(pickups(&car).len(), 2);
        assert!(
            car.state
                .lock()
                .unwrap()
                .appended("mule.dropoff")
                .is_empty()
        );
        south.add(msg.clone());
        south.add(text.clone());
        south.blob(&root, 5, true);
        step(&app).await;
        let drops: Vec<String> = car
            .state
            .lock()
            .unwrap()
            .appended("mule.dropoff")
            .iter()
            .map(|e| e.payload["msg"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(drops.len(), 2);
        assert!(drops.contains(&msg.id) && drops.contains(&text.id));
        step(&app).await;
        assert!(
            car.state.lock().unwrap().unwants.is_empty(),
            "handed over, but kept until the island says it has it"
        );
        let south_core = crate::testing::author("n0south");
        car.add(event(
            &south_core,
            1,
            "mule.receipt",
            json!({"msg": msg.id, "island": "south"}),
            now_ms(),
        ));
        step(&app).await;
        assert_eq!(
            car.state.lock().unwrap().unwants,
            [root],
            "receipted, so the car lets it go"
        );
    }

    #[tokio::test]
    async fn the_car_never_loads_more_than_it_has_room_for() {
        let d = tempfile::tempdir().unwrap();
        let car = FakeCore::start("n0car", d.path()).await;
        let north = author("n0north");
        let now = now_ms();
        let mut roots = vec![];
        for (seq, fill) in [(1, b'a'), (3, b'b')] {
            let (man, msg, root) =
                file_msg(&north, seq, "bulk", &vec![fill; 3_000_000], now - 60_000);
            car.add(man);
            car.add(msg);
            car.blob(&root, 3_000_000, false);
            roots.push(root);
        }
        let cfg = format!(
            "operator = \"N0DRV\"\n[car]\ncapacity_mb = 5\n[[islands]]\nname = \"north\"\nkeys = [\"{}\"]",
            north.key
        );
        let app = crate::testing::app(d.path(), &car, &cfg);
        step(&app).await;
        assert_eq!(
            car.state.lock().unwrap().wants.len(),
            1,
            "3 MB of 5 fits, 6 doesn't"
        );
        let first = car.state.lock().unwrap().wants[0].clone();
        car.blob(&first, 3_000_000, true);
        step(&app).await;
        step(&app).await;
        assert_eq!(car.state.lock().unwrap().appended("mule.pickup").len(), 1);
        assert_eq!(
            car.state.lock().unwrap().wants.len(),
            1,
            "what is aboard still takes up room"
        );
    }

    #[tokio::test]
    async fn a_core_that_is_not_pinned_for_the_island_gets_no_drop_off() {
        let d = tempfile::tempdir().unwrap();
        let car = FakeCore::start("n0car", d.path()).await;
        let impostor = FakeCore::start("n0impostor", d.path()).await;
        let north = author("n0north");
        let text = text_msg(&north, 1, "urgent", now_ms());
        car.add(text.clone());
        impostor.add(text);
        let cfg = format!(
            "operator = \"N0DRV\"
[car]
[[islands]]
name = \"north\"
keys = [\"{}\"]
             [[islands]]
name = \"south\"
keys = [\"{}\"]
cores = [\"{}\"]",
            north.key,
            crate::testing::key("n0south"),
            impostor.url
        );
        let app = crate::testing::app(d.path(), &car, &cfg);
        step(&app).await;
        step(&app).await;
        assert_eq!(car.state.lock().unwrap().appended("mule.pickup").len(), 1);
        assert!(
            car.state
                .lock()
                .unwrap()
                .appended("mule.dropoff")
                .is_empty()
        );
    }

    const FRI: i64 = 1_795_132_800_000;
    const H: i64 = 3_600_000;

    fn trip(id: &str, courier: &str, days: &str) -> Trip {
        Trip {
            author: "ab".repeat(32),
            author_call: "N0AAA".into(),
            id: id.into(),
            courier: courier.into(),
            from: "north".into(),
            to: "south".into(),
            days: parse_days(days).unwrap(),
            depart_min: parse_hhmm("17:00").unwrap(),
            hours: 3.0,
            capacity_mb: 100,
            note: String::new(),
            ts: 0,
        }
    }

    fn departs(d: Drive) -> Option<(String, i64, bool)> {
        match d {
            Drive::Trip {
                trip,
                depart_ms,
                chosen,
            } => Some((trip.id.clone(), depart_ms, chosen)),
            Drive::AdHoc => None,
        }
    }

    #[test]
    fn the_car_takes_its_operators_next_trip() {
        let t = [
            trip("fri", "N0DRV", "fri"),
            trip("sat", "N0DRV", "sat"),
            trip("other", "N0XYZ", "fri"),
        ];
        let next = departs(drive(&t, "N0DRV", None, FRI + 9 * H));
        assert_eq!(next, Some(("fri".into(), FRI + 17 * H, false)));
        let on_the_road = departs(drive(&t, "N0DRV", None, FRI + 18 * H));
        assert_eq!(on_the_road, Some(("fri".into(), FRI + 17 * H, false)));
        assert_eq!(drive(&t, "N0NOBODY", None, FRI), Drive::AdHoc);
        // Friday's has arrived and the next one is more than the loading window away
        assert_eq!(drive(&t[..1], "N0DRV", None, FRI + 21 * H), Drive::AdHoc);
    }

    #[test]
    fn a_picked_drive_wins_until_it_is_over() {
        let t = [trip("fri", "N0DRV", "fri"), trip("other", "N0XYZ", "sat")];
        let pick = Some((t[1].key(), FRI + 41 * H));
        let got = departs(drive(&t, "N0DRV", pick.clone(), FRI));
        assert_eq!(got, Some(("other".into(), FRI + 41 * H, true)));
        assert_eq!(drive(&t, "N0DRV", pick, FRI + 45 * H), Drive::AdHoc);
        assert_eq!(
            drive(&t, "N0DRV", Some((String::new(), 0)), FRI + 9 * H),
            Drive::AdHoc,
            "the driver said this isn't a registered trip"
        );
    }
}
