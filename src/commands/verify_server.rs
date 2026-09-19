//! Server↔client render parity (`orb verify --server`).
//!
//! The default L2 probe drives the in-process `KernelBridge` (server-kernel + client fused,
//! no HTTP). This boots `orbital-server` (the same lib the emitted `server.rs` wraps) on a
//! free localhost port and drives the SAME L2 render probe through a `SyncServerBridge` over
//! real HTTP — verifying the server's routes + serialization AND the client render together,
//! the way the TS verifier boots the compiled server and drives the client.
//!
//! Both sides use the ONE kernel and the ONE seeder, so a divergence is *caught*, not
//! introduced: the server is seeded via `OrbitalServerRuntime::seed_sample` (→ the canonical
//! `orbital_core::runtime::seed_sample_data`), identical to the in-process probe.

use orbital_client::bridge::ServerBridge;
use orbital_client::http::{BridgeError, SyncServerBridge};
use orbital_client::render_probe::{probe_schema_with_scoped, RenderReport};
use orbital_core::{EventResponse, OrbitalSchema, Value};
use orbital_server::{serve_background, BackgroundServer, InMemoryAdapter, OrbitalServerRuntime};
use std::sync::Arc;

/// A `ServerBridge` over a live in-process HTTP server that owns the server's lifecycle —
/// the server shuts down (graceful) when this bridge is dropped at the end of an orbital walk.
struct HttpServerBridge {
    inner: SyncServerBridge,
    // Kept alive for the duration of the probe; `Drop` shuts the server down.
    _server: BackgroundServer,
}

impl ServerBridge for HttpServerBridge {
    fn send_event_with_options(
        &self,
        orbital: &str,
        event: &str,
        entity_id: Option<&str>,
        trait_name: Option<&str>,
        payload: Value,
    ) -> Result<EventResponse, BridgeError> {
        self.inner
            .send_event_with_options(orbital, event, entity_id, trait_name, payload)
    }
}

/// Boot a fresh, seeded `orbital-server` for `schema`, seeded identically to the
/// in-process `KernelBridge` probe (same default-persona binding, same roster count) —
/// without it the HTTP bridge evaluates every `@read`/guard anonymously while the
/// in-process probe sees the bound viewer, and an ownership-scoped list parities as
/// empty here but populated there.
fn boot_http_server(schema: &OrbitalSchema) -> anyhow::Result<BackgroundServer> {
    let adapter = Arc::new(InMemoryAdapter::new());
    let runtime = Arc::new(OrbitalServerRuntime::new(adapter));
    runtime
        .register_schema(schema.clone())
        .map_err(|e| anyhow::anyhow!("register_schema: {e}"))?;
    match orbital_core::runtime::default_probe_persona(schema) {
        Some(user) => {
            let id = user
                .get_path("id")
                .and_then(|v| v.as_str().map(str::to_string));
            runtime.set_current_user(user);
            runtime.seed_sample_as(
                schema,
                orbital_core::runtime::DEFAULT_IDENTITY_ROSTER_COUNT,
                id,
            );
        }
        None => runtime.seed_sample(schema, 1),
    }

    Ok(serve_background(runtime)?)
}

/// Boot a fresh, seeded `orbital-server` for `schema` and return a bridge pointed at it.
fn boot_http_bridge(schema: &OrbitalSchema) -> anyhow::Result<Arc<dyn ServerBridge>> {
    let server = boot_http_server(schema)?;
    let inner = SyncServerBridge::new(server.base_url());
    Ok(Arc::new(HttpServerBridge {
        inner,
        _server: server,
    }))
}

/// Run the L2 render contract driving a live HTTP server per orbital — server↔client parity,
/// restricted to `traits` when given (`--trait`) — see `render_probe::probe_schema_scoped`.
pub(crate) fn probe_over_http(
    item: &str,
    schema: OrbitalSchema,
    traits: Option<&[String]>,
) -> anyhow::Result<RenderReport> {
    probe_schema_with_scoped(item, schema, traits, |s, _idx| boot_http_bridge(s))
}

/// §156 fixture pass: every recorded `packages/almadar-core/fixtures/events-wire/<case>/
/// request.json` POSTed through the SAME `SyncServerBridge`/HTTP path
/// [`probe_over_http`] drives, against a fresh server booted for `schema`. This is a
/// WIRE-SHAPE gate ONLY — it asserts that the server accepts every fixture's request
/// body and answers with something that deserializes as `orbital_core::EventResponse`.
/// It does NOT compare values against the fixture's recorded `response.json`: the
/// fixtures were recorded against a synthetic orbital/schema (`NoteBoard` /
/// `NoteCatalog` / `NotePersistor` / `InlineButtonRender3`), and while several of those
/// trait names happen to coincide with `std-notes.orb`'s `NoteOrbital` (so some
/// fixtures also transition for real), that coincidence is not what this checks.
pub fn run_fixture_pass(orbital_name: &str, schema: &OrbitalSchema) -> anyhow::Result<FixturePassReport> {
    let fixtures_dir = events_wire_fixtures_dir();
    if !fixtures_dir.exists() {
        return Ok(FixturePassReport { skipped: true, ..Default::default() });
    }

    let server = boot_http_server(schema)?;
    let url = format!("{}/api/orbitals/{orbital_name}/events", server.base_url());

    let mut report = FixturePassReport::default();
    for entry in std::fs::read_dir(&fixtures_dir)?.flatten() {
        let request_path = entry.path().join("request.json");
        if !request_path.exists() {
            continue;
        }
        let case = entry.file_name().to_string_lossy().to_string();
        let request_body: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&request_path)?)?;

        let response = ureq::post(&url).send_json(request_body);
        let response = match response {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r, // a 4xx/5xx still carries a JSON body to shape-check
            Err(e) => {
                report.failures.push(format!("{case}: request failed: {e}"));
                continue;
            }
        };
        let body_text = response
            .into_string()
            .map_err(|e| anyhow::anyhow!("{case}: reading response body: {e}"))?;
        match serde_json::from_str::<EventResponse>(&body_text) {
            Ok(_) => report.checked.push(case),
            Err(e) => report.failures.push(format!("{case}: reply did not deserialize as EventResponse: {e}\nbody: {body_text}")),
        }
    }
    Ok(report)
}

fn events_wire_fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../packages/almadar-core/fixtures/events-wire")
}

#[derive(Debug, Default)]
pub struct FixturePassReport {
    pub skipped: bool,
    pub checked: Vec<String>,
    pub failures: Vec<String>,
}

impl FixturePassReport {
    pub fn is_green(&self) -> bool {
        self.skipped || self.failures.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_std_notes() -> OrbitalSchema {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-schemas/std-notes.orb");
        let raw = std::fs::read_to_string(&path).expect("test-schemas/std-notes.orb must exist");
        orbital_core::schema::parse_schema(&raw).expect("std-notes.orb must parse")
    }

    #[test]
    fn fixture_pass_accepts_every_recorded_request_shape() {
        let schema = load_std_notes();
        let orbital_name = schema
            .orbitals
            .first()
            .map(|o| o.name.clone())
            .expect("std-notes.orb has an orbital");

        let report = run_fixture_pass(&orbital_name, &schema).expect("fixture pass must run");
        if report.skipped {
            eprintln!("verify_server::fixture_pass_accepts_every_recorded_request_shape: fixtures dir not found — skipping");
            return;
        }
        assert!(
            report.is_green(),
            "fixture pass had shape failures: {:#?}",
            report.failures
        );
        assert!(!report.checked.is_empty(), "expected at least one fixture to be checked");
    }
}
