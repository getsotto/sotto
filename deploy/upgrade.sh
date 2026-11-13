#!/usr/bin/env bash
# Upgrade the deployment in one command, so that no step of it can be skipped.
#
# Run it on the host, from the deploy directory, as the user that owns the deployment:
#
#     cd ~/sotto/deploy && ./upgrade.sh v0.10.3    # move the image pin, then upgrade
#     cd ~/sotto/deploy && ./upgrade.sh            # keep the current pin, upgrade the rest
#
# The steps are the ones deploy/README.md section Upgrading has always listed. They exist as a
# script because two of them were skipped in practice, and neither failure says so:
#
#   - The image pin moved but `git pull` never ran. Images and the git checkout move
#     independently, so the host kept running a backup.sh from two months earlier, and two fixes
#     to it never reached the machine that needed them.
#   - The pin was edited with sudo, which left .env owned by root. The nightly backup runs as the
#     deployment's user, could no longer read it, and stopped for over a week.
#
# So this refuses to touch a .env that the user running it does not own, and rewrites the pin
# without changing who owns the file.

set -euo pipefail
cd "$(dirname "$0")"

COMPOSE="docker compose -f docker-compose.prod.yml"

log() {
    printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"
}

fail() {
    log "error: $*" >&2
    exit 1
}

TAG="${1:-}"
# An anchored regex, not a case pattern: in `v[0-9]*.[0-9]*` each `*` matches anything at all, so
# a pattern would wave through arbitrary text into the sed expression and .env below.
if [ -n "$TAG" ] && [ "$TAG" != latest ] && ! [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    fail "usage: ./upgrade.sh [vX.Y.Z | latest] (got '$TAG')"
fi

# Before anything else, because every later step either reads .env or writes beside it. Ownership
# rather than readability is the test: a root shell can read and write anything, so a sudo run
# would pass a readability check and then leave the very file the backup cron needs owned by root.
# A deployment run entirely as root still works, since there root does own the file.
if [ ! -f .env ]; then
    fail "no deploy/.env here; this upgrades an existing deployment (see README section First deployment)"
fi
if [ ! -O .env ] || [ ! -r .env ] || [ ! -w .env ]; then
    fail "deploy/.env is not owned, readable and writable by $(id -un). Run this as the deployment's user, not with sudo. If an earlier sudo edit took the file, give it back with: sudo chown <user> $(pwd)/.env"
fi

# Fast-forward only: a host checkout with local commits or a diverged history is something a person
# should look at, not something to merge over in the middle of an upgrade.
log "updating the checkout"
git -C .. pull --ff-only
log "checkout is at $(git -C .. log --oneline -1)"

if [ -n "$TAG" ]; then
    # Written beside .env and moved over it, so a failure part way leaves the old file intact, and
    # the result is owned by whoever ran this, which the check above made the file's owner.
    tmp="$(umask 077 && mktemp ./.env.upgrading.XXXXXX)"
    trap 'rm -f "$tmp"' EXIT
    if grep -q '^SOTTO_IMAGE_TAG=' .env; then
        sed "s/^SOTTO_IMAGE_TAG=.*/SOTTO_IMAGE_TAG=$TAG/" .env > "$tmp"
    else
        cat .env > "$tmp"
        # A file whose last line has no newline would otherwise have the pin glued onto it.
        if [ -s .env ] && [ "$(tail -c 1 .env)" != "" ]; then echo >> "$tmp"; fi
        echo "SOTTO_IMAGE_TAG=$TAG" >> "$tmp"
    fi
    mv "$tmp" .env
    trap - EXIT
    log "image pin set to $TAG"
fi

# A setting renamed upstream leaves the old name in .env, read by nothing, and the feature ships
# dark. Only names are compared, never values. This warns rather than stops, because a deployment
# can legitimately carry settings the template does not list.
stale="$(comm -23 \
    <(sed -n 's/^\([A-Z_][A-Z_0-9]*\)=.*/\1/p' .env | sort -u) \
    <(sed -n 's/^#* *\([A-Z_][A-Z_0-9]*\)=.*/\1/p' .env.example | sort -u))"
if [ -n "$stale" ]; then
    log "warning: these .env settings are not in .env.example; if one was renamed upstream, nothing reads it now:"
    printf '%s\n' "$stale" | sed 's/^/  /' >&2
fi

log "pulling images"
$COMPOSE pull
log "recreating containers"
$COMPOSE up -d
# The server's container gets a new address when it is recreated, and Caddy keeps sending traffic
# to the old one, answering 502, until it restarts.
$COMPOSE restart caddy

pin="$(sed -n 's/^SOTTO_IMAGE_TAG=//p' .env | tail -n 1)"
log "upgrade ok: checkout $(git -C .. rev-parse --short HEAD), image pin ${pin:-latest (unpinned)}"
