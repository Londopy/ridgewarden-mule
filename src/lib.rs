//! ridgewarden-mule: carry messages and files between AREDN mesh islands by car, for when
//! the internet is down.
//!
//! Runs next to a ridgewarden core at every site on each island, and on the laptop that
//! plays the car. The core's normal sync does the carrying; this app writes the messages,
//! records each pickup and drop-off on the car, saves files and signs receipts at the
//! destination, and shows how long the trip really takes.

pub mod api;
pub mod app;
pub mod bpv7;
pub mod car;
pub mod cbor;
pub mod config;
pub mod deliver;
pub mod download;
pub mod forms;
pub mod held;
pub mod mule;
pub mod multipart;
pub mod plan;
pub mod send;
pub mod top;
pub mod trips;
pub mod web;

#[cfg(test)]
mod testing;

use std::sync::Arc;
use std::time::Duration;

async fn round(app: Arc<app::App>) -> anyhow::Result<()> {
    let now = ridgewarden_client::util::now_ms();
    if let Err(e) = ridgewarden_client::follow(&app.core, &app.db, app::PROJECTIONS).await {
        let full = format!("{e:#}");
        app.read_core(Some(app::short_err(e)), now);
        anyhow::bail!(full);
    }
    app.read_core(None, now);
    let Some(island) = &app.cfg.island else {
        return car::round(&app).await;
    };
    if let Err(e) = deliver::round(&app, island).await {
        tracing::warn!("delivering: {e:#}");
    }
    if let Some(b) = &app.cfg.bpv7 {
        bpv7::round(&app, island, &bpv7::Dtnd::new(b)?).await?;
    }
    Ok(())
}

/// Follow the core and do this site's part of the trip, forever. One task, so a record is
/// back in the view before the next round decides anything.
pub fn spawn_loops(app: Arc<app::App>) {
    tokio::spawn(async move {
        loop {
            // its own task, so a panic on something a peer sent is logged, not the end of the loop
            match tokio::spawn(round(app.clone())).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("{e:#}"),
                Err(e) => tracing::error!("a round stopped: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });
}
