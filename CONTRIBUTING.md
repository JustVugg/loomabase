# Contributing to Loomabase

Loomabase accepts focused changes that preserve deterministic convergence,
transactional integrity, and backward-compatible protocol behavior.

## Development

Requirements:

- Stable Rust
- SQLite development support, or the bundled SQLite feature
- PostgreSQL 15 or newer for adapter integration tests

Run the local checks:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
LOOMABASE_TEST_DATABASE_URL=postgres://postgres:postgres@localhost/loomabase \
  cargo test --test postgres_integration
```

Every behavioral change must include tests. Changes to merge semantics must
demonstrate idempotence, commutativity, deterministic tie-breaking, and
convergence under reordered delivery.

## Pull Requests

- Keep changes scoped and explain protocol or schema compatibility impact.
- Document new public APIs and security assumptions.
- Do not add unsafe Rust.
- Do not weaken transaction boundaries, input validation, or CI checks.
- Use Conventional Commit prefixes where practical.

By contributing, you agree that your contribution is licensed under
Apache-2.0.

