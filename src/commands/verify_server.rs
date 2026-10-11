//! `orb verify --server`'s wire-shape gate: every recorded events-wire fixture
//! request round-trips through a live seeded `orbital-server`. The server-driven
//! L2 render probe itself runs in `orb-desktop` (`DesktopRequest::Probe { over_http }`).

use orbital_core::{EventResponse, OrbitalSchema};

/// §156 fixture pass: every recorded `@almadar/core` `fixtures/events-wire/<case>/
/// request.json` (baked into `orbital_core::fixtures`) POSTed through the SAME
/// `SyncServerBridge`/HTTP path [`probe_over_http`] drives, against a fresh server
/// booted for `schema`. This is a WIRE-SHAPE gate ONLY — it asserts that the server
/// accepts every fixture's request body and answers with something that deserializes
/// as `orbital_core::EventResponse`. It does NOT compare values against the fixture's
/// recorded `response.json`: the fixtures were recorded against a synthetic
/// orbital/schema (`NoteBoard` / `NoteCatalog` / `NotePersistor` /
/// `InlineButtonRender3`), and while several of those trait names happen to coincide
/// with `std-notes.orb`'s `NoteOrbital` (so some fixtures also transition for real),
/// that coincidence is not what this checks.
pub fn run_fixture_pass(
    orbital_name: &str,
    schema: &OrbitalSchema,
) -> anyhow::Result<FixturePassReport> {
    let server = orbital_server::serve_seeded(schema)?;
    let url = format!("{}/api/orbitals/{orbital_name}/events", server.base_url());

    let mut report = FixturePassReport::default();
    for fixture in orbital_core::fixtures::event_wire_cases() {
        let case = fixture.name.to_string();
        let request_body: serde_json::Value = serde_json::from_str(fixture.request())?;

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
            Err(e) => report.failures.push(format!(
                "{case}: reply did not deserialize as EventResponse: {e}\nbody: {body_text}"
            )),
        }
    }
    Ok(report)
}

#[derive(Debug, Default)]
pub struct FixturePassReport {
    pub checked: Vec<String>,
    pub failures: Vec<String>,
}

impl FixturePassReport {
    pub fn is_green(&self) -> bool {
        self.failures.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_std_notes() -> OrbitalSchema {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-schemas/std-notes.orb");
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
        assert!(
            report.is_green(),
            "fixture pass had shape failures: {:#?}",
            report.failures
        );
        assert!(
            !report.checked.is_empty(),
            "expected at least one fixture to be checked"
        );
    }
}
