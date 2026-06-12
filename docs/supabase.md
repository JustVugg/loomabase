# Supabase Integration

Loomabase can use a Supabase project as its PostgreSQL system of record and
Supabase Auth as its identity provider. Clients still write through Loomabase's
`POST /sync` endpoint; direct Data API writes are not captured by Loomabase's
CRDT metadata and must not be mixed with synchronized writes.

## Architecture

1. A client signs in with Supabase Auth and receives an access token.
2. The client calls Loomabase with `Authorization: Bearer <access-token>` and a
   stable per-installation `x-device-id`.
3. Loomabase verifies the asymmetric token against the project's JWKS, maps the
   user to a tenant, namespaces the device by the token `sub`, and merges into
   the Supabase PostgreSQL database.

The default tenant is `app_metadata.tenant_id`. When absent, Loomabase uses the
token `sub`, producing an isolated per-user database. An optional
`app_metadata.loomabase_tables` string array restricts which contracts the
token may synchronize.

## Database Setup

Use the direct or session-pooler connection for migrations:

```bash
DATABASE_URL='postgresql://...' \
LOOMABASE_MIGRATE_ONLY=true \
  cargo run --release --features server --bin loomabase-server
```

Then apply [`supabase/runtime-role.sql`](../supabase/runtime-role.sql) and create
a login that is a member of `loomabase_runtime`. Do not run the service as the
Supabase `postgres` role because privileged roles can bypass RLS.

```sql
CREATE ROLE loomabase_app LOGIN PASSWORD 'use-a-generated-secret';
GRANT loomabase_runtime TO loomabase_app;
```

For persistent Loomabase servers, prefer the direct connection or Supavisor
session mode. Transaction mode on port `6543` is supported: Loomabase detects
that port and disables SQLx prepared statement caching. It can also be forced
with `LOOMABASE_DB_TRANSACTION_POOLER=true`.

## Auth Configuration

```bash
DATABASE_URL='postgresql://loomabase_app:...@.../postgres' \
LOOMABASE_SKIP_SCHEMA_INIT=true \
LOOMABASE_SUPABASE_URL='https://project-id.supabase.co' \
  cargo run --release --features server --bin loomabase-server
```

Relevant settings:

- `LOOMABASE_SUPABASE_URL`: project URL. `SUPABASE_URL` is also accepted.
- `LOOMABASE_SUPABASE_JWKS` or `SUPABASE_JWKS`: optional inline JWKS.
- `LOOMABASE_JWKS_REFRESH_SECS`: remote JWKS refresh interval, default `600`.
- `LOOMABASE_SUPABASE_TENANT_CLAIM`: default `app_metadata.tenant_id`.
- `LOOMABASE_SUPABASE_TABLES_CLAIM`: default
  `app_metadata.loomabase_tables`.
- `LOOMABASE_JWT_AUDIENCE`: default `authenticated`.
- `LOOMABASE_JWT_ISSUER`: defaults to `<project-url>/auth/v1`.

Use a Supabase Custom Access Token Hook to add workspace tenant IDs and table
authorization. Keep authorization claims in `app_metadata`, which users cannot
edit directly.

The service keeps the last valid JWKS if a refresh fails or returns an invalid
set. Alert on refresh-failure logs and exercise signing-key rotation before
launch.

## Edge Function Proxy

[`supabase/functions/loomabase-sync/index.ts`](../supabase/functions/loomabase-sync/index.ts)
is an optional same-origin proxy. Configure its target:

```bash
supabase secrets set LOOMABASE_SYNC_URL=https://sync.example.com/sync
supabase functions deploy loomabase-sync --no-verify-jwt
```

JWT verification is intentionally performed by Loomabase using the project's
rotatable JWKS. The proxy only forwards the access token and device ID.

## Limitations

- Supabase Realtime/Data API writes to synchronized application tables bypass
  the Loomabase CRDT merge. Treat those tables as read-only outside Loomabase.
- Revoke Data API grants for synchronized tables unless a read-only API is an
  explicit product requirement.
- Supabase legacy HS256 projects do not expose asymmetric JWKS. Migrate to
  asymmetric signing keys before enabling this adapter.
- Hosted backup retention, PITR, network restrictions, and database sizing
  remain Supabase project configuration responsibilities.
