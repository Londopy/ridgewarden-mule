//! What a core holds, asked over its public sync API: the same requests other cores make,
//! so it works for this site's core and for cores on other islands alike.

use anyhow::Result;
use ridgewarden_client::CoreClient;
use serde::Deserialize;

use crate::mule::Msg;

#[derive(Deserialize)]
struct Ev {
    id: String,
}

/// The message event is in the core's log and its file, if any, is complete there.
pub async fn holds(http: &reqwest::Client, core: &CoreClient, m: &Msg) -> Result<bool> {
    if let Some(f) = &m.file
        && !core.blob_status(&f.root).await?.is_some_and(|b| b.complete)
    {
        return Ok(false);
    }
    let evs: Vec<Ev> = http
        .get(format!("{}/v1/events", core.base()))
        .query(&[("author", m.author.as_str()), ("seq", &m.seq.to_string())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(evs.iter().any(|e| e.id == m.id))
}

/// A core on another island, with the key it answers as.
pub struct Remote {
    pub url: String,
    pub core: CoreClient,
    pub key: String,
}

impl Remote {
    /// Fails if the core can't be reached, which on a car usually just means it's elsewhere.
    pub async fn connect(url: &str) -> Result<Remote> {
        let core = CoreClient::new(url, None)?;
        let key = core.whoami().await?.key;
        Ok(Remote {
            url: url.to_string(),
            core,
            key,
        })
    }
}
