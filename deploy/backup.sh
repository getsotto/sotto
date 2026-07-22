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
#   2. Give this host WRITE-ONLY access to it.
#   3. Set SOTTO_BACKUP_BUCKET in deploy/.env (unquoted, like every .env value).
#   4. Install the cron entry.
#
# The dump is validated with `pg_restore --list` BEFORE upload, so a truncated or corrupt
# archive fails the run loudly (cron surfaces it) instead of silently shipping garbage.
set -eu
cd "$(dirname "$0")"

BUCKET="${SOTTO_BACKUP_BUCKET:-$(sed -n 's/^SOTTO_BACKUP_BUCKET=//p' .env 2>/dev/null)}"
if [ -z "$BUCKET" ]; then
    echo "error: SOTTO_BACKUP_BUCKET is not set - add it to deploy/.env (e.g. gs://sotto-backups)" >&2
    exit 1
fi
# Reject an unusable destination now, before the (comparatively expensive) dump.
case "$BUCKET" in
    gs://* | s3://* | *:*) ;;
    *)
        echo "error: SOTTO_BACKUP_BUCKET must be gs://…, s3://…, or an rclone remote:path" >&2
        exit 1
        ;;
esac

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
DUMP="$(mktemp /tmp/sotto-pgdump.XXXXXX)"
trap 'rm -f "$DUMP"' EXIT INT TERM

# Custom-format dump (compressed, pg_restore-able) taken inside the postgres container, so no
# Postgres client tools are needed on the host.
docker compose -f docker-compose.prod.yml exec -T postgres pg_dump -U sotto -Fc sotto > "$DUMP"

# A valid archive can list its contents; an empty or truncated file cannot.
docker compose -f docker-compose.prod.yml exec -T postgres pg_restore --list < "$DUMP" > /dev/null

DEST="$BUCKET/sotto-$STAMP.dump"
case "$BUCKET" in
    gs://*) gsutil -q cp "$DUMP" "$DEST" ;;
    s3://*) aws s3 cp --only-show-errors "$DUMP" "$DEST" ;;
    *) rclone copyto "$DUMP" "$DEST" ;;
esac
echo "backup ok: $DEST ($(wc -c < "$DUMP" | tr -d ' ') bytes)"
