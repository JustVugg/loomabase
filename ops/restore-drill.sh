#!/usr/bin/env sh
set -eu

: "${RESTORE_DATABASE_URL:?RESTORE_DATABASE_URL must point to an isolated drill database}"
: "${BACKUP_FILE:?BACKUP_FILE must point to a pg_dump custom archive}"

pg_restore "$RESTORE_DATABASE_URL" \
  --clean \
  --if-exists \
  --no-owner \
  --no-acl \
  "$BACKUP_FILE"

# A restored database may have diverged from cursors held by clients. Rotate
# the epoch and revoke every issued cursor so they perform bounded full repair.
psql "$RESTORE_DATABASE_URL" -v ON_ERROR_STOP=1 <<'SQL'
UPDATE loomabase_server_state
SET server_epoch = gen_random_uuid()::text
WHERE singleton;
DELETE FROM loomabase_cursor_lease;
SQL

psql "$RESTORE_DATABASE_URL" -v ON_ERROR_STOP=1 -c \
  "SELECT COUNT(*) AS synchronized_rows FROM todos;"
