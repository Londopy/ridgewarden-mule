//! What goes with whom. Every waiting message is cargo (offline-library updates arrive as
//! messages too); cargo is ranked by priority class, then by how long it has waited, and
//! put on the first registered departure on its corridor with room left for its file.
//! Text rides in the event log and takes no room, so it goes with the first departure.
//!
//! Every site runs the same plan over the same events, so an island sees what will leave
//! with whom, and a car loads only its own share instead of every file for every island.

use crate::mule::{Class, State, View};
use crate::trips::Trip;

/// How far ahead the plan looks.
pub const HORIZON_MS: i64 = 14 * 86_400_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Cargo {
    pub msg: String,
    /// who signed it, so the car can check the file's manifest against its claim
    pub author: String,
    pub label: String,
    pub from: String,
    pub to: String,
    pub class: Class,
    pub since_ms: i64,
    pub bytes: u64,
    /// the blob to load, if there is a file
    pub root: Option<String>,
    /// an update from ridgewarden-library
    pub library: bool,
}

#[derive(Clone, Debug)]
pub struct Run {
    pub trip: Trip,
    pub depart_ms: i64,
    /// indexes into `Plan::cargo`, in loading order
    pub load: Vec<usize>,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    NoTrip,
    TooBig,
}

impl Why {
    pub fn text(self) -> &'static str {
        match self {
            Why::NoTrip => "no courier registered on this corridor",
            Why::TooBig => "no room in the next two weeks of trips",
        }
    }
}

#[derive(Debug, Default)]
pub struct Plan {
    /// ranked: urgent first, then oldest first
    pub cargo: Vec<Cargo>,
    pub runs: Vec<Run>,
    pub unplanned: Vec<(usize, Why)>,
}

impl Plan {
    pub fn run_of(&self, i: usize) -> Option<&Run> {
        self.runs.iter().find(|r| r.load.contains(&i))
    }

    pub fn why(&self, i: usize) -> Option<Why> {
        self.unplanned
            .iter()
            .find(|(j, _)| *j == i)
            .map(|(_, w)| *w)
    }

    pub fn find(&self, msg: &str) -> Option<usize> {
        self.cargo.iter().position(|c| c.msg == msg)
    }
}

/// Waiting messages from senders pinned for their island, ranked.
pub fn cargo(views: &[View]) -> Vec<Cargo> {
    let mut out: Vec<Cargo> = views
        .iter()
        .filter(|v| v.state == State::Waiting && v.pinned)
        .map(|v| {
            let m = &v.msg;
            let mut label: String = m.text.chars().take(60).collect();
            if let Some(f) = &m.file {
                label = if label.is_empty() {
                    f.name.clone()
                } else {
                    format!("{label} [{}]", f.name)
                };
            }
            Cargo {
                msg: m.id.clone(),
                author: m.author.clone(),
                label,
                from: m.from_island.clone(),
                to: m.to_island.clone(),
                class: m.class,
                since_ms: m.sent_ms,
                bytes: m.file.as_ref().map_or(0, |f| f.size),
                root: m.file.as_ref().map(|f| f.root.clone()),
                library: m.is_library(),
            }
        })
        .collect();
    out.sort_by_key(|c| (c.class, c.since_ms));
    out
}

/// Put each piece of cargo on the first departure after `now` on its corridor with room.
pub fn plan(cargo: Vec<Cargo>, trips: &[Trip], now: i64) -> Plan {
    let mut runs: Vec<Run> = trips
        .iter()
        .flat_map(|t| {
            t.runs(now, now + HORIZON_MS)
                .into_iter()
                .filter(|d| *d >= now)
                .map(|d| Run {
                    trip: t.clone(),
                    depart_ms: d,
                    load: vec![],
                    bytes: 0,
                })
        })
        .collect();
    runs.sort_by_key(|r| r.depart_ms);
    let mut unplanned = vec![];
    for (i, c) in cargo.iter().enumerate() {
        let mut on_corridor = runs
            .iter_mut()
            .filter(|r| r.trip.from == c.from && r.trip.to == c.to)
            .peekable();
        if on_corridor.peek().is_none() {
            unplanned.push((i, Why::NoTrip));
            continue;
        }
        match on_corridor.find(|r| r.bytes + c.bytes <= r.trip.capacity_bytes()) {
            Some(r) => {
                r.load.push(i);
                r.bytes += c.bytes;
            }
            None => unplanned.push((i, Why::TooBig)),
        }
    }
    Plan {
        cargo,
        runs,
        unplanned,
    }
}

/// A drive that isn't a registered trip: whatever has no scheduled departure in the next
/// day, best first, as long as it fits.
pub fn ad_hoc(p: &Plan, capacity: u64, now: i64) -> Vec<usize> {
    let mut room = capacity;
    let mut out = vec![];
    for (i, c) in p.cargo.iter().enumerate() {
        if p.run_of(i).is_some_and(|r| r.depart_ms < now + 86_400_000) {
            continue;
        }
        if c.bytes <= room {
            room -= c.bytes;
            out.push(i);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trips::{parse_days, parse_hhmm};

    // 2026-11-20 00:00 UTC, a Friday
    const FRI: i64 = 1_795_132_800_000;
    const H: i64 = 3_600_000;
    const MB: u64 = 1_000_000;

    fn trip(id: &str, days: &str, from: &str, to: &str, capacity_mb: u64) -> Trip {
        Trip {
            author: "ab".repeat(32),
            author_call: "N0AAA".into(),
            id: id.into(),
            courier: "N0DRV".into(),
            from: from.into(),
            to: to.into(),
            days: parse_days(days).unwrap(),
            depart_min: parse_hhmm("17:00").unwrap(),
            hours: 3.0,
            capacity_mb,
            note: String::new(),
            ts: 0,
        }
    }

    fn c(id: &str, class: Class, since_ms: i64, bytes: u64) -> Cargo {
        Cargo {
            msg: id.into(),
            author: "k".into(),
            label: id.into(),
            from: "north".into(),
            to: "south".into(),
            class,
            since_ms,
            bytes,
            root: (bytes > 0).then(|| "ab".repeat(32)),
            library: false,
        }
    }

    fn ids(p: &Plan, r: &Run) -> Vec<String> {
        r.load.iter().map(|i| p.cargo[*i].label.clone()).collect()
    }

    #[test]
    fn urgent_and_old_cargo_takes_the_first_room() {
        let mut cargo = vec![
            c("bulk-new", Class::Bulk, 50, 600 * MB),
            c("bulk-old", Class::Bulk, 10, 600 * MB),
            c("urgent", Class::Urgent, 90, MB),
            c("text", Class::Routine, 99, 0),
        ];
        cargo.sort_by_key(|c| (c.class, c.since_ms));
        let trips = [
            trip("fri", "fri", "north", "south", 1000),
            trip("tue", "tue", "north", "south", 1000),
        ];
        let p = plan(cargo, &trips, FRI);
        assert_eq!(p.runs.len(), 4, "two weeks of two weekly trips");
        assert_eq!(ids(&p, &p.runs[0]), ["urgent", "text", "bulk-old"]);
        assert_eq!(
            ids(&p, &p.runs[1]),
            ["bulk-new"],
            "the next departure takes what didn't fit"
        );
        assert_eq!(p.runs[0].depart_ms, FRI + 17 * H);
        assert!(p.unplanned.is_empty());
    }

    #[test]
    fn cargo_without_a_courier_or_room_is_unplanned() {
        let mut far = c("far", Class::Routine, 0, 0);
        far.to = "east".into();
        let cargo = vec![far, c("huge", Class::Bulk, 0, 5000 * MB)];
        let p = plan(cargo, &[trip("fri", "fri", "north", "south", 1000)], FRI);
        assert_eq!(p.why(0), Some(Why::NoTrip));
        assert_eq!(p.why(1), Some(Why::TooBig));
    }

    #[test]
    fn a_trip_that_already_left_takes_nothing_more() {
        let p = plan(
            vec![c("late", Class::Urgent, 0, 0)],
            &[trip("fri", "fri", "north", "south", 10)],
            FRI + 18 * H,
        );
        assert_eq!(p.runs[0].depart_ms, FRI + 7 * 24 * H + 17 * H);
    }

    #[test]
    fn an_unscheduled_drive_takes_what_has_no_courier_soon() {
        let cargo = vec![
            c("soon", Class::Urgent, 0, MB),
            c("a", Class::Routine, 0, 3 * MB),
            c("b", Class::Bulk, 0, 3 * MB),
        ];
        let p = plan(cargo, &[trip("fri", "fri", "north", "south", 1)], FRI);
        // "soon" leaves with Friday's courier; of the rest only one fits 4 MB
        let got: Vec<&str> = ad_hoc(&p, 4 * MB, FRI)
            .iter()
            .map(|i| p.cargo[*i].label.as_str())
            .collect();
        assert_eq!(got, ["a"]);
    }
}
