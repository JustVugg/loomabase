# Production Runbook

## Release Gate

- CI formatting, Clippy, tests, `cargo audit`, and `cargo deny` are green.
- PostgreSQL integration tests pass against the target major version.
- A migration-only dry run succeeds against a recent production clone.
- Backup and restore drill meets the documented RPO/RTO.
- Load and soak tests meet the service SLO at expected peak tenant size.
- JWT key rotation and cursor epoch rotation have been exercised.
- Gateway TLS, rate limits, metrics scraping, alerts, and log retention work.

## Roles And Migrations

Run DDL/RLS changes with `LOOMABASE_MIGRATE_ONLY=true`. Run normal traffic with
`LOOMABASE_SKIP_SCHEMA_INIT=true` and a non-superuser role that has only DML,
sequence usage, and read access to `loomabase_server_state`.

The migration/maintenance connection must own the schema and be able to bypass
RLS because migrations, cursor expiry, and epoch rotation intentionally operate
across tenants. On Supabase, use the privileged migration connection only for
those short-lived jobs; never use it for normal sync traffic.

Migrations are additive and reject removed/retyped synchronized columns. A
failed migration rolls back transactionally. Rollback of an already committed
schema release is restore-forward: restore a verified backup into an isolated
database, validate it, rotate the server epoch, then switch traffic.

## Container Deployment

Build and publish an immutable image, then set `LOOMABASE_IMAGE` to its digest
before using `deploy/compose.yml`. The image runs as UID/GID `10001`, and the
compose profile drops Linux capabilities, uses a read-only root filesystem, and
expects migrations to have already run.

## Backups And Disaster Recovery

Run `ops/backup.sh` on a schedule and copy archives to encrypted immutable
storage. Run `ops/restore-drill.sh` regularly against an isolated database.
Never restore over the active production database.

After any restore or divergent failover:

```bash
DATABASE_URL='...' \
LOOMABASE_MIGRATE_ONLY=true \
LOOMABASE_ROTATE_SERVER_EPOCH=true \
  loomabase-server
```

This revokes cursor capabilities so every device performs bounded repair.

## Cursor Lease Maintenance

Expire devices that have exceeded the product's supported offline window:

```bash
DATABASE_URL='...' \
LOOMABASE_MIGRATE_ONLY=true \
LOOMABASE_EXPIRE_CURSOR_LEASES_SECS=7776000 \
  loomabase-server
```

Returning devices with expired leases automatically perform a full repair.
Tombstones are deliberately retained; deleting them without a durable
graveyard would allow stale offline replicas to resurrect deleted rows.

## SLO And Incident Response

Recommended initial SLO: 99.9% successful `/sync` requests excluding valid
client rejections, with p95 below the configured request timeout. Tune this
after load tests on representative tenant sizes.

Terminate TLS and enforce per-IP/request rate limits at the gateway. Scrape
`/metrics` only from the monitoring network and alert using
`deploy/prometheus-alerts.yml`.

Run a representative load test with a valid payload fixture:

```bash
k6 run \
  -e BASE_URL=https://sync.example.com \
  -e ACCESS_TOKEN=... \
  -e PAYLOAD_FILE=./sync-payload.json \
  -e VUS=50 \
  ops/k6-sync.js
```

The bundled thresholds are only a starting point. Set them from the actual SLO,
database tier, expected offline window, and largest supported tenant.

For a suspected compromise: revoke JWT signing keys, rotate database
credentials, rotate the Loomabase server epoch, preserve logs, and follow the
private vulnerability process in `SECURITY.md`.
