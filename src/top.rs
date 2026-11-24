//! `ridgewarden-mule top`: the mule in a terminal, for an operator on a headless Pi over SSH
//! or in the car. It reads the app's own `/api/mule` over HTTP rather than its files, so it
//! runs as any user: each message and where it is, the plan or tonight's manifest, and the
//! couriers.

use std::io;
use std::time::Duration;

use anyhow::{Result, bail};
use ridgewarden_client::tui::ratatui::Frame;
use ridgewarden_client::tui::ratatui::crossterm::event::{KeyCode, KeyEvent};
use ridgewarden_client::tui::ratatui::layout::{Constraint, Layout, Rect};
use ridgewarden_client::tui::ratatui::style::Style;
use ridgewarden_client::tui::ratatui::text::{Line, Span};
use ridgewarden_client::tui::ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use ridgewarden_client::tui::{
    self, Chrome, Fetch, Notice, Refresh, Theme, View, move_key, tab_key, word_pill, word_table,
};
use ridgewarden_client::ui::Color;
use ridgewarden_client::util::{human_duration, ms_to_rfc3339, now_ms};

use crate::api::{Carried, Courier, Departure, Item, Manifest, Message, Snapshot};
use crate::mule::{Class, Receipt, State};
use crate::plan::{Cargo, Why};
use crate::send::{cargo_size, size_text};

const ISLAND_TABS: [&str; 3] = ["Messages", "Plan", "Couriers"];
const CAR_TABS: [&str; 3] = ["Messages", "Manifest", "Couriers"];
const HELP: &str = "q quit  1-3 Tab pages  Up Down PgUp PgDn move  Enter details  r reload";
const DETAIL_HELP: &str = "Esc back  Up Down previous/next  PgUp PgDn scroll  r reload  q quit";
const START: &str = "Is it running? Start it with: sudo systemctl start ridgewarden-mule, or give its page's address with --url.";
const UPGRADE: &str = "Something answers there, but not as this version of ridgewarden-mule does: restart or upgrade it (sudo systemctl restart ridgewarden-mule), or check --url.";
const GONE: &str = "What you had open is no longer listed, so its details closed.";
const DAY: i64 = 86_400_000;

/// Run `top` until the user quits, reading `url`'s `/api/mule` every `every`.
pub fn run(url: &str, every: Duration) -> Result<()> {
    if !url.starts_with("http://") {
        bail!("--url takes the address of this app's page, like http://127.0.0.1:8800");
    }
    let base = url.trim_end_matches('/').to_string();
    let fetch = Fetch::new(Duration::from_secs(5))?;
    let api = format!("{base}/api/mule");
    let refresh = Refresh::spawn(every, async move || {
        fetch
            .json::<Snapshot>(&api)
            .await
            .map_err(|e| e.to_string())
    })?;
    let mut top = Top::new(base, Theme::from_env(), refresh);
    tui::run(&mut top, Duration::from_millis(250))?;
    Ok(())
}

/// A row's state in the pages' words, and its pill's color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Waiting,
    InTransit,
    Overdue,
    HandedOver,
    Delivered,
    Withdrawn,
    Aboard,
    Loading,
    Unknown,
}

impl Mark {
    fn word(self) -> &'static str {
        match self {
            Mark::Waiting => "WAITING",
            Mark::InTransit => "IN TRANSIT",
            Mark::Overdue => "OVERDUE",
            Mark::HandedOver => "HANDED OVER",
            Mark::Delivered => "DELIVERED",
            Mark::Withdrawn => "WITHDRAWN",
            Mark::Aboard => "ABOARD",
            Mark::Loading => "LOADING",
            Mark::Unknown => "?",
        }
    }

    fn color(self) -> Color {
        match self {
            Mark::Delivered | Mark::Aboard => Color::Green,
            Mark::InTransit | Mark::HandedOver | Mark::Loading => Color::Yellow,
            Mark::Overdue => Color::Red,
            Mark::Waiting | Mark::Withdrawn | Mark::Unknown => Color::Unknown,
        }
    }

    fn pill(self, theme: &Theme) -> Span<'static> {
        word_pill(theme, self.word(), self.color())
    }

    /// The pill as `word_table` takes it.
    fn cell(self) -> Option<(&'static str, Color)> {
        Some((self.word(), self.color()))
    }
}

/// Urgent stands out and bulk steps back, as the pages' class pills do; the words carry it
/// without color.
fn class_style(theme: &Theme, c: Class) -> Style {
    match c {
        Class::Urgent => theme.bad(),
        Class::Routine => Style::new(),
        Class::Bulk => theme.mute(),
    }
}

/// Why nothing takes a piece of cargo, short enough for a column.
fn why_word(w: Why) -> &'static str {
    match w {
        Why::NoTrip => "no courier",
        Why::TooBig => "no room",
    }
}

/// "11-23 17:30", in UTC.
fn at(ms: i64) -> String {
    let t = ms_to_rfc3339(ms);
    format!("{} {}", &t[5..10], &t[11..16])
}

/// "17:30" on the same UTC day as `now`, "11-24 17:30" on another.
fn when(ms: i64, now: i64) -> String {
    if ms.div_euclid(DAY) == now.div_euclid(DAY) {
        ms_to_rfc3339(ms)[11..16].to_string()
    } else {
        at(ms)
    }
}

/// Text from the mesh on one line, however it was written.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The message on one line, with its file, the way the pages list it.
fn preview(m: &Message) -> String {
    let mut t = one_line(&m.text);
    if let Some(f) = &m.file {
        if !t.is_empty() {
            t.push(' ');
        }
        t.push_str(&format!("[{}, {}]", f.name, size_text(f.size)));
    }
    t
}

fn label(c: &Cargo) -> String {
    let what = one_line(&c.label);
    if c.library {
        format!("library update: {what}")
    } else {
        what
    }
}

/// How many of `total` a list left out.
fn more(total: usize, listed: usize) -> usize {
    total.saturating_sub(listed)
}

/// Where a message is, worked out once for its row and its details.
enum Status<'a> {
    Planned(&'a Departure),
    Unplanned(Why),
    /// waiting, and not in the plan: e.g. its sender's key isn't pinned
    Waiting,
    Moving {
        car: &'a str,
        /// the trip its pickup names, if any
        trip: Option<&'a str>,
        expected: Option<i64>,
    },
    HandedOver,
    Delivered(Option<&'a Receipt>),
    Withdrawn,
}

fn departure_of<'a>(s: &'a Snapshot, msg: &str) -> Option<&'a Departure> {
    s.departures
        .iter()
        .find(|d| d.load.iter().any(|c| c.msg == msg))
}

fn status<'a>(m: &'a Message, s: &'a Snapshot) -> Status<'a> {
    match m.state {
        State::Waiting => match (
            departure_of(s, &m.id),
            s.unplanned.iter().find(|u| u.cargo.msg == m.id),
        ) {
            (Some(d), _) => Status::Planned(d),
            (None, Some(u)) => Status::Unplanned(u.why),
            (None, None) => Status::Waiting,
        },
        State::InTransit => {
            let pickup = m.pickups.first();
            Status::Moving {
                car: pickup.map_or("a car", |h| h.car_call.as_str()),
                trip: pickup.and_then(|h| h.trip.as_deref()),
                expected: m.expected_ms,
            }
        }
        State::HandedOver => Status::HandedOver,
        State::Delivered => Status::Delivered(m.receipt.as_ref()),
        State::Withdrawn => Status::Withdrawn,
    }
}

impl Status<'_> {
    fn mark(&self, now: i64) -> Mark {
        match self {
            Status::Planned(_) | Status::Unplanned(_) | Status::Waiting => Mark::Waiting,
            Status::Moving {
                expected: Some(t), ..
            } if *t < now => Mark::Overdue,
            Status::Moving { .. } => Mark::InTransit,
            Status::HandedOver => Mark::HandedOver,
            Status::Delivered(_) => Mark::Delivered,
            Status::Withdrawn => Mark::Withdrawn,
        }
    }

    /// Nothing will carry it until someone registers a trip or makes room.
    fn stuck(&self) -> bool {
        matches!(self, Status::Unplanned(_))
    }

    /// A few words for the table.
    fn short(&self, m: &Message, now: i64) -> String {
        let waited = human_duration(now - m.sent_ms);
        match self {
            Status::Planned(d) => format!(
                "{} leaves in {}",
                d.run.courier,
                human_duration(d.run.depart_ms - now)
            ),
            Status::Unplanned(w) => format!("{}, waiting {waited}", why_word(*w)),
            Status::Waiting => format!("waiting {waited}"),
            Status::Moving {
                car,
                expected: Some(t),
                ..
            } if *t < now => format!("on {car}, {} late", human_duration(now - t)),
            Status::Moving {
                car,
                expected: Some(t),
                ..
            } => format!("on {car}, due in {}", human_duration(t - now)),
            Status::Moving {
                car, trip: None, ..
            } => format!("on {car}, unscheduled"),
            Status::Moving { car, .. } => format!("on {car}, no expected time"),
            Status::HandedOver => format!("at {}, no receipt yet", m.to),
            Status::Delivered(Some(r)) if r.ms >= m.sent_ms => format!(
                "in {}, receipt {}",
                human_duration(r.ms - m.sent_ms),
                r.site_call
            ),
            Status::Delivered(Some(r)) => format!("receipt {}", r.site_call),
            Status::Delivered(None) => "delivered".into(),
            Status::Withdrawn => "withdrawn by the sender".into(),
        }
    }

    /// A sentence for the details.
    fn long(&self, m: &Message, now: i64) -> String {
        match self {
            Status::Planned(d) => format!(
                "Goes with {} at {} ({}), expected at {} about {}.",
                d.run.courier,
                at(d.run.depart_ms),
                d.run.name,
                d.run.to,
                at(d.run.arrives_ms)
            ),
            Status::Unplanned(w) => format!("Not on any trip: {}.", w.text()),
            Status::Waiting => format!("Waiting for a car since {}.", at(m.sent_ms)),
            Status::Moving {
                car,
                expected: Some(t),
                ..
            } if *t < now => format!(
                "Overdue: {car} was expected at {} about {}, {} ago.",
                m.to,
                at(*t),
                human_duration(now - t)
            ),
            Status::Moving {
                car,
                expected: Some(t),
                ..
            } => format!("On {car}, expected at {} about {}.", m.to, at(*t)),
            Status::Moving {
                car, trip: None, ..
            } => format!("On {car}, on a drive that isn't a registered trip, so no expected time."),
            Status::Moving {
                car, trip: Some(t), ..
            } => {
                format!("On {car} for trip {t}, which this site doesn't know, so no expected time.")
            }
            Status::HandedOver => format!("Handed to {}; its receipt isn't back yet.", m.to),
            Status::Delivered(Some(r)) if r.ms >= m.sent_ms => format!(
                "Delivered in {}, receipt from {}.",
                human_duration(r.ms - m.sent_ms),
                r.site_call
            ),
            Status::Delivered(Some(r)) => format!(
                "Delivered, receipt from {}; it says it is older than the message, so a clock is wrong.",
                r.site_call
            ),
            Status::Delivered(None) => "Delivered.".into(),
            Status::Withdrawn => "Withdrawn by the sender.".into(),
        }
    }
}

/// What the Messages page says above its table.
fn totals(s: &Snapshot) -> String {
    let overdue = s
        .messages
        .iter()
        .filter(|m| status(m, s).mark(s.now_ms) == Mark::Overdue)
        .count();
    let moving = match overdue {
        0 => format!("{} in transit", s.moving),
        n => format!("{} in transit ({n} overdue)", s.moving),
    };
    format!(
        "{} waiting, {moving}, {} delivered.",
        s.waiting, s.delivered
    )
}

/// The islands whose receipts can't count, in one line, the first few by name.
fn unpinned(s: &Snapshot) -> Option<String> {
    let names = &s.unpinned;
    let rest = more(s.unpinned_total, names.len());
    let (them, whose) = match (names.as_slice(), rest) {
        ([], _) => return None,
        ([one], 0) => (one.clone(), "its"),
        (all, 0) if all.len() <= 3 => {
            let (last, first) = all.split_last()?;
            (format!("{} and {last}", first.join(", ")), "their")
        }
        (all, _) => {
            let shown = all.len().min(3);
            let left = all.len() - shown + rest;
            (
                format!("{} and {left} more", all[..shown].join(", ")),
                "their",
            )
        }
    };
    Some(format!(
        "No keys pinned for {them}, so {whose} receipts can't count."
    ))
}

/// A line of the plan: a departure, then what goes with it; cargo nothing takes comes last.
enum PlanRow<'a> {
    Departure(&'a Departure),
    Cargo(&'a Cargo),
    Unplanned(&'a Cargo, Why),
}

fn plan_rows(s: &Snapshot) -> Vec<PlanRow<'_>> {
    let mut out = vec![];
    for d in &s.departures {
        out.push(PlanRow::Departure(d));
        out.extend(d.load.iter().map(PlanRow::Cargo));
    }
    out.extend(
        s.unplanned
            .iter()
            .map(|u| PlanRow::Unplanned(&u.cargo, u.why)),
    );
    out
}

/// A line of the manifest: what is aboard already, then what this drive still takes.
enum Load<'a> {
    Carried(&'a Carried),
    Item(&'a Item),
}

impl Load<'_> {
    fn cargo(&self) -> &Cargo {
        match self {
            Load::Carried(c) => &c.cargo,
            Load::Item(i) => &i.cargo,
        }
    }

    fn mark(&self) -> Mark {
        match self {
            Load::Carried(c) if c.handed_over => Mark::HandedOver,
            Load::Carried(_) => Mark::Aboard,
            Load::Item(i) => match i.aboard {
                Some(true) => Mark::Aboard,
                Some(false) => Mark::Loading,
                None => Mark::Unknown,
            },
        }
    }
}

fn manifest_rows(m: &Manifest) -> Vec<Load<'_>> {
    m.carried
        .iter()
        .map(Load::Carried)
        .chain(m.load.iter().map(Load::Item))
        .collect()
}

/// The manifest's verdict, in the page's words.
fn headline(m: &Manifest) -> (Option<Mark>, String) {
    let items = m.carried.len() + m.load.len();
    let bytes: u64 = manifest_rows(m).iter().map(|l| l.cargo().bytes).sum();
    let missing: u64 = m
        .load
        .iter()
        .filter(|i| i.aboard != Some(true))
        .map(|i| i.cargo.bytes)
        .sum();
    if items == 0 {
        (None, "Nothing to take on this drive.".into())
    } else if m.load.iter().any(|i| i.aboard.is_none()) {
        (
            Some(Mark::Unknown),
            "Can't tell what is aboard: the app can't reach its core.".into(),
        )
    } else if missing == 0 {
        (
            Some(Mark::Aboard),
            format!(
                "Everything is aboard: {items} item(s), {} of {}.",
                size_text(bytes),
                size_text(m.room)
            ),
        )
    } else {
        (
            Some(Mark::Loading),
            format!(
                "Still loading {} of {}.",
                size_text(missing),
                size_text(bytes)
            ),
        )
    }
}

const STAY: &str = "Stay near the node if you can; what is loaded is kept if you leave.";

/// Which drive the car is on, in a line or two that fit 80 columns.
fn drive_text(m: &Manifest, now: i64) -> Vec<String> {
    let Some(r) = &m.drive else {
        return vec![
            "Drive: not a registered trip, so it takes what has no courier in the next day.".into(),
        ];
    };
    let leaves = if r.depart_ms <= now {
        format!("Left {} ago", human_duration(now - r.depart_ms))
    } else {
        format!(
            "Leaves {} (in {})",
            when(r.depart_ms, now),
            human_duration(r.depart_ms - now)
        )
    };
    vec![
        format!(
            "Drive: {} to {} with {} ({}{})",
            r.from,
            r.to,
            r.courier,
            r.name,
            if m.chosen { ", picked on the page" } else { "" }
        ),
        format!("{leaves}, there about {}.", when(r.arrives_ms, now)),
    ]
}

/// "car / N0CAR, operator N0DRV" or "north / N0AAA", as the pages' nav bar puts it.
fn who(s: &Snapshot) -> String {
    let call = s.call.as_deref().unwrap_or("core not reachable");
    match &s.island {
        Some(i) => format!("{i} / {call}"),
        None => format!("car / {call}, operator {}", s.operator),
    }
}

/// What to do about a read that failed: an answer this version can't read means another
/// version is running there.
fn advice(err: &str) -> &'static str {
    if err.ends_with("HTTP 404") || err.ends_with("unexpected JSON") {
        UPGRADE
    } else {
        START
    }
}

/// Each row of a page, by what it shows rather than where it is, so a selection survives a
/// read that moves rows around.
fn row_ids(s: &Snapshot, tab: usize) -> Vec<String> {
    match tab {
        0 => s.messages.iter().map(|m| m.id.clone()).collect(),
        1 if s.island.is_some() => plan_rows(s)
            .iter()
            .map(|r| match r {
                PlanRow::Departure(d) => format!("{}@{}", d.run.trip, d.run.depart_ms),
                PlanRow::Cargo(c) | PlanRow::Unplanned(c, _) => c.msg.clone(),
            })
            .collect(),
        1 => s.manifest.as_ref().map_or(vec![], |m| {
            manifest_rows(m)
                .iter()
                .map(|l| l.cargo().msg.clone())
                .collect()
        }),
        _ => s.couriers.iter().map(|c| c.call.clone()).collect(),
    }
}

struct Top {
    theme: Theme,
    /// the app's page, e.g. http://127.0.0.1:8800
    base: String,
    refresh: Refresh<Result<Snapshot, String>>,
    snap: Option<Snapshot>,
    /// when the last good read came in, by this machine's clock
    got_ms: i64,
    /// why the latest read failed, if it did; the last good one stays on screen
    err: Option<String>,
    tab: usize,
    /// each page's selected row, and what is in it
    rows: [TableState; 3],
    picked: [Option<String>; 3],
    /// showing the selected row's details instead of the list
    open: bool,
    /// the first row of the details on screen
    scroll: usize,
    /// rows a list or the details had room for at the last draw, for PgUp and PgDn
    page: usize,
    /// a word about something the user had open, until the next key
    notice: Option<&'static str>,
}

impl Top {
    fn new(base: String, theme: Theme, refresh: Refresh<Result<Snapshot, String>>) -> Self {
        Top {
            theme,
            base,
            refresh,
            snap: None,
            got_ms: 0,
            err: None,
            tab: 0,
            rows: Default::default(),
            picked: Default::default(),
            open: false,
            scroll: 0,
            page: 0,
            notice: None,
        }
    }

    fn take(&mut self, read: Result<Snapshot, String>, now: i64) {
        match read {
            Ok(s) => {
                self.snap = Some(s);
                self.got_ms = now;
                self.err = None;
                self.repick();
            }
            Err(e) => self.err = Some(e),
        }
    }

    /// Find each page's selection again in a new read; what is gone closes its details.
    fn repick(&mut self) {
        for tab in 0..3 {
            let ids = self.ids(tab);
            let found = self.picked[tab]
                .as_ref()
                .and_then(|id| ids.iter().position(|x| x == id));
            match (found, self.rows[tab].selected()) {
                (Some(i), _) => self.rows[tab].select(Some(i)),
                (None, Some(i)) => {
                    if tab == self.tab && self.open {
                        self.open = false;
                        self.notice = Some(GONE);
                    }
                    // the cursor stays where it was, on whatever is there now
                    let i = (!ids.is_empty()).then(|| i.min(ids.len() - 1));
                    self.rows[tab].select(i);
                    self.picked[tab] = i.map(|i| ids[i].clone());
                }
                (None, None) => {}
            }
        }
    }

    fn ids(&self, tab: usize) -> Vec<String> {
        self.snap.as_ref().map_or(vec![], |s| row_ids(s, tab))
    }

    fn tabs(&self) -> &'static [&'static str] {
        match &self.snap {
            Some(s) if s.island.is_none() => &CAR_TABS,
            _ => &ISLAND_TABS,
        }
    }

    fn status(&self, now: i64) -> Span<'static> {
        let Some(s) = &self.snap else {
            return match self.err {
                Some(_) => Span::styled("can't reach the app", self.theme.bad()),
                None => Span::styled("loading", self.theme.mute()),
            };
        };
        let age = format!("updated {} ago", human_duration(now - self.got_ms));
        match &s.core_error {
            None => Span::styled(age, self.theme.mute()),
            Some(_) => Span::styled(format!("core trouble, {age}"), self.theme.bad()),
        }
    }

    /// Move the selection on the page in view, keeping track of what is in it.
    fn select(&mut self, k: &KeyEvent) {
        let ids = self.ids(self.tab);
        if move_key(&mut self.rows[self.tab], k, ids.len(), self.page) {
            self.picked[self.tab] = self.rows[self.tab].selected().map(|i| ids[i].clone());
            self.scroll = 0;
        }
    }
}

impl View for Top {
    fn tick(&mut self) -> io::Result<()> {
        if let Some(read) = self.refresh.latest()? {
            self.take(read, now_ms());
        }
        Ok(())
    }

    fn key(&mut self, k: KeyEvent) -> bool {
        self.notice = None;
        if let Some(t) = tab_key(&k, self.tab, self.tabs().len()) {
            self.tab = t;
            self.open = false;
            return true;
        }
        match k.code {
            KeyCode::Char('r') => self.refresh.again(),
            KeyCode::Esc | KeyCode::Backspace => self.open = false,
            KeyCode::Enter if self.open => self.open = false,
            KeyCode::Enter => {
                if self.rows[self.tab].selected().is_none() {
                    self.select(&KeyEvent::from(KeyCode::Home));
                }
                self.open = self.rows[self.tab].selected().is_some();
                self.scroll = 0;
            }
            KeyCode::PageDown if self.open => self.scroll += self.page.max(1),
            KeyCode::PageUp if self.open => self.scroll = self.scroll.saturating_sub(self.page),
            _ => self.select(&k),
        }
        true
    }

    fn draw(&mut self, f: &mut Frame) {
        let title = match &self.snap {
            Some(s) => format!("ridgewarden-mule top  {}", who(s)),
            None => "ridgewarden-mule top".into(),
        };
        let chrome = Chrome {
            title: &title,
            status: self.status(now_ms()),
            tabs: self.tabs(),
            tab: self.tab,
            help: if self.open { DETAIL_HELP } else { HELP },
        };
        let Some(mut area) = chrome.draw(f, &self.theme) else {
            return;
        };
        let theme = &self.theme;
        let mut banners = vec![];
        if let (Some(e), Some(_)) = (&self.err, &self.snap) {
            // the URL is the one we always read, and it would push the reason off the line
            let why = e
                .strip_prefix(&format!("{}/api/mule: ", self.base))
                .unwrap_or(e);
            banners.push(Line::styled(
                format!("Reading failed ({why}), so this is from earlier."),
                theme.bad(),
            ));
        }
        if let Some(e) = self.snap.as_ref().and_then(|s| s.core_error.as_ref()) {
            banners.push(Line::styled(e.clone(), theme.bad()));
        }
        if let Some(n) = self.notice {
            banners.push(Line::styled(n, theme.mute()));
        }
        if !banners.is_empty() {
            let [top, rest] = Layout::vertical([
                Constraint::Length(banners.len() as u16),
                Constraint::Fill(1),
            ])
            .areas(area);
            f.render_widget(Paragraph::new(banners), top);
            area = rest;
        }
        let Some(s) = &self.snap else {
            self.page = 0;
            let msg;
            let n = match &self.err {
                Some(e) => {
                    msg = format!("Can't reach ridgewarden-mule: {e}. {}", advice(e));
                    Notice::Error(&msg)
                }
                None => Notice::Loading,
            };
            f.render_widget(n.paragraph(theme), area);
            return;
        };
        let c = Ctx {
            theme,
            s,
            base: &self.base,
        };
        let rows = &mut self.rows[self.tab];
        if self.open
            && let Some(i) = rows.selected()
            && let Some(lines) = detail(&c, self.tab, i)
        {
            let (page, top) = details(f, area, lines, self.scroll, theme);
            (self.page, self.scroll) = (page, top);
            return;
        }
        self.page = match self.tab {
            0 => messages(&c, f, area, rows),
            1 if s.island.is_some() => plan(&c, f, area, rows),
            1 => manifest(&c, f, area, rows),
            _ => couriers(&c, f, area, rows),
        };
    }
}

/// What every page draws from.
struct Ctx<'a> {
    theme: &'a Theme,
    s: &'a Snapshot,
    base: &'a str,
}

/// Lines of text over a page's list, and the area left for the list.
fn above(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) -> Rect {
    let [head, rest] = Layout::vertical([
        Constraint::Length(lines.len() as u16 + 1),
        Constraint::Fill(1),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(lines), head);
    rest
}

/// A line under a page's list, if there is one, and the area left for the list.
fn below(f: &mut Frame, area: Rect, line: Option<Line<'static>>) -> Rect {
    let Some(line) = line else { return area };
    let [rest, foot] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    f.render_widget(line, foot);
    rest
}

/// Rows of a list that fit in `area` under its header.
fn room(area: Rect) -> usize {
    area.height.saturating_sub(1).into()
}

fn table<'a>(
    theme: &Theme,
    header: &[&'a str],
    widths: &[Constraint],
    rows: impl IntoIterator<Item = Row<'a>>,
) -> Table<'a> {
    Table::new(rows, widths.to_vec())
        .header(Row::new(header.to_vec()).style(theme.heading()))
        .row_highlight_style(theme.selected())
}

fn empty(c: &Ctx, f: &mut Frame, area: Rect, why: &str) -> usize {
    f.render_widget(Notice::Empty(why).paragraph(c.theme), area);
    0
}

fn messages(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) -> usize {
    let s = c.s;
    if s.messages.is_empty() {
        let why = if s.island.is_some() {
            "No messages yet. Send one from the Send page or with: ridgewarden-mule send"
        } else {
            "No messages for a car to carry yet."
        };
        return empty(c, f, area, why);
    }
    let mut lines = vec![Line::from(totals(s))];
    lines.extend(unpinned(s).map(|u| Line::styled(u, c.theme.bad())));
    let area = above(f, area, lines);
    let items = s.messages.iter().map(|m| {
        let st = status(m, s);
        let place = st.short(m, s.now_ms);
        let cells = vec![
            Cell::from(Span::styled(m.class.name(), class_style(c.theme, m.class))),
            Cell::from(m.to.clone()),
            Cell::from(preview(m)),
            Cell::from(Span::styled(
                place,
                if st.stuck() {
                    c.theme.bad()
                } else {
                    Style::new()
                },
            )),
        ];
        (st.mark(s.now_ms).cell(), cells)
    });
    let widths = [
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Fill(1),
        Constraint::Length(26),
    ];
    let header = ["Class", "To", "Message", "Where"];
    f.render_stateful_widget(word_table(c.theme, &header, &widths, items), area, rows);
    room(area)
}

fn cargo_cells(c: &Ctx, x: &Cargo) -> [Cell<'static>; 3] {
    [
        Cell::from(Span::styled(x.class.name(), class_style(c.theme, x.class))),
        Cell::from(cargo_size(x.bytes)),
        Cell::from(label(x)),
    ]
}

fn plan(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) -> usize {
    let s = c.s;
    let list = plan_rows(s);
    if list.is_empty() {
        return empty(
            c,
            f,
            area,
            "No courier has registered a trip and nothing is waiting. Register a trip on the Trips page or with: ridgewarden-mule trip",
        );
    }
    let area = above(
        f,
        area,
        vec![Line::styled(
            "Each message takes the first departure on its corridor with room, urgent first.",
            c.theme.mute(),
        )],
    );
    let left = (
        more(s.departures_total, s.departures.len()),
        more(s.unplanned_total, s.unplanned.len()),
    );
    let foot = match left {
        (0, 0) => None,
        (d, 0) => Some(format!("and {d} more departures, not listed")),
        (0, u) => Some(format!("and {u} more not on any trip, not listed")),
        (d, u) => Some(format!(
            "and {d} more departures and {u} more not on any trip, not listed"
        )),
    };
    let area = below(f, area, foot.map(|t| Line::styled(t, c.theme.mute())));
    let now = s.now_ms;
    let items = list.iter().map(|r| match r {
        PlanRow::Departure(d) => {
            // the room is in the details; the name tells apart two trips leaving together
            let (n, bytes) = match (d.items, d.bytes) {
                (0, _) => ("empty".to_string(), String::new()),
                (n, 0) => (format!("{n} item(s)"), String::new()),
                (n, b) => (format!("{n} item(s)"), size_text(b)),
            };
            Row::new(vec![
                Cell::from(at(d.run.depart_ms)),
                Cell::from(d.run.courier.clone()),
                Cell::from(d.run.to.clone()),
                Cell::from(n),
                Cell::from(bytes),
                Cell::from(format!(
                    "{}, in {}",
                    d.run.name,
                    human_duration(d.run.depart_ms - now)
                )),
            ])
            .style(c.theme.heading())
        }
        PlanRow::Cargo(x) => {
            let [class, size, what] = cargo_cells(c, x);
            Row::new(vec![
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                class,
                size,
                what,
            ])
        }
        PlanRow::Unplanned(x, why) => {
            let [class, size, what] = cargo_cells(c, x);
            Row::new(vec![
                Cell::from(Span::styled(why_word(*why), c.theme.bad())),
                Cell::from(""),
                Cell::from(x.to.clone()),
                class,
                size,
                what,
            ])
        }
    });
    let widths = [
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Fill(1),
    ];
    let header = ["Leaves (UTC)", "Courier", "To", "Class", "Size", "What"];
    f.render_stateful_widget(table(c.theme, &header, &widths, items), area, rows);
    room(area)
}

fn manifest(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) -> usize {
    let s = c.s;
    let Some(m) = &s.manifest else {
        let why = "The app hasn't heard from its core yet, so it doesn't know this car's key and can't work out its manifest.";
        f.render_widget(Notice::Error(why).paragraph(c.theme), area);
        return 0;
    };
    let (mark, head) = headline(m);
    let mut verdict = vec![];
    if let Some(k) = mark {
        verdict.extend([k.pill(c.theme), Span::raw(" ")]);
    }
    verdict.push(Span::raw(head));
    let mut lines: Vec<Line> = drive_text(m, s.now_ms)
        .into_iter()
        .map(Line::from)
        .collect();
    lines.push(Line::from(verdict));
    if mark == Some(Mark::Loading) {
        lines.push(Line::styled(STAY, c.theme.mute()));
    }
    let area = above(f, area, lines);
    let list = manifest_rows(m);
    if list.is_empty() {
        return 0;
    }
    let items = list.iter().map(|l| {
        let x = l.cargo();
        let [class, size, what] = cargo_cells(c, x);
        let cells = vec![
            class,
            Cell::from(x.to.clone()),
            size,
            Cell::from(human_duration(s.now_ms - x.since_ms)),
            what,
        ];
        (l.mark().cell(), cells)
    });
    let widths = [
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Fill(1),
    ];
    let header = ["Class", "To", "Size", "Waited", "What"];
    f.render_stateful_widget(word_table(c.theme, &header, &widths, items), area, rows);
    room(area)
}

/// The soonest of a courier's next departures.
fn next(x: &Courier) -> Option<i64> {
    x.trips.iter().filter_map(|t| t.next_ms).min()
}

fn couriers(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) -> usize {
    let s = c.s;
    if s.couriers.is_empty() {
        return empty(
            c,
            f,
            area,
            "No courier has registered a trip and no car has logged a pickup yet. Register a trip on the Trips page or with: ridgewarden-mule trip",
        );
    }
    let area = above(
        f,
        area,
        vec![
            Line::styled(
                "Due: their registered departures in the last 30 days, since each was registered.",
                c.theme.mute(),
            ),
            Line::styled(
                "Driven: days in the last 30 a pinned car logged a pickup naming them.",
                c.theme.mute(),
            ),
        ],
    );
    let foot = match more(s.couriers_total, s.couriers.len()) {
        0 => None,
        n => Some(Line::styled(
            format!("and {n} more couriers, not listed"),
            c.theme.mute(),
        )),
    };
    let area = below(f, area, foot);
    let ever = |ms: Option<i64>| ms.map_or("never".to_string(), at);
    let items = s.couriers.iter().map(|x| {
        Row::new(vec![
            Cell::from(x.call.clone()),
            Cell::from(next(x).map_or("-".to_string(), at)),
            Cell::from(x.trips.len().to_string()),
            Cell::from(x.due.to_string()),
            Cell::from(x.drive_days.to_string()),
            Cell::from(ever(x.last_pickup_ms)),
            Cell::from(ever(x.last_dropoff_ms)),
        ])
    });
    let widths = [
        Constraint::Length(9),
        Constraint::Length(11),
        Constraint::Length(5),
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Length(11),
        Constraint::Fill(1),
    ];
    let header = [
        "Courier",
        "Next (UTC)",
        "Trips",
        "Due",
        "Driven",
        "Last pickup",
        "Last drop-off",
    ];
    f.render_stateful_widget(table(c.theme, &header, &widths, items), area, rows);
    room(area)
}

/// Lines cut to fit `width` columns, between words where they can be, each piece keeping
/// its style. The details are cut here rather than wrapped by ratatui so that they can say
/// how much there is to scroll through.
fn fit(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut out = vec![];
    for line in lines {
        let style = line.style;
        let mut row: Vec<Span<'static>> = vec![];
        let mut used = 0;
        for span in line.spans {
            for word in span.content.split_inclusive(' ') {
                let mut rest = word;
                while !rest.is_empty() {
                    let w = Span::raw(rest).width();
                    if used + w <= width {
                        row.push(Span::styled(rest.to_string(), span.style));
                        used += w;
                        break;
                    }
                    if used > 0 {
                        out.push(Line::from(std::mem::take(&mut row)).style(style));
                        used = 0;
                        continue;
                    }
                    // a word longer than a whole row is cut where the row ends
                    let mut cut = 0;
                    for (i, ch) in rest.char_indices() {
                        if Span::raw(&rest[..i + ch.len_utf8()]).width() > width {
                            break;
                        }
                        cut = i + ch.len_utf8();
                    }
                    let cut = cut.max(rest.chars().next().map_or(1, char::len_utf8));
                    row.push(Span::styled(rest[..cut].to_string(), span.style));
                    out.push(Line::from(std::mem::take(&mut row)).style(style));
                    rest = &rest[cut..];
                }
            }
        }
        out.push(Line::from(row).style(style));
    }
    out
}

/// Draw the details from line `scroll` on, saying where in them the screen is when they
/// don't all fit. Returns the rows a page holds and the first row shown.
fn details(
    f: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    scroll: usize,
    theme: &Theme,
) -> (usize, usize) {
    let rows = fit(lines, area.width);
    let height = usize::from(area.height);
    if rows.len() <= height {
        f.render_widget(Paragraph::new(rows), area);
        return (height, 0);
    }
    let page = height.saturating_sub(1).max(1);
    let top = scroll.min(rows.len() - page);
    let shown = rows[top..top + page].to_vec();
    let [text, foot] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    f.render_widget(Paragraph::new(shown), text);
    f.render_widget(
        Line::styled(
            format!(
                "Lines {}-{} of {}; PgUp and PgDn scroll",
                top + 1,
                top + page,
                rows.len()
            ),
            theme.mute(),
        ),
        foot,
    );
    (page, top)
}

/// The selected row of a page in full.
fn detail(c: &Ctx, tab: usize, i: usize) -> Option<Vec<Line<'static>>> {
    let s = c.s;
    match tab {
        0 => s.messages.get(i).map(|m| message_detail(c, m)),
        1 if s.island.is_some() => plan_rows(s).get(i).map(|r| match r {
            PlanRow::Departure(d) => departure_detail(c, d),
            PlanRow::Cargo(x) | PlanRow::Unplanned(x, _) => cargo_detail(c, x),
        }),
        1 => s
            .manifest
            .as_ref()
            .and_then(|m| manifest_rows(m).get(i).map(|l| cargo_detail(c, l.cargo()))),
        _ => s.couriers.get(i).map(|x| courier_detail(c, x)),
    }
}

fn steps(m: &Message) -> Vec<(i64, String)> {
    let mut out = vec![(m.sent_ms, format!("sent from {}", m.from_island))];
    for h in &m.pickups {
        let trip = h
            .trip
            .as_ref()
            .map(|t| format!(", trip {t}"))
            .unwrap_or_default();
        out.push((
            h.ms,
            format!(
                "picked up by car {}, control operator {}{trip}",
                h.car_call, h.operator
            ),
        ));
    }
    for h in &m.dropoffs {
        out.push((
            h.ms,
            format!(
                "handed to {} by car {}, control operator {}",
                h.island.as_deref().unwrap_or("?"),
                h.car_call,
                h.operator
            ),
        ));
    }
    if let Some(r) = &m.receipt {
        out.push((
            r.ms,
            format!("receipt for {} signed by {}", r.island, r.site_call),
        ));
    }
    out.sort_by_key(|s| s.0);
    out
}

fn message_detail(c: &Ctx, m: &Message) -> Vec<Line<'static>> {
    let st = status(m, c.s);
    let mut head = vec![
        st.mark(c.s.now_ms).pill(c.theme),
        Span::raw(" "),
        Span::styled(m.class.name(), class_style(c.theme, m.class)),
        Span::styled(
            format!("  {} to {}", m.from_island, m.to),
            c.theme.heading(),
        ),
    ];
    if let Some(call) = &m.to_call {
        head.push(Span::styled(format!(", for {call}"), c.theme.heading()));
    }
    let mut by = format!(
        "From {} at {}, signed by {}",
        m.from, m.from_island, m.signed_by
    );
    if let Some(w) = &m.on_behalf {
        by.push_str(&format!(", third-party traffic on behalf of {w}"));
    }
    let mut lines = vec![
        Line::from(head),
        Line::from(by),
        Line::raw(""),
        Line::styled("Trip (UTC)", c.theme.heading()),
    ];
    for (ms, what) in steps(m) {
        lines.push(Line::from(format!("{}  {what}", at(ms))));
    }
    let outlook = st.long(m, c.s.now_ms);
    lines.push(if st.stuck() || st.mark(c.s.now_ms) == Mark::Overdue {
        Line::styled(outlook, c.theme.bad())
    } else {
        Line::from(outlook)
    });
    lines.push(Line::raw(""));
    if let Some(f) = &m.file {
        lines.push(Line::from(format!(
            "File: {} ({})",
            f.name,
            size_text(f.size)
        )));
    }
    if m.library {
        lines.push(Line::styled(
            "An offline-library update: at the far island the library app takes the file itself.",
            c.theme.mute(),
        ));
    }
    lines.push(Line::styled(
        format!("Its page: {}/msg?id={}", c.base, m.id),
        c.theme.mute(),
    ));
    if !m.text.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(m.text.lines().map(|l| Line::raw(l.to_string())));
    }
    lines
}

/// A piece of cargo's message in full, or what the plan knows of it when the message is
/// past the first ones listed.
fn cargo_detail(c: &Ctx, x: &Cargo) -> Vec<Line<'static>> {
    if let Some(m) = c.s.messages.iter().find(|m| m.id == x.msg) {
        return message_detail(c, m);
    }
    vec![
        Line::from(vec![
            Span::styled(x.class.name(), class_style(c.theme, x.class)),
            Span::styled(format!("  {} to {}", x.from, x.to), c.theme.heading()),
        ]),
        Line::from(format!(
            "{} ({}), waiting since {}",
            label(x),
            cargo_size(x.bytes),
            at(x.since_ms)
        )),
        Line::styled(
            format!("Its page: {}/msg?id={}", c.base, x.msg),
            c.theme.mute(),
        ),
    ]
}

fn departure_detail(c: &Ctx, d: &Departure) -> Vec<Line<'static>> {
    let r = &d.run;
    let now = c.s.now_ms;
    let mut lines = vec![
        Line::styled(
            format!("{}: {} to {} with {}", r.name, r.from, r.to, r.courier),
            c.theme.heading(),
        ),
        Line::from(format!(
            "Leaves {} (in {}), there about {}.",
            at(r.depart_ms),
            human_duration(r.depart_ms - now),
            at(r.arrives_ms)
        )),
        Line::from(match (d.items, d.bytes) {
            (0, _) => format!(
                "Nothing planned on it yet; room for {}.",
                size_text(r.capacity)
            ),
            (n, 0) => format!(
                "{n} item(s), text only; room for {}.",
                size_text(r.capacity)
            ),
            (n, b) => format!(
                "{n} item(s), {} of {} room.",
                size_text(b),
                size_text(r.capacity)
            ),
        }),
        Line::raw(""),
    ];
    for x in &d.load {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<8}", x.class.name()),
                class_style(c.theme, x.class),
            ),
            Span::raw(format!("{:>9}  {}", cargo_size(x.bytes), label(x))),
        ]));
    }
    if d.items > d.load.len() {
        lines.push(Line::styled(
            format!("and {} more", d.items - d.load.len()),
            c.theme.mute(),
        ));
    }
    lines
}

fn courier_detail(c: &Ctx, x: &Courier) -> Vec<Line<'static>> {
    let ever = |ms: Option<i64>| ms.map_or("never".to_string(), at);
    let mut lines = vec![
        Line::styled(x.call.clone(), c.theme.heading()),
        Line::from(format!(
            "{} registered trip(s); in the last 30 days {} departure(s) due and {} day(s) driven.",
            x.trips.len(),
            x.due,
            x.drive_days
        )),
        Line::from(format!(
            "Last pickup {}, last drop-off {} (UTC).",
            ever(x.last_pickup_ms),
            ever(x.last_dropoff_ms)
        )),
    ];
    for t in &x.trips {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("{}: {} to {}, {}", t.name, t.from, t.to, t.schedule),
            c.theme.heading(),
        ));
        lines.push(Line::from(format!(
            "Next {}, room {}, registered by {}.",
            t.next_ms
                .map_or("not in the next two weeks".to_string(), at),
            size_text(t.capacity),
            t.by
        )));
        if !t.note.is_empty() {
            lines.push(Line::styled(t.note.clone(), c.theme.mute()));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Run, TripInfo, Unplanned};
    use crate::mule::{FileRef, Hop};
    use ridgewarden_client::tui::MIN_SIZE;
    use ridgewarden_client::tui::ratatui::Terminal;
    use ridgewarden_client::tui::ratatui::backend::TestBackend;
    use ridgewarden_client::tui::ratatui::buffer::Buffer;
    use ridgewarden_client::tui::ratatui::crossterm::event::KeyModifiers;
    use ridgewarden_client::tui::ratatui::style::{Color as Ansi, Modifier};
    use ridgewarden_client::util::esc;

    const MIN: i64 = 60_000;
    const H: i64 = 60 * MIN;
    /// 2026-11-20 17:00 UTC
    const NOW: i64 = 1_795_132_800_000 + 17 * H;

    fn msg(id: &str, class: Class, state: State, sent_ms: i64) -> Message {
        Message {
            id: id.into(),
            class,
            state,
            from_island: "north".into(),
            from: "N0AAA".into(),
            to: "south".into(),
            to_call: None,
            signed_by: "N0AAA".into(),
            on_behalf: None,
            text: format!("message {id}"),
            file: None,
            library: false,
            sent_ms,
            pickups: vec![],
            dropoffs: vec![],
            receipt: None,
            expected_ms: None,
        }
    }

    fn hop(ms: i64, trip: Option<&str>, island: Option<&str>) -> Hop {
        Hop {
            car: "cc".repeat(32),
            car_call: "N0CAR".into(),
            operator: "N0DRV".into(),
            island: island.map(Into::into),
            trip: trip.map(Into::into),
            ms,
        }
    }

    fn cargo(m: &Message) -> Cargo {
        Cargo {
            msg: m.id.clone(),
            author: "aa".repeat(32),
            label: m.text.clone(),
            from: m.from_island.clone(),
            to: m.to.clone(),
            class: m.class,
            since_ms: m.sent_ms,
            bytes: m.file.as_ref().map_or(0, |f| f.size),
            root: m.file.as_ref().map(|f| f.root.clone()),
            library: m.library,
        }
    }

    fn run(name: &str, depart_ms: i64) -> Run {
        Run {
            trip: format!("aaaaaaaa:{name}"),
            name: name.into(),
            courier: "N0DRV".into(),
            from: "north".into(),
            to: "south".into(),
            depart_ms,
            arrives_ms: depart_ms + 2 * H,
            capacity: 50_000_000,
        }
    }

    /// An island with one message at each step of its trip.
    fn island() -> Snapshot {
        let planned = msg("planned", Class::Urgent, State::Waiting, NOW - 20 * MIN);
        let stuck = Message {
            to: "east".into(),
            ..msg("stuck", Class::Routine, State::Waiting, NOW - 3 * H)
        };
        let late = Message {
            pickups: vec![hop(NOW - 5 * H, Some("aaaaaaaa:early"), None)],
            expected_ms: Some(NOW - 40 * MIN),
            ..msg("late", Class::Routine, State::InTransit, NOW - 6 * H)
        };
        let due = Message {
            pickups: vec![hop(NOW - 10 * MIN, Some("aaaaaaaa:evening"), None)],
            expected_ms: Some(NOW + 80 * MIN),
            file: Some(FileRef {
                root: "dd".repeat(32),
                name: "roster.csv".into(),
                size: 1_200_000,
            }),
            ..msg("due", Class::Bulk, State::InTransit, NOW - 2 * H)
        };
        let handed = Message {
            pickups: vec![hop(NOW - 4 * H, None, None)],
            dropoffs: vec![hop(NOW - H, None, Some("south"))],
            ..msg("handed", Class::Routine, State::HandedOver, NOW - 5 * H)
        };
        let done = Message {
            receipt: Some(Receipt {
                site_call: "N0BBB".into(),
                island: "south".into(),
                ms: NOW - 2 * H,
            }),
            ..msg("done", Class::Urgent, State::Delivered, NOW - 5 * H)
        };
        Snapshot {
            now_ms: NOW,
            island: Some("north".into()),
            operator: "N0AAA".into(),
            call: Some("N0AAA".into()),
            core_error: None,
            waiting: 2,
            moving: 3,
            delivered: 1,
            departures: vec![
                Departure {
                    run: run("evening", NOW + 25 * MIN),
                    items: 1,
                    bytes: 0,
                    load: vec![cargo(&planned)],
                },
                Departure {
                    run: run("sat", NOW + 30 * H),
                    items: 0,
                    bytes: 0,
                    load: vec![],
                },
            ],
            unplanned: vec![Unplanned {
                cargo: cargo(&stuck),
                why: Why::NoTrip,
            }],
            messages: vec![planned, stuck, late, due, handed, done],
            unpinned: vec!["east".into()],
            unpinned_total: 1,
            departures_total: 2,
            unplanned_total: 1,
            manifest: None,
            couriers: vec![Courier {
                call: "N0DRV".into(),
                trips: vec![TripInfo {
                    name: "evening".into(),
                    from: "north".into(),
                    to: "south".into(),
                    schedule: "daily at 17:25 UTC, about 2 h".into(),
                    next_ms: Some(NOW + 25 * MIN),
                    capacity: 50_000_000,
                    note: "parks by the fire station node".into(),
                    by: "N0AAA".into(),
                }],
                due: 4,
                drive_days: 2,
                last_pickup_ms: Some(NOW - 10 * MIN),
                last_dropoff_ms: None,
            }],
            couriers_total: 1,
        }
    }

    /// The same messages seen from the car: the one due later is aboard, a file loading.
    fn car() -> Snapshot {
        let s = island();
        let due = s.messages.iter().find(|m| m.id == "due").unwrap();
        let roster = Message {
            file: due.file.clone(),
            ..msg("roster", Class::Routine, State::Waiting, NOW - 5 * MIN)
        };
        Snapshot {
            island: None,
            operator: "N0DRV".into(),
            call: Some("N0CAR".into()),
            manifest: Some(Manifest {
                drive: Some(run("evening", NOW + 25 * MIN)),
                chosen: false,
                room: 50_000_000,
                load: vec![
                    Item {
                        cargo: cargo(&s.messages[0]),
                        aboard: Some(true),
                    },
                    Item {
                        cargo: cargo(&roster),
                        aboard: Some(false),
                    },
                ],
                carried: vec![Carried {
                    cargo: cargo(due),
                    handed_over: false,
                }],
            }),
            ..s
        }
    }

    fn empty() -> Snapshot {
        Snapshot {
            waiting: 0,
            moving: 0,
            delivered: 0,
            messages: vec![],
            unpinned: vec![],
            unpinned_total: 0,
            departures: vec![],
            departures_total: 0,
            unplanned: vec![],
            unplanned_total: 0,
            couriers: vec![],
            couriers_total: 0,
            ..island()
        }
    }

    /// A view of `snap`, read five minutes ago so its age reads the same however slow the
    /// test is.
    fn top(snap: Option<Snapshot>) -> Top {
        let refresh = Refresh::spawn(Duration::from_secs(3600), async || {
            Err("not read in this test".to_string())
        })
        .unwrap();
        let mut t = Top::new(
            "http://127.0.0.1:8800".into(),
            Theme { color: false },
            refresh,
        );
        t.snap = snap;
        t.got_ms = now_ms() - 5 * MIN;
        t.page = 10;
        t
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// The text on a `w` by `h` terminal after one draw.
    fn screen(t: &mut Top, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| t.draw(f)).unwrap();
        let b = term.backend().buffer();
        let rows = (0..h).map(|y| (0..w).map(|x| b[(x, y)].symbol()).collect::<String>());
        rows.collect::<Vec<_>>().join("\n")
    }

    /// The screen as one line of single spaces, so a wrapped sentence reads as one.
    fn flat(screen: &str) -> String {
        screen.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn each_message_says_where_it_is_and_how_it_looks() {
        let s = island();
        let rows: Vec<(Mark, String, bool)> = s
            .messages
            .iter()
            .map(|m| {
                let st = status(m, &s);
                (st.mark(NOW), st.short(m, NOW), st.stuck())
            })
            .collect();
        let want = |k, t: &str, stuck| (k, t.to_string(), stuck);
        assert_eq!(
            rows,
            [
                want(Mark::Waiting, "N0DRV leaves in 25 min", false),
                want(Mark::Waiting, "no courier, waiting 3 h", true),
                want(Mark::Overdue, "on N0CAR, 40 min late", false),
                want(Mark::InTransit, "on N0CAR, due in 80 min", false),
                want(Mark::HandedOver, "at south, no receipt yet", false),
                want(Mark::Delivered, "in 3 h, receipt N0BBB", false),
            ]
        );
        let colors: Vec<Color> = rows.iter().map(|r| r.0.color()).collect();
        assert_eq!(
            colors,
            [
                Color::Unknown,
                Color::Unknown,
                Color::Red,
                Color::Yellow,
                Color::Yellow,
                Color::Green
            ]
        );
        assert_eq!(
            totals(&s),
            "2 waiting, 3 in transit (1 overdue), 1 delivered."
        );

        let on_time = Message {
            expected_ms: Some(NOW),
            ..s.messages[3].clone()
        };
        assert_eq!(
            status(&on_time, &s).mark(NOW),
            Mark::InTransit,
            "due now is not late yet"
        );
        let unscheduled = Message {
            expected_ms: None,
            pickups: vec![hop(NOW - H, None, None)],
            ..s.messages[3].clone()
        };
        let st = status(&unscheduled, &s);
        assert_eq!(st.short(&unscheduled, NOW), "on N0CAR, unscheduled");
        assert!(
            st.long(&unscheduled, NOW)
                .contains("isn't a registered trip")
        );
        let unknown = Message {
            expected_ms: None,
            pickups: vec![hop(NOW - H, Some("aaaaaaaa:gone"), None)],
            ..s.messages[3].clone()
        };
        let st = status(&unknown, &s);
        assert_eq!(
            st.short(&unknown, NOW),
            "on N0CAR, no expected time",
            "a trip it names but nobody registered here is still a trip"
        );
        assert_eq!(
            st.long(&unknown, NOW),
            "On N0CAR for trip aaaaaaaa:gone, which this site doesn't know, so no expected time."
        );
        let skewed = Message {
            sent_ms: NOW,
            ..s.messages[5].clone()
        };
        assert_eq!(
            status(&skewed, &s).short(&skewed, NOW),
            "receipt N0BBB",
            "a receipt older than its message says nothing about how long it took"
        );
        assert!(
            status(&skewed, &s)
                .long(&skewed, NOW)
                .ends_with("so a clock is wrong.")
        );
    }

    #[test]
    fn the_manifest_says_whether_everything_is_aboard() {
        let m = car().manifest.unwrap();
        let marks: Vec<Mark> = manifest_rows(&m).iter().map(Load::mark).collect();
        assert_eq!(marks, [Mark::Aboard, Mark::Aboard, Mark::Loading]);
        assert_eq!(
            headline(&m),
            (
                Some(Mark::Loading),
                "Still loading 1.2 MB of 2.4 MB.".into()
            )
        );
        let mut done = m.clone();
        done.load[1].aboard = Some(true);
        assert_eq!(
            headline(&done),
            (
                Some(Mark::Aboard),
                "Everything is aboard: 3 item(s), 2.4 MB of 50.0 MB.".into()
            )
        );
        let mut blind = m.clone();
        blind.load[1].aboard = None;
        assert_eq!(headline(&blind).0, Some(Mark::Unknown));
        let nothing = Manifest {
            load: vec![],
            carried: vec![],
            ..m.clone()
        };
        assert_eq!(
            headline(&nothing),
            (None, "Nothing to take on this drive.".into())
        );
        let mut handed = m.clone();
        handed.carried[0].handed_over = true;
        assert_eq!(Load::Carried(&handed.carried[0]).mark(), Mark::HandedOver);

        assert_eq!(
            drive_text(&m, NOW),
            [
                "Drive: north to south with N0DRV (evening)",
                "Leaves 17:25 (in 25 min), there about 19:25."
            ]
        );
        assert_eq!(
            drive_text(&m, NOW + H)[1],
            "Left 35 min ago, there about 19:25."
        );
        assert_eq!(
            drive_text(&m, NOW - 18 * H)[1],
            "Leaves 11-20 17:25 (in 18 h), there about 11-20 19:25.",
            "a drive on another day says which"
        );
        let picked = Manifest {
            chosen: true,
            ..m.clone()
        };
        assert!(drive_text(&picked, NOW)[0].ends_with(", picked on the page)"));
        let ad_hoc = Manifest { drive: None, ..m };
        let alone = drive_text(&ad_hoc, NOW);
        assert_eq!(alone.len(), 1);
        assert!(alone[0].starts_with("Drive: not a registered trip"));
    }

    #[test]
    fn keys_switch_pages_open_details_and_move() {
        let mut t = top(Some(island()));
        assert!(t.key(key(KeyCode::Char('2'))));
        assert_eq!(t.tab, 1);
        t.key(key(KeyCode::Tab));
        t.key(key(KeyCode::Tab));
        assert_eq!(t.tab, 0, "Tab wraps around");
        t.key(key(KeyCode::Down));
        t.key(key(KeyCode::Down));
        assert_eq!(t.rows[0].selected(), Some(1));
        t.key(key(KeyCode::End));
        assert_eq!(t.rows[0].selected(), Some(5), "the last of six messages");
        t.key(key(KeyCode::Enter));
        assert!(t.open);
        t.key(key(KeyCode::Up));
        assert_eq!(
            (t.open, t.rows[0].selected()),
            (true, Some(4)),
            "the details follow the selection"
        );
        t.key(key(KeyCode::Esc));
        assert!(!t.open);
        t.key(key(KeyCode::Enter));
        t.key(key(KeyCode::Enter));
        assert!(!t.open, "Enter closes the details too");
        t.key(key(KeyCode::Char('3')));
        t.key(key(KeyCode::Enter));
        assert_eq!(
            (t.open, t.rows[2].selected()),
            (true, Some(0)),
            "Enter with nothing picked opens the first row"
        );
        t.key(key(KeyCode::Left));
        assert_eq!((t.tab, t.open), (1, false), "a new page starts on its list");
        t.key(key(KeyCode::Char('9')));
        assert_eq!(t.tab, 1, "there is no ninth page");

        let mut e = top(Some(empty()));
        e.key(key(KeyCode::Enter));
        assert!(!e.open, "nothing to open on an empty page");
        let mut none = top(None);
        none.key(key(KeyCode::Down));
        assert_eq!(none.rows[0].selected(), None);
    }

    #[test]
    fn times_and_sizes_read_plainly() {
        assert_eq!(at(NOW), "11-20 17:00");
        assert_eq!(when(NOW + H, NOW), "18:00");
        assert_eq!(when(NOW + 8 * H, NOW), "11-21 01:00", "tomorrow, in UTC");
        assert_eq!(
            (cargo_size(0), cargo_size(2_621_500)),
            ("text".into(), "2.6 MB".into())
        );
        let m = Message {
            text: "Shelter at the high school\nneeds 20 cots.".into(),
            file: Some(FileRef {
                root: "dd".repeat(32),
                name: "plan.pdf".into(),
                size: 1_500_000,
            }),
            ..msg("x", Class::Bulk, State::Waiting, NOW)
        };
        assert_eq!(
            preview(&m),
            "Shelter at the high school needs 20 cots. [plan.pdf, 1.5 MB]"
        );
        let lib = Cargo {
            library: true,
            label: "Offline library update: maps 2026-12.".into(),
            ..cargo(&m)
        };
        assert_eq!(
            label(&lib),
            "library update: Offline library update: maps 2026-12."
        );
    }

    #[test]
    fn every_page_fits_80x24() {
        let mut t = top(Some(island()));
        let messages = screen(&mut t, 80, 24);
        for want in [
            "ridgewarden-mule top  north / N0AAA",
            " 1 Messages  2 Plan  3 Couriers",
            "2 waiting, 3 in transit (1 overdue), 1 delivered.",
            "No keys pinned for east, so its receipts can't count.",
            " OVERDUE ",
            "on N0CAR, 40 min late",
            " DELIVERED ",
            "q quit  1-3 Tab pages",
        ] {
            assert!(messages.contains(want), "{want:?} in\n{messages}");
        }
        t.tab = 1;
        let plan = screen(&mut t, 80, 24);
        for want in [
            "Leaves (UTC)",
            "11-20 17:25  N0DRV    south    1 item(s)          evening, in 25 min",
            "11-21 23:00  N0DRV    south    empty              sat, in 30 h",
            "no courier",
        ] {
            assert!(plan.contains(want), "{want:?} in\n{plan}");
        }
        t.tab = 2;
        let couriers = screen(&mut t, 80, 24);
        let row = couriers.lines().find(|l| l.starts_with("N0DRV")).unwrap();
        let cells: Vec<&str> = row
            .split("  ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        assert_eq!(
            cells,
            ["N0DRV", "11-20 17:25 1", "4", "2", "11-20 16:50 never"],
            "next departure, trips, due, driven, last pickup and drop-off: {couriers}"
        );

        let mut c = top(Some(car()));
        c.tab = 1;
        let manifest = screen(&mut c, 80, 24);
        for want in [
            "ridgewarden-mule top  car / N0CAR, operator N0DRV",
            " 1 Messages  2 Manifest  3 Couriers",
            "Drive: north to south with N0DRV (evening)",
            "Leaves 17:25 (in 25 min), there about 19:25.",
            " LOADING  Still loading 1.2 MB of 2.4 MB.",
            "Stay near the node if you can; what is loaded is kept if you leave.",
            " ABOARD ",
        ] {
            assert!(manifest.contains(want), "{want:?} in\n{manifest}");
        }
        for (tab, snap) in [(0, island()), (1, island()), (1, car()), (2, island())] {
            let mut t = top(Some(snap));
            t.tab = tab;
            assert!(!screen(&mut t, MIN_SIZE.0, MIN_SIZE.1).contains("Make the terminal"));
            assert!(
                screen(&mut t, MIN_SIZE.0 - 1, MIN_SIZE.1).contains("Make the terminal at least"),
                "too small to be any use"
            );
        }
    }

    #[test]
    fn details_show_the_whole_trip() {
        let mut t = top(Some(island()));
        t.rows[0].select(Some(4));
        t.open = true;
        let handed = screen(&mut t, 80, 24);
        for want in [
            " HANDED OVER  routine  north to south",
            "From N0AAA at north, signed by N0AAA",
            "11-20 12:00  sent from north",
            "11-20 13:00  picked up by car N0CAR, control operator N0DRV",
            "11-20 16:00  handed to south by car N0CAR, control operator N0DRV",
            "Handed to south; its receipt isn't back yet.",
            "Its page: http://127.0.0.1:8800/msg?id=handed",
            "Esc back",
        ] {
            assert!(handed.contains(want), "{want:?} in\n{handed}");
        }
        t.rows[0].select(Some(2));
        assert!(
            screen(&mut t, 80, 24)
                .contains("Overdue: N0CAR was expected at south about 11-20 16:20, 40 min ago.")
        );
        t.tab = 1;
        t.rows[1].select(Some(0));
        let run = screen(&mut t, 80, 24);
        assert!(run.contains("evening: north to south with N0DRV"), "{run}");
        assert!(
            run.contains("1 item(s), text only; room for 50.0 MB."),
            "{run}"
        );
        t.rows[1].select(Some(1));
        assert!(
            screen(&mut t, 80, 24).contains("Goes with N0DRV at 11-20 17:25 (evening)"),
            "a planned message opens in full"
        );
        t.tab = 2;
        t.rows[2].select(Some(0));
        let courier = screen(&mut t, 80, 24);
        assert!(courier.contains("Next 11-20 17:25, room 50.0 MB, registered by N0AAA."));
        assert!(courier.contains("parks by the fire station node"));
    }

    #[test]
    fn it_says_so_while_loading_when_unreachable_and_when_empty() {
        let mut t = top(None);
        assert!(screen(&mut t, 80, 24).contains("Loading..."));
        t.take(
            Err("http://127.0.0.1:8800/api/mule: could not connect".into()),
            NOW,
        );
        let down = flat(&screen(&mut t, 80, 24));
        assert!(
            down.contains(
                "Can't reach ridgewarden-mule: http://127.0.0.1:8800/api/mule: could not connect."
            ),
            "{down}"
        );
        t.take(Ok(island()), NOW);
        t.take(Err("http://127.0.0.1:8800/api/mule: timed out".into()), NOW);
        let stale = flat(&screen(&mut t, 80, 24));
        assert!(stale.contains("Reading failed (timed out), so this is from earlier."));
        assert!(stale.contains(" OVERDUE "), "the last good read stays");

        let lost = Snapshot {
            core_error: Some(
                "Can't read the core: could not connect. The last good read was 4 min ago.".into(),
            ),
            ..island()
        };
        let mut t = top(Some(lost));
        let blind = screen(&mut t, 80, 24);
        assert!(blind.contains("core trouble, updated 5 min ago"), "{blind}");
        assert!(
            blind.contains(
                "Can't read the core: could not connect. The last good read was 4 min ago."
            )
        );

        let mut t = top(Some(empty()));
        let wants = [
            "No messages yet. Send one from the Send page",
            "No courier has registered a trip and nothing is waiting.",
            "No courier has registered a trip and no car has logged a pickup yet.",
        ];
        for (tab, want) in wants.iter().enumerate() {
            t.tab = tab;
            assert!(
                flat(&screen(&mut t, 80, 24)).contains(want),
                "page {tab}: {want}"
            );
        }
        let mut c = top(Some(Snapshot {
            manifest: None,
            core_error: Some("Can't reach the core: could not connect.".into()),
            ..car()
        }));
        c.tab = 1;
        assert!(flat(&screen(&mut c, 80, 24)).contains("so it doesn't know this car's key"));
    }

    #[test]
    fn details_follow_their_message_across_reads() {
        let mut t = top(Some(island()));
        for _ in 0..3 {
            t.key(key(KeyCode::Down));
        }
        t.key(key(KeyCode::Enter));
        assert_eq!((t.open, t.picked[0].as_deref()), (true, Some("late")));
        let mut s = island();
        for id in ["new", "newer"] {
            s.messages
                .insert(0, msg(id, Class::Urgent, State::Waiting, NOW));
        }
        t.take(Ok(s), NOW);
        assert_eq!(
            (t.open, t.rows[0].selected()),
            (true, Some(4)),
            "two new rows above it; still the same message"
        );
        assert!(screen(&mut t, 80, 24).contains("Overdue: N0CAR was expected"));

        let mut s = island();
        s.messages.retain(|m| m.id != "late");
        t.take(Ok(s), NOW);
        assert!(!t.open);
        assert!(screen(&mut t, 80, 24).contains(GONE));
        t.key(key(KeyCode::Down));
        assert!(
            !screen(&mut t, 80, 24).contains(GONE),
            "the notice lasts until the next key"
        );
    }

    #[test]
    fn long_details_say_there_is_more_and_scroll() {
        let mut s = island();
        s.messages[0].text = (1..=60)
            .map(|i| format!("line {i} of the roster"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut t = top(Some(s));
        t.key(key(KeyCode::Enter));
        let first = screen(&mut t, 80, 24);
        assert!(
            first.contains("Lines 1-20 of 69; PgUp and PgDn scroll"),
            "{first}"
        );
        assert!(!first.contains("line 60 of the roster"));
        t.key(key(KeyCode::PageDown));
        assert!(screen(&mut t, 80, 24).contains("Lines 21-40 of 69"));
        for _ in 0..5 {
            t.key(key(KeyCode::PageDown));
        }
        let end = screen(&mut t, 80, 24);
        assert!(end.contains("line 60 of the roster") && end.contains("Lines 50-69 of 69"));
        t.key(key(KeyCode::PageUp));
        assert!(screen(&mut t, 80, 24).contains("Lines 30-49 of 69"));
        t.key(key(KeyCode::Down));
        assert!(
            screen(&mut t, 80, 24).contains(" WAITING  routine  north to east"),
            "the next message, from its top"
        );
    }

    #[test]
    fn details_are_cut_between_words_to_fit() {
        let lines = vec![
            Line::styled(
                "one two three four",
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::raw("abcdefghij"),
        ];
        let rows = fit(lines, 9);
        let text: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text, ["one two ", "three ", "four", "", "abcdefghi", "j"]);
        assert_eq!(
            rows[1].style,
            Style::new().add_modifier(Modifier::BOLD),
            "each piece keeps its style"
        );
    }

    #[test]
    fn r_reads_again_and_backspace_closes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let reads = Arc::new(AtomicUsize::new(0));
        let n = reads.clone();
        let refresh = Refresh::spawn(Duration::from_secs(3600), async move || {
            n.fetch_add(1, SeqCst);
            Err("not read in this test".to_string())
        })
        .unwrap();
        let mut t = Top::new(
            "http://127.0.0.1:8800".into(),
            Theme { color: false },
            refresh,
        );
        let until = |want| {
            let end = std::time::Instant::now() + Duration::from_secs(10);
            while reads.load(SeqCst) < want {
                assert!(std::time::Instant::now() < end, "read {want} never came");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        until(1);
        t.key(key(KeyCode::Char('r')));
        until(2);

        t.snap = Some(island());
        t.key(key(KeyCode::Enter));
        assert!(t.open);
        t.key(key(KeyCode::Backspace));
        assert!(!t.open);
    }

    #[test]
    fn an_answer_it_cant_read_says_to_restart_or_upgrade() {
        let mut t = top(None);
        for (err, want) in [
            ("HTTP 404", UPGRADE),
            ("unexpected JSON", UPGRADE),
            ("could not connect", START),
        ] {
            t.take(Err(format!("http://127.0.0.1:8800/api/mule: {err}")), NOW);
            let page = flat(&screen(&mut t, 80, 24));
            assert!(page.contains(want), "{err}: {page}");
            assert!(
                page.contains("can't reach the app"),
                "the status says so too"
            );
        }
    }

    #[test]
    fn long_lists_say_how_many_more_there_are() {
        let s = Snapshot {
            unpinned: ["east", "west", "far", "deep"].map(String::from).to_vec(),
            unpinned_total: 9,
            departures_total: 5,
            unplanned_total: 3,
            couriers_total: 4,
            ..island()
        };
        assert_eq!(
            unpinned(&s).as_deref(),
            Some("No keys pinned for east, west, far and 6 more, so their receipts can't count.")
        );
        let two = Snapshot {
            unpinned: ["east", "west"].map(String::from).to_vec(),
            unpinned_total: 2,
            ..island()
        };
        assert_eq!(
            unpinned(&two).as_deref(),
            Some("No keys pinned for east and west, so their receipts can't count.")
        );
        let mut t = top(Some(s));
        assert!(screen(&mut t, 80, 24).contains("east, west, far and 6 more"));
        t.tab = 1;
        assert!(
            screen(&mut t, 80, 24)
                .contains("and 3 more departures and 2 more not on any trip, not listed")
        );
        t.tab = 2;
        assert!(screen(&mut t, 80, 24).contains("and 3 more couriers, not listed"));
        let mut all = top(Some(island()));
        all.tab = 1;
        assert!(!screen(&mut all, 80, 24).contains("not listed"));
    }

    #[test]
    fn page_keys_go_by_the_rows_on_screen() {
        let mut s = island();
        for i in 0..100 {
            s.messages
                .push(msg(&format!("m{i}"), Class::Bulk, State::Waiting, NOW));
        }
        let mut t = top(Some(s));
        screen(&mut t, 80, 24);
        t.key(key(KeyCode::PageDown));
        // 24 rows, less the title, tabs, help, two lines above, a gap and the header: 17
        assert_eq!(t.rows[0].selected(), Some(16));
        screen(&mut t, 80, 40);
        t.key(key(KeyCode::PageDown));
        assert_eq!(t.rows[0].selected(), Some(16 + 33));
    }

    /// What `/api/mule` returned on the simulated north island, and in the car while it
    /// loaded a file, in the scene the README's pictures come from.
    const NORTH: &str = include_str!("../tests/fixtures/mule-north.json");
    const CAR: &str = include_str!("../tests/fixtures/mule-car.json");

    fn fixture(json: &str) -> Snapshot {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_real_island_reads_the_way_its_operator_needs() {
        let s = fixture(NORTH);
        let rows: Vec<(Mark, String)> = s
            .messages
            .iter()
            .map(|m| {
                let st = status(m, &s);
                (st.mark(s.now_ms), st.short(m, s.now_ms))
            })
            .collect();
        let want = |k, t: &str| (k, t.to_string());
        assert_eq!(
            rows,
            [
                want(Mark::Waiting, "N0DRV leaves in 15 min"),
                want(Mark::Waiting, "no courier, waiting 20 s"),
                want(Mark::Waiting, "N0DRV leaves in 15 min"),
                want(Mark::Waiting, "N0DRV leaves in 23 h"),
                want(Mark::Overdue, "on N0CAR, 7 min late"),
                want(Mark::InTransit, "on N0CAR, due in 1 h"),
                want(Mark::Delivered, "in 3 min, receipt N0BBB"),
                want(Mark::Delivered, "in 3 min, receipt N0BBB"),
                want(Mark::Delivered, "in 3 min, receipt N0BBB"),
                want(Mark::Delivered, "in 2 min, receipt N0BBB"),
            ]
        );
        assert_eq!(
            totals(&s),
            "4 waiting, 2 in transit (1 overdue), 4 delivered."
        );
        let plan: Vec<String> = plan_rows(&s)
            .iter()
            .map(|r| match r {
                PlanRow::Departure(d) => format!("{} {}", d.run.name, d.items),
                PlanRow::Cargo(c) => c.label.chars().take(12).collect(),
                PlanRow::Unplanned(c, w) => format!("{} {}", why_word(*w), c.to),
            })
            .collect();
        assert_eq!(
            plan,
            [
                "evening-south 2",
                "Radio check ",
                "Updated map ",
                "first-south 1",
                "Offline libr",
                "relay-south 0",
                "sat-south 0",
                "no courier east"
            ],
            "the December maps did not fit in evening-south's 5 MB, so they wait a day"
        );

        let mut t = top(Some(s));
        for tab in 0..3 {
            t.tab = tab;
            let page = screen(&mut t, 80, 24);
            assert!(!page.contains("Make the terminal"), "{page}");
            assert!(page.contains("ridgewarden-mule top  north / N0AAA"));
        }
        t.tab = 0;
        let page = screen(&mut t, 80, 24);
        let overdue = row(&page, " OVERDUE ");
        for want in [
            "routine",
            "south",
            "Generator at the",
            "on N0CAR, 7 min late",
        ] {
            assert!(overdue.contains(want), "{want:?} in {overdue:?}");
        }
        assert!(row(&page, "no courier, waiting 20 s").contains("east"));
    }

    /// The one line of the screen that shows `needle`.
    fn row<'a>(screen: &'a str, needle: &str) -> &'a str {
        let found: Vec<&str> = screen.lines().filter(|l| l.contains(needle)).collect();
        assert_eq!(found.len(), 1, "{needle:?} once in\n{screen}");
        found[0]
    }

    #[test]
    fn a_real_car_says_what_is_still_loading() {
        let s = fixture(CAR);
        let m = s.manifest.clone().unwrap();
        assert_eq!(
            drive_text(&m, s.now_ms),
            [
                "Drive: north to south with N0DRV (evening-south)",
                "Leaves 11-24 00:17 (in 19 min), there about 11-24 01:47."
            ]
        );
        let marks: Vec<Mark> = manifest_rows(&m).iter().map(Load::mark).collect();
        assert_eq!(
            marks,
            [Mark::Aboard, Mark::Loading],
            "the fuel run is aboard, the roster is not yet"
        );
        assert_eq!(
            headline(&m),
            (
                Some(Mark::Loading),
                "Still loading 1.2 MB of 1.2 MB.".into()
            )
        );
        let mut t = top(Some(s));
        t.tab = 1;
        let page = screen(&mut t, 80, 24);
        for want in [
            " 1 Messages  2 Manifest  3 Couriers",
            " LOADING  Still loading 1.2 MB of 1.2 MB.",
        ] {
            assert!(page.contains(want), "{want:?} in\n{page}");
        }
        let fuel = row(&page, "Generator at the fire");
        assert!(
            fuel.starts_with(" ABOARD ") && fuel.contains(" text "),
            "{fuel:?}"
        );
        let roster = row(&page, "Shelter roster");
        assert!(
            roster.starts_with(" LOADING ") && roster.contains(" 1.2 MB "),
            "{roster:?}"
        );
    }

    /// A dark terminal's colors, for the README picture.
    fn rgb(c: Ansi, default: &'static str) -> &'static str {
        match c {
            Ansi::Black => "#0d1117",
            Ansi::Red => "#e5534b",
            Ansi::Green => "#3fb950",
            Ansi::Yellow => "#d29922",
            Ansi::Gray => "#a5aeb8",
            Ansi::White => "#ffffff",
            _ => default,
        }
    }

    fn span(style: &str, text: &str) -> String {
        format!("<span style=\"{style}\">{}</span>", esc(text))
    }

    /// The buffer as a page of colored monospace cells, which a browser can take a picture of.
    fn html(b: &Buffer) -> String {
        const FG: &str = "#e6edf3";
        const BG: &str = "#0d1117";
        let mut out = String::from(
            "<!doctype html><meta charset=utf-8><title>ridgewarden-mule top</title><style>\
             body{margin:0;background:#fff}pre{margin:0;padding:14px 16px;display:inline-block;\
             background:#0d1117;color:#e6edf3;font:15px/1.4 \"Cascadia Mono\",Consolas,monospace}\
             </style><pre>",
        );
        for y in 0..b.area.height {
            let mut run: Option<(String, String)> = None;
            for x in 0..b.area.width {
                let cell = &b[(x, y)];
                let (mut fg, mut bg) = (rgb(cell.fg, FG), rgb(cell.bg, BG));
                if cell.modifier.contains(Modifier::REVERSED) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.modifier.contains(Modifier::DIM) && fg == FG {
                    fg = "#8b949e";
                }
                let weight = if cell.modifier.contains(Modifier::BOLD) {
                    700
                } else {
                    400
                };
                let css = format!("color:{fg};background:{bg};font-weight:{weight}");
                match &mut run {
                    Some((style, text)) if *style == css => text.push_str(cell.symbol()),
                    _ => {
                        if let Some((style, text)) = run.take() {
                            out.push_str(&span(&style, &text));
                        }
                        run = Some((css, cell.symbol().to_string()));
                    }
                }
            }
            if let Some((style, text)) = run {
                out.push_str(&span(&style, &text));
            }
            out.push('\n');
        }
        out.push_str("</pre>");
        out
    }

    /// Writes the README's picture of `top` as a page to screenshot, from the north fixture,
    /// only when README_SHOTS=1.
    #[test]
    fn readme_shot() {
        if std::env::var("README_SHOTS").as_deref() != Ok("1") {
            return;
        }
        let mut t = top(Some(fixture(NORTH)));
        t.theme = Theme { color: true };
        t.got_ms = now_ms();
        t.rows[0].select(Some(4));
        let mut term = Terminal::new(TestBackend::new(80, 18)).unwrap();
        term.draw(|f| t.draw(f)).unwrap();
        let path = std::env::temp_dir().join("ridgewarden-mule-top.html");
        std::fs::write(&path, html(term.backend().buffer())).unwrap();
        println!("wrote {}", path.display());
    }
}
