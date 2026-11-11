<!-- Gap ledger for this repo: the source of truth for its open gaps. -->
# orbital-cli — open gaps

Every open gap this repo owns lives here. This file is the source of truth; the monorepo's `docs/Almadar_Gaps.md` only rolls it up.

- **One entry per gap:** `- **<code>** — <what is wrong and where>. <owning package> [mechanical|architectural] — <evidence, prevention rung>`.
- **Codes:** new gaps use this repo's prefix `G-ORBITAL-CLI-`. Take the "Next code" below, then bump it in the same edit. Codes are never reused or renamed.
- **Close by deleting.** Remove the entry in the same commit as the fix.

Next code: `G-ORBITAL-CLI-003`

- **G-ORBITAL-CLI-001** [mechanical] — `orb serve <program>` scaffolds a whole app (~100 MB: kernel, packages, lockfile) under `<cwd>/.orbital/serve/<name>` per program and never removes or reuses it across programs, so a batch that serves many programs fills the disk (the H4 parity gate reached ~336 GB on 2026-11-06 before the gate learned to delete it per pair). Owner: orbital-cli serve. Fix: share one scaffold (the program is the only per-run file) or remove the scaffold on exit. Prevention rung: none (resource hygiene). Severity: medium. Root cause: one-off.
