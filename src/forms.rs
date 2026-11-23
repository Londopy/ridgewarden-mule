//! Everything that changes state from a page: sending, withdrawing, registering and
//! retiring trips, and picking the car's drive. Every post must come from this site's own
//! page, and must carry the send key. An address gets one post every few seconds and two
//! open at once, and nothing else is held for it until the key checks out: a file in the
//! send form then takes one of the upload slots before any of it is written, and every
//! other post one of the form slots before it is acted on. The key travels in clear HTTP
//! like everything on the mesh, so it keeps casual visitors and spectators out; it is not a
//! secret from anyone listening on the RF link.

use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use ridgewarden_client::util::now_ms;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::SemaphorePermit;

use crate::app::{Address, App};
use crate::car;
use crate::mule::{self, Class, State as Trip, is_id};
use crate::multipart::{self, Ev, Parser};
use crate::send::{Outgoing, Staged, UPLOAD_PREFIX, a_call, send, staged_path};
use crate::trips::{self, parse_days, parse_hhmm};
use crate::web::{Fail, page, send_form, trips_page_body};

/// All the text fields of a form together; files don't count.
const MAX_FIELDS: usize = 16 << 10;
/// One post per address this often, so the key can't be guessed quickly.
const POST_GAP: Duration = Duration::from_secs(3);
/// Posts one address may have open at once: an upload and a message, say.
const OPEN_PER_ADDRESS: usize = 2;
/// Room for what may follow the file part in a body: the closing boundary, a stray field.
const AFTER_FILE: u64 = 4096;

/// Whether a form may carry a file. A file part has to pass the check first: it gets the
/// fields in front of it and answers with how big the file may be, or why not at all.
#[derive(Clone, Copy)]
pub enum Takes<'c> {
    Fields,
    File(&'c (dyn Fn(&Form) -> Result<u64, String> + Sync)),
}

/// A posted form: its text fields, and the file it brought, already in the core's import
/// folder. A file nobody takes is deleted when its `Staged` goes.
pub struct Form {
    fields: Vec<(String, String)>,
    pub file: Option<Staged>,
}

impl Form {
    pub fn get(&self, name: &str) -> String {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

/// A form post from another site's page carries that site's Origin.
fn same_origin(h: &HeaderMap) -> bool {
    let Some(origin) = h.get(ORIGIN) else {
        return true;
    };
    let host = h.get(HOST).and_then(|v| v.to_str().ok());
    origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, rest)| rest)
        == host
}

/// One address's open post; it stops counting when this goes.
pub struct Open<'a> {
    app: &'a App,
    ip: IpAddr,
}

impl Drop for Open<'_> {
    fn drop(&mut self) {
        let mut posts = self.app.posts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(a) = posts.get_mut(&self.ip) {
            a.open = a.open.saturating_sub(1);
        }
    }
}

/// Let a post from `ip` in, unless that address posted a moment ago or has too many open.
fn admit(app: &App, ip: IpAddr) -> Result<Open<'_>, Box<Response>> {
    let mut posts = app.posts.lock().unwrap_or_else(|e| e.into_inner());
    let recent = |a: &Address| a.last.is_some_and(|t| t.elapsed() < POST_GAP);
    posts.retain(|_, a| a.open > 0 || recent(a));
    let a = posts.entry(ip).or_insert(Address {
        last: None,
        open: 0,
    });
    if recent(a) {
        return Err(refused(
            StatusCode::TOO_MANY_REQUESTS,
            "Wait a few seconds before posting again.",
        ));
    }
    if a.open >= OPEN_PER_ADDRESS {
        return Err(refused(
            StatusCode::TOO_MANY_REQUESTS,
            "Your earlier posts are still going; try again when one has finished.",
        ));
    }
    a.last = Some(Instant::now());
    a.open += 1;
    Ok(Open { app, ip })
}

// compare hashes, so the time taken doesn't say how much of the key matched
pub fn key_ok(expected: &str, got: &str) -> bool {
    blake3::hash(got.trim().as_bytes()) == blake3::hash(expected.as_bytes())
}

fn refuse(code: StatusCode, why: &str) -> Response {
    (code, why.to_string()).into_response()
}

fn refused(code: StatusCode, why: &str) -> Box<Response> {
    Box::new(refuse(code, why))
}

/// Why reading a post stopped. Only a refusal is worth draining the rest of the body for;
/// a stalled or broken connection has nothing left to say.
enum Stop {
    Refuse(Box<Response>),
    Gone(Box<Response>),
}

impl From<Box<Response>> for Stop {
    fn from(r: Box<Response>) -> Self {
        Stop::Refuse(r)
    }
}

/// What a post that got through holds while it is acted on.
pub struct Held<'a> {
    _open: Open<'a>,
    _slot: SemaphorePermit<'a>,
}

/// The part of the form being read.
enum Cur {
    None,
    Field(String, Vec<u8>),
    File(tokio::fs::File, u64),
    /// a file input left empty
    Skip,
}

/// Everything a post has to get past, then its body, a piece at a time.
pub async fn read<'a>(
    app: &'a App,
    peer: SocketAddr,
    req: Request,
    takes: Takes<'_>,
) -> Result<(Form, Held<'a>), Box<Response>> {
    if !same_origin(req.headers()) {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "Posts have to come from this site's own pages.",
        ));
    }
    let Some(key) = app.send_key.as_deref() else {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "This site has no send key (see its log), so its forms are off. The command line still works.",
        ));
    };
    let open = admit(app, peer.ip())?;
    let Some(bd) = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(multipart::boundary)
    else {
        return Err(refused(
            StatusCode::BAD_REQUEST,
            "Expected one of this site's forms (multipart/form-data).",
        ));
    };
    let declared = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut body = req.into_body();
    let mut upload = None;
    let form = match read_body(app, key, &mut body, &bd, takes, declared, &mut upload).await {
        Ok(form) => form,
        Err(Stop::Gone(r)) => return Err(r),
        Err(Stop::Refuse(r)) => {
            drain(&mut body).await;
            return Err(r);
        }
    };
    let slot = match upload {
        Some(s) => s,
        None => app.forms.try_acquire().map_err(|_| {
            refused(
                StatusCode::TOO_MANY_REQUESTS,
                "Others are posting right now; try again in a moment.",
            )
        })?,
    };
    Ok((
        form,
        Held {
            _open: open,
            _slot: slot,
        },
    ))
}

/// Wait for the next piece of the body, for at most `wait`.
async fn next(body: &mut Body, wait: Duration) -> Result<Option<Bytes>, Stop> {
    match tokio::time::timeout(
        wait,
        std::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)),
    )
    .await
    {
        Err(_) => Err(Stop::Gone(refused(
            StatusCode::REQUEST_TIMEOUT,
            "The post stalled, so it was dropped. Try again, or use the send command on the site's box.",
        ))),
        Ok(None) => Ok(None),
        Ok(Some(Err(_))) => Err(Stop::Gone(refused(
            StatusCode::BAD_REQUEST,
            "The post broke off.",
        ))),
        // trailers carry no form data
        Ok(Some(Ok(f))) => Ok(Some(f.into_data().unwrap_or_default())),
    }
}

async fn read_body<'a>(
    app: &'a App,
    key: &str,
    body: &mut Body,
    bd: &str,
    takes: Takes<'_>,
    declared: Option<u64>,
    upload: &mut Option<SemaphorePermit<'a>>,
) -> Result<Form, Stop> {
    let started = Instant::now();
    let waits = app.post_waits;
    let mut parser = Parser::new(bd);
    let mut form = Form {
        fields: vec![],
        file: None,
    };
    let mut cur = Cur::None;
    // text, and anything thrown away, before a file part has shown the key
    let mut small = 0;
    let mut read = 0u64;
    let mut keyed = false;
    let bad = |why: &str| refused(StatusCode::BAD_REQUEST, why);
    let wrong_key = || refused(StatusCode::FORBIDDEN, "That send key is not right.");
    let failed = |what: &str, e: std::io::Error| {
        refused(StatusCode::INTERNAL_SERVER_ERROR, &format!("{what}: {e}"))
    };
    while !parser.done() {
        // a slow RF link may take hours over a big file, but it has to keep moving; before
        // the key has checked out nobody gets long
        let wait = if keyed {
            waits.idle
        } else {
            waits.before_key.saturating_sub(started.elapsed())
        };
        let Some(data) = next(body, wait.max(Duration::from_millis(1))).await? else {
            break;
        };
        read += data.len() as u64;
        for ev in parser.feed(&data).map_err(bad)? {
            match ev {
                Ev::Part { name, filename } => match filename {
                    Some(f) if !f.is_empty() => {
                        let Takes::File(check) = takes else {
                            return Err(bad("This form takes no file.").into());
                        };
                        // nothing is written to disk for someone without the key
                        if !key_ok(key, &form.get("key")) {
                            return Err(wrong_key().into());
                        }
                        if form.file.is_some() {
                            return Err(bad("One file per message.").into());
                        }
                        // and nothing is written for a message that would be refused anyway
                        let limit = check(&form).map_err(|e| bad(&e))?;
                        let too_big = |limit: u64| {
                            refused(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                &format!(
                                    "The file is bigger than {} takes here; pick bulk, or use the send command on the site's box.",
                                    crate::send::size_text(limit)
                                ),
                            )
                        };
                        if declared.is_some_and(|n| n.saturating_sub(read) > limit + AFTER_FILE) {
                            return Err(too_big(limit).into());
                        }
                        *upload = Some(app.uploads.try_acquire().map_err(|_| {
                            refused(
                                StatusCode::TOO_MANY_REQUESTS,
                                "Two uploads are already being taken in; try again when one has finished, or send the text without the file.",
                            )
                        })?);
                        keyed = true;
                        let path = staged_path(&app.cfg, UPLOAD_PREFIX, &f).map_err(|e| {
                            refused(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"))
                        })?;
                        // in the form before it exists, so it goes even if this stops halfway
                        form.file = Some(Staged {
                            path: path.clone(),
                            name: f,
                            size: 0,
                        });
                        let file = tokio::fs::File::create(&path)
                            .await
                            .map_err(|e| failed(&format!("creating {}", path.display()), e))?;
                        cur = Cur::File(file, limit);
                    }
                    Some(_) => cur = Cur::Skip,
                    None => cur = Cur::Field(name, vec![]),
                },
                Ev::Data(d) => match &mut cur {
                    Cur::File(file, limit) => {
                        let staged = form.file.as_mut().expect("a file part has its staged file");
                        staged.size += d.len() as u64;
                        if staged.size > *limit {
                            return Err(refused(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                &format!(
                                    "The file is bigger than {} takes here; pick bulk, or use the send command on the site's box.",
                                    crate::send::size_text(*limit)
                                ),
                            )
                            .into());
                        }
                        file.write_all(&d)
                            .await
                            .map_err(|e| failed("writing the upload", e))?;
                    }
                    other => {
                        small += d.len();
                        if small > MAX_FIELDS {
                            return Err(refused(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                "The form's text is longer than it takes.",
                            )
                            .into());
                        }
                        if let Cur::Field(_, v) = other {
                            v.extend(d);
                        }
                    }
                },
                Ev::End => match std::mem::replace(&mut cur, Cur::None) {
                    Cur::Field(name, v) => form
                        .fields
                        .push((name, String::from_utf8_lossy(&v).trim().to_string())),
                    Cur::File(mut file, _) => file
                        .flush()
                        .await
                        .map_err(|e| failed("writing the upload", e))?,
                    Cur::None | Cur::Skip => {}
                },
            }
        }
    }
    parser.finish().map_err(bad)?;
    if !key_ok(key, &form.get("key")) {
        return Err(wrong_key().into());
    }
    Ok(form)
}

/// Read and drop some of the rest of a refused post, so the browser gets to show why
/// instead of a reset connection: up to 16 MiB or 10 s, and only while it keeps coming.
async fn drain(body: &mut Body) {
    let until = Instant::now() + Duration::from_secs(10);
    let mut left: usize = 16 << 20;
    while left > 0 {
        let Some(wait) = until.checked_duration_since(Instant::now()) else {
            return;
        };
        match next(body, wait.min(Duration::from_secs(1))).await {
            Ok(Some(d)) => left = left.saturating_sub(d.len()),
            _ => return,
        }
    }
}

/// The message the send form describes; the file is checked separately.
fn outgoing(f: &Form) -> Result<Outgoing, String> {
    let class = Class::parse(&f.get("class")).ok_or("Pick a priority from the list.")?;
    Ok(Outgoing {
        to: f.get("to"),
        from: f.get("from"),
        to_call: Some(f.get("call")),
        class,
        text: f.get("text"),
        on_behalf: Some(f.get("for")),
    })
}

pub async fn send_post(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Result<Response, Fail> {
    if app.cfg.island.is_none() {
        return Ok(refuse(
            StatusCode::FORBIDDEN,
            "This is a car; messages are sent from sites on an island.",
        ));
    }
    let site_max = app.cfg.upload_max_mb.saturating_mul(1_000_000);
    let check = |f: &Form| -> Result<u64, String> {
        let o = outgoing(f)?;
        crate::send::payload(&app.cfg, &o, Some(1)).map_err(|e| format!("{e:#}"))?;
        Ok(o.class.max_file().min(site_max))
    };
    let (mut f, _held) = match read(&app, peer, req, Takes::File(&check)).await {
        Ok(x) => x,
        Err(r) => return Ok(*r),
    };
    let fields = crate::web::Fields {
        from: f.get("from"),
        to: f.get("to"),
        call: f.get("call"),
        class: f.get("class"),
        text: f.get("text"),
        on_behalf: f.get("for"),
    };
    let result = match outgoing(&f) {
        Err(e) => Err(e),
        Ok(o) => send(&app.core, &app.cfg, &o, f.file.take())
            .await
            .map_err(|e| format!("{e:#}")),
    };
    match result {
        Ok(ev) => {
            tracing::info!("sent {} to {}", &ev.id[..12], fields.to);
            refresh(&app).await;
            Ok(Redirect::to(&format!("/msg?id={}", ev.id)).into_response())
        }
        Err(e) => {
            let body = send_form(&app, &fields, &mule::islands(&app.db())?, Some(&e));
            Ok((
                StatusCode::BAD_REQUEST,
                page(&app, "/send", "Send", &body, false).await,
            )
                .into_response())
        }
    }
}

/// Read our own new event back into the views before showing the next page.
async fn refresh(app: &App) {
    if let Err(e) = ridgewarden_client::follow(&app.core, &app.db, crate::app::PROJECTIONS).await {
        tracing::warn!("following the core after a post: {e:#}");
    }
}

pub async fn withdraw_post(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Result<Response, Fail> {
    let (f, _held) = match read(&app, peer, req, Takes::Fields).await {
        Ok(x) => x,
        Err(r) => return Ok(*r),
    };
    let id = f.get("id");
    let me = app.whoami().await?;
    let v = is_id(&id)
        .then(|| mule::find(&app.db(), app.cfg.third_party, &id))
        .transpose()?
        .flatten();
    let Some(v) = v.filter(|v| v.msg.author == me.key) else {
        return Ok(refuse(
            StatusCode::FORBIDDEN,
            "Only the site that sent a message can withdraw it.",
        ));
    };
    if v.state == Trip::Delivered {
        return Ok(refuse(
            StatusCode::CONFLICT,
            "That message has already been delivered.",
        ));
    }
    app.core.append("mule.withdraw", json!({"msg": id})).await?;
    tracing::info!("withdrew {}", &id[..12]);
    refresh(&app).await;
    Ok(Redirect::to(&format!("/msg?id={id}")).into_response())
}

/// A trip from the registration form, checked.
fn trip_from(f: &Form, author: &str) -> Result<trips::Trip, String> {
    let id = f.get("trip").to_ascii_lowercase();
    if !ridgewarden_client::util::valid_node(&id) || id.len() > 40 {
        return Err("Give the trip a short name: lowercase letters, digits and hyphens.".into());
    }
    let courier = f.get("courier").to_ascii_uppercase();
    if !a_call(&courier) {
        return Err(format!("{courier:?} is not a callsign."));
    }
    let island = |k: &str| {
        let i = f.get(k).to_ascii_lowercase();
        ridgewarden_client::util::valid_node(&i)
            .then_some(i)
            .ok_or_else(|| {
                format!("{k} must be an island name (lowercase letters, digits and hyphens).")
            })
    };
    let days = parse_days(&f.get("days")).ok_or("Days look like \"mon,fri\" or \"daily\".")?;
    let depart_min =
        parse_hhmm(&f.get("depart")).ok_or("Departure time looks like 17:30 (UTC).")?;
    let hours: f64 = f
        .get("hours")
        .parse()
        .map_err(|_| "Hours is a number, like 3 or 2.5.")?;
    let capacity_mb: u64 = f
        .get("capacity_mb")
        .parse()
        .map_err(|_| "Room for files is a whole number of MB.")?;
    let note: String = f.get("note").chars().take(200).collect();
    Ok(trips::Trip {
        author: author.to_string(),
        author_call: String::new(),
        id,
        courier,
        from: island("from")?,
        to: island("to")?,
        days,
        depart_min,
        hours,
        capacity_mb,
        note,
        ts: 0,
    })
}

pub async fn trips_post(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Result<Response, Fail> {
    let (f, _held) = match read(&app, peer, req, Takes::Fields).await {
        Ok(x) => x,
        Err(r) => return Ok(*r),
    };
    let me = app.whoami().await?;
    let retire = f.get("retire");
    let payload = if !retire.is_empty() {
        let mine = trips::trips(&app.db())?
            .into_iter()
            .find(|t| t.id == retire && t.author == me.key);
        let Some(t) = mine else {
            return Ok(refuse(
                StatusCode::FORBIDDEN,
                "Only the site that registered a trip can retire it.",
            ));
        };
        let mut p = trips::payload(&t)?;
        p["retired"] = json!(true);
        Ok(p)
    } else {
        trip_from(&f, &me.key).and_then(|t| trips::payload(&t).map_err(|e| format!("{e:#}")))
    };
    match payload {
        Ok(p) => {
            app.core.append("mule.trip", p).await?;
            refresh(&app).await;
            Ok(Redirect::to("/trips").into_response())
        }
        Err(e) => {
            let body = trips_page_body(&app, now_ms(), Some(&e))?;
            Ok((
                StatusCode::BAD_REQUEST,
                page(&app, "/trips", "Trips", &body, false).await,
            )
                .into_response())
        }
    }
}

pub async fn drive_post(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Result<Response, Fail> {
    if app.cfg.car.is_none() {
        return Ok(refuse(StatusCode::FORBIDDEN, "Only a car picks a drive."));
    }
    let (f, _held) = match read(&app, peer, req, Takes::Fields).await {
        Ok(x) => x,
        Err(r) => return Ok(*r),
    };
    let now = now_ms();
    match f.get("drive").as_str() {
        "auto" => car::forget_choice(&app.db())?,
        "adhoc" => car::choose(&app.db(), None, now)?,
        pick => {
            let Some((key, dep)) = pick
                .rsplit_once('|')
                .and_then(|(k, d)| Some((k, d.parse::<i64>().ok()?)))
            else {
                return Ok(refuse(
                    StatusCode::BAD_REQUEST,
                    "Pick a drive from the list.",
                ));
            };
            if !trips::trips(&app.db())?.iter().any(|t| t.key() == key) {
                return Ok(refuse(
                    StatusCode::BAD_REQUEST,
                    "That trip is no longer registered.",
                ));
            }
            car::choose(&app.db(), Some((key, dep)), now)?;
        }
    }
    Ok(Redirect::to("/manifest").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeCore;

    fn multipart(fields: &[(&str, &str)]) -> Vec<u8> {
        let mut b = Vec::new();
        for (k, v) in fields {
            b.extend(
                format!("--XyZ\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                    .bytes(),
            );
        }
        b.extend(b"--XyZ--\r\n");
        b
    }

    /// A send form posted the way a browser does it: the fields in page order, then the file.
    fn send_body(key: &str, class: &str, file: Option<&[u8]>) -> Vec<u8> {
        let mut b = multipart(&[
            ("key", key),
            ("from", "N0AAA"),
            ("to", "south"),
            ("class", class),
            ("text", "tiles"),
        ]);
        b.truncate(b.len() - b"--XyZ--\r\n".len());
        if let Some(data) = file {
            b.extend(b"--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"tiles.bin\"\r\n\r\n");
            b.extend(data);
            b.extend(b"\r\n");
        }
        b.extend(b"--XyZ--\r\n");
        b
    }

    async fn serve(d: &std::path::Path) -> (FakeCore, Arc<App>, String) {
        serve_with(d, 200, |_| {}).await
    }

    async fn serve_with(
        d: &std::path::Path,
        upload_max_mb: u64,
        tweak: impl FnOnce(&mut App),
    ) -> (FakeCore, Arc<App>, String) {
        let core = FakeCore::start("n0north", d).await;
        let cfg =
            format!("island = \"north\"\noperator = \"N0AAA\"\nupload_max_mb = {upload_max_mb}");
        let mut app = crate::testing::app(d, &core, &cfg);
        tweak(&mut app);
        let app = Arc::new(app);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/send", l.local_addr().unwrap());
        let svc =
            crate::web::router(app.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(l, svc).await.unwrap() });
        (core, app, url)
    }

    async fn post(url: &str, body: Vec<u8>) -> reqwest::Response {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .post(url)
            .header("content-type", "multipart/form-data; boundary=XyZ")
            .body(body)
            .send()
            .await
            .unwrap()
    }

    fn imports(d: &std::path::Path) -> usize {
        std::fs::read_dir(d.join("imports")).map_or(0, |r| r.count())
    }

    /// The send form's head: key, sender, destination and priority, then the start of a
    /// file part, as a browser sends them.
    fn head_of_upload(key: &str, class: &str) -> Vec<u8> {
        let mut b = send_body(key, class, None);
        b.truncate(b.len() - b"--XyZ--\r\n".len());
        b.extend(
            b"--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"t.bin\"\r\n\r\n",
        );
        b
    }

    /// A post from its own loopback address, so the per-address limits don't mix tests up,
    /// that sends `first` of a body declared `total` long and then waits.
    async fn raw_post(url: &str, from: u8, total: usize, first: &[u8]) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt;
        let addr: SocketAddr = url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let path = &url[url.find(&addr.to_string()).unwrap() + addr.to_string().len()..];
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        sock.bind(SocketAddr::from(([127, 0, 0, from], 0))).unwrap();
        let mut c = sock.connect(addr).await.unwrap();
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: multipart/form-data; boundary=XyZ\r\nContent-Length: {total}\r\n\r\n"
        );
        c.write_all(head.as_bytes()).await.unwrap();
        c.write_all(first).await.unwrap();
        c
    }

    /// The status and start of the answer on a raw connection.
    async fn answer(c: &mut tokio::net::TcpStream) -> (u16, String) {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(10), c.read(&mut buf))
            .await
            .expect("an answer")
            .unwrap();
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        let code = text.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
        (code, text)
    }

    #[tokio::test]
    async fn the_site_limit_caps_uploads_below_the_class_limit() {
        let d = tempfile::tempdir().unwrap();
        let (core, app, url) = serve_with(d.path(), 1, |_| {}).await;
        let r = post(
            &url,
            send_body(
                app.send_key.as_ref().unwrap(),
                "bulk",
                Some(&vec![3u8; 2_000_000]),
            ),
        )
        .await;
        assert_eq!(r.status(), 413, "bulk takes 1 TB, but this site only 1 MB");
        assert!(r.text().await.unwrap().contains("1.0 MB"));
        assert_eq!(imports(d.path()), 0);
        assert!(core.state.lock().unwrap().added.is_empty());
    }

    #[tokio::test]
    async fn a_file_declared_too_big_is_refused_before_it_is_read() {
        let d = tempfile::tempdir().unwrap();
        let (_core, app, url) = serve(d.path()).await;
        let head = head_of_upload(app.send_key.as_ref().unwrap(), "urgent");
        let mut c = raw_post(&url, 11, head.len() + 3_000_000, &head).await;
        let (code, text) = answer(&mut c).await;
        assert_eq!(code, 413, "{text}");
        assert_eq!(imports(d.path()), 0);
    }

    #[tokio::test]
    async fn stalled_posts_time_out_and_leave_nothing() {
        let d = tempfile::tempdir().unwrap();
        let (_core, app, url) = serve_with(d.path(), 200, |a| {
            a.post_waits = crate::app::PostWaits {
                before_key: Duration::from_millis(300),
                idle: Duration::from_millis(300),
            };
        })
        .await;
        let key = app.send_key.clone().unwrap();
        // before the key: just the first field, then nothing
        let mut c = raw_post(
            &url,
            12,
            10_000,
            b"--XyZ\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\n",
        )
        .await;
        let (code, _) = answer(&mut c).await;
        assert_eq!(code, 408);
        // after the key, halfway through the file
        let mut first = head_of_upload(&key, "bulk");
        first.extend(vec![1u8; 50_000]);
        let mut c = raw_post(&url, 13, first.len() + 1_000_000, &first).await;
        let (code, _) = answer(&mut c).await;
        assert_eq!(code, 408);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(imports(d.path()), 0, "the part written is gone");
    }

    #[tokio::test]
    async fn two_uploads_fill_the_upload_slots_but_text_still_goes_through() {
        let d = tempfile::tempdir().unwrap();
        let (core, app, url) = serve(d.path()).await;
        let key = app.send_key.clone().unwrap();
        let mut first = head_of_upload(&key, "bulk");
        first.extend(vec![1u8; 10_000]);
        let total = first.len() + 5_000_000;
        let _a = raw_post(&url, 21, total, &first).await;
        let _b = raw_post(&url, 22, total, &first).await;
        for _ in 0..50 {
            if imports(d.path()) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(imports(d.path()), 2, "both are being written");
        let mut c = raw_post(&url, 23, total, &first).await;
        let (code, text) = answer(&mut c).await;
        assert_eq!(code, 429, "{text}");
        assert!(text.contains("Two uploads"));
        let text_only = send_body(&key, "urgent", None);
        let mut c = raw_post(&url, 24, text_only.len(), &text_only).await;
        let (code, text) = answer(&mut c).await;
        assert_eq!(code, 303, "{text}");
        assert_eq!(core.state.lock().unwrap().appended("mule.msg").len(), 1);
    }

    #[tokio::test]
    async fn a_second_file_or_a_file_on_another_form_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (_core, app, url) = serve(d.path()).await;
        let key = app.send_key.clone().unwrap();
        let mut b = head_of_upload(&key, "bulk");
        b.extend(b"aa\r\n--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"u.bin\"\r\n\r\nbb\r\n--XyZ--\r\n");
        let r = post(&url, b).await;
        assert_eq!(r.status(), 400);
        assert!(r.text().await.unwrap().contains("One file per message"));
        let mut b = multipart(&[("key", &key), ("id", &"a".repeat(64))]);
        b.truncate(b.len() - b"--XyZ--\r\n".len());
        b.extend(b"--XyZ\r\nContent-Disposition: form-data; name=\"x\"; filename=\"x.bin\"\r\n\r\nzz\r\n--XyZ--\r\n");
        let withdraw = url.replace("/send", "/withdraw");
        let mut c = raw_post(&withdraw, 31, b.len(), &b).await;
        let (code, text) = answer(&mut c).await;
        assert_eq!(code, 400);
        assert!(text.contains("This form takes no file"));
        assert_eq!(imports(d.path()), 0);
    }

    #[tokio::test]
    async fn a_big_file_goes_to_disk_and_on_to_the_core() {
        let d = tempfile::tempdir().unwrap();
        let (core, app, url) = serve(d.path()).await;
        let data: Vec<u8> = (0..70_000_000u64).map(|i| (i % 253) as u8).collect();
        let r = post(
            &url,
            send_body(app.send_key.as_ref().unwrap(), "bulk", Some(&data)),
        )
        .await;
        assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
        let added = core.state.lock().unwrap().added.clone();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].0, "ridgewarden-mule/tiles.bin");
        assert_eq!(
            (added[0].1, added[0].2.clone()),
            (70_000_000, blake3::hash(&data).to_hex().to_string())
        );
        let msgs = core.state.lock().unwrap().appended("mule.msg").len();
        assert_eq!(msgs, 1);
        assert_eq!(
            imports(d.path()),
            0,
            "the staged copy is gone once the core has it"
        );
    }

    #[tokio::test]
    async fn a_file_over_its_class_limit_is_cut_off_and_removed() {
        let d = tempfile::tempdir().unwrap();
        let (core, app, url) = serve(d.path()).await;
        let r = post(
            &url,
            send_body(
                app.send_key.as_ref().unwrap(),
                "urgent",
                Some(&vec![7u8; 2_000_000]),
            ),
        )
        .await;
        assert_eq!(r.status(), 413);
        assert!(core.state.lock().unwrap().added.is_empty());
        assert_eq!(imports(d.path()), 0);
    }

    #[tokio::test]
    async fn a_message_that_would_be_refused_writes_no_file() {
        let d = tempfile::tempdir().unwrap();
        let (core, app, url) = serve(d.path()).await;
        let key = app.send_key.clone().unwrap();
        let r = post(&url, send_body(&key, "", Some(b"data"))).await;
        assert_eq!(r.status(), 400);
        assert!(r.text().await.unwrap().contains("priority"));
        assert_eq!(imports(d.path()), 0);
        tokio::time::sleep(POST_GAP + Duration::from_millis(100)).await;
        // sending to this site's own island is refused before the file is read
        let mut b = multipart(&[
            ("key", &key),
            ("from", "N0AAA"),
            ("to", "north"),
            ("class", "bulk"),
        ]);
        b.truncate(b.len() - b"--XyZ--\r\n".len());
        b.extend(b"--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a\"\r\n\r\nzz\r\n--XyZ--\r\n");
        let r = post(&url, b).await;
        assert_eq!(r.status(), 400);
        assert!(r.text().await.unwrap().contains("this island"));
        assert_eq!(imports(d.path()), 0);
        assert!(core.state.lock().unwrap().added.is_empty());
    }

    #[tokio::test]
    async fn an_empty_file_input_sends_just_the_text() {
        let d = tempfile::tempdir().unwrap();
        let (core, app, url) = serve(d.path()).await;
        let mut b = send_body(app.send_key.as_ref().unwrap(), "routine", None);
        b.truncate(b.len() - b"--XyZ--\r\n".len());
        b.extend(b"--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"\"\r\n\r\n\r\n--XyZ--\r\n");
        assert_eq!(post(&url, b).await.status(), 303);
        let msgs = core.state.lock().unwrap().appended("mule.msg").len();
        assert_eq!(msgs, 1);
        assert!(core.state.lock().unwrap().added.is_empty());
    }

    #[tokio::test]
    async fn an_upload_cut_off_midway_leaves_nothing_behind() {
        use tokio::io::AsyncWriteExt;
        let d = tempfile::tempdir().unwrap();
        let (core, app, url) = serve(d.path()).await;
        let full = send_body(
            app.send_key.as_ref().unwrap(),
            "bulk",
            Some(&vec![1u8; 5_000_000]),
        );
        let addr = url
            .trim_start_matches("http://")
            .trim_end_matches("/send")
            .to_string();
        let mut c = tokio::net::TcpStream::connect(&addr).await.unwrap();
        let head = format!(
            "POST /send HTTP/1.1\r\nHost: {addr}\r\nContent-Type: multipart/form-data; boundary=XyZ\r\nContent-Length: {}\r\n\r\n",
            full.len()
        );
        c.write_all(head.as_bytes()).await.unwrap();
        c.write_all(&full[..2_000_000]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            imports(d.path()),
            1,
            "the file is being written as it comes"
        );
        drop(c);
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if imports(d.path()) == 0 {
                break;
            }
        }
        assert_eq!(
            imports(d.path()),
            0,
            "a dropped upload leaves no file behind"
        );
        assert!(core.state.lock().unwrap().appended("mule.msg").is_empty());
    }

    #[tokio::test]
    async fn nothing_is_written_for_a_wrong_key_or_a_file_before_the_key() {
        let d = tempfile::tempdir().unwrap();
        let (core, app, url) = serve(d.path()).await;
        let r = post(&url, send_body("wrong", "bulk", Some(b"data"))).await;
        assert_eq!(r.status(), 403);
        assert_eq!(imports(d.path()), 0);
        tokio::time::sleep(POST_GAP + Duration::from_millis(100)).await;
        let key = app.send_key.clone().unwrap();
        let mut b =
            b"--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a\"\r\n\r\nzz\r\n"
                .to_vec();
        b.extend(multipart(&[("key", &key), ("class", "bulk")]));
        assert_eq!(post(&url, b).await.status(), 403);
        assert_eq!(imports(d.path()), 0);
        assert!(core.state.lock().unwrap().appended("mule.msg").is_empty());
    }

    #[tokio::test]
    async fn posts_are_gated_before_anything_is_sent() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0north", d.path()).await;
        let app = Arc::new(crate::testing::app(
            d.path(),
            &core,
            "island = \"north\"\noperator = \"N0AAA\"",
        ));
        let key = app.send_key.clone().unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let svc =
            crate::web::router(app.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(l, svc).await.unwrap() });
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let post = |path: &str, body: Vec<u8>| {
            http.post(format!("{url}{path}"))
                .header("content-type", "multipart/form-data; boundary=XyZ")
                .body(body)
                .send()
        };
        let msg = |k: &str| {
            multipart(&[
                ("key", k),
                ("from", "N0AAA"),
                ("to", "south"),
                ("class", "urgent"),
                ("text", "need cots"),
            ])
        };
        assert_eq!(post("/send", msg("wrong")).await.unwrap().status(), 403);
        assert_eq!(
            post("/send", msg(&key)).await.unwrap().status(),
            429,
            "too soon after the last post"
        );
        tokio::time::sleep(POST_GAP + Duration::from_millis(100)).await;
        let r = post("/send", msg(&key)).await.unwrap();
        assert_eq!(r.status(), 303);
        assert!(
            r.headers()["location"]
                .to_str()
                .unwrap()
                .starts_with("/msg?id=")
        );
        assert_eq!(core.state.lock().unwrap().appended("mule.msg").len(), 1);
        tokio::time::sleep(POST_GAP + Duration::from_millis(100)).await;
        let big = multipart(&[("key", &key), ("id", &"x".repeat(MAX_FIELDS))]);
        assert_eq!(post("/withdraw", big).await.unwrap().status(), 413);
        assert_eq!(core.state.lock().unwrap().appended("mule.msg").len(), 1);
    }

    #[test]
    fn keys_compare_exactly() {
        assert!(key_ok("abc123", " abc123 "));
        assert!(!key_ok("abc123", "abc12"));
        assert!(!key_ok("abc123", ""));
    }
}
