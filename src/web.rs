//! The pages: the mule page (waiting, in transit, delivered, corridor times), lists a page
//! at a time, a page per message, the plan and tonight's manifest, the trips couriers have
//! registered, the send form, and delivered files.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ridgewarden_client::ui::{Color, Page, pill};
use ridgewarden_client::util::{esc, human_duration, now_ms, short_time};
use serde::Deserialize;
use serde_json::json;

use crate::app::{App, DOWNLOADS};
use crate::car::{self, Drive};
use crate::config::Config;
use crate::forms;
use crate::mule::{self, Class, Corridor, State as Trip, View, is_id};
use crate::plan::Plan;
use crate::send::{cargo_size, size_text};
use crate::trips;

const STYLE: &str = "<style>
form.send label{display:block;margin:10px 0}
form.send input,form.send select,form.send textarea{display:block;margin-top:4px;font:inherit;padding:6px 8px;width:100%;max-width:34em}
main p{overflow-wrap:anywhere}
form.send button,form.inline button{font:inherit;padding:6px 14px;margin-top:6px}
form.inline{display:inline}
pre.text{white-space:pre-wrap;background:var(--card);padding:10px 12px;border-radius:6px;font:inherit}
.err{color:var(--red)}td.n,th.n{text-align:right}
</style>";

/// Rows per section on the main page, and per page of a full list.
const SECTION: usize = 25;
const PAGE: usize = 50;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(mule_page))
        .route("/list", get(list_page))
        .route("/msg", get(msg_page))
        .route("/send", get(send_page).post(forms::send_post))
        .route("/withdraw", post(forms::withdraw_post))
        .route("/trips", get(trips_page).post(forms::trips_post))
        .route("/manifest", get(manifest_page))
        .route("/drive", post(forms::drive_post))
        .route("/file", get(file))
        .route("/api/corridors", get(corridors_json))
        .route("/api/mule", get(crate::api::mule_json))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

pub struct Fail(pub anyhow::Error);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.into())
    }
}

pub async fn page(app: &App, active: &str, title: &str, body: &str, refresh: bool) -> Html<String> {
    let place = app.cfg.island.as_deref().unwrap_or("car");
    let who = match app.whoami().await {
        Ok(w) => format!("{place} / {}", w.call),
        Err(_) => format!("{place} / core not reachable"),
    };
    let nav: &[(&str, &str)] = if app.cfg.island.is_some() {
        &[
            ("/", "Mule"),
            ("/send", "Send"),
            ("/manifest", "Plan"),
            ("/trips", "Trips"),
        ]
    } else {
        &[
            ("/", "Mule"),
            ("/manifest", "Manifest"),
            ("/trips", "Trips"),
        ]
    };
    let p = Page {
        app: "ridgewarden-mule",
        nav,
        active,
        who: &who,
        refresh,
    };
    let trouble = match app.core_trouble(now_ms()) {
        Some(t) => format!(
            "<div class=banner>{} What this page shows may be out of date.</div>",
            esc(&t)
        ),
        None => String::new(),
    };
    Html(p.render(title, &format!("{STYLE}{trouble}{body}")))
}

fn state_pill(s: Trip) -> String {
    match s {
        Trip::Waiting => pill(Color::Unknown, "WAITING"),
        Trip::InTransit => pill(Color::Yellow, "IN TRANSIT"),
        Trip::HandedOver => pill(Color::Yellow, "HANDED OVER"),
        Trip::Delivered => pill(Color::Green, "DELIVERED"),
        Trip::Withdrawn => pill(Color::Unknown, "WITHDRAWN"),
    }
}

fn class_pill(c: Class) -> String {
    pill(c.color(), c.name())
}

fn run_text(r: &crate::plan::Run) -> String {
    format!(
        "{} on {} ({}), expected at {} about {}",
        esc(&r.trip.courier),
        short_time(r.depart_ms),
        esc(&r.trip.id),
        esc(&r.trip.to),
        short_time(r.arrives_ms())
    )
}

/// Where the message is on its trip, in words.
fn trip(v: &View, plan: Option<&Plan>, now: i64) -> String {
    let car = |h: &mule::Hop| format!("{} (operator {})", esc(&h.car_call), esc(&h.operator));
    match v.state {
        Trip::Waiting => {
            let mut t = format!("waiting {}", human_duration(now - v.msg.sent_ms));
            if let Some(p) = plan
                && let Some(i) = p.find(&v.msg.id)
            {
                match (p.run_of(i), p.why(i)) {
                    (Some(r), _) => {
                        let _ = write!(t, "; goes with {}", run_text(r));
                    }
                    (None, Some(why)) => {
                        let _ = write!(t, "; {}", why.text());
                    }
                    _ => {}
                }
            }
            t
        }
        Trip::InTransit => {
            let h = &v.pickups[0];
            let mut t = format!("picked up by {} {}", car(h), short_time(h.ms));
            let Some(key) = h.trip.as_deref() else {
                t.push_str("; not a registered trip, so no expected time");
                return t;
            };
            match plan.and_then(|p| crate::trips::arrival(&p.trips, key, h.ms)) {
                Some(ms) if ms >= now => {
                    let _ = write!(
                        t,
                        "; expected at {} about {}",
                        esc(&v.msg.to_island),
                        short_time(ms)
                    );
                }
                Some(ms) => {
                    let _ = write!(t, "; overdue, expected about {}", short_time(ms));
                }
                None => {}
            }
            t
        }
        Trip::HandedOver => {
            let h = &v.dropoffs[0];
            format!(
                "handed to {} by {} {}, receipt not back yet",
                esc(h.island.as_deref().unwrap_or("?")),
                car(h),
                short_time(h.ms)
            )
        }
        Trip::Delivered => match &v.receipt {
            Some(r) => format!(
                "delivered in {}, receipt from {}",
                human_duration(r.ms - v.msg.sent_ms),
                esc(&r.site_call)
            ),
            None => "delivered".into(),
        },
        Trip::Withdrawn => "withdrawn by the sender".into(),
    }
}

fn preview(v: &View) -> String {
    let m = &v.msg;
    let mut t: String = m.text.chars().take(80).collect();
    if m.text.chars().count() > 80 {
        t.push_str("...");
    }
    if let Some(f) = &m.file {
        if !t.is_empty() {
            t.push(' ');
        }
        let _ = write!(t, "[file {}, {}]", f.name, size_text(f.size));
    }
    t
}

fn rows(views: &[View], plan: Option<&Plan>, now: i64) -> String {
    if views.is_empty() {
        return "<p class=mute>None.</p>".into();
    }
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Class</th><th>From</th><th>To</th><th>Message</th><th>Trip</th></tr>",
    );
    for v in views {
        let m = &v.msg;
        let to = match &m.to_call {
            Some(c) => format!("{} ({})", esc(&m.to_island), esc(c)),
            None => esc(&m.to_island),
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{} ({})</td><td>{to}</td><td><a href=\"/msg?id={}\">{}</a></td><td class=small>{}</td></tr>",
            class_pill(m.class),
            esc(&m.from_island),
            esc(&m.from_call),
            m.id,
            esc(&preview(v)),
            trip(v, plan, now)
        );
    }
    b.push_str("</table></div>");
    b
}

fn corridor_table(c: &[Corridor]) -> String {
    if c.is_empty() {
        return "<p class=mute>No receipts yet, so nothing measured.</p>".into();
    }
    let mut b = String::from(
        "<div class=wrap><table><tr><th>From</th><th>To</th><th>Class</th><th class=n>Delivered</th>\
         <th>Median, send to receipt</th><th>Median drive, car's clock</th></tr>",
    );
    let mut skewed = 0;
    for x in c {
        skewed += x.skewed;
        let car = if x.car_count == 0 {
            "<span class=mute>no car records</span>".to_string()
        } else {
            format!(
                "{} ({} drives)",
                human_duration(x.car_median_ms),
                x.car_count
            )
        };
        let median = if x.count == 0 {
            "<span class=mute>none</span>".to_string()
        } else {
            human_duration(x.median_ms)
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td class=n>{}</td><td>{median}</td><td>{car}</td></tr>",
            esc(&x.from),
            esc(&x.to),
            class_pill(x.class),
            x.count
        );
    }
    b.push_str("</table></div>");
    b.push_str(
        "<p class=\"mute small\">Send to receipt mixes two stations' clocks (the sender's and the \
         receiver's), and a Pi without a clock battery can be off. The drive column uses only the \
         car's clock, from pickup to drop-off.",
    );
    if skewed > 0 {
        let _ = write!(
            b,
            " {skewed} receipt(s) claim to be older than their message, so a clock is wrong; they are left out of the medians."
        );
    }
    b.push_str("</p>");
    b
}

/// The rows the main page shows for one section, and how many there are in all.
pub struct Section {
    pub views: Vec<View>,
    pub total: usize,
}

fn section(conn: &rusqlite::Connection, tp: bool, states: &[Trip]) -> anyhow::Result<Section> {
    Ok(Section {
        views: mule::list(conn, tp, states, SECTION, 0)?,
        total: mule::count(conn, tp, states)?,
    })
}

fn more(s: &Section, state: &str) -> String {
    if s.total > s.views.len() {
        format!(
            "<p><a href=\"/list?state={state}\">All {} {}</a></p>",
            s.total,
            state.replace('_', " ")
        )
    } else {
        String::new()
    }
}

/// Islands that messages here go to but whose receipts can't count, for lack of keys.
pub fn unpinned(cfg: &Config, own_key: Option<&str>, views: &[View]) -> Vec<String> {
    let mut out: Vec<String> = views
        .iter()
        .map(|v| v.msg.to_island.clone())
        .filter(|i| cfg.island_keys(i, own_key).is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The main page's body.
pub fn mule_body(
    cfg: &Config,
    sections: [&Section; 3],
    plan: &Plan,
    corridors: &[Corridor],
    unpinned: &[String],
    now: i64,
) -> String {
    let [waiting, moving, done] = sections;
    let mut b = match &cfg.island {
        Some(i) => format!("<h1>Mule: island {}</h1>", esc(i)),
        None => format!("<h1>Mule: car, operator {}</h1>", esc(&cfg.operator)),
    };
    b.push_str(
        "<p class=mute>For when the internet is down: messages and files ride a car between mesh \
         islands, and receipts come back the same way or any other.</p>",
    );
    for i in unpinned {
        let _ = write!(
            b,
            "<div class=banner>Messages here go to {0}, but no keys are pinned for {0} under \
             [[islands]] in this site's config, so its receipts can't count and those messages \
             will never show as delivered. Add the public keys of {0}'s cores.</div>",
            esc(i)
        );
    }
    let _ = write!(
        b,
        "<div class=cards><div class=card><div class=n>{}</div>waiting</div>\
         <div class=card><div class=n>{}</div>in transit</div>\
         <div class=card><div class=n>{}</div>delivered</div></div>",
        waiting.total, moving.total, done.total
    );
    if cfg.island.is_some() {
        b.push_str(
            "<p><a href=\"/send\">Send a message</a> or see <a href=\"/manifest\">what goes with whom</a>.</p>",
        );
    } else {
        b.push_str("<p>Before you leave, check <a href=\"/manifest\">tonight's manifest</a>.</p>");
    }
    b.push_str("<h2>Waiting for a car</h2>");
    b.push_str(&rows(&waiting.views, Some(plan), now));
    b.push_str(&more(waiting, "waiting"));
    b.push_str("<h2>In transit</h2>");
    b.push_str(&rows(&moving.views, Some(plan), now));
    b.push_str(&more(moving, "in_transit"));
    b.push_str("<h2>Delivered</h2>");
    b.push_str(&rows(&done.views, None, now));
    b.push_str(&more(done, "delivered"));
    b.push_str("<h2>Corridor times</h2><p class=\"mute small\">By priority class. Also as JSON at <a href=\"/api/corridors\">/api/corridors</a>.</p>");
    b.push_str(&corridor_table(corridors));
    b
}

async fn mule_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let own = app.whoami().await.ok().map(|w| w.key);
    let plan = app.plan(now)?;
    let body = {
        let db = app.db();
        let tp = app.cfg.third_party;
        let w = section(&db, tp, &[Trip::Waiting])?;
        let m = section(&db, tp, &[Trip::InTransit, Trip::HandedOver])?;
        let d = section(&db, tp, &[Trip::Delivered])?;
        let c = mule::corridors(&db, tp)?;
        let missing = unpinned(&app.cfg, own.as_deref(), &mule::active(&db, tp)?);
        mule_body(&app.cfg, [&w, &m, &d], &plan, &c, &missing, now)
    };
    Ok(page(&app, "/", "Mule", &body, true).await)
}

#[derive(Deserialize)]
struct ListQ {
    state: String,
    #[serde(default)]
    page: usize,
}

async fn list_page(State(app): State<Arc<App>>, Query(q): Query<ListQ>) -> Result<Response, Fail> {
    let (states, title): (&[Trip], &str) = match q.state.as_str() {
        "waiting" => (&[Trip::Waiting], "Waiting for a car"),
        "in_transit" => (&[Trip::InTransit, Trip::HandedOver], "In transit"),
        "delivered" => (&[Trip::Delivered], "Delivered"),
        "withdrawn" => (&[Trip::Withdrawn], "Withdrawn"),
        _ => return Ok((StatusCode::NOT_FOUND, "no such list").into_response()),
    };
    let now = now_ms();
    let plan = app.plan(now)?;
    let (views, total) = {
        let db = app.db();
        let tp = app.cfg.third_party;
        (
            mule::list(&db, tp, states, PAGE, q.page.saturating_mul(PAGE))?,
            mule::count(&db, tp, states)?,
        )
    };
    let mut b = format!("<h1>{title}</h1><p class=mute>{total} in all.</p>");
    b.push_str(&rows(&views, Some(&plan), now));
    b.push_str("<p>");
    if q.page > 0 {
        let _ = write!(
            b,
            "<a href=\"/list?state={}&page={}\">Newer</a> ",
            q.state,
            q.page - 1
        );
    }
    if (q.page + 1) * PAGE < total {
        let _ = write!(
            b,
            "<a href=\"/list?state={}&page={}\">More</a>",
            q.state,
            q.page + 1
        );
    }
    b.push_str("</p>");
    Ok(page(&app, "/", title, &b, false).await.into_response())
}

#[derive(Deserialize)]
struct IdQ {
    id: String,
}

/// What this site alone knows about a message, for its page.
pub struct Local {
    pub saved: Option<mule::Saved>,
    pub receipts: Vec<(mule::Receipt, bool)>,
    pub bundle: Option<(String, i64)>,
    pub mine: bool,
    /// where it is, in words, with its expected arrival
    pub status: String,
}

/// One message with its whole trip, including who was control operator for each hop.
pub fn msg_body(v: &View, l: &Local) -> String {
    let m = &v.msg;
    let mut b = format!(
        "<h1>{} to {}</h1><p>{} {}</p>",
        esc(&m.from_island),
        esc(&m.to_island),
        class_pill(m.class),
        state_pill(v.state)
    );
    let _ = write!(
        b,
        "<p>From {} at {}, signed by {}{}{}{}</p>",
        esc(&m.from_call),
        esc(&m.from_island),
        esc(&m.author_call),
        m.to_call
            .as_ref()
            .map(|c| format!(", for {}", esc(c)))
            .unwrap_or_default(),
        m.on_behalf
            .as_ref()
            .map(|w| format!(", third-party traffic on behalf of {}", esc(w)))
            .unwrap_or_default(),
        m.relay
            .as_ref()
            .map(|r| format!(", relayed from a BPv7 bundle signed by {}", esc(&r.call)))
            .unwrap_or_default()
    );
    if !m.text.is_empty() {
        let _ = write!(b, "<pre class=text>{}</pre>", esc(&m.text));
    }
    if m.is_library() {
        b.push_str(
            "<p class=mute>An offline-library update. At the destination the library app takes              the file from its core, so this page doesn't keep a copy to download.</p>",
        );
    }
    if let Some(f) = &m.file {
        let _ = write!(
            b,
            "<p>File: {} ({}), blob <code class=small>{}</code></p>",
            esc(&f.name),
            size_text(f.size),
            &f.root[..16]
        );
        match &l.saved {
            Some(mule::Saved {
                path: Some(path), ..
            }) => {
                let _ = write!(
                    b,
                    "<p><a href=\"/file?id={}\">Download</a> <span class=\"mute small\">(saved and checked here as {})</span></p>",
                    m.id,
                    esc(path)
                );
            }
            Some(mule::Saved {
                error: Some(err),
                attempts,
                next_ms,
                ..
            }) => {
                let _ = write!(
                    b,
                    "<p class=err>Not delivered here yet: {} (attempt {attempts}, next try {}).</p>",
                    esc(err),
                    short_time(*next_ms)
                );
            }
            _ => {}
        }
    }
    if let Some((eid, ms)) = &l.bundle {
        let _ = write!(
            b,
            "<p>Sent as a BPv7 bundle to {} at {}.</p>",
            esc(eid),
            short_time(*ms)
        );
    }
    b.push_str("<h2>Trip</h2><div class=wrap><table><tr><th>When</th><th>What</th></tr>");
    let mut steps = vec![(m.sent_ms, format!("sent from {}", esc(&m.from_island)))];
    for h in &v.pickups {
        steps.push((
            h.ms,
            format!(
                "picked up by car {}, control operator {}{}",
                esc(&h.car_call),
                esc(&h.operator),
                h.trip
                    .as_ref()
                    .map(|t| format!(", trip {}", esc(t)))
                    .unwrap_or_default()
            ),
        ));
    }
    for h in &v.dropoffs {
        steps.push((
            h.ms,
            format!(
                "handed to {} by car {}, control operator {}",
                esc(h.island.as_deref().unwrap_or("?")),
                esc(&h.car_call),
                esc(&h.operator)
            ),
        ));
    }
    for (r, pinned) in &l.receipts {
        let note = if *pinned {
            String::new()
        } else {
            format!(
                " (not counted: that station's key is not pinned for {})",
                esc(&r.island)
            )
        };
        steps.push((
            r.ms,
            format!(
                "receipt for {} signed by {}{note}",
                esc(&r.island),
                esc(&r.site_call)
            ),
        ));
    }
    steps.sort_by_key(|s| s.0);
    for (ms, what) in steps {
        let _ = write!(b, "<tr><td>{}</td><td>{what}</td></tr>", short_time(ms));
    }
    b.push_str("</table></div>");
    let _ = write!(b, "<p class=\"mute small\">{}</p>", l.status);
    if l.mine && !matches!(v.state, Trip::Delivered | Trip::Withdrawn) {
        let _ = write!(
            b,
            "<form class=inline method=post action=\"/withdraw\" enctype=\"multipart/form-data\">\
             <input type=hidden name=id value=\"{}\"><label>Send key <input type=password name=key required autocomplete=off></label> \
             <button type=submit>Withdraw this message</button></form>\
             <p class=\"mute small\">Withdrawing stops cars loading it. A copy already on a car or at the destination still exists, since nothing on the mesh is erased.</p>",
            m.id
        );
    }
    if !m.from_island.is_empty() {
        let _ = write!(
            b,
            "<p><a href=\"/send?to={}&call={}\">Reply</a></p>",
            esc(&m.from_island),
            esc(&m.from_call)
        );
    }
    b
}

fn find(app: &App, id: &str) -> anyhow::Result<Option<View>> {
    if !is_id(id) {
        return Ok(None);
    }
    mule::find(&app.db(), app.cfg.third_party, id)
}

async fn msg_page(State(app): State<Arc<App>>, Query(q): Query<IdQ>) -> Result<Response, Fail> {
    let Some(v) = find(&app, &q.id)? else {
        let b = "<h1>No such message</h1><p>This site doesn't know that message (yet). <a href=\"/\">Back</a></p>";
        return Ok((
            StatusCode::NOT_FOUND,
            page(&app, "", "No such message", b, false).await,
        )
            .into_response());
    };
    let me = app.whoami().await.ok().map(|w| w.key);
    let now = now_ms();
    // only a message still on its way has a departure or an arrival to show
    let status = match v.state {
        Trip::Waiting | Trip::InTransit => trip(&v, Some(&app.plan(now)?), now),
        _ => trip(&v, None, now),
    };
    // one lock at a time: a guard in a struct literal lives until the whole literal is built
    let saved = mule::saved(&app.db(), &v.msg.id)?;
    let receipts = mule::receipts(&app.db(), &v.msg.id)?;
    let l = Local {
        saved,
        receipts,
        bundle: crate::bpv7::sent_as(&app, &v.msg.id)?,
        mine: me.as_deref() == Some(v.msg.author.as_str()),
        status,
    };
    let body = msg_body(&v, &l);
    Ok(page(&app, "", "Message", &body, false)
        .await
        .into_response())
}

async fn file(
    State(app): State<Arc<App>>,
    Query(q): Query<IdQ>,
    headers: axum::http::HeaderMap,
) -> Result<Response, Fail> {
    let file = find(&app, &q.id)?.and_then(|v| v.msg.file);
    let saved = match &file {
        Some(_) => mule::saved(&app.db(), &q.id)?.and_then(|s| s.path),
        None => None,
    };
    let (Some(f), Some(path)) = (file, saved) else {
        return Ok((
            StatusCode::NOT_FOUND,
            "no file for that message has been saved here",
        )
            .into_response());
    };
    let Ok(slot) = app.downloads.clone().try_acquire_owned() else {
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            format!("{DOWNLOADS} downloads are already going; try again shortly"),
        )
            .into_response());
    };
    let etag = format!("\"{}\"", f.root);
    let on_disk = std::path::Path::new(&path);
    match crate::download::serve(on_disk, &f.name, &etag, &headers, slot, app.download_stall).await
    {
        Ok(r) => Ok(r),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((
            StatusCode::NOT_FOUND,
            format!("{path} is gone from the core's export folder"),
        )
            .into_response()),
        Err(e) => Err(Fail(
            anyhow::Error::new(e).context(format!("reading {path}")),
        )),
    }
}

async fn corridors_json(State(app): State<Arc<App>>) -> Result<Json<serde_json::Value>, Fail> {
    let c = mule::corridors(&app.db(), app.cfg.third_party)?;
    let rows: Vec<serde_json::Value> = c
        .iter()
        .map(|x| {
            json!({"from": x.from, "to": x.to, "class": x.class.name(), "delivered": x.count,
                   "median_s": x.median_ms / 1000, "clock_skewed": x.skewed,
                   "car_drives": x.car_count, "car_median_s": x.car_median_ms / 1000})
        })
        .collect();
    Ok(Json(
        json!({"corridors": rows, "as_of": ridgewarden_client::util::ms_to_rfc3339(now_ms())}),
    ))
}

#[derive(Default, Deserialize)]
pub struct Fields {
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub call: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub on_behalf: String,
}

pub fn send_form(app: &App, f: &Fields, islands: &[String], error: Option<&str>) -> String {
    let mut b = String::from("<h1>Send a message</h1>");
    if let Some(e) = error {
        let _ = write!(b, "<div class=banner>{}</div>", esc(e));
    }
    b.push_str(
        "<p class=\"mute small\">Messages are signed by this site's core under its licensee's callsign, \
         name you as the operator who wrote them, and are never encrypted: anyone on the mesh can read them. \
         No business traffic. Urgent goes first, so keep it short.</p>\
         <form class=send method=post action=\"/send\" enctype=\"multipart/form-data\">\
         <label>Send key<input type=password name=key required autocomplete=off></label>",
    );
    let from = if f.from.is_empty() {
        &app.cfg.operator
    } else {
        &f.from
    };
    let _ = write!(
        b,
        "<label>Your callsign<input name=from value=\"{}\" required maxlength=20 autocapitalize=characters spellcheck=false></label>\
         <label>To island<input name=to value=\"{}\" required maxlength=63 list=islands autocapitalize=none spellcheck=false></label>\
         <datalist id=islands>",
        esc(from),
        esc(&f.to)
    );
    for i in islands
        .iter()
        .filter(|i| Some(i.as_str()) != app.cfg.island.as_deref())
    {
        let _ = write!(b, "<option value=\"{}\">", esc(i));
    }
    let _ = write!(
        b,
        "</datalist><label>For callsign (optional)<input name=call value=\"{}\" maxlength=20 autocapitalize=characters spellcheck=false></label>\
         <label>Priority<select name=class>",
        esc(&f.call)
    );
    for c in Class::ALL {
        let sel = if f.class == c.name() || (f.class.is_empty() && c == Class::Routine) {
            " selected"
        } else {
            ""
        };
        let hint = match c {
            Class::Urgent => format!("urgent (short; files up to {})", size_text(c.max_file())),
            Class::Routine => format!("routine (files up to {})", size_text(c.max_file())),
            Class::Bulk => format!(
                "bulk (big files, goes last; up to {} through this page)",
                size_text(app.cfg.upload_max_mb.saturating_mul(1_000_000))
            ),
        };
        let _ = write!(b, "<option value={}{sel}>{hint}</option>", c.name());
    }
    let _ = write!(
        b,
        "</select></label><label>Message<textarea name=text rows=6 maxlength={}>{}</textarea></label>",
        mule::MAX_TEXT,
        esc(&f.text),
    );
    if app.cfg.third_party {
        let _ = write!(
            b,
            "<label>On behalf of (third-party traffic, optional)<input name=for value=\"{}\" maxlength=100></label>",
            esc(&f.on_behalf)
        );
    }
    // the file comes last: the server checks the key and priority above it before
    // writing any of it to disk
    b.push_str(
        "<label>File (optional, up to the priority's limit)<input type=file name=file></label>\
         <button type=submit>Send</button></form>\
         <p class=\"mute small\">The send key is in this site's <code>send.key</code> file; ask the station's owner. \
         It goes over the mesh in clear like everything else, so it keeps visitors out, not eavesdroppers.</p>",
    );
    b
}

/// Why this site can't take messages from the form, if it can't.
fn closed(app: &App) -> Option<&'static str> {
    if app.cfg.island.is_none() {
        Some("This is a car. Messages are sent from sites on an island.")
    } else if app.send_key.is_none() {
        Some(
            "The send form is off because this app has no send key (see its log). The send command still works.",
        )
    } else {
        None
    }
}

async fn send_page(
    State(app): State<Arc<App>>,
    Query(f): Query<Fields>,
) -> Result<Html<String>, Fail> {
    let body = match closed(&app) {
        Some(why) => format!("<h1>Send a message</h1><p>{why}</p>"),
        None => send_form(&app, &f, &mule::islands(&app.db())?, None),
    };
    Ok(page(&app, "/send", "Send", &body, false).await)
}

fn trips_table(all: &[trips::Trip], me: Option<&str>, now: i64) -> String {
    if all.is_empty() {
        return "<p class=mute>No courier has registered a trip yet, so cargo waits for whichever car comes by.</p>".into();
    }
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Courier</th><th>From</th><th>To</th><th>When</th>\
         <th>Next</th><th class=n>Room</th><th>Registered by</th><th></th></tr>",
    );
    for t in all {
        let next = t
            .next(now)
            .map_or("not in the next two weeks".into(), short_time);
        let retire = if me == Some(t.author.as_str()) {
            format!(
                "<form class=inline method=post action=\"/trips\" enctype=\"multipart/form-data\">\
                 <input type=hidden name=retire value=\"{}\"><input type=password name=key required placeholder=\"send key\" autocomplete=off size=10> \
                 <button type=submit>Retire</button></form>",
                esc(&t.id)
            )
        } else {
            String::new()
        };
        let _ = write!(
            b,
            "<tr><td>{}<div class=\"mute small\">{}</div></td><td>{}</td><td>{}</td><td>{}</td><td>{next}</td>\
             <td class=n>{}</td><td>{}</td><td>{retire}</td></tr>",
            esc(&t.courier),
            esc(&t.note),
            esc(&t.from),
            esc(&t.to),
            esc(&t.schedule_text()),
            size_text(t.capacity_bytes()),
            esc(&t.author_call)
        );
    }
    b.push_str("</table></div>");
    b
}

/// Who has been driving: the cars' records next to what each courier registered, so the
/// people running the mesh can see which trips really happen.
pub fn couriers_table(all: &[trips::Trip], seen: &[mule::Seen], now: i64) -> String {
    let couriers = trips::couriers(all, seen, now);
    if couriers.is_empty() {
        return String::new();
    }
    let mut b = String::from(
        "<h2>Couriers</h2><div class=wrap><table><tr><th>Courier</th><th class=n>Trips</th>         <th class=n>Departures due, last 30 days</th><th class=n>Days with a pickup, last 30 days</th>         <th>Last pickup</th><th>Last drop-off</th></tr>",
    );
    let when = |ms: Option<i64>| ms.map_or("never".to_string(), short_time);
    for c in couriers {
        let _ = write!(
            b,
            "<tr><td>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td>{}</td><td>{}</td></tr>",
            esc(c.call),
            c.trips.len(),
            c.due,
            c.seen.map_or(0, |s| s.drive_days),
            when(c.seen.and_then(|s| s.last_pickup)),
            when(c.seen.and_then(|s| s.last_dropoff))
        );
    }
    b.push_str(
        "</table></div><p class=\"mute small\">Departures due counts this courier's registered departures since each trip was registered; a day with a pickup is one on which a pinned car logged a pickup naming this callsign as control operator.</p>",
    );
    b
}

pub fn trips_page_body(app: &App, now: i64, error: Option<&str>) -> anyhow::Result<String> {
    let all = trips::trips(&app.db())?;
    let me = app.whoami_cached().map(|w| w.key);
    let mut b = String::from(
        "<h1>Trips</h1><p class=mute>Couriers' regular drives between islands: the contact plan. \
         Cargo goes with the first departure on its corridor that has room, urgent first, then oldest first. \
         Times are UTC.</p>",
    );
    if let Some(e) = error {
        let _ = write!(b, "<div class=banner>{}</div>", esc(e));
    }
    b.push_str(&trips_table(&all, me.as_deref(), now));
    let seen = mule::couriers(&app.db(), now - trips::MONTH)?;
    b.push_str(&couriers_table(&all, &seen, now));
    if app.send_key.is_none() {
        b.push_str("<p class=mute>Registering trips from this page is off because this site has no send key.</p>");
        return Ok(b);
    }
    let _ = write!(
        b,
        "<h2>Register a trip</h2><form class=send method=post action=\"/trips\" enctype=\"multipart/form-data\">\
         <label>Short name (registering it again changes it)<input name=trip required maxlength=40 placeholder=\"friday-south\" spellcheck=false></label>\
         <label>Courier's callsign<input name=courier required maxlength=20 value=\"{}\" autocapitalize=characters spellcheck=false></label>\
         <label>From island<input name=from required maxlength=63 value=\"{}\" spellcheck=false></label>\
         <label>To island<input name=to required maxlength=63 spellcheck=false></label>\
         <label>Days<input name=days required placeholder=\"fri or mon,thu or daily\"></label>\
         <label>Leaves at (UTC)<input name=depart required placeholder=\"17:30\" maxlength=5></label>\
         <label>Drive length, hours<input name=hours required value=\"3\"></label>\
         <label>Room for files, MB<input name=capacity_mb required value=\"8000\"></label>\
         <label>Note (optional)<input name=note maxlength=200 placeholder=\"parks near the fire station node\"></label>\
         <label>Send key<input type=password name=key required autocomplete=off></label>\
         <button type=submit>Register</button></form>",
        esc(&app.cfg.operator),
        esc(app.cfg.island.as_deref().unwrap_or_default())
    );
    Ok(b)
}

async fn trips_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let _ = app.whoami().await;
    let body = trips_page_body(&app, now_ms(), None)?;
    Ok(page(&app, "/trips", "Trips", &body, false).await)
}

fn cargo_rows(p: &Plan, load: &[usize], status: &dyn Fn(usize) -> String, now: i64) -> String {
    if load.is_empty() {
        return "<p class=mute>Nothing.</p>".into();
    }
    let mut b = String::from(
        "<div class=wrap><table><tr><th class=n>#</th><th>Class</th><th>What</th><th>To</th>\
         <th class=n>Size</th><th>Waiting</th><th></th></tr>",
    );
    for (n, i) in load.iter().enumerate() {
        let c = &p.cargo[*i];
        let what = format!(
            "{}<a href=\"/msg?id={}\">{}</a>",
            if c.library { "library update: " } else { "" },
            c.msg,
            esc(&c.label)
        );
        let _ = write!(
            b,
            "<tr><td class=n>{}</td><td>{}</td><td>{what}</td><td>{}</td><td class=n>{}</td><td>{}</td><td class=small>{}</td></tr>",
            n + 1,
            class_pill(c.class),
            esc(&c.to),
            cargo_size(c.bytes),
            human_duration(now - c.since_ms),
            status(*i)
        );
    }
    b.push_str("</table></div>");
    b
}

/// The plan on an island: every upcoming departure and what goes with it.
pub fn plan_body(p: &Plan, now: i64) -> String {
    let mut b = String::from(
        "<h1>What goes with whom</h1><p class=mute>Every site works this out from the same events. \
         Messages wait for the first registered trip on their corridor with room, urgent first, then oldest.</p>",
    );
    let busy: Vec<&crate::plan::Run> = p.runs.iter().filter(|r| !r.load.is_empty()).collect();
    if busy.is_empty() {
        b.push_str("<p class=mute>Nothing is planned onto a trip.</p>");
    }
    for r in busy {
        let _ = write!(
            b,
            "<h2>{} to {}: {}</h2><p class=\"mute small\">{} of {} used</p>",
            esc(&r.trip.from),
            esc(&r.trip.to),
            run_text(r),
            size_text(r.bytes),
            size_text(r.trip.capacity_bytes())
        );
        b.push_str(&cargo_rows(p, &r.load, &|_| String::new(), now));
    }
    if !p.unplanned.is_empty() {
        b.push_str("<h2>Not on any trip</h2>");
        let idx: Vec<usize> = p.unplanned.iter().map(|(i, _)| *i).collect();
        b.push_str(&cargo_rows(
            p,
            &idx,
            &|i| p.why(i).map_or("", |w| w.text()).to_string(),
            now,
        ));
    }
    b
}

/// Tonight's manifest on the car: which drive, what goes, and whether it's all aboard yet.
pub fn manifest_body(
    m: &car::Manifest,
    carried: &[View],
    loaded: &dyn Fn(usize) -> bool,
    capacity: u64,
    choices: &[(String, String)],
    now: i64,
) -> String {
    let mut b = String::from("<h1>Tonight's manifest</h1>");
    let (room, what) = match &m.drive {
        Drive::Trip {
            trip,
            depart_ms,
            chosen,
        } => (
            trip.capacity_bytes(),
            format!(
                "{} to {}, {} on {}, arriving about {} ({}){}",
                esc(&trip.from),
                esc(&trip.to),
                esc(&trip.courier),
                short_time(*depart_ms),
                short_time(depart_ms + trip.length_ms()),
                if *depart_ms <= now {
                    "on the road"
                } else {
                    "not left yet"
                },
                if *chosen { ", picked on this page" } else { "" }
            ),
        ),
        Drive::AdHoc => (
            capacity,
            "not a registered trip: whatever has no courier in the next day".into(),
        ),
    };
    let held: u64 = carried
        .iter()
        .map(|v| v.msg.file.as_ref().map_or(0, |f| f.size))
        .sum();
    let bytes: u64 = held + m.load.iter().map(|i| m.plan.cargo[*i].bytes).sum::<u64>();
    let items = m.load.len() + carried.len();
    let missing: u64 = m
        .load
        .iter()
        .filter(|i| !loaded(**i))
        .map(|i| m.plan.cargo[*i].bytes)
        .sum();
    let _ = write!(b, "<p>Drive: {what}.</p>");
    let (color, head) = if items == 0 {
        (Color::Unknown, "Nothing to take on this drive.".to_string())
    } else if missing == 0 {
        (
            Color::Green,
            format!(
                "Everything is aboard: {items} item(s), {} of {}.",
                size_text(bytes),
                size_text(room)
            ),
        )
    } else {
        (
            Color::Yellow,
            format!(
                "Still loading {} of {}. Stay near the node if you can; what is loaded is kept if you leave.",
                size_text(missing),
                size_text(bytes)
            ),
        )
    };
    let _ = write!(
        b,
        "<div class=\"headline {}\">{}</div>",
        color.css(),
        esc(&head)
    );
    if !carried.is_empty() {
        b.push_str("<h2>Aboard until the destination receipts it</h2>");
        b.push_str(&rows(carried, Some(&m.plan), now));
        b.push_str("<h2>Still to load for this drive</h2>");
    }
    if !m.load.is_empty() {
        b.push_str(&cargo_rows(
            &m.plan,
            &m.load,
            &|i| {
                if loaded(i) {
                    "aboard".into()
                } else {
                    "loading".into()
                }
            },
            now,
        ));
    }
    b.push_str(
        "<h2>Change the drive</h2><form class=send method=post action=\"/drive\" enctype=\"multipart/form-data\">\
         <label>Drive<select name=drive><option value=auto>my next registered trip (automatic)</option>\
         <option value=adhoc>not a registered trip</option>",
    );
    for (v, label) in choices {
        let _ = write!(b, "<option value=\"{}\">{}</option>", esc(v), esc(label));
    }
    b.push_str(
        "</select></label><label>Send key<input type=password name=key required autocomplete=off></label>\
         <button type=submit>Use this drive</button></form>",
    );
    b
}

async fn manifest_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    if app.cfg.island.is_some() {
        let body = plan_body(&app.plan(now)?, now);
        return Ok(page(&app, "/manifest", "Plan", &body, true).await);
    }
    let me = app.whoami().await?.key;
    let m = car::manifest(&app, &me, now)?;
    let aboard = car::aboard(&app, &m).await?;
    let choices: Vec<(String, String)> = trips::trips(&app.db())?
        .iter()
        .flat_map(|t| {
            t.runs(now, now + 3 * 86_400_000)
                .into_iter()
                .filter(move |d| *d + t.length_ms() > now)
                .map(move |d| {
                    (
                        format!("{}|{d}", t.key()),
                        format!("{} to {}, {} on {}", t.from, t.to, t.courier, short_time(d)),
                    )
                })
        })
        .collect();
    let capacity = app.cfg.car.as_ref().map_or(0, |c| c.capacity_mb) * 1_000_000;
    let loaded = |i: usize| m.load.iter().zip(&aboard).any(|(j, ok)| *j == i && *ok);
    let active = app.active()?;
    let carried: Vec<View> = crate::mule::carrying(&active, &me)
        .into_iter()
        .cloned()
        .collect();
    let body = manifest_body(&m, &carried, &loaded, capacity, &choices, now);
    Ok(page(&app, "/manifest", "Manifest", &body, true).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mule::testing::Sites;
    use serde_json::json;

    fn cfg() -> Config {
        toml::from_str("island = \"south\"").unwrap()
    }

    fn sections(s: &Sites) -> [Section; 3] {
        [
            section(&s.conn, false, &[Trip::Waiting]).unwrap(),
            section(&s.conn, false, &[Trip::InTransit, Trip::HandedOver]).unwrap(),
            section(&s.conn, false, &[Trip::Delivered]).unwrap(),
        ]
    }

    fn body(s: &Sites, now: i64) -> String {
        let [w, m, d] = sections(s);
        let c = mule::corridors(&s.conn, false).unwrap();
        mule_body(&cfg(), [&w, &m, &d], &Plan::default(), &c, &[], now)
    }

    #[tokio::test]
    async fn every_page_answers() {
        let d = tempfile::tempdir().unwrap();
        let core = crate::testing::FakeCore::start("n0south", d.path()).await;
        let north = crate::testing::author("n0north");
        let msg = crate::testing::text_msg(&north, 1, "urgent", now_ms());
        core.add(msg.clone());
        let cfg = format!(
            "island = \"south\"\n[[islands]]\nname = \"north\"\nkeys = [\"{}\"]",
            north.key
        );
        let app = Arc::new(crate::testing::app(d.path(), &core, &cfg));
        ridgewarden_client::follow(&app.core, &app.db, crate::app::PROJECTIONS)
            .await
            .unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let svc = router(app).into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(l, svc).await.unwrap() });
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let paths = [
            "/".to_string(),
            format!("/msg?id={}", msg.id),
            format!("/msg?id={}", msg.id),
            "/list?state=waiting".into(),
            "/manifest".into(),
            "/trips".into(),
            "/send?to=north&call=N0AAA".into(),
            "/api/corridors".into(),
            "/api/mule".into(),
        ];
        for p in paths {
            let r = http.get(format!("{url}{p}")).send().await.unwrap();
            assert_eq!(r.status(), 200, "{p}");
        }
        let r = http
            .get(format!("{url}/msg?id={}", "0".repeat(64)))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
    }

    #[test]
    fn couriers_show_what_they_registered_next_to_what_was_seen() {
        const FRI: i64 = 1_795_132_800_000;
        const DAY: i64 = 86_400_000;
        let t = trips::Trip {
            author: "ab".repeat(32),
            author_call: "N0AAA".into(),
            id: "fri".into(),
            courier: "N0DRV".into(),
            from: "north".into(),
            to: "south".into(),
            days: trips::parse_days("fri").unwrap(),
            depart_min: 17 * 60,
            hours: 3.0,
            capacity_mb: 10,
            note: String::new(),
            ts: 0,
        };
        let seen = [mule::Seen {
            call: "N0XYZ".into(),
            last_pickup: Some(FRI),
            last_dropoff: None,
            drive_days: 2,
        }];
        let b = couriers_table(std::slice::from_ref(&t), &seen, FRI + 28 * DAY);
        assert!(
            b.contains(
                "<td>N0DRV</td><td class=n>1</td><td class=n>4</td><td class=n>0</td><td>never</td>"
            ),
            "{b}"
        );
        assert!(b.contains("<td>N0XYZ</td><td class=n>0</td><td class=n>0</td><td class=n>2</td>"));
        let fresh = trips::Trip {
            ts: FRI + 20 * DAY,
            ..t
        };
        let b = couriers_table(&[fresh], &[], FRI + 28 * DAY);
        assert!(
            b.contains("<td>N0DRV</td><td class=n>1</td><td class=n>1</td>"),
            "only the Friday since it was registered: {b}"
        );
    }

    #[test]
    fn in_transit_rows_say_when_to_expect_the_load() {
        let mut s = Sites::default();
        let id = s.msg("urgent", "south", 0);
        let t = trips::Trip {
            author: crate::mule::testing::key("n0north"),
            author_call: "N0NORTH".into(),
            id: "daily".into(),
            courier: "N0DRV".into(),
            from: "north".into(),
            to: "south".into(),
            days: 0x7f,
            depart_min: 60,
            hours: 2.0,
            capacity_mb: 10,
            note: String::new(),
            ts: 0,
        };
        let key = t.key();
        s.add(
            "n0car",
            "mule.pickup",
            json!({"msg": id, "operator": "N0DRV", "trip": key}),
            30 * 60_000,
        );
        let plan = crate::plan::plan(vec![], &[t], 0);
        let v = s.one(&id);
        assert!(trip(&v, Some(&plan), 0).contains("expected at south about 1970-01-01 03:00 UTC"));
        assert!(trip(&v, Some(&plan), 4 * 3_600_000).contains("overdue"));
    }

    #[test]
    fn the_page_sorts_messages_by_where_they_are() {
        let mut s = Sites::default();
        let a = s.msg("urgent", "south", 0);
        s.msg("bulk", "south", 0);
        s.add(
            "n0car",
            "mule.pickup",
            json!({"msg": a, "operator": "N0DRV"}),
            60_000,
        );
        let b = body(&s, 3_600_000);
        let waiting = b.find("Waiting for a car").unwrap();
        let transit = b.find("In transit</h2>").unwrap();
        let bulk = b.find(">bulk<").unwrap();
        let urgent = b.find(">urgent<").unwrap();
        assert!(waiting < bulk && bulk < transit && transit < urgent);
        assert!(b.contains("picked up by N0CAR (operator N0DRV)"));
        assert!(b.contains("No receipts yet"));
    }

    #[test]
    fn long_sections_link_to_the_full_list() {
        let mut s = Sites::default();
        for i in 0..(SECTION as i64 + 3) {
            s.msg("routine", "south", i);
        }
        assert!(body(&s, 0).contains("All 28 waiting"));
    }

    #[test]
    fn text_from_the_mesh_is_escaped() {
        let mut s = Sites::default();
        let root = "ab".repeat(32);
        s.add(
            "n0north",
            "blob.manifest",
            json!({"root": root, "size": 3}),
            0,
        );
        let p = json!({"from_island": "north", "to": "south", "from": "N0AAA", "class": "routine",
                       "text": "<script>alert(1)</script>",
                       "file": {"root": root, "name": "<b>x</b>", "size": 3}});
        s.add("n0north", "mule.msg", p, 0);
        let v = s.views();
        let l = Local {
            saved: None,
            receipts: vec![],
            bundle: None,
            mine: false,
            status: String::new(),
        };
        let page = body(&s, 0) + &msg_body(&v[0], &l);
        assert!(!page.contains("<script>") && !page.contains("<b>x"));
        assert!(page.contains("&lt;script&gt;"));
    }

    #[test]
    fn the_message_page_names_each_control_operator() {
        let mut s = Sites::default();
        let id = s.msg("urgent", "south", 0);
        s.add(
            "n0car",
            "mule.pickup",
            json!({"msg": id, "operator": "N0DRV", "trip": "abcd1234:fri"}),
            60_000,
        );
        s.add(
            "n0car",
            "mule.dropoff",
            json!({"msg": id, "operator": "N0DRV", "island": "south"}),
            120_000,
        );
        s.add(
            "n0rogue",
            "mule.receipt",
            json!({"msg": id, "island": "south"}),
            150_000,
        );
        s.add(
            "n0south",
            "mule.receipt",
            json!({"msg": id, "island": "south"}),
            180_000,
        );
        let l = Local {
            saved: None,
            receipts: mule::receipts(&s.conn, &id).unwrap(),
            bundle: None,
            mine: true,
            status: String::new(),
        };
        let b = msg_body(&s.one(&id), &l);
        assert!(b.contains("picked up by car N0CAR, control operator N0DRV, trip abcd1234:fri"));
        assert!(b.contains("handed to south by car N0CAR, control operator N0DRV"));
        assert!(b.contains("receipt for south signed by N0SOUTH<"));
        assert!(b.contains("signed by N0ROGUE (not counted"));
        assert!(b.contains("DELIVERED"));
        assert!(!b.contains("Withdraw this message"), "too late to withdraw");
    }

    #[test]
    fn the_corridor_table_owns_up_to_clock_skew() {
        let mut s = Sites::default();
        let id = s.msg("urgent", "south", 10 * 60_000);
        s.add(
            "n0south",
            "mule.receipt",
            json!({"msg": id, "island": "south"}),
            60_000,
        );
        let b = corridor_table(&mule::corridors(&s.conn, false).unwrap());
        assert!(b.contains("1 receipt(s) claim to be older"));
    }
}
