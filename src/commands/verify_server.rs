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

/// Boot a fresh, seeded `orbital-server` for `schema` and return a bridge pointed at it.
fn boot_http_bridge(schema: &OrbitalSchema) -> anyhow::Result<Arc<dyn ServerBridge>> {
    let adapter = Arc::new(InMemoryAdapter::new());
    let runtime = Arc::new(OrbitalServerRuntime::new(adapter));
    runtime
        .register_schema(schema.clone())
        .map_err(|e| anyhow::anyhow!("register_schema: {e}"))?;
    // SAME default-persona binding as the in-process `KernelBridge` probe (identical
    // seed count, same first-roster-persona rule) — without it the HTTP bridge evaluates
    // every `@read`/guard anonymously while the in-process probe sees the bound viewer,
    // and an ownership-scoped list parities as empty here but populated there.
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

    let server = serve_background(runtime)?;
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
