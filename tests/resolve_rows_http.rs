//! `SyncServerBridge::resolve_rows` over real HTTP against `orbital-server`'s `POST /api/orbitals/resolve-rows`:
//! the viewer the server is bound to decides which rows come back (a hidden row is omitted, the total is re-counted).

use orbital_core::client::bridge::ServerBridge;
use orbital_core::client::http::SyncServerBridge;
use orbital_core::runtime::row_query::RowSetRef;
use orbital_core::{OrbitalSchema, Value};
use orbital_server::{serve_background, InMemoryAdapter, OrbitalServerRuntime};
use serde_json::json;
use std::sync::Arc;

fn schema() -> OrbitalSchema {
    serde_json::from_value(json!({
        "name": "ResolveApp",
        "orbitals": [{
            "name": "Notes",
            "entity": {
                "name": "Note", "persistence": "persistent",
                "fields": [{ "name": "id", "type": "string", "primaryKey": true }, { "name": "ownerId", "type": "string" }],
                "read_policy": ["=", ["object/get", "@entity", "ownerId"], "@user.id"]
            },
            "traits": [{
                "name": "NoteBrowse", "linkedEntity": "Note",
                "stateMachine": { "states": [{ "name": "idle", "isInitial": true }], "transitions": [] }
            }],
            "pages": []
        }]
    }))
    .unwrap()
}

fn reference(ids: &[&str]) -> RowSetRef {
    RowSetRef {
        entity_type: "Note".into(),
        ids: ids.iter().map(|s| s.to_string()).collect(),
        total: ids.len(),
        query: Default::default(),
    }
}

fn serve(viewer: serde_json::Value) -> orbital_server::BackgroundServer {
    let runtime = Arc::new(OrbitalServerRuntime::new(Arc::new(InMemoryAdapter::new())));
    runtime.register_schema(schema()).unwrap();
    for (id, owner) in [("a1", "alice"), ("a2", "alice"), ("b1", "bob")] {
        let row: Value = serde_json::from_value(json!({ "id": id, "ownerId": owner })).unwrap();
        runtime.persistence().create("Note", &row).unwrap();
    }
    runtime.set_current_user(serde_json::from_value(viewer).unwrap());
    serve_background(runtime).unwrap()
}

fn ids(bridge: &SyncServerBridge, reference: &RowSetRef) -> Vec<String> {
    ServerBridge::resolve_rows(bridge, reference)
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            r.as_object()
                .unwrap()
                .get("id")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn the_bridge_resolves_for_the_viewer_the_server_serves_and_omits_a_hidden_row() {
    let server = serve(json!({ "id": "alice", "role": "member" }));
    let bridge = SyncServerBridge::new(server.base_url());
    assert_eq!(
        ids(&bridge, &reference(&["a1", "b1", "a2"])),
        vec!["a1", "a2"]
    );
}

#[test]
fn control_another_viewer_sees_their_own_rows() {
    let server = serve(json!({ "id": "bob", "role": "member" }));
    let bridge = SyncServerBridge::new(server.base_url());
    assert_eq!(ids(&bridge, &reference(&["a1", "b1", "a2"])), vec!["b1"]);
}

#[test]
fn control_a_server_that_is_down_is_an_error_not_an_empty_page() {
    let bridge = SyncServerBridge::new("http://127.0.0.1:1");
    assert!(ServerBridge::resolve_rows(&bridge, &reference(&["a1"])).is_err());
}
