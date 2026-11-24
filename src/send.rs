//! Writing a message: checking it, publishing its file through the core, and signing it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ridgewarden_client::event::valid_call;
use ridgewarden_client::util::valid_node;
use ridgewarden_client::{CoreClient, Event};
use serde_json::{Value, json};

use crate::config::Config;
use crate::mule::{BLOB_PREFIX, Class, MAX_TEXT};

pub struct Outgoing {
    pub to: String,
    /// empty means the site's configured operator
    pub from: String,
    pub to_call: Option<String>,
    pub class: Class,
    pub text: String,
    pub on_behalf: Option<String>,
}

/// A file copied into the core's import folder, ready to publish. The copy goes when this
/// does: once the core has the chunks, or when the send was refused or abandoned.
pub struct Staged {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let path = std::mem::take(&mut self.path);
        // unlinking a big file can take a while; not on an async worker if there is a choice
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn_blocking(move || unstage(&path));
            }
            Err(_) => unstage(&path),
        }
    }
}

fn unstage(path: &Path) {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            tracing::warn!("removing {}: {e}", path.display());
        }
        _ => {}
    }
}

/// Where the send form's uploads go in the import folder. A restart drops any upload in
/// progress, so at startup every one of these is a leftover.
pub const UPLOAD_PREFIX: &str = "mule-up-";
/// Where the `send` command's copies go. Another one may be running while the service
/// starts, so only a day-old copy is taken for a leftover.
pub const COPY_PREFIX: &str = "mule-";

/// Remove what unfinished sends left in the import folder (a crash, or a restart in the
/// middle of an upload); returns how many files went.
pub fn sweep(cfg: &Config, now: std::time::SystemTime) -> usize {
    let Ok(dir) = std::fs::read_dir(&cfg.core.imports_dir) else {
        return 0;
    };
    let day_old = now - std::time::Duration::from_secs(86_400);
    let mut n = 0;
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < day_old);
        let leftover = name.starts_with(UPLOAD_PREFIX) || (name.starts_with(COPY_PREFIX) && old);
        if !leftover {
            continue;
        }
        match std::fs::remove_file(e.path()) {
            Ok(()) => {
                n += 1;
                tracing::info!("removed {}, left by an unfinished send", e.path().display());
            }
            Err(err) => tracing::warn!("removing {}: {err}", e.path().display()),
        }
    }
    n
}

/// Letters, digits, '-', '_' and '.', without a leading dot: safe as a file name anywhere.
pub fn plain(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let s: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect();
    let s = s.trim_start_matches('.');
    if s.is_empty() {
        "file".into()
    } else {
        s.into()
    }
}

/// Decimal units, the way trips' room is given in MB.
pub fn size_text(n: u64) -> String {
    match n {
        n if n < 1000 => format!("{n} B"),
        n if n < 1_000_000 => format!("{:.1} kB", n as f64 / 1e3),
        n if n < 1_000_000_000 => format!("{:.1} MB", n as f64 / 1e6),
        n => format!("{:.1} GB", n as f64 / 1e9),
    }
}

/// A message's file size, or "text" when it has no file.
pub fn cargo_size(bytes: u64) -> String {
    if bytes == 0 {
        "text".into()
    } else {
        size_text(bytes)
    }
}

pub fn a_call(s: &str) -> bool {
    valid_call(s) && s.bytes().any(|b| b.is_ascii_digit())
}

/// Check a message before anything is published, and build its payload.
pub fn payload(cfg: &Config, o: &Outgoing, file_size: Option<u64>) -> Result<Value> {
    let Some(here) = &cfg.island else {
        bail!("this is a car; send messages from a site on an island");
    };
    let from = match o.from.trim() {
        "" => cfg.operator.clone(),
        f => f.to_ascii_uppercase(),
    };
    if from.is_empty() {
        bail!("say who is sending: a callsign (or set operator in the config)");
    }
    if !a_call(&from) {
        bail!("{from:?} is not a callsign");
    }
    let to = o.to.trim().to_ascii_lowercase();
    if !valid_node(&to) {
        bail!("island {to:?} must be lowercase letters, digits and hyphens");
    }
    if &to == here {
        bail!("{to} is this island; the mule is for other islands");
    }
    let text = o.text.trim();
    if text.chars().count() > MAX_TEXT {
        bail!("the text is longer than {MAX_TEXT} characters; send it as a file");
    }
    if text.is_empty() && file_size.is_none() {
        bail!("a message needs text or a file");
    }
    match file_size {
        Some(0) => bail!("the file is empty"),
        Some(n) if n > o.class.max_file() => bail!(
            "{} files can be at most {}; send this one as {}",
            o.class.name(),
            size_text(o.class.max_file()),
            match o.class {
                Class::Urgent => "routine or bulk",
                _ => "bulk, or split it",
            }
        ),
        _ => {}
    }
    let mut p = json!({"from_island": here, "to": to, "from": from, "class": o.class.name()});
    if !text.is_empty() {
        p["text"] = json!(text);
    }
    if let Some(c) = o
        .to_call
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        let c = c.to_ascii_uppercase();
        if !a_call(&c) {
            bail!("{c:?} is not a callsign");
        }
        p["to_call"] = json!(c);
    }
    if let Some(who) = o
        .on_behalf
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        if !cfg.third_party {
            bail!("third-party traffic is off on this site (third_party in the config)");
        }
        if who.chars().count() > 100 {
            bail!("the third party's name is longer than 100 characters");
        }
        p["for"] = json!(who);
    }
    Ok(p)
}

/// A fresh name in the core's import folder for a file called `name`.
pub fn staged_path(cfg: &Config, prefix: &str, name: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(&cfg.core.imports_dir)
        .with_context(|| format!("creating {}", cfg.core.imports_dir.display()))?;
    let tag = hex::encode(
        getrandom::u64()
            .map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?
            .to_le_bytes(),
    );
    Ok(cfg
        .core
        .imports_dir
        .join(format!("{prefix}{tag}-{}", plain(name))))
}

/// Copy a local file into the core's import folder.
pub fn stage_file(cfg: &Config, src: &Path) -> Result<Staged> {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let path = staged_path(cfg, COPY_PREFIX, &name)?;
    let size = std::fs::copy(src, &path)
        .with_context(|| format!("copying {} to {}", src.display(), path.display()))?;
    // copy keeps the source's mode; an owner-only source would be unreadable to the core
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
    }
    Ok(Staged { path, name, size })
}

/// Publish the file (if any) at the class's priority, then sign and log the message.
pub async fn send(
    core: &CoreClient,
    cfg: &Config,
    o: &Outgoing,
    file: Option<Staged>,
) -> Result<Event> {
    let mut p = payload(cfg, o, file.as_ref().map(|f| f.size))?;
    if let Some(f) = file {
        let staged = f
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let blob_name = format!("{BLOB_PREFIX}{}", plain(&f.name));
        let root = core
            .blob_add(&staged, &blob_name, o.class.blob_priority())
            .await?;
        let name: String = f.name.chars().take(200).collect();
        p["file"] = json!({"root": root, "name": name, "size": f.size});
    }
    core.append("mule.msg", p).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_removes_every_upload_and_day_old_copies() {
        let d = tempfile::tempdir().unwrap();
        let mut c = cfg();
        c.core.imports_dir = d.path().to_path_buf();
        let now = std::time::SystemTime::now();
        let make = |name: &str, age_s: u64| {
            let f = std::fs::File::create(d.path().join(name)).unwrap();
            f.set_modified(now - std::time::Duration::from_secs(age_s))
                .unwrap();
        };
        make("mule-up-1a2b-tiles.bin", 5);
        make("mule-9f8e-old.zip", 2 * 86_400);
        make("mule-7c6d-new.zip", 60);
        make("lib.pmtiles", 3 * 86_400);
        assert_eq!(sweep(&c, now), 2);
        let mut left: Vec<String> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["lib.pmtiles", "mule-7c6d-new.zip"],
            "another app's file and a send in progress stay"
        );
    }

    fn cfg() -> Config {
        toml::from_str("island = \"north\"\noperator = \"N0AAA\"").unwrap()
    }

    fn out(to: &str, class: Class, text: &str) -> Outgoing {
        Outgoing {
            to: to.into(),
            from: String::new(),
            to_call: None,
            class,
            text: text.into(),
            on_behalf: None,
        }
    }

    #[test]
    fn a_plain_message_takes_the_site_operator() {
        let mut o = out("South", Class::Urgent, " need 20 cots ");
        o.to_call = Some("n0bbb".into());
        let p = payload(&cfg(), &o, None).unwrap();
        assert_eq!(
            p,
            json!({"from_island": "north", "to": "south", "from": "N0AAA", "to_call": "N0BBB",
                   "class": "urgent", "text": "need 20 cots"})
        );
    }

    #[test]
    fn bad_messages_are_refused() {
        let c = cfg();
        assert!(
            payload(&c, &out("north", Class::Routine, "x"), None).is_err(),
            "own island"
        );
        assert!(
            payload(&c, &out("south", Class::Routine, ""), None).is_err(),
            "empty"
        );
        assert!(payload(&c, &out("south bay", Class::Routine, "x"), None).is_err());
        let mut o = out("south", Class::Routine, "x");
        o.from = "spectator".into();
        assert!(payload(&c, &o, None).is_err(), "sender must be a callsign");
        let long = "x".repeat(MAX_TEXT + 1);
        assert!(payload(&c, &out("south", Class::Routine, &long), None).is_err());
    }

    #[test]
    fn urgent_files_stay_small() {
        let c = cfg();
        assert!(
            payload(
                &c,
                &out("south", Class::Urgent, ""),
                Some(Class::Urgent.max_file())
            )
            .is_ok()
        );
        assert!(
            payload(
                &c,
                &out("south", Class::Urgent, ""),
                Some(Class::Urgent.max_file() + 1)
            )
            .is_err()
        );
        assert!(payload(&c, &out("south", Class::Bulk, ""), Some(50 << 20)).is_ok());
        assert!(payload(&c, &out("south", Class::Bulk, ""), Some(0)).is_err());
    }

    #[test]
    fn third_party_traffic_needs_the_setting() {
        let mut o = out("south", Class::Routine, "all well here");
        o.on_behalf = Some("Pat".into());
        assert!(payload(&cfg(), &o, None).is_err());
        let mut c = cfg();
        c.third_party = true;
        assert_eq!(payload(&c, &o, None).unwrap()["for"], "Pat");
    }

    #[test]
    fn a_car_does_not_send() {
        let c: Config = toml::from_str("operator = \"N0DRV\"\n[car]\n").unwrap();
        assert!(payload(&c, &out("south", Class::Routine, "x"), None).is_err());
    }

    #[test]
    fn file_names_are_made_plain() {
        assert_eq!(plain("../../etc/passwd"), "passwd");
        assert_eq!(plain("C:\\maps\\shelter list.pdf"), "shelter_list.pdf");
        assert_eq!(plain("..."), "file");
        assert_eq!(plain(".hidden"), "hidden");
    }
}
