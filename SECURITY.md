# Security Policy

## Reporting a Vulnerability

Do not open a public issue for a suspected vulnerability. Report it privately
through GitHub Security Advisories for the Loomabase repository. Include affected
versions, impact, reproduction steps, and any proposed mitigation.

The maintainers will acknowledge a complete report within five business days,
coordinate remediation and disclosure with the reporter, and publish a
security advisory when a fix is available.

## Security Model

Loomabase treats every synchronization payload as untrusted input.

- The API layer must authenticate devices before calling `merge_crdt_states`.
- The authenticated device identifier is checked against every submitted
  mutation to prevent attribution spoofing.
- Table and column names come from a validated contract (`TableDef`): strict
  lowercase SQL identifiers, never payload-controlled. Payload values are always
  passed through bound SQL parameters.
- Payload row/cell counts, identifier lengths, value sizes, finite numeric
  values, value types, storable Lamport clocks, and per-sync client clock
  advances are validated before mutation.
- Server responses are bounded by cell and byte budgets. Change-feed cursors
  are accepted only with an opaque capability previously issued for the
  authenticated tenant/device/table and current server epoch; invalid, expired,
  restored, or future cursors trigger a bounded full repair.
- Every payload carries a contract fingerprint; a schema mismatch is rejected
  before any mutation.
- The server identity is reserved and cannot authenticate as a client.
- SQLite and PostgreSQL writes are enclosed in explicit transactions.
- The generated PostgreSQL schema keys every table by `tenant_id` and enables
  forced Row-Level Security. `merge_crdt_states` sets the per-transaction tenant
  context, and the `tenant_id` is supplied by the authenticated caller, never by
  the payload. Connect as a non-superuser role so the policy is enforced as
  defense in depth.
- Secrets and bearer tokens must never be stored in synchronized tables.

## Deployment Responsibilities

The crate is a synchronization engine, not an authorization server. Production
operators are responsible for TLS, authorization policy, backups, monitoring,
gateway rate limits, secret rotation, and incident response. Apply schema
migrations with a dedicated DDL role (`LOOMABASE_MIGRATE_ONLY=true`), then run
with a non-superuser DML-only role and `LOOMABASE_SKIP_SCHEMA_INIT=true` so RLS
is enforced.

The reference `loomabase-server` applies body, concurrency, request, statement,
lock, and pool limits. It verifies signed `Authorization: Bearer` JWTs: Supabase
asymmetric JWKS (`RS256`, `ES256`, or `EdDSA` with trusted `kid`), `RS256`
against a configured RSA public key, or `HS256` against a shared secret. Each
mode restricts accepted algorithms and can require audience/issuer. Without a
verifier the binary fails closed; insecure header authentication requires the
explicit development-only `LOOMABASE_ALLOW_INSECURE_HEADERS=true`.

Supabase authorization defaults to trusted `app_metadata` claims. Do not move
tenant or table authorization into user-editable `user_metadata`.

## Automated Checks

CI runs formatting, Clippy with warnings denied, unit and integration tests,
`cargo audit`, and `cargo deny`. Dependabot checks Rust and GitHub Actions
dependencies weekly.
