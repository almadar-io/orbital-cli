//! `orb` runs the native GUI (window, Layer 2 render probe, screenshots) as the
//! `orb-desktop` program (`orbital_core::desktop_protocol`); it links no GUI.

use orbital_core::client::render_probe::{self, GameCanvasOutcome, RenderTarget};
use orbital_core::desktop_protocol::{self, DesktopReply, DesktopRequest};
use orbital_core::render_report::RenderReport;
use orbital_core::OrbitalSchema;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn failure(f: orbital_core::shell_protocol::ShellFailure) -> anyhow::Error {
    anyhow::anyhow!("{f}")
}

fn locate() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let user_store = orbital_core::behavior_packages::user_store_dir().unwrap_or_else(|| cwd.clone());
    orbital_core::shell_protocol::find_program(
        &desktop_protocol::program(),
        super::shell_programs::own_dir().as_deref(),
        &cwd,
        &user_store,
    )
    .map_err(failure)
}

fn files(reply: DesktopReply) -> anyhow::Result<Vec<PathBuf>> {
    match reply {
        DesktopReply::Files(paths) => Ok(paths),
        other => anyhow::bail!("orb-desktop answered {other:?} where files were expected"),
    }
}

/// The renderer `orb` verifies and screenshots through.
pub(crate) const RENDERER: &str = "dioxus";

/// The desktop program's renderer as a Layer 2 probe target: its coverage
/// lists, and game canvases painted by the program.
pub(crate) struct RemoteTarget {
    binary: PathBuf,
    supported: HashSet<String>,
    stubbed: HashSet<String>,
}

impl RenderTarget for RemoteTarget {
    fn supports(&self, pattern: &str) -> bool {
        self.supported.contains(pattern)
    }

    fn is_stub(&self, pattern: &str) -> bool {
        self.stubbed.contains(pattern)
    }

    fn probe_game_canvas(&self, pattern: &str, props: &orbital_core::Value) -> GameCanvasOutcome {
        let request = DesktopRequest::PaintCanvas { renderer: RENDERER.to_string(), pattern: pattern.to_string(), props: props.clone() };
        match desktop_protocol::request(&self.binary, request) {
            Ok(DesktopReply::Canvas(outcome)) => outcome,
            Ok(other) => GameCanvasOutcome::Fail(format!("`{pattern}`: orb-desktop answered {other:?} to a canvas paint")),
            Err(f) => GameCanvasOutcome::Fail(format!("`{pattern}`: {f}")),
        }
    }
}

pub(crate) fn target() -> anyhow::Result<RemoteTarget> {
    let binary = locate()?;
    match desktop_protocol::request(&binary, DesktopRequest::Coverage { renderer: RENDERER.to_string() }).map_err(failure)? {
        DesktopReply::Coverage { supported, stubbed } => Ok(RemoteTarget {
            binary,
            supported: supported.into_iter().collect(),
            stubbed: stubbed.into_iter().collect(),
        }),
        other => anyhow::bail!("orb-desktop answered {other:?} to a coverage request"),
    }
}

/// Layer 2: the client render contract (`orbital_core::client::render_probe`)
/// against the desktop renderer's coverage through the in-process kernel.
pub(crate) fn probe(item: &str, schema: OrbitalSchema, traits: Option<&[String]>, over_http: bool) -> anyhow::Result<RenderReport> {
    if over_http {
        anyhow::bail!("the HTTP desktop probe is temporarily unavailable: the legacy server is archived; the native std-http-server probe integration remains to be completed");
    }
    let target = target()?;
    let report = render_probe::probe(item, schema, traits, &target);
    report.map_err(|e| anyhow::anyhow!(e))
}

pub(crate) fn screenshot(item: &str, schema: OrbitalSchema, out_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let request = DesktopRequest::Screenshot { renderer: RENDERER.to_string(), item: item.to_string(), schema: Box::new(schema), out_dir: out_dir.to_path_buf() };
    files(desktop_protocol::request(&locate()?, request).map_err(failure)?)
}

pub(crate) fn frames(item: &str, schema: OrbitalSchema, out_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let request = DesktopRequest::Frames { renderer: RENDERER.to_string(), item: item.to_string(), schema: Box::new(schema), out_dir: out_dir.to_path_buf() };
    files(desktop_protocol::request(&locate()?, request).map_err(failure)?)
}

