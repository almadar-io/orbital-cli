<div align="center">

# orbital-cli

**The `orb` binary: the command line for the Almadar language, aggregating the lowerer, compiler, kernel and shells.**

[![CI](https://github.com/almadar-io/orbital-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/almadar-io/orbital-cli/actions/workflows/ci.yml) [![License: BUSL-1.1](https://img.shields.io/badge/license-BUSL--1.1-blue)](LICENSE)

</div>

<p align="center"><sub>Part of <a href="https://github.com/almadar-io/orb"><b>Orb</b></a>: the language where the rule is the program. <a href="https://orb.almadar.io">orb.almadar.io</a></sub></p>

`orbital-cli` builds two binaries, `orb` and `orbital`. Most `orb` commands are not written in Rust: `orb <command>` resolves an installed std behavior in the `cli` topic that declares the hook `cli-<command>`, boots it on the kernel and prints its answer. The one native subcommand is `orb run <program>`. Code generation (`--shell`) runs each shell as a separate `orb-shell-<name>` program.

## Where this fits

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/almadar-io/orb/main/static/img/readme/architecture/orbital-cli-dark.svg">
  <img src="https://raw.githubusercontent.com/almadar-io/orb/main/static/img/readme/architecture/orbital-cli-light.svg" width="100%" alt="Where orbital-cli sits in the Almadar architecture">
</picture>

| Upstream (this repo depends on) | Downstream (depends on this repo) |
|---|---|
| [orbital-core](https://github.com/almadar-io/orbital-core): kernel, evaluator, behavior-package and language-data readers | [orb](https://github.com/almadar-io/orb): npm and release distribution of the `orb` binary |
| [orbital-lolo](https://github.com/almadar-io/orbital-lolo): `.lolo` to `.orb` | |
| [orbital-compiler](https://github.com/almadar-io/orbital-compiler): validate and generate | |
| [orbital-shell-rust](https://github.com/almadar-io/orbital-shell-rust): the native host that runs command programs | |

The other shells ([typescript](https://github.com/almadar-io/orbital-shell-typescript), [python](https://github.com/almadar-io/orbital-shell-python), [ios](https://github.com/almadar-io/orbital-shell-ios), [android](https://github.com/almadar-io/orbital-shell-android), [os](https://github.com/almadar-io/orbital-shell-os)) are not linked: `orb` finds each as an `orb-shell-<name>` program, next to the `orb` executable, in the working directory or in the user store, and refuses one whose IR range does not accept this orb's.

## Quick start

Install `orb` from [orb](https://github.com/almadar-io/orb), then:

```bash
orb --version        # release and the language versions this orb reads
orb --help
orb run <program.lolo>
```

To build it from this repo (needs the language data installed, see Development):

```bash
cargo build --bin orb
```

`orb` reads the language data (`@almadar/core`, `@almadar/std`, the language packs) from installed packages and reports a typed error when it is missing. `orb update` installs and updates them; when a newer orb is in the user store, a run is handed off to it.

## What's inside

| Path | Role |
|---|---|
| `src/bin/orb.rs`, `src/bin/orbital.rs` | The two binaries; both call `orbital_cli::run()` |
| `src/lib.rs` | Argument parsing (`clap`), command resolution, the `--shell` targets (`typescript`, `python`, `rust`, `android`, `ios`, `chrome-extension`, `os`) |
| `src/commands/` | `program_command` (installed-behavior commands), `program_compile`, `program_host` (the native host of the language's server effects), `program_inspect`, `dev` |
| `src/reflect/` | `describe` and `index`: reflection over behaviors |
| `src/utils/` | `bootstrap` (the `orb update` repair path), `shell_programs` (locating and running shells), `terminal` (logging flags, spinners, timings), validation, i18n, detection |
| `tests/` | CLI integration tests (emit, validate diagnostics, source maps, catalogs) and `fixtures/` |

## Development

The crate reads the Almadar language data (patterns, operators, themes, vocabularies) from installed npm packages, so install those first (this is the CI recipe; the toolchain is pinned to Rust 1.93.0 in `rust-toolchain.toml`):

```bash
npm install --no-save --no-package-lock @almadar/core @almadar/std @almadar/lolo-ar @almadar/lolo-sl
cargo test
```

Its `orbital-*` dependencies are git dependencies on the `main` branch of their own repos.

Open gaps are tracked in [Almadar_Gaps.md](Almadar_Gaps.md). Work on `main`; never create a feature branch. Inside the monorepo this repo is the submodule `orbital-rust/crates/orbital-cli`; work from `orbital-rust` with `cargo test -p orbital-cli`.

## Docs

- [Orb documentation](https://orb.almadar.io/docs): the language, the `orb` CLI and how the pieces fit

## License

[Business Source License 1.1](LICENSE): free for non-commercial use and internal evaluation; commercial production use needs a licence from Almadar FZE. Converts to Apache-2.0 on 2030-02-01.
