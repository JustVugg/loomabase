# Independent Security Audit Scope

An independent audit cannot be performed by Loomabase's implementation author.
This document defines a concrete engagement for an external security team.

## In Scope

- untrusted `SyncPayload` parsing, limits, protocol compatibility, and schema
  fingerprints;
- CRDT merge atomicity, cursor capabilities, epoch rotation, and anti-entropy;
- SQL generation, identifier validation, parameter binding, migrations, and
  Row-Level Security;
- JWT/Supabase JWKS verification, algorithm confusion, claim mapping, and
  authorization;
- HTTP limits, timeout behavior, metrics leakage, and error mapping;
- SQLite trigger/application transaction boundaries;
- C ABI pointer ownership, panic containment, thread safety, and fuzzability;
- Docker/runtime hardening and operational runbooks.

## Required Deliverables

- threat model and trust-boundary review;
- reproducible findings with severity and affected commit;
- verification of remediations;
- public summary and private full report;
- recommended fuzzing corpus and penetration-test regression cases.

## Release Gate

No release should claim an independent audit until a named external auditor
has completed these deliverables against an immutable commit.

