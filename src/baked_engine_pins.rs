//! Pins of the packages this orb was built against (core, std, language packs, shell programs). The verify engines retired.
//!
//! Source: installed-package identity (or workspace package.json fallback in
//! dev checkouts) for @almadar-io/runtime-verify-tool (both engines: the walk, and --compiled via its orbital-verify bin).
//!
//! Last synced: 2026-10-18
//! To regenerate: npx almadar-sync engine-pins (also runs as one step of `npx almadar-sync rust`)
//!
//! DO NOT EDIT MANUALLY - changes will be overwritten on next sync.

pub const BAKED_CORE: (&str, &str) = ("@almadar/core", "10.129.0");
pub const BAKED_STD_RANGE: (&str, &str) = ("@almadar/std", "^16");
pub const BAKED_LANGUAGE_PACKS: &[(&str, &str)] = &[("@almadar/lolo-ar", "1.3.0"), ("@almadar/lolo-sl", "1.3.0")];
pub const BAKED_PROGRAMS: &[(&str, &str)] = &[("@almadar/orb-shell-typescript", "^18"), ("@almadar/orb-desktop", "^18")];
