//! Serving a delivered file: streamed from disk a chunk at a time, so its size doesn't
//! matter, with single byte ranges so an interrupted download over a slow link can resume.

use std::io::SeekFrom;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, ReadBuf};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::io::ReaderStream;

use crate::send::plain;

/// A download's slot, and when the client last took any of the file.
struct Slot {
    permit: Mutex<Option<OwnedSemaphorePermit>>,
    started: Instant,
    read_ms: AtomicU64,
}

/// A reader that keeps its download slot until the response body is dropped (the download
/// finished or the client went away), or the client has stopped taking data for a while.
struct Slotted<R> {
    inner: R,
    slot: Arc<Slot>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Slotted<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let ms = self.slot.started.elapsed().as_millis() as u64;
        self.slot.read_ms.store(ms, Ordering::Relaxed);
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

/// Give the slot back once the client has taken nothing for `stall`; the download itself
/// goes on if the client comes back.
fn watch(slot: &Arc<Slot>, stall: Duration) {
    let weak = Arc::downgrade(slot);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(stall / 4).await;
            let Some(s) = weak.upgrade() else { return };
            let quiet = s.started.elapsed().as_millis() as u64 - s.read_ms.load(Ordering::Relaxed);
            if quiet >= stall.as_millis() as u64 {
                s.permit.lock().unwrap_or_else(|e| e.into_inner()).take();
                return;
            }
        }
    });
}

#[derive(Debug, PartialEq)]
pub enum Range {
    Whole,
    /// first and last byte, inclusive
    Part(u64, u64),
    Unsatisfiable,
}

/// One `bytes=` range against a file of `len` bytes. Several ranges, or anything that
/// isn't a byte range, get the whole file, which the RFC allows.
pub fn range(header: Option<&str>, len: u64) -> Range {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return Range::Whole;
    };
    if spec.contains(',') {
        return Range::Whole;
    }
    let Some((a, b)) = spec.split_once('-') else {
        return Range::Whole;
    };
    let (a, b) = (a.trim(), b.trim());
    let part = match (a.is_empty(), b.is_empty()) {
        (true, false) => match b.parse::<u64>() {
            Ok(0) | Err(_) => return Range::Unsatisfiable,
            Ok(n) => (len.saturating_sub(n), len.saturating_sub(1)),
        },
        (false, _) => {
            let Ok(first) = a.parse::<u64>() else {
                return Range::Whole;
            };
            let last = if b.is_empty() {
                len.saturating_sub(1)
            } else {
                match b.parse::<u64>() {
                    Ok(n) if n >= first => n.min(len.saturating_sub(1)),
                    _ => return Range::Whole,
                }
            };
            (first, last)
        }
        (true, true) => return Range::Whole,
    };
    if len == 0 || part.0 >= len {
        return Range::Unsatisfiable;
    }
    Range::Part(part.0, part.1)
}

/// Stream `path` as an attachment called `name`, honouring a single byte range. `etag`
/// names the content (the blob id), so a resumed download can't splice two versions.
pub async fn serve(
    path: &Path,
    name: &str,
    etag: &str,
    headers: &HeaderMap,
    slot: OwnedSemaphorePermit,
    stall: Duration,
) -> std::io::Result<Response> {
    let mut f = tokio::fs::File::open(path).await?;
    let len = f.metadata().await?.len();
    let header = |k: &str| headers.get(k).and_then(|v| v.to_str().ok());
    // a range is only for the copy the client already has part of
    let same = header("if-range").is_none_or(|v| v.trim() == etag);
    let asked = header("range").filter(|_| same);
    let (status, first, count) = match range(asked, len) {
        Range::Whole => (StatusCode::OK, 0, len),
        Range::Part(a, b) => (StatusCode::PARTIAL_CONTENT, a, b - a + 1),
        Range::Unsatisfiable => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(CONTENT_RANGE, format!("bytes */{len}"))],
            )
                .into_response());
        }
    };
    if first > 0 {
        f.seek(SeekFrom::Start(first)).await?;
    }
    let slot = Arc::new(Slot {
        permit: Mutex::new(Some(slot)),
        started: Instant::now(),
        read_ms: AtomicU64::new(0),
    });
    watch(&slot, stall);
    let reader = Slotted {
        inner: f.take(count),
        slot,
    };
    let mut r = Response::new(Body::from_stream(ReaderStream::with_capacity(
        reader,
        64 * 1024,
    )));
    *r.status_mut() = status;
    let h = r.headers_mut();
    let set = |h: &mut HeaderMap, k, v: String| {
        if let Ok(v) = v.parse() {
            h.insert(k, v);
        }
    };
    set(h, CONTENT_TYPE, "application/octet-stream".into());
    set(h, X_CONTENT_TYPE_OPTIONS, "nosniff".into());
    set(h, ACCEPT_RANGES, "bytes".into());
    set(h, ETAG, etag.to_string());
    set(h, CONTENT_LENGTH, count.to_string());
    set(
        h,
        CONTENT_DISPOSITION,
        format!("attachment; filename=\"{}\"", plain(name)),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        set(
            h,
            CONTENT_RANGE,
            format!("bytes {first}-{}/{len}", first + count - 1),
        );
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::net::SocketAddr;
    use std::sync::Arc;

    use super::*;
    use crate::app::DOWNLOADS;
    use crate::testing::{FakeCore, author, file_msg};

    /// Big enough that reading it into memory would be a problem on a Pi.
    const BIG: u64 = 65_000_000;

    fn byte(i: u64) -> u8 {
        (i % 251) as u8
    }

    /// The test file repeats one megabyte block.
    fn at(p: u64) -> u8 {
        byte(p % 1_000_000)
    }

    async fn r_etag(http: &reqwest::Client, url: &str) -> String {
        let r = http
            .get(url)
            .header("range", "bytes=0-0")
            .send()
            .await
            .unwrap();
        r.headers()["etag"].to_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn big_files_stream_whole_or_by_range_a_few_at_a_time() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0south", d.path()).await;
        let north = author("n0north");
        let (man, msg, _) = file_msg(&north, 1, "bulk", b"x", ridgewarden_client::util::now_ms());
        core.add(man);
        core.add(msg.clone());
        let path = d.path().join("big.bin");
        let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        let mut want = blake3::Hasher::new();
        let block: Vec<u8> = (0..1_000_000u64).map(byte).collect();
        for _ in 0..BIG / 1_000_000 {
            f.write_all(&block).unwrap();
            want.update(&block);
        }
        drop(f);
        let cfg = format!(
            "island = \"south\"
[[islands]]
name = \"north\"
keys = [\"{}\"]",
            north.key
        );
        let mut app = crate::testing::app(d.path(), &core, &cfg);
        app.download_stall = std::time::Duration::from_secs(1);
        let app = Arc::new(app);
        ridgewarden_client::follow(&app.core, &app.db, crate::app::PROJECTIONS)
            .await
            .unwrap();
        crate::mule::note_path(&app.db(), &msg.id, &path.display().to_string(), 0).unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/file?id={}", l.local_addr().unwrap(), msg.id);
        let svc = crate::web::router(app).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(l, svc).await.unwrap() });
        let http = reqwest::Client::new();

        let mut r = http.get(&url).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-length"], BIG.to_string().as_str());
        assert_eq!(r.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            r.headers()["content-disposition"],
            "attachment; filename=\"tiles.bin\""
        );
        let mut got = blake3::Hasher::new();
        let mut n = 0u64;
        while let Some(c) = r.chunk().await.unwrap() {
            n += c.len() as u64;
            got.update(&c);
        }
        assert_eq!((n, got.finalize()), (BIG, want.finalize()));

        let get = |range: &str| http.get(&url).header("range", range).send();
        let r = get("bytes=10-19").await.unwrap();
        assert_eq!(r.status(), 206);
        assert_eq!(
            r.headers()["content-range"],
            format!("bytes 10-19/{BIG}").as_str()
        );
        assert_eq!(
            r.bytes().await.unwrap().to_vec(),
            (10..20).map(at).collect::<Vec<_>>()
        );
        let r = get("bytes=-3").await.unwrap();
        assert_eq!(
            r.bytes().await.unwrap().to_vec(),
            (BIG - 3..BIG).map(at).collect::<Vec<_>>()
        );
        let etag = r_etag(&http, &url).await;
        let r = http
            .get(&url)
            .header("range", "bytes=10-19")
            .header("if-range", "\"something else\"")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "a changed file is sent whole");
        drop(r);
        let r = http
            .get(&url)
            .header("range", "bytes=10-19")
            .header("if-range", &etag)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 206, "the same file resumes");
        drop(r);
        let r = get(&format!("bytes={BIG}-")).await.unwrap();
        assert_eq!(r.status(), 416);
        assert_eq!(
            r.headers()["content-range"],
            format!("bytes */{BIG}").as_str()
        );

        // slow clients hold their slots until they finish or go away
        let mut open = vec![];
        for _ in 0..DOWNLOADS {
            let r = http.get(&url).send().await.unwrap();
            assert_eq!(r.status(), 200);
            open.push(r);
        }
        assert_eq!(http.get(&url).send().await.unwrap().status(), 429);
        open.clear();
        let mut status = 0;
        // well inside the one-second stall limit, so this is the leaving that frees them
        for _ in 0..6 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            status = get("bytes=0-0").await.unwrap().status().as_u16();
            if status == 206 {
                break;
            }
        }
        assert_eq!(status, 206, "slots come back when clients leave");

        // a client that stops taking data gives its slot back after a while
        let mut open = vec![];
        for _ in 0..DOWNLOADS {
            open.push(http.get(&url).send().await.unwrap());
        }
        assert_eq!(http.get(&url).send().await.unwrap().status(), 429);
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert_eq!(
            get("bytes=0-0").await.unwrap().status(),
            206,
            "four stalled clients no longer hold every slot"
        );
        drop(open);
    }

    #[test]
    fn ranges_follow_rfc_9110() {
        assert_eq!(range(None, 100), Range::Whole);
        assert_eq!(range(Some("bytes=10-19"), 100), Range::Part(10, 19));
        assert_eq!(range(Some("bytes=90-"), 100), Range::Part(90, 99));
        assert_eq!(range(Some("bytes=-5"), 100), Range::Part(95, 99));
        assert_eq!(range(Some("bytes=-500"), 100), Range::Part(0, 99));
        assert_eq!(range(Some("bytes=50-5000"), 100), Range::Part(50, 99));
        assert_eq!(range(Some("bytes=100-"), 100), Range::Unsatisfiable);
        assert_eq!(range(Some("bytes=-0"), 100), Range::Unsatisfiable);
        assert_eq!(range(Some("bytes=0-1,5-6"), 100), Range::Whole);
        assert_eq!(range(Some("bytes=9-3"), 100), Range::Whole);
        assert_eq!(range(Some("items=0-1"), 100), Range::Whole);
    }
}
