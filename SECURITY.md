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
- `merge_crdt_states_with_security` and `app_with_config_and_security` run
  pluggable `SyncAuthorizer` and `SyncValidator` hooks before the LWW merge.
  Valid cells denied by these hooks are returned in `SyncPayload.rejections`
  and are not applied.
- When database audit is enabled, merge decisions are inserted into
  `loomabase_audit_log` in the same PostgreSQL transaction as the sync.
- Secrets and bearer tokens must never be stored in synchronized tables.

Conflict resolution is deliberately deterministic, not trust-based. A valid
authenticated writer can submit a valid newer CRDT cell and win the LWW order,
so applications must enforce write authorization and domain validation before
calling the merge. Malformed versions, schema mismatches, type mismatches,
oversized payloads, spoofed device attribution, cursor forgeries, excessive
Lamport advances, non-finite numbers, and equal CRDT versions with different
values are rejected before mutation. Valid cells denied by authorization or
business validation are structured rejections, not silent drops.

When exposing conflict decisions to users or operators, use
`explain::explain_lww` instead of reimplementing the ordering rules in an API or
UI layer.

## Integration Best Practices

Validate synchronized data at the application boundary:

- authorize each write against the authenticated user, tenant, table, row, and
  field;
- reject fields the caller cannot edit, even if the contract contains them;
- enforce domain rules such as string length, enum membership, numeric ranges,
  foreign-key ownership, and allowed state transitions;
- keep credentials, secrets, access tokens, API keys, and password material out
  of synchronized tables;
- use PostgreSQL constraints and forced RLS as defense in depth;
- keep database audit enabled for security-sensitive deployments, and monitor
  rejected authorization and validation outcomes.

For browser clients, expose sync endpoints with a strict CORS allowlist:

```http
Access-Control-Allow-Origin: https://app.example.com
Access-Control-Allow-Methods: POST, OPTIONS
Access-Control-Allow-Headers: Authorization, Content-Type
Access-Control-Max-Age: 600
Vary: Origin
```

Do not use wildcard origins for authenticated sync traffic. If cookies are used
instead of bearer tokens, require `Secure`, `SameSite=Lax` or
`SameSite=Strict`, and CSRF protection.

Content Security Policy is configured on browser-facing HTML, not inside the
CRDT merge. A typical web application should restrict sync egress:

```http
Content-Security-Policy: default-src 'self'; connect-src 'self' https://api.example.com https://*.supabase.co; object-src 'none'; base-uri 'none'; frame-ancestors 'none'
```

For JSON sync responses, set `Content-Type: application/json`,
`X-Content-Type-Options: nosniff`, `Cache-Control: no-store`,
`Referrer-Policy: no-referrer`, and HSTS at the TLS termination layer.

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
