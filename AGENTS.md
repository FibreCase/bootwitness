# Repository agent guide

## Scope and mission

These instructions apply to the entire repository.

`bootwitness` is a small reliability tool for monitoring Linux boot continuity.
The deployment target is Debian 13 on amd64 and arm64 with systemd. Development
and core logic tests must continue to work on macOS, but macOS must never
emulate or pretend to provide the real Linux continuity probe.

Read `README.md` before making changes that affect event semantics, persistence,
deployment, or operator-facing commands.

## Correctness invariants

- A changed Linux boot ID is the authoritative signal for `boot_changed`.
- A daemon restart under the same boot ID is `monitor_restarted` and must not be
  reported as an operating-system reboot.
- Never claim to know the exact time of an unobserved power loss. Preserve the
  evidence window from the last durable heartbeat to the estimated next boot.
- `graceful` means that the monitor observed a normal termination signal. It is
  evidence about the monitor, not proof that the operating system shut down
  cleanly.
- Startup event insertion and current-state replacement must remain in one
  SQLite transaction.
- Event fingerprints must remain idempotent so retries cannot duplicate a boot
  transition.
- Heartbeats are durability-sensitive. Do not weaken WAL or
  `synchronous=FULL` without documenting and testing the changed failure model.
- The real probe must read the kernel boot ID and `CLOCK_BOOTTIME` only on Linux.
- A failure in the monitor must be visible as an error; do not silently create a
  new baseline after database corruption or an incompatible schema.

## Source layout

- `src/platform/`: operating-system probes and platform gating.
- `src/storage/`: SQLite schema, state transitions, event history, and
  migrations.
- `src/monitor.rs`: daemon loop, signal handling, heartbeats, and process lock.
- `src/main.rs`: CLI parsing, output, and exit-code behavior.
- `tests/storage.rs`: platform-independent state-machine and durability tests.
- `packaging/systemd/`: production systemd unit.
- `packaging/debian/`: Debian installation helpers.

Keep policy and state-transition logic out of the CLI presentation layer.

## Rust and dependency policy

- Keep compatibility with Rust 1.85, the compiler shipped by Debian 13.
- Keep Rust edition 2021 unless a deliberate compatibility change is approved.
- Dependencies are pinned intentionally. Update `Cargo.toml` and `Cargo.lock`
  together and verify the minimum toolchain after dependency changes.
- Prefer synchronous, small, auditable code for this daemon. Do not introduce an
  async runtime unless the requirements actually need one.
- Propagate actionable errors with context. Panics and unchecked `unwrap` calls
  do not belong in production paths.
- Every `unsafe` block needs a local safety explanation.
- Preserve Linux-only code behind conditional compilation so macOS builds do not
  compile Linux APIs.

## Database changes

- Treat schema version 1 as persistent user data.
- Add explicit forward migrations for schema changes; never replace an existing
  schema destructively.
- Do not automatically delete event history.
- New event kinds need storage tests, human-readable CLI output, JSON output,
  and README documentation.
- Keep status/history readers compatible with a live WAL database.

## systemd and packaging

- The service must run without root privileges and retain
  `DynamicUser=yes`/`StateDirectory=bootwitness` unless a concrete requirement
  proves otherwise.
- Preserve systemd hardening unless a setting prevents a documented required
  operation.
- The daemon must not require network access.
- Keep the default database at
  `/var/lib/bootwitness/bootwitness.sqlite3`.
- Do not enable, stop, reboot, power off, or modify a real host as part of an
  automated test.
- Reboot and forced-power-loss acceptance tests require explicit operator
  approval and an appropriate disposable or maintenance-window machine.

## Required verification

Run before handing off changes:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all
```

When changing dependencies or compatibility-sensitive code, also run:

```bash
cargo +1.85.0 check --locked
cargo +1.85.0 test --locked --all
```

For Debian/systemd changes, run the Debian 13 GitHub Actions job or equivalent
container checks, including a release build and `systemd-analyze verify`. For
release changes, preserve native amd64/arm64 builds and static-link validation.

## Handoff expectations

Summarize behavior changes, list the checks actually run, and clearly distinguish
simulated coverage from real Debian reboot or power-loss validation.
