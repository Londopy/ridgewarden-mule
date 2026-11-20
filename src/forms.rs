//! Everything that changes state from a page: sending, withdrawing, registering and
//! retiring trips, and picking the car's drive. Every post must come from this site's own
//! page, is limited to one per address every few seconds and two in memory at once, and
//! must carry the send key. The key travels in clear HTTP like everything on the mesh, so
//! it keeps casual visitors and spectators out; it is not a secret from anyone listening
//! on the RF link.

use std::net::SocketAddr;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{CONTENT_TYPE, HOST, ORIGIN};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use ridgewarden_client::util::now_ms;
use serde_json::json;
use tokio::sync::SemaphorePermit;

use crate::app::App;
use crate::car;
use crate::mule::{self, Class, State as Trip, is_id};
use crate::multipart;
use crate::send::{Outgoing, a_call, send, stage_bytes};
use crate::trips::{self, parse_days, parse_hhmm};
use crate::web::{Fail, page, send_form, trips_page_body};

/// The biggest upload the send form takes; bigger files go with the `send` command.
pub const MAX_UPLOAD: usize = 64_000_000;
/// The other forms are a few fields.
const MAX_SMALL: usize = 16 << 10;
/// Longest a post's body may take to arrive.
const UPLOAD_TIME: Duration = Duration::from_secs(300);
/// One post per address this often, so the key can't be guessed quickly.
const POST_GAP: Duration = Duration::from_secs(3);

/// A posted form, parsed off the request threads.
pub struct Form {
    body: Bytes,
    fields: Vec<(String, Option<String>, Range<usize>)>,
}

impl Form {
    pub fn parse(content_type: &str, body: Bytes) -> Option<Form> {
        let bd = multipart::boundary(content_type)?;
        let base = body.as_ptr() as usize;
        let fields = multipart::parse(&body, &bd)?
            .into_iter()
            .map(|p| {
                let start = p.data.as_ptr() as usize - base;
                (p.name, p.filename, start..start + p.data.len())
            })
            .collect();
        Some(Form { body, fields })
    }

    pub fn get(&self, name: &str) -> String {
        self.fields
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, _, r)| {
                String::from_utf8_lossy(&self.body[r.clone()])
                    .trim()
                    .to_string()
            })
            .unwrap_or_default()
    }

    /// An uploaded file: its name and bytes, if one was chosen.
    pub fn file(&self, name: &str) -> Option<(&str, &[u8])> {
        self.fields
            .iter()
            .find(|(n, f, r)| {
                n == name && f.as_deref().is_some_and(|f| !f.is_empty()) && !r.is_empty()
            })
            .map(|(_, f, r)| (f.as_deref().unwrap_or("file"), &self.body[r.clone()]))
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

fn too_soon(app: &App, peer: &SocketAddr) -> bool {
    let mut posts = app.posts.lock().unwrap_or_else(|e| e.into_inner());
    let ip = peer.ip();
    if posts.get(&ip).is_some_and(|t| t.elapsed() < POST_GAP) {
        return true;
    }
    posts.retain(|_, t| t.elapsed() < POST_GAP);
    posts.insert(ip, Instant::now());
    false
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

/// Everything a post has to get past before its body is even read, then the body.
pub async fn read<'a>(
    app: &'a App,
    peer: SocketAddr,
    req: Request,
    limit: usize,
) -> Result<(Form, SemaphorePermit<'a>), Box<Response>> {
    if !same_origin(req.headers()) {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "Posts have to come from this site's own pages.",
        ));
    }
    if app.send_key.is_none() {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "This site has no send key (see its log), so its forms are off. The command line still works.",
        ));
    }
    if too_soon(app, &peer) {
        return Err(refused(
            StatusCode::TOO_MANY_REQUESTS,
            "Wait a few seconds before posting again.",
        ));
    }
    let Ok(permit) = app.uploads.try_acquire() else {
        return Err(refused(
            StatusCode::TOO_MANY_REQUESTS,
            "Another upload is being taken in; try again in a moment.",
        ));
    };
    let ct = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // a slow RF link needs minutes for a big upload, but not forever with a slot held
    let body = match tokio::time::timeout(UPLOAD_TIME, axum::body::to_bytes(req.into_body(), limit))
        .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => {
            return Err(refused(
                StatusCode::PAYLOAD_TOO_LARGE,
                "That is more than this form takes; use the send command for big files.",
            ));
        }
        Err(_) => {
            return Err(refused(
                StatusCode::REQUEST_TIMEOUT,
                "The upload took too long; for big files over a slow link use the send command on the site's box.",
            ));
        }
    };
    let parsed = tokio::task::spawn_blocking(move || Form::parse(&ct, body))
        .await
        .map_err(|e| {
            refused(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("reading the form stopped: {e}"),
            )
        })?;
    let Some(form) = parsed else {
        return Err(refused(
            StatusCode::BAD_REQUEST,
            "Expected one of this site's forms (multipart/form-data).",
        ));
    };
    if !key_ok(
        app.send_key.as_deref().unwrap_or_default(),
        &form.get("key"),
    ) {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "That send key is not right.",
        ));
    }
    Ok((form, permit))
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
    let (f, _permit) = match read(&app, peer, req, MAX_UPLOAD + 64 * 1024).await {
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
    let upload = f.file("file");
    let result = match Class::parse(&fields.class) {
        None => Err("Pick a priority from the list.".to_string()),
        Some(class) => {
            let o = Outgoing {
                to: fields.to.clone(),
                from: fields.from.clone(),
                to_call: Some(fields.call.clone()),
                class,
                text: fields.text.clone(),
                on_behalf: Some(fields.on_behalf.clone()),
            };
            match crate::send::payload(&app.cfg, &o, upload.map(|u| u.1.len() as u64)) {
                Err(e) => Err(format!("{e:#}")),
                Ok(_) => {
                    let staged = upload
                        .map(|(name, data)| stage_bytes(&app.cfg, name, data))
                        .transpose();
                    match staged {
                        Ok(s) => send(&app.core, &app.cfg, &o, s)
                            .await
                            .map_err(|e| format!("{e:#}")),
                        Err(e) => Err(format!("{e:#}")),
                    }
                }
            }
        }
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
    let (f, _permit) = match read(&app, peer, req, MAX_SMALL).await {
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
    let (f, _permit) = match read(&app, peer, req, MAX_SMALL).await {
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
    let (f, _permit) = match read(&app, peer, req, MAX_SMALL).await {
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
        let big = multipart(&[("key", &key), ("id", &"x".repeat(MAX_SMALL))]);
        assert_eq!(post("/withdraw", big).await.unwrap().status(), 413);
        assert_eq!(core.state.lock().unwrap().appended("mule.msg").len(), 1);
    }

    #[test]
    fn keys_compare_exactly() {
        assert!(key_ok("abc123", " abc123 "));
        assert!(!key_ok("abc123", "abc12"));
        assert!(!key_ok("abc123", ""));
    }

    #[test]
    fn forms_keep_their_fields_and_file() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\n k1 \r\n\
--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nhi\r\n\
--b\r\nContent-Disposition: form-data; name=\"none\"; filename=\"\"\r\n\r\n\r\n--b--\r\n";
        let f = Form::parse("multipart/form-data; boundary=b", Bytes::from_static(body)).unwrap();
        assert_eq!(f.get("key"), "k1");
        assert_eq!(f.file("file"), Some(("a.txt", &b"hi"[..])));
        assert_eq!(f.file("none"), None, "an empty file input is no file");
        assert_eq!(f.get("missing"), "");
    }
}
