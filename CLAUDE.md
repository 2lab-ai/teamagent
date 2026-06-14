# CLAUDE.md

Read **AGENTS.md** first — it is the canonical guide (architecture rules; conventions:
conventional commits lowercase, no emojis, no AI co-author lines; `just check` green before
every commit). Product contract: `.prd/01-spec.md` + `.prd/02-architecture.md`. Scheduler
design history: `.prd/06-scheduler-current.md`, `.prd/07-scheduler-research.md`.

## Runbooks

Three operational skills live in `.claude/skills/` (shared mechanics in
`.claude/skills/_shared/cd-reference.md`). Invoke by intent:

- **build** (빌드) — local build → hot-deploy to the local daemon → commit → push to a
  **feature branch** (never master).
- **deploy** (배포 / "배포해줘") — push to **master** → CI **preview** prerelease → refresh
  `llmux-preview` brew formula → verify → hot-deploy + restart.
- **release** (릴리즈 / "릴리즈해줘") — bump version → tag `v*` → CI **stable** release →
  refresh `llmux` brew formula → verify → hot-deploy + restart → `llmux status`
  (client + server).

## Load-bearing facts (don't relearn the hard way)

- **A stable release requires a version bump.** The release workflow fails if the `v*` tag
  ≠ `Cargo.toml` version, and the last version's tag already exists. Pick the next version
  *with the user*.
- **The brew tap does not auto-bump.** `2lab-ai/homebrew-tap`'s `bump.yml` runs on
  `workflow_dispatch` or a 6h schedule. For a prompt brew update, `gh workflow run bump.yml
  --repo 2lab-ai/homebrew-tap`, wait for it, then `brew update && brew upgrade`.
- **Local hot-deploy gotcha.** The Cellar binary is read-only (`r-xr-xr-x`), so `cp` over it
  fails — `rm -f "$(readlink -f /opt/homebrew/bin/llmux)"` first, then `cp`, `chmod 755`,
  then `llmux restart`. A later `brew upgrade` overwrites a hot-deployed dev binary.
- **Push fallback** if the remote's `ghs_` token is stale:
  `git push "https://x-access-token:$(gh auth token)@github.com/2lab-ai/llmux" <ref>`.
- The `/api/oauth/usage` endpoint returns **percentages (0–100)**, not fractions — each
  evidence source has a fixed scale (see `src/scheduler/usage.rs`).
