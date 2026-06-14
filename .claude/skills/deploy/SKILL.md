---
name: deploy
description: Use when the user says "배포", "배포해줘", "deploy", or "ship a preview" for llmux. Pushes to master, lets CI publish a preview prerelease, refreshes the llmux-preview brew formula, verifies brew actually updated, then hot-deploys that build locally and restarts.
---

# deploy (배포) — preview channel

Ship the current work as a **preview**: master → CI preview build → brew `llmux-preview`
→ local. For a local-only dev loop use `build`; for a stable release use `release`.

Shared mechanics: `.claude/skills/_shared/cd-reference.md` (procedure A = hot-deploy,
procedure B = publish+verify brew).

## Steps

1. **Pre-flight.** `just check` green; know what's uncommitted (`git status`). If dirty, ask
   whether to commit (and the message) or stash. *(Decision point.)*
2. **Land on master.** If on a branch, merge/fast-forward into master per repo norm.
   **Confirm with the user before pushing master** (public preview channel), then
   `git push origin master` (token fallback if needed).
3. **Watch the preview build.**
   ```bash
   rid=$(gh run list --repo 2lab-ai/llmux --workflow preview.yml -L1 --json databaseId -q '.[0].databaseId')
   gh run watch --repo 2lab-ai/llmux "$rid" --exit-status
   ```
   Success publishes prerelease `preview-<YYYY-MM-DD-HHMM>-<sha12>`.
4. **Confirm the prerelease.** `gh release list --repo 2lab-ai/llmux -L5` (preview is a
   *prerelease* — `gh release view` without a tag shows the stable one, not this). Note the
   new `preview-*` tag.
5. **Publish + verify brew** — procedure B with `formula=llmux-preview`. Dispatch the tap
   `bump.yml`, watch it, `brew update && brew upgrade llmux-preview`, confirm the brew
   version (`YYYY.MM.DD.HHMM`) matches the new preview tag's timestamp.
6. **Hot-deploy + restart.** The brew build is already in the Cellar after upgrade, so:
   `/opt/homebrew/bin/llmux restart`. Verify `--version` reports `(preview <id>)`.
7. **Verify.** `/opt/homebrew/bin/llmux status` — daemon on the new preview build.
8. **Report** preview tag, brew version, running daemon version.

## Common mistakes

- Assuming the tap auto-bumps — it does not; dispatch `bump.yml` or brew stays stale.
- `gh release view` hiding the prerelease (shows stable) — use `gh release list`.
- "Already up-to-date" after a bump → `brew update` first, re-check `brew info`.
- CI latency — poll with `gh run watch`, don't assume instant.
