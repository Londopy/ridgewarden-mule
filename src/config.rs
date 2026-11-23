//! `mule.toml`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ridgewarden_client::event::valid_call;
use ridgewarden_client::util::valid_node;
use serde::{Deserialize, Serialize};

use crate::mule::is_id;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub core: CoreCfg,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// the island this site is on, a short name every site on it uses; unset on a car
    #[serde(default)]
    pub island: Option<String>,
    /// default sender for messages from this site; on a car, the control operator
    #[serde(default)]
    pub operator: String,
    /// send, carry and deliver messages written on behalf of people without a licence
    #[serde(default)]
    pub third_party: bool,
    /// the biggest file the send form takes, written to disk as it arrives
    #[serde(default = "d_upload")]
    pub upload_max_mb: u64,
    /// the islands this site deals with: whose receipts count, and where their cores answer
    #[serde(default)]
    pub islands: Vec<IslandCfg>,
    /// the cars' core keys: only their pickups and drop-offs count
    #[serde(default)]
    pub cars: Vec<String>,
    /// set on the laptop or box that plays the car
    #[serde(default)]
    pub car: Option<CarCfg>,
    /// an optional bridge to a Bundle Protocol 7 network through a dtn7-rs daemon
    #[serde(default)]
    pub bpv7: Option<Bpv7Cfg>,
}

/// Where the ridgewarden core runs on this box.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CoreCfg {
    #[serde(default = "d_core_url")]
    pub url: String,
    /// the core's `local.token`; needed to sign messages, receipts and pickups
    #[serde(default = "d_token")]
    pub token_file: PathBuf,
    /// the core's import folder (`<core data_dir>/imports`), where files to send go
    #[serde(default = "d_imports")]
    pub imports_dir: PathBuf,
}

/// An island: the public keys of the cores on it (`ridgewarden whoami`), which are the only
/// stations whose receipts for it count, and the URLs where a car can reach them.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct IslandCfg {
    pub name: String,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub cores: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CarCfg {
    /// room for files on a drive that isn't a registered trip
    #[serde(default = "d_capacity")]
    pub capacity_mb: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Bpv7Cfg {
    /// dtnd's HTTP interface, which only answers on localhost
    #[serde(default = "d_dtnd")]
    pub url: String,
    /// the endpoint this site registers and reads bundles from
    #[serde(default = "d_endpoint")]
    pub endpoint: String,
    /// bundle lifetime, in dtnd's duration syntax ("24h", "90m")
    #[serde(default = "d_lifetime")]
    pub lifetime: String,
    #[serde(default, rename = "route")]
    pub routes: Vec<RouteCfg>,
}

/// Messages for `island` leave as bundles to `eid`, the far gateway's endpoint.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct RouteCfg {
    pub island: String,
    pub eid: String,
}

fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden-mule")
}
fn d_listen() -> SocketAddr {
    "0.0.0.0:8800".parse().unwrap()
}
fn d_core_url() -> String {
    "http://127.0.0.1:8790".into()
}
fn d_token() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/local.token")
}
fn d_imports() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/imports")
}
fn d_upload() -> u64 {
    4_000
}
fn d_capacity() -> u64 {
    8_000
}
fn d_dtnd() -> String {
    "http://127.0.0.1:3000".into()
}
fn d_endpoint() -> String {
    "mule".into()
}
fn d_lifetime() -> String {
    "72h".into()
}

impl Default for CoreCfg {
    fn default() -> Self {
        Self {
            url: d_core_url(),
            token_file: d_token(),
            imports_dir: d_imports(),
        }
    }
}

fn http_url(u: &str) -> bool {
    u.starts_with("http://") || u.starts_with("https://")
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.operator = cfg.operator.trim().to_ascii_uppercase();
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        match (&self.island, &self.car) {
            (Some(_), Some(_)) => bail!("set island on a site or [car] on a car, not both"),
            (None, None) => bail!("set island (the island this site is on) or [car]"),
            _ => {}
        }
        if let Some(i) = &self.island
            && !valid_node(i)
        {
            bail!("island {i:?} must be lowercase letters, digits and hyphens");
        }
        if !self.operator.is_empty() && !valid_call(&self.operator) {
            bail!("operator {:?} is not a callsign", self.operator);
        }
        if self.car.is_some() && self.operator.is_empty() {
            bail!(
                "a car needs operator, the control operator's callsign, for its pickup and drop-off records"
            );
        }
        for (n, i) in self.islands.iter().enumerate() {
            if !valid_node(&i.name) {
                bail!(
                    "[[islands]] name {:?} must be lowercase letters, digits and hyphens",
                    i.name
                );
            }
            if self.islands[..n].iter().any(|j| j.name == i.name) {
                bail!("island {} is listed twice under [[islands]]", i.name);
            }
            if let Some(k) = i.keys.iter().find(|k| !is_id(k)) {
                bail!(
                    "island {} key {k:?} is not a 64-hex-digit public key",
                    i.name
                );
            }
            if let Some(u) = i.cores.iter().find(|u| !http_url(u)) {
                bail!("island {} core {u:?} must be an http:// URL", i.name);
            }
        }
        if let Some(k) = self.cars.iter().find(|k| !is_id(k)) {
            bail!("cars entry {k:?} is not a 64-hex-digit public key");
        }
        if let Some(b) = &self.bpv7 {
            if !http_url(&b.url) {
                bail!("bpv7.url {:?} must be an http:// URL", b.url);
            }
            // dtnd takes only alphanumeric service names
            if b.endpoint.is_empty()
                || b.endpoint.len() > 60
                || !b.endpoint.bytes().all(|c| c.is_ascii_alphanumeric())
            {
                bail!("bpv7.endpoint must be letters and digits only");
            }
            for r in &b.routes {
                if !valid_node(&r.island) {
                    bail!(
                        "bpv7 route island {:?} must be lowercase letters, digits and hyphens",
                        r.island
                    );
                }
                let scheme = r.eid.starts_with("dtn://") || r.eid.starts_with("ipn:");
                if !scheme || r.eid.len() > 200 || r.eid.contains(['&', '#', ' ']) {
                    bail!(
                        "bpv7 route eid {:?} must be a dtn:// or ipn: endpoint id",
                        r.eid
                    );
                }
            }
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("mule.db")
    }

    /// What people type into the send form; made on first run, readable only by the service.
    pub fn key_path(&self) -> PathBuf {
        self.data_dir.join("send.key")
    }

    pub fn island_cfg(&self, name: &str) -> Option<&IslandCfg> {
        self.islands.iter().find(|i| i.name == name)
    }

    /// The cores a car checks for drop-offs on `island`.
    pub fn island_cores(&self, island: &str) -> &[String] {
        self.island_cfg(island).map_or(&[], |i| i.cores.as_slice())
    }

    /// The keys whose receipts count for `island`. This site's own core always counts for
    /// its own island.
    pub fn island_keys(&self, island: &str, own_key: Option<&str>) -> Vec<String> {
        let mut keys: Vec<String> = self
            .island_cfg(island)
            .map(|i| i.keys.clone())
            .unwrap_or_default();
        if let Some(k) = own_key
            && self.island.as_deref() == Some(island)
            && !keys.iter().any(|x| x == k)
        {
            keys.push(k.to_string());
        }
        keys
    }

    /// Every (island, key) pin, this site's own core included.
    pub fn pins(&self, own_key: Option<&str>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .islands
            .iter()
            .flat_map(|i| i.keys.iter().map(|k| (i.name.clone(), k.clone())))
            .collect();
        if let (Some(i), Some(k)) = (&self.island, own_key) {
            out.push((i.clone(), k.to_string()));
            // a gateway re-signs the receipts it brings in from the islands it routes to
            for r in self.bpv7.iter().flat_map(|b| &b.routes) {
                out.push((r.island.clone(), k.to_string()));
            }
        }
        out
    }

    /// The cars whose records count; a car's own core always does.
    pub fn car_keys(&self, own_key: Option<&str>) -> Vec<String> {
        let mut out = self.cars.clone();
        if let Some(k) = own_key.filter(|_| self.car.is_some()) {
            out.push(k.to_string());
        }
        out
    }

    pub fn route(&self, island: &str) -> Option<&str> {
        self.bpv7
            .iter()
            .flat_map(|b| &b.routes)
            .find(|r| r.island == island)
            .map(|r| r.eid.as_str())
    }
}

pub const EXAMPLE: &str = r#"# ridgewarden-mule configuration
data_dir = "/var/lib/ridgewarden-mule"
listen = "0.0.0.0:8800"

# The island this site is on: a short name (lowercase, digits, hyphens) that every site on
# the same mesh island uses. Leave it out on the car and fill in [car] instead.
island = "north"

# Default sender callsign for messages sent from this site. On the car this is the control
# operator, recorded with every pickup and drop-off.
operator = ""

# Part 97.115 third-party traffic (messages written on behalf of people without a licence)
# is refused, not carried and not delivered unless this is true.
third_party = false

# The biggest file the send form takes. Uploads go to disk as they arrive, so this is
# about the Pi's disk, not its memory; the send command has no limit but the class's.
upload_max_mb = 4000

# The cars' core keys. Pickups and drop-offs from any other station are ignored, so a
# stray record can't make a message look picked up.
# cars = ["<64 hex digits>"]

[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"
# the core's import folder; files you send are copied here and published by the core
imports_dir = "/var/lib/ridgewarden/imports"

# Every island this site deals with. Receipts and drop-offs for an island count only when
# they come from one of its keys (each core's `ridgewarden whoami`), so list the cores of
# the other islands here; this site's own core always counts for its own island. A car
# also uses `cores` to check what it has handed over.
# [[islands]]
# name = "north"
# keys = ["<64 hex digits>"]
# cores = ["http://tst-north-pi.local.mesh:8790"]
# [[islands]]
# name = "south"
# keys = ["<64 hex digits>"]
# cores = ["http://tst-south-pi.local.mesh:8790"]

# On the car only: remove `island` above, set `operator`, and keep this section.
# [car]
# room for files on a drive that isn't a registered trip
# capacity_mb = 8000

# Optional bridge to a Bundle Protocol 7 network through dtnd (dtn7-rs) on this box.
# Messages for a routed island leave as bundles; bundles that arrive become messages here.
# [bpv7]
# url = "http://127.0.0.1:3000"
# endpoint = "mule"
# lifetime = "72h"
# [[bpv7.route]]
# island = "east"
# eid = "dtn://tst-east-gw/mule"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config> {
        let c: Config = toml::from_str(s)?;
        c.validate()?;
        Ok(c)
    }

    fn uncomment(s: &str, prefixes: &[&str]) -> String {
        s.lines()
            .map(|l| {
                if prefixes.iter().any(|p| l.starts_with(&format!("# {p}"))) {
                    l[2..].to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn example_parses() {
        let c = parse(EXAMPLE).unwrap();
        assert_eq!(c.island.as_deref(), Some("north"));
        assert!(c.car.is_none() && !c.third_party && c.cars.is_empty());
    }

    #[test]
    fn car_example_parses_once_uncommented() {
        let car = uncomment(
            EXAMPLE,
            &[
                "[car]",
                "capacity_mb",
                "cars",
                "[[islands]]",
                "name",
                "keys",
                "cores",
                "[bpv7]",
                "url",
                "endpoint",
                "lifetime",
                "[[bpv7.route]]",
                "island =",
                "eid",
            ],
        )
        .replace("island = \"north\"\n", "")
        .replace("operator = \"\"", "operator = \"N0CALL\"")
        .replace("<64 hex digits>", &"ab".repeat(32));
        let c = parse(&car).unwrap();
        assert_eq!(
            c.island_cores("south"),
            ["http://tst-south-pi.local.mesh:8790"]
        );
        assert!(c.island_cores("east").is_empty());
        assert_eq!(c.car.as_ref().unwrap().capacity_mb, 8000);
        assert_eq!(c.route("east"), Some("dtn://tst-east-gw/mule"));
    }

    #[test]
    fn site_is_an_island_or_a_car() {
        assert!(parse("").is_err());
        assert!(parse("island = \"n\"\noperator = \"N0CALL\"\n[car]\n").is_err());
        assert!(
            parse("[car]\n").is_err(),
            "a car needs its control operator"
        );
        assert!(parse("island = \"North Coast\"").is_err());
    }

    #[test]
    fn receipts_count_only_from_pinned_keys() {
        let k = "cd".repeat(32);
        let c = parse(&format!(
            "island = \"north\"\n[[islands]]\nname = \"south\"\nkeys = [\"{k}\"]\n"
        ))
        .unwrap();
        assert_eq!(c.island_keys("south", Some("me")), [k.as_str()]);
        assert_eq!(c.island_keys("north", Some("me")), ["me"]);
        assert!(c.island_keys("east", Some("me")).is_empty());
        assert_eq!(c.pins(Some("me")).len(), 2);
        let bad = "island = \"north\"\n[[islands]]\nname = \"south\"\nkeys = [\"short\"]\n";
        assert!(parse(bad).is_err());
    }
}
