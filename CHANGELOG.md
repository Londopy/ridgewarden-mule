# Changelog

All notable changes to ridgewarden-mule are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is
what appears on its GitHub release page. Validated in CI by
[patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

### Added

- Runs as its own system user, `ridgewarden-mule`, in the core's `ridgewarden` group: it can sign events through the core but not read the station key.
- Messages to another mesh island, from a send form (behind a send key) or the `send` command: destination island, optional addressee callsign, priority class, text and an optional file. The sending site can withdraw a message until it is delivered; message pages have a reply link.
- Priority classes urgent, routine and bulk. Files are published through the core's blob store at priority 0, 4 and 8, so a car with a short sync window fetches urgent files before bulk ones. Urgent files are capped at 1 MB, routine at 50 MB and bulk at 1 TB, both when sending and when reading other stations' events.
- Couriers' regular trips (`trip`, `retire`, and the Trips page) and a plan every site works out the same way: each message goes with the first departure on its corridor with room, urgent first, then oldest first, and says why when nothing fits.
- Tonight's manifest in the car (page and `manifest` command): which drive, what goes, how long each item has waited, and whether it is all aboard. The car's core fetches only its drive's files, within the trip's room and its own, and lets them go once receipted.
- The car records each pickup and drop-off as a signed event naming the control operator and the trip. Only the pinned cars' records count, a drop-off is logged only when the core it checked answers with a key pinned for that island, and the car keeps each file until the receipt is back.
- The destination island saves each file, checks it against the blob id and the manifest's size, and signs a receipt that returns to the sender by any path. Only receipts from keys pinned for the destination island (`[[islands]]`) count. Failed deliveries back off from one minute to six hours, and a saved file is never lost to a failed receipt.
- The mule page: waiting, in transit and delivered, a page at a time, with a page per message showing its whole trip and every receipt, and corridor times (send to receipt, and the drive by the car's own clock) with clock skew owned up to. Also as JSON at `/api/corridors`.
- Offline-content updates from ridgewarden-library ride as bulk messages; at the far island the library app takes the file itself, so only the receipt is signed here.
- An optional BPv7 bridge through dtnd (dtn7-rs): text messages leave as bundles carrying the signed event, and incoming bundles are checked, re-published and receipted back.
- Third-party traffic (Part 97.115) is refused, not carried, not bridged and not delivered unless a site turns it on.
- The forms check origin and rate before reading a body, hold at most two uploads in memory, and then need the send key.
- A build guide for a car kit and parking-lot syncs (docs/car-kit.md).

[Unreleased]: https://github.com/Londopy/ridgewarden-mule/commits/main
