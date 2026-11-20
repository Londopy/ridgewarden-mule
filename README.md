# ridgewarden-mule

[![CI](https://github.com/Londopy/ridgewarden-mule/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden-mule/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden-mule)](https://github.com/Londopy/ridgewarden-mule/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden-mule/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden-mule)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(Pi%20%7C%20x86__64)-555)](#building-from-source)

**Carry messages and files between AREDN mesh islands by car, for when the internet is down.**

A ridgewarden app for AREDN meshes. Two meshes with no radio link between them (say one around one town and one around the next town down the highway) normally talk through an AREDN tunnel over the internet. This app is for when the internet is down or was never there; if you have a working tunnel, use the tunnel. Hams who already drive between the islands register their regular trips. A laptop or Pi in the car runs a ridgewarden core: it syncs with one island while parked near a node, drives, syncs with the other island on arrival, and brings the replies and delivery receipts back.

It needs a [ridgewarden](https://github.com/Londopy/ridgewarden) core on the same box, at each site and in the car. The core's normal sync does the carrying, so nothing special runs at either end. Spec sheet 12 of the AREDN app suite.

## What it does

- **Messages to another island**, from a form or the `send` command: destination island, optional addressee callsign, priority class, text, and optionally a file. A sending site can withdraw a message until it is delivered, and the message page has a reply link.
- **Priority classes.** Urgent (files up to 1 MB), routine (up to 50 MB) and bulk (big files, up to 1 TB). Message text travels in the core's event log, which is small and syncs first. Files go into the core's blob store at priority 0, 4 and 8, so a short sync in a parking lot moves urgent traffic before bulk, and an interrupted transfer resumes where it stopped.
- **Couriers' trips and a plan.** A courier registers a regular drive: from which island to which, on which days, leaving when (UTC), how long it takes and how much room there is for files. Every site works out the same plan from the same events: each waiting message goes with the first registered departure on its corridor that has room, urgent first, then oldest first. Cars load only their own share instead of every file for every island. Cargo with no courier, or no room in the next two weeks, says so.
- **Tonight's manifest.** In the car, the manifest page shows which drive it is on (its operator's next registered trip, one picked by hand, or an unscheduled drive), what goes on it ranked by priority and age, how long each item has waited, and whether everything is aboard yet. The car's core fetches exactly those files, never more than the trip's room or the car's own, counting what is already aboard, and lets each go once the destination's receipt is back.
- **An audit trail.** The car records each pickup (the message and its file are aboard) and each drop-off (a pinned core on the destination island holds them) as signed events naming the control operator and the trip. Only pickups and drop-offs from the cars' pinned keys count.
- **Receipts by any path.** The destination island saves each file, checks it against the blob id the message names, and signs a receipt. The receipt is an ordinary signed event, so it gets back to the sender with the next car, over a tunnel once the internet is back, or over a BPv7 link. Only receipts from the destination island's pinned keys count. Failed deliveries are retried with a growing wait, and the page says why.
- **Measured corridor times**: from sending to the receipt, and the drive itself by the car's own clock, median and count per island pair and priority class, on the page and as JSON at `/api/corridors`.
- **Offline-content updates between regions.** [ridgewarden-library](https://github.com/Londopy/ridgewarden-library) sends each new version of an item (a Wikipedia file, regional maps) to the islands in its `[mule] to` as a bulk message with the file attached. Cars plan and carry it like any other message; at the far island the library app takes the file from its core, so this app only checks that the file arrived and receipts it, without saving a second copy.
- **An optional BPv7 bridge** through dtnd from [dtn7-rs](https://github.com/dtn7/dtn7-rs): text messages for a routed island leave as bundles carrying the signed event, and bundles that arrive become messages here, receipts included.

## How a message is attributed

There are no accounts. A message is signed by the sending site's core, under the callsign of that core's licensee, and names the operator who wrote it (a callsign, required). The site's forms ask for a send key that the station's owner hands to the operators allowed to use them. The car's records are signed by the car's core and name its control operator. Everything is signed and nothing is encrypted: anyone on either mesh can read every message.

The send key travels in clear HTTP like everything else on the mesh. It keeps visitors and spectators out of the forms; it is not a secret from someone listening on the RF link. Posts are limited to one per address every few seconds and two in memory at once, and nothing in a post is read before those checks pass.

## Install

On a Raspberry Pi or Linux box that already runs the [ridgewarden core](https://github.com/Londopy/ridgewarden), at every site that sends or receives, and in the car:

```sh
curl -fsSL https://github.com/Londopy/ridgewarden-mule/releases/latest/download/install.sh | sudo sh
```

`install.sh` checks the download against the release key your core already trusts. Or take the `.deb` from the [releases page](https://github.com/Londopy/ridgewarden-mule/releases/latest) and check it yourself with `ridgewarden verify-release SHA256SUMS.txt` before `sudo apt install ./ridgewarden-mule_<version>_arm64.deb` (`_amd64` on a PC). Either way `/etc/ridgewarden/mule.toml` is written if missing, and the service starts once the core is set up. The page is on port 8800.

For a permanent install in a car, see [docs/car-kit.md](docs/car-kit.md).

Every release also has each download as a `.torrent`, with its GitHub download as the web seed, and signed `SHA256SUMS.txt` checksums.

The service runs as its own user, `ridgewarden-mule`, in the core's `ridgewarden` group, so it can sign events through the core but can't read the station's key. Run commands that touch the app's data as that user, so files keep the right owner: `sudo -u ridgewarden-mule ridgewarden-mule --config /etc/ridgewarden/mule.toml <command>`.

## Usage

```sh
ridgewarden-mule init --config /etc/ridgewarden/mule.toml
ridgewarden-mule run  --config /etc/ridgewarden/mule.toml
```

The page is on `http://<pi>:8800/`. Add it to the core's dashboard with an `[[app]]` entry in `ridgewarden.toml`.

The forms take the send key in `/var/lib/ridgewarden-mule/send.key`, made on first run. The command line works as the service's user, since it needs the core's token and import folder:

```sh
sudo -u ridgewarden ridgewarden-mule send --to south --call N0BBB --class urgent \
    "Shelter at the high school needs 20 cots and water by tomorrow."
sudo -u ridgewarden ridgewarden-mule send --to south --class bulk --file tiles.zip "Updated map tiles"
sudo -u ridgewarden ridgewarden-mule withdraw <message id>

# a courier who leaves north at 17:30 Pacific on Fridays, which is Saturday 01:30 UTC,
# and drives back on Sundays at 14:00 Pacific (22:00 UTC)
sudo -u ridgewarden ridgewarden-mule trip friday-south --courier N0DRV --from north --to south \
    --days sat --depart 01:30 --hours 4 --capacity-mb 20000 --note "parks by the fire station node"
sudo -u ridgewarden ridgewarden-mule trip sunday-north --courier N0DRV --from south --to north \
    --days sun --depart 22:00 --hours 4 --capacity-mb 20000
sudo -u ridgewarden ridgewarden-mule retire sunday-north

# on an island: what goes with whom; in the car: tonight's manifest
sudo -u ridgewarden ridgewarden-mule manifest
```

Trips can also be registered and retired on the Trips page; registering one again with the same name changes it. Days are `fri`, `mon,thu`, `mon-fri` or `daily`. Days and times are UTC, like every other time in the suite, so an evening departure in the Americas is often the next day in UTC. A delivered file is saved in the core's export folder (`/var/lib/ridgewarden/exports`), and the message's page links to it if it is 64 MB or smaller.

In the car, open the Manifest page before you leave. It picks the operator's next registered trip that leaves within 18 hours (or is on the road); pick a different one, or "not a registered trip", from the list on the page. An unscheduled drive takes whatever has no courier in the next day, up to `[car] capacity_mb`.

## Configuration

`/etc/ridgewarden/mule.toml` (`init` writes a commented one). At a site on an island:

```toml
island = "north"          # same name at every site on this island
operator = "N0AAA"        # default sender callsign
third_party = false       # Part 97.115; see below

[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"
imports_dir = "/var/lib/ridgewarden/imports"

# the cars' core keys; only their pickups and drop-offs count
cars = ["<key of the car's core>"]

# every island this site deals with; only these keys' receipts count for each island
[[islands]]
name = "north"
keys = ["<key of the other core on this island>"]
[[islands]]
name = "south"
keys = ["<keys of south's cores>"]
```

In the car, leave out `island`, set `operator` to the control operator, add `[car]`, and list every island with its keys and the URLs where its cores answer:

```toml
operator = "N0DRV"

[car]
capacity_mb = 8000        # room for files on a drive that isn't a registered trip

[[islands]]
name = "north"
keys = ["<keys of north's cores>"]
cores = ["http://tst-north-pi.local.mesh:8790"]
[[islands]]
name = "south"
keys = ["<keys of south's cores>"]
cores = ["http://tst-south-pi.local.mesh:8790"]
```

The car records a drop-off only when the core answering at one of an island's `cores` URLs has one of that island's `keys`, so a box that merely claims the name can't make the car stop carrying.

### Setting up the cores

The car is just another ridgewarden core. Each island's cores pull from it while it is parked there, and it pulls from them; the rest of the time they can't reach each other, and that's fine, since sync is pull-only and picks up where it left off.

In the car's `ridgewarden.toml`:

```toml
[sync]
peers = ["http://tst-north-pi.local.mesh:8790", "http://tst-south-pi.local.mesh:8790"]
telemetry = false         # carry messages and receipts, not 30 days of polls
```

In each island core's `ridgewarden.toml`, add the car to `[sync] peers` (or let discovery find it if the car's node advertises a `ridgewarden` service). Leave `ridgewarden-mule/` and `library/` out of the car core's `[blobs] auto_want`: the car's app asks its core for exactly its drive's files and lets them go after the receipt, and an `auto_want` would fetch every file for every island again, or fetch back a file the car just let go. Each island's app asks for the files addressed to it.

Trust decides what each app sees. Apps only see events from stations their core trusts, so set up `[trust] keys` (the keys `ridgewarden whoami` prints) like this:

- the car's core trusts the cores on every island, so its app sees their messages and trips;
- each island's cores trust the other islands' cores, so their apps show the messages and receipts;
- each island's cores trust the car, so the pickup and drop-off records show on their pages.

Then pin keys in every site's `mule.toml`: each island's cores under `[[islands]]` (a core's own key always counts for its own island) and the cars' cores under `cars` (a car's own key always counts in the car). A message is carried and delivered only if it is signed by a key pinned for the island it says it comes from. The page warns about any island that messages go to but that has no pinned keys, since its receipts can't count. A club that already shares one signed trust list (`ridgewarden invite` and `join`) can add the car and the other island to it instead of `[trust] keys`; `[[islands]]` is still needed.

### Offline content

Run ridgewarden-library on both islands, and on the fetcher set `[mule] island` and `[mule] to` so it sends each new version to the other islands. The far library must trust the fetcher's core (`[trust]`), and the fetcher's core must be pinned to its island under `[[islands]]` at the cars and the far sites. The update waits for a courier like any bulk message, and big content needs a trip with that much room: English Wikipedia without pictures is about 49 GB.

### BPv7 bridge

Optional, on one site of an island that also runs dtnd ([dtn7-rs](https://github.com/dtn7/dtn7-rs)) with a link to a far network:

```toml
[bpv7]
url = "http://127.0.0.1:3000"   # dtnd's HTTP interface, localhost only
endpoint = "mule"               # registered with dtnd; letters and digits only
lifetime = "72h"
[[bpv7.route]]
island = "east"
eid = "dtn://tst-east-gw/mule"
```

Messages for `east` leave as bundles whose payload is the signed `mule.msg` event, unchanged. A bundle that arrives must carry a signed event whose signature checks and whose author is pinned for the island it claims; the gateway's core then re-publishes it here, naming the original. When one of this island's sites receipts it, the gateway signs a receipt for the original and sends it back. Files stay on the mesh, since a bundle would carry a blob id the far side can't fetch. Other sites on this island should pin the gateway's key for `east` under `[[islands]]` so the receipts it brings back count there too.

## Part 97

- 97.221(b) permits automatic control for data on 6 m and shorter, which covers AREDN's bands, so a parked or moving car node exchanging data looks permitted. That is our reading, not legal advice; confirm with the AREDN forum or the ARRL before running a car node unattended.
- Station identification (97.119) for the car node still needs checking. This app does not identify the car's station on the air; the AREDN node does what it does.
- Content must be Part 97-clean: no business traffic. Messages are signed, never encrypted, over the mesh and over the BPv7 bridge alike.
- Third-party traffic (97.115), messages written on behalf of people without a licence, is excluded by default: the form and `send` refuse it, cars don't load its files or log it as cargo, destinations don't deliver or receipt it, and the bridge refuses it, unless that site sets `third_party = true`. Its text still travels in the core's log like any signed event, as everything a trusted station writes does. With it on, the form has an "on behalf of" field and the message says so.
- Every pickup and drop-off names the control operator, so who was responsible for each transfer is on record.

## Limits

- Tested end to end with three real cores, two simulated meshes and a laptop playing the car, never in a real car. [docs/car-kit.md](docs/car-kit.md) is a build guide for the kit (node, Pi, SSD, ignition-sensing 12 V power) and parking-lot syncs; it has not been built.
- The BPv7 bridge follows dtnd's documented HTTP API and its source, and decodes bundles per RFC 9171; it has been tested against a stand-in dtnd, not a running one. It carries text only.
- A drop-off is recorded when the car first sees a core answering with a pinned key on the destination island holding the message and its whole file. The key is what the core says over HTTP, which the mesh does not authenticate, so the car keeps the file until a signed receipt from a pinned key comes back. If the message also got there another way, the car still records a drop-off when it arrives.
- A car that was pinned, picked a message up and never arrived holds that message up: other cars don't plan it again. Withdraw and resend it, or retire the car's key from `cars`.
- Any trusted, pinned station can send a site a file up to the class limit (1 TB for bulk); there is no disk quota beyond what the core enforces.
- Send-to-receipt times mix the sender's and the receiver's clocks; receipts that claim to be older than their message are counted as clock skew and left out. The drive times use the car's clock alone.
- Files of 64 MB or less download from the message page (read into memory, two at a time); bigger ones stay in the core's export folder for copying off the box.
- The car carries every author's main log, not only mule traffic; with `telemetry = false` that is messages, receipts and other apps' lasting records, which stay small.
- Not done, because it isn't software: finding couriers who drive the route, finding nodes reachable from public parking, and a real message and receipt between two regions with no internet used (the spec's success measures). The corridor times page and `/api/corridors` are where that result would be published.

## Building from source

This repo builds against the core's shared client crate, so check the core out next to it:

```sh
git clone https://github.com/Londopy/ridgewarden
git clone https://github.com/Londopy/ridgewarden-mule
cd ridgewarden-mule
cargo build --release
cargo test
```

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
