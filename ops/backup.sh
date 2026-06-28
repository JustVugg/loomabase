#!/usr/bin/env sh
set -eu

: "${DATABASE_URL:?DATABASE_URL must point to the source database}"
BACKUP_DIR="${BACKUP_DIR:-./backups}"
mkdir -p "$BACKUP_DIR"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUTPUT="$BACKUP_DIR/loomabase-$STAMP.dump"

pg_dump "$DATABASE_URL" \
  --format=custom \
  --no-owner \
  --no-acl \
  --file="$OUTPUT"

pg_restore --list "$OUTPUT" >/dev/null
printf '%s\n' "$OUTPUT"
