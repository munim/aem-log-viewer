# Release verification

Issue #25. Production `aemlog` stays live-AIO only. Automated checks use anonymized fixtures and a fake `aio` binary; they never need Adobe credentials.

Do not commit private AEM logs, credentials, local absolute paths, generated benchmark output, placeholders, or unfinished implementations.

Supported targets: macOS and Linux. Supported Rust: 1.85 (MSRV) and current stable. CI runs both on `macos-latest` and `ubuntu-latest`.

## Static checks and tests

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release --bins
```

Observed on `aarch64-apple-darwin` with `rustc 1.98.0` (2026-08-27):

- `cargo fmt --all -- --check`: pass
- `cargo clippy --all-targets -- -D warnings`: no issues found
- `cargo test --all-targets`: 258 passed
- `cargo build --release --bins`: finished `release` profile

## Installed CLI

Install into an isolated prefix. Do not install into a shared user Cargo home for this check.

```sh
PREFIX=$(mktemp -d)
cargo install --path . --root "$PREFIX" --force
"$PREFIX/bin/aemlog" --help
"$PREFIX/bin/aemlog"; echo "missing-args exit=$?"
"$PREFIX/bin/aemlog" --program-id p1 --environment-id e1 --service author; echo "non-tty exit=$?"
```

Observed:

- `cargo install --path . --root "$PREFIX" --force` installed `aemlog` (and the non-public `aemlog-perf` harness binary)
- `"$PREFIX/bin/aemlog" --help` exit 0. Documents `--program-id`, `--environment-id`, `--service`, `--level`, `--ims-context`, `--config`, `--timezone`, `--json`, `--raw-sample`, author/publish, ERROR, TTY, status 1, status 2, `~/aemlog.toml`, never merged, `version = 1`. No `--file`, `--input`, offline, or replay flags.
- Missing required arguments: exit 2. Stderr lists `--program-id`, `--environment-id`, `--service`.
- Redirected stdout without `--json`: exit 2. Stderr: `stdout is not a terminal. TUI mode requires a TTY; use --json for redirected or piped output.`

## Schema fixtures

```sh
cargo test --test schema
```

Observed: 4 passed. Every file under `tests/fixtures/ndjson/valid/` conforms to `schema/aemlog-v1.json`. Invalid fixtures fail for the intended reason:

- `additional_property.json`: additionalProperties (`extra`)
- `missing_required.json`: required (`levels`)
- `non_finite_rate.json`: maximum (`fast_rate`)
- `unknown_type.json`: type const (`/type`)
- `wrong_field_type.json`: type (`group_id`)

## Fake-AIO live smoke

```sh
cargo test --test live fake_aio
```

Observed: 2 passed.

`fake_aio_emits_ndjson_session_groups_and_unexpected_end` starts one Author session, emits `session_started`, grouped ERROR `group_created`/`group_updated`, then NDJSON `source_ended` with aio status 0. The process still exits 1 with `source ended unexpectedly` because a tail ending is unexpected.

`fake_aio_author_session_merges_updates_and_redacts_unexpected_end` exercises a merge (`group_merged` survivor 1, removed 2, count 6, finite `fast_rate`/`baseline_rate`) and redacted evidence (`[REDACTED:email]` in the sample, `[REDACTED:token]` and `[REDACTED:email]` on `source_ended.stderr`). Process exit remains 1.

## PTY smoke and terminal restoration

```sh
cargo test --test live pty_
```

Observed: 7 passed.

- Volume list, detail, search, New/Increasing/Muted, mute/unmute, help, resize gate, then `q` exit 0 with alternate screen left, cursor shown, cooked ECHO/ICANON, and a subsequent `stty -a` usable.
- Unexpected aio exit freezes, Enter acknowledges, exit 1, terminal restored.
- Ctrl-C and SIGTERM restore the tty and reap the aio process group.
- `q` on a held aio group restores and reaps.
- Missing aio after partial TUI startup leaves the tty cooked.

## Performance harness

Production CLI is not used. See `docs/performance.md`.

```sh
cargo run --release --bin aemlog-perf
```

Observed on the same workstation (thresholds not relaxed):

```
aemlog perf harness (release internals; production CLI stays live-AIO only)
mixed-guard: events/s=3329 bytes/s=426233 peak_rss=10862592 allocations=n/a groups=5 overflow=0 checksum=12755843614500604807 selected=26 elapsed_ms=8
sustain-60s: events/s=103903 bytes/s=200649246 peak_rss=11091968 allocations=n/a groups=6 overflow=0 checksum=9886520181385369789 selected=6234168 elapsed_ms=60000
adversarial-100k: events/s=137602 bytes/s=14417681 peak_rss=185122816 allocations=n/a groups=100000 overflow=32 checksum=2207432124103473872 selected=100032 elapsed_ms=726
full-day: skipped (set AEMLOG_DAY_LOG to a private, uncommitted log)
```

- sustain-60s ≥ 100,000 events/s, selected equals framed events
- adversarial 100,000 normal groups, overflow 32, peak RSS 185,122,816 bytes (< 512 MiB)
- full-day log not present; skip is expected without a private uncommitted log

Small automated guard:

```sh
cargo test --lib app::perf::tests::mixed_guard_is_deterministic
```

Observed: pass.

## Reproduce on Linux

Same commands. CI matrix is `ubuntu-latest` with Rust 1.85 and stable. Fake-AIO and PTY tests are Unix-only and run there.
