//! `orb` runs the native GUI (window, Layer 2 render probe, screenshots) as the
//! `orb-desktop` program (`orbital_core::desktop_protocol`); it links no GUI.

use orbital_core::client::render_probe::{GameCanvasOutcome, RenderTarget};
use orbital_core::desktop_protocol::{self, DesktopReply, DesktopRequest};
use std::collections::HashSet;
use std::path::PathBuf;

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


