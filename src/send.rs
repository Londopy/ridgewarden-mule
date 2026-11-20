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

/// A file copied into the core's import folder, ready to publish.
pub struct Staged {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
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

fn staged_path(cfg: &Config, name: &str) -> Result<PathBuf> {
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
        .join(format!("mule-{tag}-{}", plain(name))))
}

/// Copy a local file into the core's import folder.
pub fn stage_file(cfg: &Config, src: &Path) -> Result<Staged> {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let path = staged_path(cfg, &name)?;
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

/// Write uploaded bytes into the core's import folder.
pub fn stage_bytes(cfg: &Config, name: &str, data: &[u8]) -> Result<Staged> {
    let path = staged_path(cfg, name)?;
    std::fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
    Ok(Staged {
        path,
        name: name.to_string(),
        size: data.len() as u64,
    })
}

/// Publish the file (if any) at the class's priority, then sign and log the message.
pub async fn send(
    core: &CoreClient,
    cfg: &Config,
    o: &Outgoing,
    file: Option<Staged>,
) -> Result<Event> {
    let checked = payload(cfg, o, file.as_ref().map(|f| f.size));
    let mut p = match (checked, &file) {
        (Ok(p), _) => p,
        (Err(e), Some(f)) => {
            unstage(&f.path);
            return Err(e);
        }
        (Err(e), None) => return Err(e),
    };
    if let Some(f) = file {
        let staged = f
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let blob_name = format!("{BLOB_PREFIX}{}", plain(&f.name));
        let added = core
            .blob_add(&staged, &blob_name, o.class.blob_priority())
            .await;
        // the core has the chunks now (or refused them); the copy is no longer needed
        unstage(&f.path);
        let root = added?;
        let name: String = f.name.chars().take(200).collect();
        p["file"] = json!({"root": root, "name": name, "size": f.size});
    }
    core.append("mule.msg", p).await
}

fn unstage(p: &Path) {
    if let Err(e) = std::fs::remove_file(p) {
        tracing::warn!("removing {}: {e}", p.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
