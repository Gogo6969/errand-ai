#!/usr/bin/env bash
# Drive the real daemon through the real API and check what a person would see.
#
# This exists because unit tests have twice passed while the feature they cover
# was unreachable: the side-effect fence had five green tests and no callers,
# and allowed_domains was read in three places and written by nothing, so every
# task shipped unable to open a website. Both were invisible to `cargo test`
# and obvious the moment anything went through the front door.
#
# So: no raw SQL, no test helpers, no internal functions. Every step below is a
# request a user's own client could make, and every assertion is something a
# user would notice.
#
#   ./scripts/smoke.sh            build first, then run
#   ./scripts/smoke.sh --no-build use the binary that is already there
set -uo pipefail
cd "$(dirname "$0")/.."

PORT=${SMOKE_PORT:-4493}
DATA=$(mktemp -d "${TMPDIR:-/tmp}/errand-smoke.XXXXXX")
BIN=target/debug/errandd
PASS=0
FAIL=0
DAEMON_PID=""

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  # The log is worth keeping when something failed; the database is not.
  if [ "$FAIL" -ne 0 ]; then
    echo
    echo "The daemon log is at $DATA/daemon.log"
  else
    rm -rf "$DATA"
  fi
}
trap cleanup EXIT

ok()   { PASS=$((PASS+1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  \033[31m✗\033[0m %s\n' "$1"; [ $# -gt 1 ] && printf '      %s\n' "$2"; }
head2(){ printf '\n\033[1m%s\033[0m\n' "$1"; }

# Assert on a body: `expect "what this proves" "$body" 'python expression over d'`
expect() {
  local what="$1" body="$2" test="$3"
  if BODY="$body" python3 -c "
import json, os, sys
raw = os.environ['BODY']
try:
    d = json.loads(raw) if raw.strip() else {}
except Exception:
    sys.exit(2)
sys.exit(0 if ($test) else 1)
" 2>/dev/null; then
    ok "$what"
  else
    bad "$what" "$(printf '%s' "$body" | head -c 300)"
  fi
}

api() { # api METHOD PATH [BODY]
  local m="$1" p="$2" b="${3:-}"
  if [ -n "$b" ]; then
    curl -s -m 30 -X "$m" -H "Authorization: Bearer $TOKEN" \
      -H 'Content-Type: application/json' -d "$b" "http://127.0.0.1:$PORT$p"
  else
    curl -s -m 30 -X "$m" -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT$p"
  fi
}
code() { # code METHOD PATH [BODY] -> HTTP status only
  local m="$1" p="$2" b="${3:-}"
  if [ -n "$b" ]; then
    curl -s -o /dev/null -w '%{http_code}' -m 30 -X "$m" -H "Authorization: Bearer $TOKEN" \
      -H 'Content-Type: application/json' -d "$b" "http://127.0.0.1:$PORT$p"
  else
    curl -s -o /dev/null -w '%{http_code}' -m 30 -X "$m" -H "Authorization: Bearer $TOKEN" \
      "http://127.0.0.1:$PORT$p"
  fi
}

if [ "${1:-}" != "--no-build" ]; then
  echo "Building"
  cargo build -q -p errand-runner || { echo "build failed"; exit 1; }
fi

export ERRAND_DATA_DIR="$DATA"
export ERRAND_API_PORT="$PORT"
# Never let a smoke test send a real message to a real person.
export ERRAND_APPLE_DRY=1

# The binary under test is a debug build, so its secrets go to a file beside
# this scratch database rather than into the real keychain. That is what stops
# this script raising an OS permission dialog, and what stops it overwriting the
# token the installed release daemon is using.
export ERRAND_KEYCHAIN=off
TOKEN=$("$BIN" token --new 2>/dev/null | head -1)
[ -n "$TOKEN" ] || { echo "could not mint a token"; exit 1; }

"$BIN" --foreground >"$DATA/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 60); do
  curl -s -m 1 "http://127.0.0.1:$PORT/v1/health" >/dev/null 2>&1 && break
  sleep 0.25
done
curl -s -m 2 "http://127.0.0.1:$PORT/v1/health" >/dev/null || { echo "daemon did not start"; exit 1; }

# ---------------------------------------------------------------------------
head2 "A task can be given the sites it may open"

T=$(api POST /v1/tasks '{"name":"Smoke","description":"A task used by the smoke test.","allowed_domains":["HTTPS://Example.COM/some/path","www.example.org"]}')
TID=$(printf '%s' "$T" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))' 2>/dev/null)
expect "a task is created with sites, and they are stored the way the browser will compare them" \
  "$T" "d.get('allowed_domains') == ['example.com','www.example.org']"
[ -n "$TID" ] || { bad "no task id came back, so nothing below can run"; echo; echo "$PASS passed, $FAIL failed"; exit 1; }

expect "a wildcard is refused, because it would save happily and match nothing" \
  "$(api POST /v1/tasks '{"name":"W","description":"x","allowed_domains":["*.example.com"]}')" \
  "'*' not in str(d.get('allowed_domains','')) and d.get('code')=='bad_request'"

expect "a bare suffix is refused, because it would allow every site ending that way" \
  "$(api POST /v1/tasks '{"name":"S","description":"x","allowed_domains":["co.uk"]}')" \
  "d.get('code')=='bad_request'"

expect "the sites can be changed afterwards" \
  "$(api PATCH "/v1/tasks/$TID" '{"allowed_domains":["example.com","other.example"]}')" \
  "'other.example' in json.dumps(d)"

# ---------------------------------------------------------------------------
head2 "A schedule can be set, and says what it really means"

P=$(api POST /v1/schedule/preview '{"kind":"cron","expr":"0 0 8 * * *","tz":"UTC"}')
expect "the engine describes a schedule in words rather than leaving a cron expression on screen" \
  "$P" "d.get('valid') is True and '08:00' in d.get('describes','')"
expect "and says when the next runs actually are" \
  "$P" "len(d.get('preview',[])) >= 1"
expect "a nonsense schedule is reported, not crashed on" \
  "$(api POST /v1/schedule/preview '{"kind":"cron","expr":"quarter past banana","tz":"UTC"}')" \
  "d.get('valid') is False and len(d.get('problem') or '') > 0"

# ---------------------------------------------------------------------------
head2 "Changing a schedule does not replay the past"
# The bug this guards: the scheduler's catch-up cursor is global, so a task
# switched onto a daily cron can have this morning's slot treated as 'missed'
# and fired immediately. A booking for a slot that has already passed.

BEFORE=$(api GET "/v1/runs?task_id=$TID" | python3 -c 'import sys,json;print(len(json.load(sys.stdin).get("items",[])))' 2>/dev/null || echo 0)
api PATCH "/v1/tasks/$TID" '{"schedule":{"kind":"cron","expr":"0 0 8 * * *","tz":"UTC"}}' >/dev/null
sleep 25   # longer than one scheduler tick
AFTER=$(api GET "/v1/runs?task_id=$TID" | python3 -c 'import sys,json;print(len(json.load(sys.stdin).get("items",[])))' 2>/dev/null || echo 0)
if [ "$BEFORE" = "$AFTER" ]; then
  ok "switching to a daily schedule did not fire a run for a slot that had already gone"
else
  bad "switching to a daily schedule fired $((AFTER-BEFORE)) historical run(s)" \
      "this is the catch-up burst; the per-task floor is not being applied"
fi

expect "and the next run it promises is in the future" \
  "$(api GET "/v1/tasks/$TID")" \
  "(lambda n: n is None or n > __import__('datetime').datetime.now(__import__('datetime').timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'))(d.get('next_run_at'))"

# ---------------------------------------------------------------------------
head2 "Editing a schedule does not quietly delete the parts the form cannot show"
# The schedule editor builds a spec from five fields. A booking window and a
# catch-up grace can only be set through the API, so a save from the form must
# carry them across rather than dropping them — a task that armed five minutes
# early would otherwise stop doing so with nothing said.

api PATCH "/v1/tasks/$TID" '{"schedule":{"kind":"cron","expr":"0 0 8 * * MON","tz":"UTC","catch_up_grace_min":1440,"window":{"not_before":"08:00","not_after":"09:00","arm_early_s":300}}}' >/dev/null
WITH=$(api GET "/v1/tasks/$TID" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("schedule_describes",""))')
# Exactly what the form sends back: the five fields it models, plus what it carries.
api PATCH "/v1/tasks/$TID" '{"schedule":{"kind":"cron","expr":"0 0 8 * * MON","tz":"UTC","catch_up":"run_once_late","jitter_s":0,"catch_up_grace_min":1440,"window":{"not_before":"08:00","not_after":"09:00","arm_early_s":300}}}' >/dev/null
AFTER=$(api GET "/v1/tasks/$TID" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("schedule_describes",""))')
if [ "$WITH" = "$AFTER" ]; then
  ok "a save that changes nothing really changes nothing"
else
  bad "re-saving the same schedule altered it" "was: $WITH"$'\n'"      now: $AFTER"
fi

# ---------------------------------------------------------------------------
head2 "Editing one limit does not wipe the others"
# A PATCH of a single limit must not drop the message ceiling, which is what
# stops a run messaging everyone it can reach.

api PATCH "/v1/tasks/$TID" '{"limits":{"max_steps":40,"max_minutes":10,"max_usd":0.25,"max_heal_cycles":1,"max_messages":2}}' >/dev/null
api PATCH "/v1/tasks/$TID" '{"limits":{"max_usd":2.0}}' >/dev/null
expect "changing the money ceiling leaves the message ceiling alone" \
  "$(api GET "/v1/tasks/$TID")" \
  "d.get('limits',{}).get('max_messages')==2 and d.get('limits',{}).get('max_usd')==2.0"

# ---------------------------------------------------------------------------
head2 "An untaught task cannot be put on a schedule behind the gate's back"
# activate() refuses an untaught task only when it is already scheduled, so a
# PATCH onto a cron must apply the same rule or it becomes the way round it.

U=$(api POST /v1/tasks '{"name":"Untaught","description":"never taught","allowed_domains":["example.com"]}')
UID_=$(printf '%s' "$U" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))' 2>/dev/null)
api POST "/v1/tasks/$UID_/activate" >/dev/null
C=$(code PATCH "/v1/tasks/$UID_" '{"schedule":{"kind":"cron","expr":"0 0 9 * * *","tz":"UTC"}}')
if [ "$C" = "409" ]; then
  ok "putting an untaught task on a schedule is refused (409)"
else
  bad "an untaught task was allowed onto a schedule (HTTP $C)" \
      "this is the gate between 'tried once while watched' and 'runs alone at 3am'"
fi

# ---------------------------------------------------------------------------
head2 "People a task may message"

R=$(api POST /v1/recipients '{"label":"Test Person","channel":"apple_mail","address":"nobody@example.com"}')
RID=$(printf '%s' "$R" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("id",""))' 2>/dev/null)
expect "a person can be saved" "$R" "len(d.get('id',''))>0"

if [ -n "$RID" ]; then
  api POST "/v1/tasks/$TID/recipients" "{\"recipient_id\":\"$RID\",\"on_success\":true,\"on_failure\":true}" >/dev/null
  expect "and linked to one task, which is the grant that lets that task message them" \
    "$(api GET "/v1/tasks/$TID/recipients")" \
    "any(i.get('id')=='$RID' for i in d.get('items',[]))"
  expect "a different task does not inherit that permission" \
    "$(api GET "/v1/tasks/$UID_/recipients")" \
    "not any(i.get('id')=='$RID' for i in d.get('items',[]))"
fi

expect "an address that is not an address is refused" \
  "$(api POST /v1/recipients '{"label":"Bad","channel":"apple_mail","address":"not-an-email"}')" \
  "d.get('code')=='bad_request'"

# ---------------------------------------------------------------------------
head2 "Quiet hours can be set at all"
# Read since M1, writable by nothing until now.

api POST /v1/channels/telegram/config \
  '{"secrets":{},"settings":{"messaging.quiet":{"from":23,"to":6,"failures_break_through":true}}}' >/dev/null
expect "quiet hours round-trip" \
  "$(api GET /v1/settings)" \
  "d.get('messaging.quiet',{}).get('from')==23 and d.get('messaging.quiet',{}).get('to')==6"
expect "an hour that is not an hour is refused" \
  "$(api POST /v1/channels/telegram/config '{"secrets":{},"settings":{"messaging.quiet":{"from":99,"to":6}}}')" \
  "d.get('code')=='bad_request'"

# ---------------------------------------------------------------------------
head2 "The shipped app would be able to browse"
# Not an API check: the three files whose absence means a stranger's copy can
# open no page at all.

SIDECAR=sidecars/browser-agent/src/main.mjs
[ -f "$SIDECAR" ] && ok "the sidecar script is in the repo" || bad "the sidecar script is missing"
[ -d sidecars/browser-agent/node_modules/playwright-core ] \
  && ok "the sidecar's dependencies are installed" \
  || bad "playwright-core is not installed" "run: npm --prefix sidecars/browser-agent install"
if python3 -c "
import json,sys
c=json.load(open('app/tauri.conf.json'))
r=c.get('bundle',{}).get('resources')
sys.exit(0 if isinstance(r,dict) and any('browser-agent' in v for v in r.values()) else 1)"; then
  ok "the bundle is configured to ship the sidecar where the daemon looks for it"
else
  bad "tauri.conf.json does not bundle the sidecar as a resources map" \
      "the array form silently files it under _up_/ where nothing will find it"
fi
if grep -q "channel" "$SIDECAR" 2>/dev/null && grep -qi "chrome" "$SIDECAR" 2>/dev/null; then
  ok "the sidecar looks for a browser that is actually installed"
else
  bad "the sidecar still expects a browser playwright-core never downloads"
fi

# ---------------------------------------------------------------------------
head2 "A scan opens enough sockets to be worth running"
# The bug this guards: launchd gives its children 256 open files while a
# terminal gets a million, so a sweep tuned for a terminal blew past the limit
# and every connection failed instantly. The scan finished in half a second and
# found nothing, which reads exactly like a network with nothing on it.

expect "a scan of this machine reports how hard it looked" \
  "$(api POST '/v1/ai/discover?scan_network=false')" \
  "d.get('ports', 0) >= 15 and d.get('addresses') == 1"

# Loopback alone must still find whatever is running here, if anything is.
LOCAL=$(api POST '/v1/ai/discover?scan_network=false')
if printf '%s' "$LOCAL" | grep -q '"found":\[\]'; then
  ok "no model server is running on this machine, and the scan says so plainly"
else
  expect "a model server on this machine is found, not silently skipped" \
    "$LOCAL" "all(f['base_url'].startswith('http://127.0.0.1:') for f in d['found'])"
fi

# ---------------------------------------------------------------------------
head2 "Nothing leaked"
# The database and the log are the two places a secret most easily ends up.

if grep -rqi "err_v1_\|sk-ant-\|bot_token" "$DATA/daemon.log" 2>/dev/null; then
  bad "something secret-shaped is in the daemon log"
else
  ok "no token or key shape appears in the log"
fi

echo
if [ "$FAIL" -eq 0 ]; then
  printf '\033[32m%s checks passed.\033[0m\n' "$PASS"
else
  printf '\033[31m%s passed, %s failed.\033[0m\n' "$PASS" "$FAIL"
fi
exit $(( FAIL > 0 ))
