<!-- Gap ledger for this repo: the source of truth for its open gaps. -->
# orbital-cli — open gaps

Every open gap this repo owns lives here. This file is the source of truth; the monorepo's `docs/Almadar_Gaps.md` only rolls it up.

- **One entry per gap:** `- **<code>** — <what is wrong and where>. <owning package> [mechanical|architectural] — <evidence, prevention rung>`.
- **Codes:** new gaps use this repo's prefix `G-ORBITAL-CLI-`. Take the "Next code" below, then bump it in the same edit. Codes are never reused or renamed.
- **Close by deleting.** Remove the entry in the same commit as the fix.

Next code: `G-ORBITAL-CLI-004`

- **G-ORBITAL-CLI-003** [mechanical] — `orb-dev signatures` fails inside a workspace package: run from `packages/almadar-std` or `packages/almadar-behaviors` it answers `os/read-file: …/node_modules/@almadar/core/package.json resolves to …/orbital-rust/crates/orbital-shell-typescript/core/package.json, outside the grant`, because pnpm links `@almadar/core` to the workspace and the command's file grant covers only the package's own tree. `orb-dev reference` from the same folders works, and `orb-dev signatures` from the monorepo root works, so only the signatures command's grant differs. Seen 2026-11-12. Reproduce: `cd packages/almadar-std && orb-dev signatures`. Fix: the signatures command reads the installed core through the same resolution `reference` uses (follow the package link, granted as the resolved root). Prevention rung: none (tooling). Severity: medium. Root cause: one-off.
- **G-ORBITAL-CLI-001** [mechanical] — `orb serve <program>` scaffolds a whole app (~100 MB: kernel, packages, lockfile) under `<cwd>/.orbital/serve/<name>` per program and never removes or reuses it across programs, so a batch that serves many programs fills the disk (the H4 parity gate reached ~336 GB on 2026-11-06 before the gate learned to delete it per pair). Owner: orbital-cli serve. Fix: share one scaffold (the program is the only per-run file) or remove the scaffold on exit. Prevention rung: none (resource hygiene). Severity: medium. Root cause: one-off.
