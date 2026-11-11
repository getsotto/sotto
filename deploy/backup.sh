#!/usr/bin/env sh
# Nightly off-box backup: a verified pg_dump (custom format) shipped to object storage.
#
# The destination's scheme picks the upload tool, so any provider works:
#   SOTTO_BACKUP_BUCKET=gs://bucket          → gsutil     (Google Cloud Storage)
#   SOTTO_BACKUP_BUCKET=s3://bucket          → aws s3 cp  (S3 and S3-compatibles)
#   SOTTO_BACKUP_BUCKET=remote:path          → rclone     (40+ backends: B2, SFTP, a NAS, …)
#
# One-time setup (full walkthrough in deploy/README.md § Backups):
#   1. Create a bucket with a ~30-day deletion lifecycle at your provider.
#   2. Give this host APPEND-ONLY access to it (create + list, never read or delete - the
#      exact roles per provider are in the README; create alone cannot upload with gsutil).
#   3. Set SOTTO_BACKUP_BUCKET in deploy/.env (unquoted, like every .env value).
#   4. Install the cron entry.
#
# The dump is validated with `pg_restore --list` BEFORE upload, so a truncated or corrupt
# archive fails the run with a non-zero exit instead of silently shipping garbage.
#
# Every line this prints is timestamped, and every run ends in exactly one of `backup ok` or
# `backup FAILED while <step>`. Cron appends all of it to one log that is only ever read after the
# freshness alarm has fired, by which time the question is which night failed and at what. Bare
# tool errors with no date answer neither.
set -eu
cd "$(dirname "$0")"

log() {
    printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"
}

STEP="reading configuration"
DUMP=""
JOURNAL=""
finish() {
    status=$?
    # Never leave a dump of the vault database in /tmp, on any exit path.
    for file in "$DUMP" "$JOURNAL"; do
        if [ -n "$file" ]; then rm -f "$file"; fi
    done
    if [ "$status" -ne 0 ]; then
        log "backup FAILED while $STEP (exit $status)" >&2
    fi
}
trap finish EXIT
# Route signals through exit so the EXIT trap reports them like any other failure.
trap 'exit 130' INT
trap 'exit 143' TERM

# Which version of this script ran. Image deploys never move the host's git checkout, and a host
# once ran a two-month-old copy of this file without anything showing it. A deployment that is not
# a git checkout logs `unknown`, which is not a failure.
log "backup started (script $(git -C .. rev-parse --short HEAD 2>/dev/null || echo unknown))"

# A .env this user cannot read is not the same as a setting that is missing, and must not be
# reported as one. Editing it with sudo can leave it owned by root with mode 600; every later step
# then fails, docker compose included, since compose reads the same file. Older versions of this
# script exited here without printing anything, which hid exactly that for over a week.
if [ -e .env ] && [ ! -r .env ]; then
    log "error: deploy/.env exists but $(id -un) cannot read it. If it was edited with sudo, give it back with: sudo chown $(id -un) $(pwd)/.env" >&2
    exit 1
fi

# A missing .env is the unset case below, not a failure here: without `|| true`, set -e would end
# the run on sed's exit status before the error that explains it.
BUCKET="${SOTTO_BACKUP_BUCKET:-$(sed -n 's/^SOTTO_BACKUP_BUCKET=//p' .env 2>/dev/null || true)}"
if [ -z "$BUCKET" ]; then
    log "error: SOTTO_BACKUP_BUCKET is not set - add it to deploy/.env (e.g. gs://sotto-backups)" >&2
    exit 1
fi
# Reject an unusable destination now, before the (comparatively expensive) dump.
case "$BUCKET" in
    gs://*) UPLOADER=gsutil ;;
    s3://*) UPLOADER=aws ;;
    *:*) UPLOADER=rclone ;;
    *)
        log "error: SOTTO_BACKUP_BUCKET must be gs://…, s3://…, or an rclone remote:path" >&2
        exit 1
        ;;
esac

# Cron runs with a PATH of little more than /usr/bin:/bin, so a tool installed under /snap/bin,
# /usr/local/bin or a home directory works by hand and is missing every night. Find that out
# before the dump, and say so in words, rather than as `not found` after it.
STEP="checking tools"
for tool in docker python3 "$UPLOADER"; do
    if ! command -v "$tool" > /dev/null 2>&1; then
        log "error: $tool is not on PATH ($PATH). Cron does not read your shell profile; set PATH at the top of the crontab." >&2
        exit 127
    fi
done
# `docker compose` is a plugin, not the binary: docker being present does not mean the
# subcommand is installed, and without it the dump step is the first thing to fail.
if ! docker compose version > /dev/null 2>&1; then
    log "error: 'docker compose' is not available. Install the compose plugin; the docker binary alone is not enough." >&2
    exit 127
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
DUMP="$(mktemp /tmp/sotto-pgdump.XXXXXX)"
JOURNAL="$(mktemp /tmp/sotto-retention-journal.XXXXXX)"

# Custom-format dump (compressed, pg_restore-able) taken inside the postgres container, so no
# Postgres client tools are needed on the host.
STEP="dumping the database"
docker compose -f docker-compose.prod.yml exec -T postgres pg_dump -U sotto -Fc sotto > "$DUMP"

# A valid archive can list its contents; an empty or truncated file cannot.
STEP="verifying the dump"
docker compose -f docker-compose.prod.yml exec -T postgres pg_restore --list < "$DUMP" > /dev/null

# Retention deletions are kept in a cumulative sidecar. A dump can predate a purge, so restoring
# only the database bytes would resurrect the deleted ciphertext. Validate the sidecar before it
# leaves the host; the validator prints a count and never prints resource identifiers.
STEP="exporting the retention journal"
../scripts/export-retention-journal "$JOURNAL"
STEP="validating the retention journal"
../scripts/replay-retention-journal --validate-only --journal "$JOURNAL"

DEST="$BUCKET/sotto-$STAMP.dump"
JOURNAL_DEST="$BUCKET/sotto-$STAMP.tombstones.jsonl"
# One missed night breaches the 26-hour freshness threshold, so a transient upload blip must
# not fail the run. Retry briefly before giving up for the night. The `if` around the case
# is load-bearing: under `set -e` a bare failing upload would end the script on the first
# attempt instead of reaching the retry.
upload() {
    attempt=1
    while :; do
        if case "$UPLOADER" in
            gsutil) gsutil -q cp "$1" "$2" ;;
            aws) aws s3 cp --only-show-errors "$1" "$2" ;;
            rclone) rclone copyto "$1" "$2" ;;
        esac; then
            return 0
        fi
        if [ "$attempt" -ge 3 ]; then
            return 1
        fi
        log "upload attempt $attempt for $2 failed, retrying" >&2
        attempt=$((attempt + 1))
        sleep 5
    done
}
STEP="uploading the dump to $DEST"
upload "$DUMP" "$DEST"
STEP="uploading the retention journal to $JOURNAL_DEST"
upload "$JOURNAL" "$JOURNAL_DEST"

log "backup ok: $DEST ($(wc -c < "$DUMP" | tr -d ' ') bytes)"
log "retention journal ok: $JOURNAL_DEST ($(wc -c < "$JOURNAL" | tr -d ' ') bytes)"
