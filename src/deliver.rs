//! An island's side: take in messages addressed to it, save and check their files, and sign
//! a receipt, which goes back to the sender by whatever path comes next.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use ridgewarden_client::util::now_ms;
use serde_json::json;

use crate::app::{App, short_err};
use crate::mule::{FileRef, View, note_failure, note_path, saved, to_deliver};
use crate::send::plain;

/// The core's chunk size; a blob's id is the BLAKE3 of its chunk hashes in order.
const CHUNK: usize = 1 << 20;

pub async fn round(app: &App, island: &str) -> Result<()> {
    app.pin().await?;
    let now = now_ms();
    let views = app.active()?;
    // urgent first; and at most one file export per round, since a big one takes minutes
    let mut exported = false;
    for v in to_deliver(&views, island) {
        let s = saved(&app.db(), &v.msg.id)?.unwrap_or_default();
        if s.next_ms > now {
            continue;
        }
        match deliver(app, island, v, &mut exported).await {
            Ok(true) => tracing::info!(
                "delivered {} ({}, from {})",
                &v.msg.id[..12],
                v.msg.class.name(),
                v.msg.from_island
            ),
            Ok(false) => {}
            Err(e) => {
                let full = format!("{e:#}");
                let s = note_failure(&app.db(), &v.msg.id, &short_err(e), now_ms())?;
                tracing::warn!(
                    "delivering {} (attempt {}): {full}",
                    &v.msg.id[..12],
                    s.attempts
                );
            }
        }
    }
    Ok(())
}

/// Receipt one message; false while its file is still on the way.
async fn deliver(app: &App, island: &str, v: &View, exported: &mut bool) -> Result<bool> {
    if let Some(f) = &v.msg.file
        && !save_file(app, v, f, exported).await?
    {
        return Ok(false);
    }
    app.core
        .append("mule.receipt", json!({"msg": v.msg.id, "island": island}))
        .await?;
    Ok(true)
}

async fn save_file(app: &App, v: &View, f: &FileRef, exported: &mut bool) -> Result<bool> {
    if saved(&app.db(), &v.msg.id)?.is_some_and(|s| s.path.is_some()) {
        return Ok(true);
    }
    // the manifest travels just ahead of the message; it may lag a round
    let Some(st) = app.core.blob_status(&f.root).await? else {
        return Ok(false);
    };
    if st.size != f.size {
        bail!(
            "the blob's manifest says {} bytes but the message says {}; not fetching it",
            st.size,
            f.size
        );
    }
    if !st.wanted {
        app.core.blob_want(&f.root).await?;
    }
    if !st.complete {
        return Ok(false);
    }
    // the library app exports and checks its own copy; a second one would only take disk
    if v.msg.is_library() {
        return Ok(true);
    }
    if *exported {
        return Ok(false);
    }
    *exported = true;
    let name = format!("mule-{}-{}", &v.msg.id[..12], plain(&f.name));
    let path = app.core.blob_export(&f.root, &name).await?;
    let p = path.clone();
    let (root, size) = tokio::task::spawn_blocking(move || blob_root(Path::new(&p))).await??;
    if root != f.root || size != f.size {
        bail!("{path} does not match the blob the message names (got {size} bytes, id {root})");
    }
    note_path(&app.db(), &v.msg.id, &path, now_ms())?;
    Ok(true)
}

/// A file's blob id and size, computed the way the core does it.
pub fn blob_root(path: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut ids = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut size = 0u64;
    loop {
        let mut n = 0;
        while n < CHUNK {
            let k = f.read(&mut buf[n..])?;
            if k == 0 {
                break;
            }
            n += k;
        }
        if n == 0 {
            break;
        }
        ids.update(blake3::hash(&buf[..n]).as_bytes());
        size += n as u64;
        if n < CHUNK {
            break;
        }
    }
    Ok((ids.finalize().to_hex().to_string(), size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeCore, author, file_msg, text_msg};
    use serde_json::Value;

    async fn step(app: &App) {
        ridgewarden_client::follow(&app.core, &app.db, crate::app::PROJECTIONS)
            .await
            .unwrap();
        round(app, "south").await.unwrap();
    }

    fn site(north: &str) -> String {
        format!(
            "island = \"south\"
[[islands]]
name = \"north\"
keys = [\"{north}\"]"
        )
    }

    #[tokio::test]
    async fn a_file_is_saved_checked_and_receipted() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0south", d.path()).await;
        let north = author("n0north");
        let data = b"tile data ".repeat(150_000);
        let (man, msg, root) = file_msg(&north, 1, "bulk", &data, now_ms());
        core.add(man);
        core.add(msg.clone());
        let app = crate::testing::app(d.path(), &core, &site(&north.key));
        step(&app).await;
        assert!(
            core.state
                .lock()
                .unwrap()
                .appended("mule.receipt")
                .is_empty(),
            "no manifest yet"
        );
        core.blob(&root, data.len() as u64, false);
        step(&app).await;
        assert_eq!(core.state.lock().unwrap().wants, [root.as_str()]);
        assert!(
            core.state
                .lock()
                .unwrap()
                .appended("mule.receipt")
                .is_empty(),
            "still fetching"
        );
        core.blob(&root, data.len() as u64, true);
        core.state
            .lock()
            .unwrap()
            .files
            .insert(root.clone(), data.clone());
        step(&app).await;
        let receipts: Vec<Value> = core
            .state
            .lock()
            .unwrap()
            .appended("mule.receipt")
            .iter()
            .map(|e| e.payload.clone())
            .collect();
        assert_eq!(receipts, [json!({"msg": msg.id, "island": "south"})]);
        let path = saved(&app.db(), &msg.id).unwrap().unwrap().path.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), data);
        step(&app).await;
        assert_eq!(
            core.state.lock().unwrap().appended("mule.receipt").len(),
            1,
            "receipted once"
        );
    }

    #[tokio::test]
    async fn a_file_that_does_not_match_is_not_receipted() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0south", d.path()).await;
        let north = author("n0north");
        let (man, msg, root) = file_msg(&north, 1, "bulk", b"the real thing", now_ms());
        core.add(man);
        core.add(msg.clone());
        core.blob(&root, 14, true);
        core.state
            .lock()
            .unwrap()
            .files
            .insert(root.clone(), b"something else".to_vec());
        let app = crate::testing::app(d.path(), &core, &site(&north.key));
        step(&app).await;
        assert!(
            core.state
                .lock()
                .unwrap()
                .appended("mule.receipt")
                .is_empty()
        );
        let s = saved(&app.db(), &msg.id).unwrap().unwrap();
        assert_eq!((s.path, s.attempts), (None, 1));
        assert!(s.error.unwrap().contains("does not match"));
        step(&app).await;
        assert_eq!(
            saved(&app.db(), &msg.id).unwrap().unwrap().attempts,
            1,
            "waits before trying again"
        );
    }

    #[tokio::test]
    async fn only_pinned_receipts_stop_a_delivery() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0south", d.path()).await;
        let north = author("n0north");
        core.add(text_msg(&north, 1, "urgent", now_ms()));
        // a receipt from a station that isn't pinned for south doesn't stop ours
        let rogue = author("n0rogue");
        let id = core.state.lock().unwrap().events[0].id.clone();
        core.add(crate::testing::event(
            &rogue,
            1,
            "mule.receipt",
            json!({"msg": id, "island": "south"}),
            now_ms(),
        ));
        let app = crate::testing::app(d.path(), &core, &site(&north.key));
        step(&app).await;
        assert_eq!(core.state.lock().unwrap().appended("mule.receipt").len(), 1);
    }

    #[tokio::test]
    async fn a_library_update_is_receipted_once_its_blob_is_here_without_a_second_copy() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0south", d.path()).await;
        let north = author("n0north");
        let (man, mut msg, root) = file_msg(&north, 1, "bulk", b"a zim file", now_ms());
        msg.payload["app"] = json!("ridgewarden-library");
        core.add(man);
        core.add(msg.clone());
        core.blob(&root, 10, false);
        let app = crate::testing::app(d.path(), &core, &site(&north.key));
        step(&app).await;
        assert!(
            core.state
                .lock()
                .unwrap()
                .appended("mule.receipt")
                .is_empty()
        );
        core.blob(&root, 10, true);
        step(&app).await;
        assert_eq!(core.state.lock().unwrap().appended("mule.receipt").len(), 1);
        assert!(
            saved(&app.db(), &msg.id).unwrap().is_none(),
            "nothing exported"
        );
    }

    #[tokio::test]
    async fn a_message_from_a_key_not_pinned_for_its_island_is_not_delivered() {
        let d = tempfile::tempdir().unwrap();
        let core = FakeCore::start("n0south", d.path()).await;
        let rogue = author("n0rogue");
        let (man, msg, root) = file_msg(&rogue, 1, "bulk", b"fill your disk", now_ms());
        core.add(man);
        core.add(msg);
        core.blob(&root, 14, false);
        let app = crate::testing::app(d.path(), &core, &site(&author("n0north").key));
        step(&app).await;
        assert!(
            core.state.lock().unwrap().wants.is_empty(),
            "nothing fetched for it"
        );
        assert!(
            core.state
                .lock()
                .unwrap()
                .appended("mule.receipt")
                .is_empty()
        );
    }

    #[test]
    fn blob_id_hashes_each_mebibyte_then_the_list() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        let data: Vec<u8> = (0..CHUNK * 2 + 5).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let mut want = blake3::Hasher::new();
        for part in data.chunks(CHUNK) {
            want.update(blake3::hash(part).as_bytes());
        }
        let (root, size) = blob_root(&p).unwrap();
        assert_eq!(root, want.finalize().to_hex().to_string());
        assert_eq!(size, data.len() as u64);
    }

    #[test]
    fn exactly_one_chunk_has_no_empty_tail() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        let data = vec![7u8; CHUNK];
        std::fs::write(&p, &data).unwrap();
        let mut want = blake3::Hasher::new();
        want.update(blake3::hash(&data).as_bytes());
        assert_eq!(
            blob_root(&p).unwrap().0,
            want.finalize().to_hex().to_string()
        );
    }
}
