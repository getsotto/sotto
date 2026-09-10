#!/usr/bin/env bash
# Rotate the organisation-deletion tokens in place, on the host, and prove the rotation took.
#
# Run it on the deployment, from the deploy directory:
#
#     cd ~/sotto/deploy && ./rotate-deletion-tokens.sh
#
# It never prints a token. Not to the terminal, not to a log, not into whatever is reading over
# your shoulder or recording your session: the new values are written to .env and read back from
# there for the checks. A rotation that leaks the replacement on the way in has rotated nothing.
#
# Both tokens are treated as one operation because they are one blast radius. The metrics token
# reads aggregate counters and the operator token records billing observations, and an exposure
# that reached one plausibly reached the other, since they live two lines apart in the same file.

set -euo pipefail

COMPOSE="docker compose -f docker-compose.prod.yml"
METRICS_VAR=SOTTO_ORGANISATION_DELETION_METRICS_TOKEN
OPERATOR_VAR=SOTTO_ORGANISATION_DELETION_OPERATOR_TOKEN

# Registered before anything creates a temporary file, which is the only ordering that works:
# both kinds hold a token. The header files hold one being sent, and the half-written .env holds
# the fresh one on its way in, so a failure or a kill between writing and moving it would leave
# a live secret on disk with nothing to clear it away.
cleanup_temporaries() {
  rm -f ./.sotto-rotate-hdr.* ./.env.rotating.* 2>/dev/null || true
}
trap cleanup_temporaries EXIT

if [ ! -f .env ]; then
  echo "no .env here; run this from the deploy directory on the host" >&2
  exit 1
fi

read_var() {
  # First match only, which is safe because a repeated key is refused outright below. Left in
  # place regardless: this value goes into an Authorization header, and a multi-line value there
  # is a second header rather than a wrong token.
  grep -m1 "^$1=" .env | cut -d= -f2- || true
}

refuse_duplicates() {
  local count
  count="$(grep -c "^$1=" .env || true)"
  if [ "$count" -gt 1 ]; then
    # Not tidiness. Compose reads the last occurrence and this script reads the first, so with a
    # duplicate the "old token is rejected" check would test a value the server never used and
    # pass without proving anything. An ambiguous secrets file is worth fixing before rotating
    # the secrets in it.
    echo "$1 appears ${count} times in .env; remove the duplicates before rotating" >&2
    echo "the server uses the last occurrence and this script reads the first, so a rotation" >&2
    echo "here could verify itself against a value that was never live" >&2
    exit 1
  fi
}
refuse_duplicates "$METRICS_VAR"
refuse_duplicates "$OPERATOR_VAR"

# Only now that .env has been found sound. Copying first would leave a spare copy of the secrets
# behind after a run that refused to do anything, which is a poor trade for a file this script
# then has to tell you to delete.
#
# Restrictive mode from the start rather than after the fact: a backup of a secrets file is a
# secrets file.
backup=".env.before-rotation-$(date -u +%Y%m%dT%H%M%SZ)"
(umask 077 && cp .env "$backup")
echo "previous .env saved as $backup"

old_metrics="$(read_var "$METRICS_VAR")"
old_operator="$(read_var "$OPERATOR_VAR")"
for pair in "$METRICS_VAR:$old_metrics" "$OPERATOR_VAR:$old_operator"; do
  if [ -z "${pair#*:}" ]; then
    echo "${pair%%:*} is not set in .env; nothing to rotate" >&2
    exit 1
  fi
done

# Hex, so the value can never contain a character that .env parsing, sed, or a shell would treat
# as special. An .env value must also be unquoted here, and hex is safely bare.
# Written through a temporary file rather than edited in place. `sed -i` means different things
# to GNU and BSD sed, so in-place editing is only testable on whichever the author happens to
# have; and for a file full of secrets, writing a fresh one at mode 600 and moving it over is a
# better shape than mutating the original anyway. If .env was more permissive than that, this
# tightens it, which is not a regression.
rotate() {
  local fresh tmp
  fresh="$(openssl rand -hex 32)"
  tmp="$(umask 077 && mktemp ./.env.rotating.XXXXXX)"
  awk -v name="$1" -v value="$fresh" \
    'index($0, name "=") == 1 { print name "=" value; next } { print }' .env > "$tmp"
  mv "$tmp" .env
}
rotate "$METRICS_VAR"
rotate "$OPERATOR_VAR"
echo "both tokens replaced"

# `up -d`, never `restart`: restart reuses the existing environment and would leave the old
# tokens live while .env claimed otherwise, which is the worst of both.
$COMPOSE up -d server
echo "server restarted with the new environment"

# Wait for it to answer at all before concluding anything about what it answers, and say so if
# it never does. Falling through a timed-out wait would run every check against a server that is
# not listening, turning one clear problem into five confusing ones.
healthy=no
for _ in $(seq 30); do
  if curl -fsS -o /dev/null http://127.0.0.1:8080/health 2>/dev/null; then
    healthy=yes
    break
  fi
  sleep 2
done
if [ "$healthy" != yes ]; then
  echo "the server did not come back within 60 seconds; not verifying anything against it" >&2
  echo "the previous .env is at ${backup}; restore it and run \`${COMPOSE} up -d server\`" >&2
  exit 1
fi

# Header files, so a token is never an argument to anything. argv is world readable: `ps` on a
# shared host hands the bearer token to any local user for as long as the request runs, which
# would be a poor way to end a script whose whole purpose is retiring exposed tokens.

with_token() {
  local token="$1" code hdr
  shift
  hdr="$(umask 077 && mktemp ./.sotto-rotate-hdr.XXXXXX)"
  printf 'Authorization: Bearer %s\n' "$token" > "$hdr"
  code="$(status -H "@$hdr" "$@")"
  rm -f "$hdr"
  printf '%s' "$code"
}

status() {
  # Prints a status code and nothing else, so a token used here cannot reach the output.
  #
  # Always succeeds, and always prints three digits. curl exits non-zero when it cannot connect,
  # and this runs inside a command substitution: in an assignment that would end the script
  # under `set -e`, silently, at the exact moment the operator most needs to be told what went
  # wrong. `000` is curl's own way of saying there was no response, so it is what gets reported.
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' "$@" || true)"
  printf '%s' "${code:-000}"
}

metrics_url="http://127.0.0.1:8080/ops/organisation-deletion/metrics"
observation_url="http://127.0.0.1:8080/ops/organisation-deletion/rotation-check/billing-observation"
new_metrics="$(read_var "$METRICS_VAR")"
new_operator="$(read_var "$OPERATOR_VAR")"

failures=0
check() {
  local label="$1" expected="$2" actual="$3"
  if [ "$actual" = "$expected" ]; then
    echo "  ok    ${label} (${actual})"
  else
    echo "  FAIL  ${label}: expected ${expected}, got ${actual}" >&2
    failures=$((failures + 1))
  fi
}

echo "checking:"
# The new token works, which proves the server actually reloaded rather than merely restarting.
check "metrics accepts the new token" 200 "$(with_token "$new_metrics" "$metrics_url")"
# The old one does not, which is the only evidence that rotation happened at all. Without this a
# no-op edit and a successful rotation look identical.
check "metrics rejects the old token" 401 "$(with_token "$old_metrics" "$metrics_url")"
check "metrics rejects no token" 401 "$(status "$metrics_url")"
check "operator rejects the old token" 401 \
  "$(with_token "$old_operator" -X POST -H 'content-type: application/json' -d '{}' \
      "$observation_url")"
# Anything but 401 means the new token got past the bearer check; the request itself is expected
# to fail afterwards, because `rotation-check` is not an organisation.
operator_new="$(with_token "$new_operator" -X POST -H 'content-type: application/json' \
  -d '{}' "$observation_url")"
case "$operator_new" in
  401)
    echo "  FAIL  operator accepts the new token: refused it (401)" >&2
    failures=$((failures + 1))
    ;;
  000)
    # No answer is not acceptance. Reading it as one turned a dead server into a tick, which is
    # the only kind of check worth nothing at all.
    echo "  FAIL  operator accepts the new token: no answer from the server (000)" >&2
    failures=$((failures + 1))
    ;;
  *)
    echo "  ok    operator accepts the new token (${operator_new}, past the bearer check)"
    ;;
esac

unset new_metrics new_operator old_metrics old_operator

if [ "$failures" -ne 0 ]; then
  echo >&2
  echo "rotation did not verify. The previous .env is at ${backup}; restore it and run" >&2
  echo "\`${COMPOSE} up -d server\` to go back." >&2
  exit 1
fi

echo
echo "rotated and verified. Delete ${backup} once you are satisfied: it still holds the old"
echo "tokens, and a rotation that leaves the old values on disk has moved them rather than"
echo "retired them."
