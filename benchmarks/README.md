# Public Benchmark Methodology

Loomabase benchmark results must be reproducible and must not compare unlike
durability or conflict semantics.

## Local Core Baseline

```bash
cargo run --release --bin loomabase-bench
LOOMABASE_BENCH_OPERATIONS=1000000 cargo run --release --bin loomabase-bench
```

The command emits machine-readable JSON. It measures the deterministic
in-memory reference merge, not PostgreSQL or HTTP throughput.

## HTTP/PostgreSQL Load

Use `ops/k6-sync.js` against an isolated deployment. Record:

- exact Loomabase commit and container digest;
- PostgreSQL version, instance shape, storage, and durability settings;
- client VUs, payload distribution, data set size, and tenant count;
- p50/p95/p99 latency, throughput, errors, CPU, memory, and database I/O.

## Competitor Comparisons

ElectricSQL, PowerSync, SQLite Sync, and Loomabase differ in topology,
licensing, conflict granularity, and hosted components. A published comparison
must use each project's documented production configuration, equivalent
durability, the same application workload, and unmodified results. Keep raw
scripts and output in a dated result directory. Do not publish comparative
claims until the maintainers of each tested project can reproduce them.

