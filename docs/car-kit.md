# Building a car kit

This is a build guide, not a tested product: the app has been run with a laptop playing the car, never in a real car. It describes what a permanent car install needs and why. Check every part against your own vehicle and the AREDN documentation before you buy anything.

## What goes in the car

- **An AREDN node.** Any small node from the AREDN supported-device list (on arednmesh.org) on a band the islands' nodes use. A sector or omni antenna on the roof or behind the rear window does better than one inside the cabin. Give it its own node name and set it up as you would any other node; it joins whichever island it is parked near.
- **A Raspberry Pi 4 or 5** (or any small Linux box) on the node's LAN port, running the ridgewarden core and this app with `[car]` in `mule.toml` and `telemetry = false` in `ridgewarden.toml`.
- **An SSD on USB 3** for the core's data directory (`/var/lib/ridgewarden`). SD cards wear out under the blob store's writes and fail badly when power is cut mid-write; an SSD copes better. Set `[car] capacity_mb` in `mule.toml` to what it can spare; the car never loads more than that, whatever a trip says, and counts what is still aboard.
- **12 V power with ignition sensing.** The car's 12 V supply feeds a fused DC-DC converter for the node (most take 12 V or PoE) and a 5 V converter for the Pi. An ignition-sense signal (a switched 12 V line, through a relay or optocoupler) tells the Pi the engine is off; the Pi keeps running for a set time so a parked car can finish its sync, then shuts down cleanly and cuts its own power. Off-the-shelf Pi power boards with ignition sensing and a shutdown timer exist; pick one with a configurable delay. Never draw from an unfused circuit, and keep the shutdown delay short enough not to flatten the battery.

## Parking-lot sync

The car syncs while parked within reach of an island node: in a public lot, at a ham's house, or outside an emergency operations centre. What it needs:

- **A line of sight to a node.** Check the car node's status page for a link before you rely on a spot. A few minutes of a weak link moves the messages and receipts; files take longer.
- **Time.** Open the car's mule page before leaving (on a phone on the car node's Wi-Fi, or on the laptop). The manifest says "Everything is aboard" or how much is still loading. Urgent files load first, so a short stop still takes the urgent traffic; whatever didn't finish resumes at the next stop, chunk by chunk.
- **Nothing to do on arrival.** The destination island's cores pull from the car on their own once the car's node links up. The car page shows each drop-off as the island's cores confirm they hold the cargo.

## If the car is stolen or the Pi is lost

Everything on it is public and signed anyway; nothing is encrypted. The car's key is its own device key: run `ridgewarden revoke` from another station (see the core's README) so the mesh stops believing anything signed with it, and set the car up again with a new key. Remove its key from `[[islands]]` and `[trust]` at the islands' sites.

## Part 97

- 97.221(b) permits automatic control for data on 6 m and shorter, which covers AREDN's bands, so a parked or moving car node exchanging data looks permitted. That is a reading, not legal advice; confirm with the AREDN forum or the ARRL before running unattended.
- Station identification (97.119) for the car node still needs checking: whether the AREDN firmware's own identification covers a mobile node, and whose callsign it should carry when the driver isn't the node's licensee. The app records the control operator's callsign with every pickup and drop-off, but it does not identify the station on the air.
