#!/bin/bash
# End-to-end run with real processes: two simulated meshes ("north" and "south"), three
# ridgewarden cores (one per island and one playing the car) and three mule apps.
#
#   north sends an urgent message (web form), a bulk file (CLI) and a library update;
#   the car syncs with north only, then, with north unreachable, with south only;
#   south delivers and receipts all three; the car goes back and north sees them delivered.
#
# "Unreachable" is real: each core is restarted with a different sync.peers list.
# Needs a sibling checkout of the core with its binaries built (cargo build there), and
# this repo built. Takes about four minutes. Exits non-zero if any step fails.
#
#   bash scripts/three-cores.sh
#
# Override paths with CORE_BIN, MULE_BIN and SCENARIO, and ports with SIM_PORTS (two),
# CORE_PORTS and WEB_PORTS (three each: north, south, car).
set -u

here=$(cd "$(dirname "$0")/.." && pwd)
CORE_BIN=${CORE_BIN:-$here/../ridgewarden/target/debug}
MULE_BIN=${MULE_BIN:-${CARGO_TARGET_DIR:-$here/target}/debug/ridgewarden-mule}
SCENARIO=${SCENARIO:-$here/../ridgewarden/examples/slo-demo.toml}
read -r -a SIMP <<<"${SIM_PORTS:-9171 9172}"
read -r -a COREP <<<"${CORE_PORTS:-8871 8872 8873}"
read -r -a WEBP <<<"${WEB_PORTS:-9173 9174 9175}"
exe=""
[ -x "$CORE_BIN/ridgewarden.exe" ] && exe=.exe
[ -x "$MULE_BIN.exe" ] && MULE_BIN=$MULE_BIN.exe
for b in "$CORE_BIN/ridgewarden$exe" "$CORE_BIN/ridgewarden-sim$exe" "$MULE_BIN"; do
    [ -x "$b" ] || { echo "missing $b (build the core and this repo first)" >&2; exit 2; }
done

S=$(mktemp -d)
# the binaries read paths from config files, so on Windows they need a Windows path
command -v cygpath >/dev/null && S=$(cygpath -m "$S")
failed=0
log() { echo "[$(date +%T)] $*"; }
declare -A PID
start() {
    local n=$1
    shift
    "$@" >>"$S/$n.log" 2>&1 &
    PID[$n]=$!
}
stop() {
    kill "${PID[$1]}" 2>/dev/null
    wait "${PID[$1]}" 2>/dev/null
}
cleanup() {
    for n in "${!PID[@]}"; do stop "$n"; done
    if [ $failed -eq 0 ]; then rm -rf "$S"; else echo "logs and data kept in $S" >&2; fi
}
# check <description> <command...>: run a setup step and count it if it fails
check() {
    local what=$1
    shift
    if "$@" >>"$S/steps.log" 2>&1; then
        log "  $what"
    else
        log "  FAILED: $what"
        failed=$((failed + 1))
    fi
}
trap cleanup EXIT

# wait_for <url> <pattern> <seconds>
wait_for() {
    local t=0
    while [ $t -lt "$3" ]; do
        if curl -s --max-time 5 "$1" | grep -q "$2"; then
            log "  ok after ${t}s: $2"
            return 0
        fi
        sleep 5
        t=$((t + 5))
    done
    log "  FAILED waiting for $2 at $1"
    failed=$((failed + 1))
    return 1
}
counts() { curl -s --max-time 5 "$1/" | grep -o '<div class=n>[0-9]*</div>[a-z ]*' | sed 's/<[^>]*>//g' | tr '\n' ' '; }

for p in "${SIMP[@]}" "${COREP[@]}" "${WEBP[@]}"; do
    if curl -s --max-time 2 -o /dev/null "http://127.0.0.1:$p/"; then
        echo "something already answers on port $p; pick other ports (see the top of this script)" >&2
        exit 2
    fi
done

SIM1=${SIMP[0]}
SIM2=${SIMP[1]}
declare -A CALL=([a]=N0AAA [b]=N0BBB [car]=N0CAR)
declare -A PORT=([a]=${COREP[0]} [b]=${COREP[1]} [car]=${COREP[2]})
declare -A WEB=([a]=${WEBP[0]} [b]=${WEBP[1]} [car]=${WEBP[2]})
declare -A SIM=([a]=$SIM1 [b]=$SIM2 [car]=$SIM1)
declare -A KEY
A=http://127.0.0.1:${PORT[a]}
B=http://127.0.0.1:${PORT[b]}
WA=http://127.0.0.1:${WEB[a]}
WB=http://127.0.0.1:${WEB[b]}
WCAR=http://127.0.0.1:${WEB[car]}

start sim1 "$CORE_BIN/ridgewarden-sim$exe" --listen 127.0.0.1:$SIM1 "$SCENARIO"
start sim2 "$CORE_BIN/ridgewarden-sim$exe" --listen 127.0.0.1:$SIM2 "$SCENARIO"
for n in a b car; do
    "$CORE_BIN/ridgewarden$exe" --config "$S/$n.toml" init --callsign "${CALL[$n]}" --data-dir "$S/$n" >/dev/null
    KEY[$n]=$("$CORE_BIN/ridgewarden$exe" --config "$S/$n.toml" whoami | cut -d' ' -f2)
done

# core <name> <peers...>: (re)start a core allowed to pull from only these peers
core() {
    local n=$1
    shift
    local peers="" p trust="" t telemetry=true
    for p in "$@"; do peers="$peers\"http://127.0.0.1:${PORT[$p]}\","; done
    for t in a b car; do [ "$t" != "$n" ] && trust="$trust\"${KEY[$t]}\","; done
    [ "$n" = car ] && telemetry=false
    cat >"$S/$n.toml" <<EOF
callsign = "${CALL[$n]}"
data_dir = "$S/$n"
listen = "127.0.0.1:${PORT[$n]}"
[node]
url = "http://127.0.0.1:${SIM[$n]}/n/TST-CUESTA"
poll_interval_s = 60
[sync]
interval_s = 5
fanout = 2
peers = [${peers%,}]
discover = false
telemetry = $telemetry
[trust]
keys = [${trust%,}]
[blobs]
rate_kbps = 20000
EOF
    [ -n "${PID[$n]:-}" ] && stop "$n"
    start "$n" "$CORE_BIN/ridgewarden$exe" --config "$S/$n.toml" run
    log "core $n now pulls from: ${*:-nobody}"
}

# app <name> <island|car> [extra config]
app() {
    local head
    if [ "$2" = car ]; then
        head='operator = "N0DRV"'
    else
        head="island = \"$2\"
operator = \"${CALL[$1]}\""
    fi
    cat >"$S/mule-$1.toml" <<EOF
data_dir = "$S/mule-$1"
listen = "127.0.0.1:${WEB[$1]}"
$head
cars = ["${KEY[car]}"]
[core]
url = "http://127.0.0.1:${PORT[$1]}"
token_file = "$S/$1/local.token"
imports_dir = "$S/$1/imports"
[[islands]]
name = "north"
keys = ["${KEY[a]}"]
cores = ["$A"]
[[islands]]
name = "south"
keys = ["${KEY[b]}"]
cores = ["$B"]
${3:-}
EOF
    start "mule-$1" "$MULE_BIN" --config "$S/mule-$1.toml" run
}

log "the car is parked at north"
core b
core car a
core a car
sleep 3
app a north
app b south
app car car '[car]
capacity_mb = 100'
sleep 4
dep=$(date -u -d '+30 min' +%H:%M)
check "trip registered" "$MULE_BIN" --config "$S/mule-a.toml" trip daily-south --courier N0DRV \
    --from north --to south --days daily --depart "$dep" --hours 2 --capacity-mb 50
key=$(cat "$S/mule-a/send.key" 2>/dev/null)
code=$(curl -s -o /dev/null -w '%{http_code}' --form-string "key=$key" --form-string from=N0AAA \
    --form-string to=south --form-string call=N0BBB --form-string class=urgent \
    --form-string "text=Shelter at the high school needs 20 cots and water by tomorrow." "$WA/send")
[ "$code" = 303 ] && log "  urgent message sent from the form" || { log "  FAILED form post: $code"; failed=$((failed + 1)); }
code=$(curl -s -o /dev/null -w '%{http_code}' --form-string key=wrong "$WA/send")
[ "$code" = 429 ] && log "  a second post straight after is told to wait" || { log "  FAILED: expected 429, got $code"; failed=$((failed + 1)); }
sleep 4
# a file through the form, the key and message first as on the page
head -c 1500000 /dev/urandom >"$S/form.bin"
code=$(curl -s -o /dev/null -w '%{http_code}' --form-string "key=$key" --form-string from=N0AAA     --form-string to=south --form-string class=bulk --form-string "text=Shelter floor plan"     -F "file=@$S/form.bin" "$WA/send")
[ "$code" = 303 ] && log "  file sent from the form" || { log "  FAILED form upload: $code"; failed=$((failed + 1)); }
head -c 2621500 /dev/urandom >"$S/maps.bin"
check "bulk file sent from the command line" "$MULE_BIN" --config "$S/mule-a.toml" send --to south \
    --class bulk --file "$S/maps.bin" "Map tiles"
# what ridgewarden-library's fetcher logs for a new version with [mule] to = ["south"]
head -c 3000000 /dev/urandom >"$S/a/imports/lib.pmtiles"
tok=$(cat "$S/a/local.token")
root=$(curl -s -H "Authorization: Bearer $tok" -H 'content-type: application/json' \
    -d '{"file":"lib.pmtiles","name":"library/maps/2026-11","priority":8}' "$A/local/v1/blob/add" |
    sed 's/.*"root":"\([0-9a-f]*\)".*/\1/')
if [ ${#root} -ne 64 ]; then
    log "  FAILED: the core did not publish the library file"
    failed=$((failed + 1))
fi
cat >"$S/libmsg.json" <<EOF
{"kind":"mule.msg","payload":{"app":"ridgewarden-library","item":"maps","version":"2026-11","from_island":"north","to":"south","from":"N0AAA","class":"bulk","text":"Offline library update: test maps 2026-11.","file":{"root":"$root","name":"maps_2026-11.pmtiles","size":3000000}}}
EOF
check "library update sent" curl -sf -o /dev/null -H "Authorization: Bearer $tok" \
    -H 'content-type: application/json' --data-binary "@$S/libmsg.json" "$A/local/v1/append"
wait_for "$WCAR/manifest" 'Everything is aboard: 4 item' 240
wait_for "$WCAR/" '<div class=n>4</div>in transit' 120

log "the car drives to south; north can no longer reach it"
core a
core car b
core b car
log "  north, cut off: $(counts "$WA")"
wait_for "$WB/" '<div class=n>4</div>delivered' 300
wait_for "$WCAR/" '<div class=n>4</div>delivered' 120
wait_for "$WCAR/manifest" 'Nothing to take' 120

log "the car drives back to north"
core b
core car a
core a car
wait_for "$WA/" '<div class=n>4</div>delivered' 180
wait_for "$WA/api/corridors" '"urgent"' 10

files=0
maps=""
for id in $(curl -s --max-time 5 "$WB/" | grep -o 'msg?id=[0-9a-f]*' | cut -d= -f2 | sort -u); do
    if curl -s --max-time 5 "$WB/msg?id=$id" | grep -q Download; then
        files=$((files + 1))
        curl -s --max-time 60 -o "$S/got.bin" "$WB/file?id=$id"
        if cmp -s "$S/got.bin" "$S/maps.bin"; then
            maps=$id
            log "  the command line's file at south matches what north sent"
        elif cmp -s "$S/got.bin" "$S/form.bin"; then
            log "  the form's file at south matches what north sent"
        else
            log "  FAILED: a file at south differs from both sent"
            failed=$((failed + 1))
        fi
    fi
done
if [ $files -ne 2 ] || [ -z "$maps" ]; then
    log "  FAILED: expected the two files at south, found $files"
    failed=$((failed + 1))
else
    curl -s --max-time 10 -r 1000-1099 -o "$S/range.bin" "$WB/file?id=$maps"
    if cmp -s "$S/range.bin" <(head -c 1100 "$S/maps.bin" | tail -c 100); then
        log "  a byte range comes back right"
    else
        log "  FAILED: the byte range differs"
        failed=$((failed + 1))
    fi
    head -c 1000000 "$S/maps.bin" >"$S/resumed.bin"
    curl -s --max-time 60 -C - -o "$S/resumed.bin" "$WB/file?id=$maps"
    if cmp -s "$S/resumed.bin" "$S/maps.bin"; then
        log "  a download resumed from 1 MB matches"
    else
        log "  FAILED: the resumed download differs"
        failed=$((failed + 1))
    fi
fi
for w in "$WA" "$WB" "$WCAR"; do
    for path in / /send /manifest /trips "/list?state=delivered" /api/corridors; do
        code=$(curl -s --max-time 10 -o /dev/null -w '%{http_code}' "$w$path")
        [ "$code" = 200 ] || { log "  FAILED $w$path: $code"; failed=$((failed + 1)); }
    done
done
curl -s --max-time 5 "$WA/api/corridors"
echo
if [ $failed -gt 0 ]; then
    log "$failed step(s) failed"
    exit 1
fi
log "all steps passed"
