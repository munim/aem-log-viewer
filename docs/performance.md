# Performance contracts

Issue #24. Production `aemlog` stays live-AIO only. This harness feeds parser and analyzer internals; it is not a public offline-input command.

Do not commit private AEM logs, extracted customer strings, benchmark output, or local absolute paths.

## One-command reproduction

```sh
cargo run --release --bin aemlog-perf
```

Reports events/second, bytes/second, peak RSS, allocations where measurable, group count, overflow count, and count checksum. Thresholds are not relaxed. Failure prints `BLOCKED …` evidence.

Optional full-day log (kept outside the repo):

```sh
AEMLOG_DAY_LOG=/path/to/uncommitted.log AEMLOG_DAY_EVENTS=1490000 cargo run --release --bin aemlog-perf
```

## Small automated guard

```sh
cargo test --lib app::perf::tests::mixed_guard_is_deterministic
```

Covers mixed framed events, exact known-group checksums, overflow routing, coalesced NDJSON, bounded `PIPE_QUEUE` backpressure, and input-to-visible-selection latency.

## Contracts

| Check | Threshold |
| --- | --- |
| Sustain mixed framed ingest 60s | ≥ 100,000 events/s, no dropped known counts |
| PTY/input-to-visible selection | < 100 ms |
| Full-day log (~1.49M events) | peak RSS < 128 MiB |
| 100,000 unique groups then overflow | peak RSS < 512 MiB |

If a check fails on the reference workstation, keep the exact report as blocking evidence. Do not lower the number.
