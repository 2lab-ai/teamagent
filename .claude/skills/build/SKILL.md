---
name: build
description: Use for normal llmux dev iteration when the user says "빌드", "빌드해줘", "build", "build and deploy locally", or wants to compile + run a change locally and save it. Builds release, hot-deploys the binary to the local daemon, commits, and pushes to a feature branch (never master).
---

# build (빌드) — local build + branch push

Normal dev-loop runbook: compile a release binary, hot-deploy it to the running local
daemon, commit, and push to a **feature branch** (never master — that triggers a public
preview). For preview/stable channels use the `deploy` / `release` skills instead.

Shared mechanics: `.claude/skills/_shared/cd-reference.md` (procedure A = hot-deploy).

## Steps

1. **Branch, not master.** `git rev-parse --abbrev-ref HEAD`. If on `master`/`main`, ask the
   user for a branch name (or propose `feat/<short-desc>`) and `git switch -c <branch>`.
   *(Decision point: branch name needs the user.)*
2. **Gate.** `just check` (fmt + clippy -D warnings + tests) must be green before committing.
   Fix failures; do not commit red.
3. **Build + hot-deploy locally** — procedure A in the shared reference (`cargo build
   --release --locked`; `rm` the read-only Cellar file; `cp`; `chmod 755`;
   `llmux restart`). Autonomous — this is the whole point of `build`.
4. **Smoke check.** `/opt/homebrew/bin/llmux status` — daemon back up on the new binary,
   `in_flight` was 0 before restart.
5. **Commit.** Conventional, lowercase, no emoji, no AI co-author line (AGENTS.md):
   `git add -A && git commit -m "<type>: <summary>"`.
6. **Push to the branch** (never master): `git push -u origin <branch>`. On a stale `ghs_`
   token, use the `gh auth token` fallback (shared reference).
7. **Report** branch, commit, and the deployed local version.

## Common mistakes

- `cp` over the read-only Cellar binary → EACCES. `rm -f "$(readlink -f
  /opt/homebrew/bin/llmux)"` first (procedure A).
- Pushing to master from `build` — that fires the preview pipeline. Branch only.
- Committing with `just check` red, or with an emoji / AI co-author line.
- A later `brew upgrade` will overwrite this hot-deployed `dev dev` binary — expected;
  re-run `build` to restore.
