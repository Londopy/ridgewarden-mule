use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use ridgewarden_client::CoreClient;
use ridgewarden_client::util::{now_ms, short_time, valid_node};
use ridgewarden_mule::app::App;
use ridgewarden_mule::car::{self, Drive};
use ridgewarden_mule::config::{Config, EXAMPLE};
use ridgewarden_mule::mule::Class;
use ridgewarden_mule::plan::Plan;
use ridgewarden_mule::send::size_text;
use ridgewarden_mule::send::{Outgoing, send, stage_file};
use ridgewarden_mule::trips::{self, Trip, parse_days, parse_hhmm};
use ridgewarden_mule::{spawn_loops, web};

#[derive(Parser)]
#[command(
    version,
    about = "Data mules for AREDN meshes: carry messages and files between islands by car (a ridgewarden app)"
)]
struct Cli {
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/ridgewarden/mule.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config
    Init,
    /// Follow the core, do this site's part of the trip, serve the mule page
    Run,
    /// Send a message (and a file) to another island
    Send {
        /// the destination island
        #[arg(long)]
        to: String,
        /// your callsign; defaults to operator in the config
        #[arg(long, default_value = "")]
        from: String,
        /// the addressee's callsign, if it's for someone in particular
        #[arg(long)]
        call: Option<String>,
        /// urgent, routine or bulk
        #[arg(long, default_value = "routine")]
        class: String,
        /// a file to send with it
        #[arg(long)]
        file: Option<PathBuf>,
        /// third-party traffic: the person without a licence it is written for
        #[arg(long = "for")]
        on_behalf: Option<String>,
        /// the message text
        #[arg(default_value = "")]
        text: String,
    },
    /// Take back a message this site sent, before it is delivered
    Withdraw {
        /// the message's id, as its page shows it
        id: String,
    },
    /// Register a courier's regular trip, or change one by registering it again
    Trip {
        /// a short name for the trip, unique for this site
        name: String,
        #[arg(long)]
        courier: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        /// "fri", "mon,thu" or "daily"
        #[arg(long)]
        days: String,
        /// departure time, UTC, like 17:30
        #[arg(long)]
        depart: String,
        /// how long the drive takes
        #[arg(long, default_value_t = 3.0)]
        hours: f64,
        /// room for files on this drive
        #[arg(long, default_value_t = 8000)]
        capacity_mb: u64,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Take a trip this site registered off the plan
    Retire { name: String },
    /// Print the plan, or on a car tonight's manifest
    Manifest,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ridgewarden_mule=info,ridgewarden_client=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => {
            if cli.config.exists() {
                bail!(
                    "{} already exists; not overwriting it",
                    cli.config.display()
                );
            }
            if let Some(d) = cli.config.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&cli.config, EXAMPLE)?;
            println!("Wrote {}", cli.config.display());
            Ok(())
        }
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(run(cfg))
        }
        Cmd::Send {
            to,
            from,
            call,
            class,
            file,
            on_behalf,
            text,
        } => {
            let cfg = Config::load(&cli.config)?;
            let Some(class) = Class::parse(&class) else {
                bail!("class is urgent, routine or bulk, not {class:?}");
            };
            let o = Outgoing {
                to,
                from,
                to_call: call,
                class,
                text,
                on_behalf,
            };
            // refuse before copying what could be a big file
            let size = file
                .as_ref()
                .map(|f| std::fs::metadata(f).with_context(|| format!("reading {}", f.display())))
                .transpose()?
                .map(|m| m.len());
            ridgewarden_mule::send::payload(&cfg, &o, size)?;
            let staged = file.map(|f| stage_file(&cfg, &f)).transpose()?;
            let core = CoreClient::new(&cfg.core.url, Some(&cfg.core.token_file))?;
            let ev = tokio::runtime::Runtime::new()?
                .block_on(send(&core, &cfg, &o, staged))
                .context("sending")?;
            println!("Sent {} ({} to {})", ev.id, class.name(), ev.payload["to"]);
            Ok(())
        }
        Cmd::Withdraw { id } => {
            let cfg = Config::load(&cli.config)?;
            if !ridgewarden_mule::mule::is_id(&id) {
                bail!("{id:?} is not a message id (64 hex digits)");
            }
            let core = CoreClient::new(&cfg.core.url, Some(&cfg.core.token_file))?;
            tokio::runtime::Runtime::new()?
                .block_on(core.append("mule.withdraw", serde_json::json!({"msg": id})))?;
            println!(
                "Withdrew {id}. Only the site that sent a message can withdraw it; from anywhere else this does nothing."
            );
            Ok(())
        }
        Cmd::Trip {
            name,
            courier,
            from,
            to,
            days,
            depart,
            hours,
            capacity_mb,
            note,
        } => {
            let cfg = Config::load(&cli.config)?;
            let t = Trip {
                author: String::new(),
                author_call: String::new(),
                id: name.to_ascii_lowercase(),
                courier: courier.to_ascii_uppercase(),
                from: from.to_ascii_lowercase(),
                to: to.to_ascii_lowercase(),
                days: parse_days(&days).context("days look like fri, mon,thu or daily")?,
                depart_min: parse_hhmm(&depart).context("depart looks like 17:30 (UTC)")?,
                hours,
                capacity_mb,
                note,
                ts: 0,
            };
            if !valid_node(&t.id) || !valid_node(&t.from) || !valid_node(&t.to) {
                bail!("trip, --from and --to are lowercase letters, digits and hyphens");
            }
            if !ridgewarden_mule::send::a_call(&t.courier) {
                bail!("{:?} is not a callsign", t.courier);
            }
            let p = trips::payload(&t)?;
            let core = CoreClient::new(&cfg.core.url, Some(&cfg.core.token_file))?;
            tokio::runtime::Runtime::new()?.block_on(core.append("mule.trip", p))?;
            println!(
                "Registered {}: {} from {} to {}, {}",
                t.id,
                t.courier,
                t.from,
                t.to,
                t.schedule_text()
            );
            Ok(())
        }
        Cmd::Retire { name } => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(retire(cfg, &name))
        }
        Cmd::Manifest => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(print_manifest(cfg))
        }
    }
}

async fn caught_up(cfg: Config) -> Result<App> {
    let app = App::new(cfg)?;
    app.pin().await.context("asking the core who it is")?;
    ridgewarden_client::follow(&app.core, &app.db, ridgewarden_mule::app::PROJECTIONS).await?;
    Ok(app)
}

async fn retire(cfg: Config, name: &str) -> Result<()> {
    let app = caught_up(cfg).await?;
    let me = app.whoami().await?;
    let Some(t) = trips::trips(&app.db())?
        .into_iter()
        .find(|t| t.id == name && t.author == me.key)
    else {
        bail!("this site has no registered trip called {name}");
    };
    let mut p = trips::payload(&t)?;
    p["retired"] = serde_json::json!(true);
    app.core.append("mule.trip", p).await?;
    println!("Retired {name}");
    Ok(())
}

async fn print_manifest(cfg: Config) -> Result<()> {
    let app = caught_up(cfg).await?;
    let now = now_ms();
    let line = |p: &Plan, i: usize| {
        let c = &p.cargo[i];
        let size = if c.bytes == 0 {
            "text".to_string()
        } else {
            size_text(c.bytes)
        };
        format!(
            "  {:<8} {:<10} {:>10}  {}",
            c.class.name(),
            c.to,
            size,
            c.label
        )
    };
    if app.cfg.car.is_some() {
        let me = app.whoami().await?.key;
        let m = car::manifest(&app, &me, now)?;
        match &m.drive {
            Drive::Trip {
                trip, depart_ms, ..
            } => println!(
                "Drive: {} to {}, {} leaving {}",
                trip.from,
                trip.to,
                trip.courier,
                short_time(*depart_ms)
            ),
            Drive::AdHoc => println!("Drive: not a registered trip"),
        }
        if m.load.is_empty() {
            println!("Nothing to take.");
        }
        for i in &m.load {
            let c = &m.plan.cargo[*i];
            let aboard = match &c.root {
                None => true,
                Some(r) => app.core.blob_status(r).await?.is_some_and(|b| b.complete),
            };
            println!(
                "{}  {}",
                line(&m.plan, *i),
                if aboard { "aboard" } else { "loading" }
            );
        }
        return Ok(());
    }
    let p = app.plan(now)?;
    for r in p.runs.iter().filter(|r| !r.load.is_empty()) {
        println!(
            "{} to {}, {} leaving {} ({} of {}):",
            r.trip.from,
            r.trip.to,
            r.trip.courier,
            short_time(r.depart_ms),
            size_text(r.bytes),
            size_text(r.trip.capacity_bytes())
        );
        for i in &r.load {
            println!("{}", line(&p, *i));
        }
    }
    if !p.unplanned.is_empty() {
        println!("Not on any trip:");
        for (i, why) in &p.unplanned {
            println!("{}  ({})", line(&p, *i), why.text());
        }
    }
    if p.cargo.is_empty() {
        println!("Nothing is waiting.");
    }
    Ok(())
}

async fn run(cfg: Config) -> Result<()> {
    let listen = cfg.listen;
    let app = Arc::new(App::new(cfg)?);
    tracing::info!(
        "ridgewarden-mule {} following {}, page on http://{listen}",
        env!("CARGO_PKG_VERSION"),
        app.core.base()
    );
    spawn_loops(app.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    axum::serve(
        listener,
        web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
