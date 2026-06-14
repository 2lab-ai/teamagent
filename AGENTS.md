# llmux — agent guide

Contract: `.prd/01-spec.md` (what) + `.prd/02-architecture.md` (how). Read both before
non-trivial changes.

## Architecture rules

- **Scheduler decisions are pure functions over snapshots.** `scheduler/select.rs` does no
  IO, reads no clocks, takes `(&PoolSnapshot, &SelectParams, now)` and returns a `Decision`.
  All impure work (locks, CAS commit, timers) stays in `scheduler/mod.rs`.
- **State/runtime separation** (herdr pattern): `PoolState` mutations are sync and IO-free
  behind a std `RwLock`; never hold the lock across an `.await`.
- **No `unwrap()`/`expect()` in production paths.** Errors are typed (`thiserror`) and
  propagate; `expect` is acceptable only for invariants that cannot fail (e.g. poisoned-lock
  policy, documented at the call site) and in tests.
- Never log or print raw credentials — route through `proxy::logging::mask_credentials`.

## Conventions

- Conventional commits, lowercase, no emojis, no AI co-author lines.
- `just check` (fmt + clippy -D warnings + tests) must pass before every commit.
- Config writes are read-merge-write (`config::update`) — never load/edit/save around a
  running server.
