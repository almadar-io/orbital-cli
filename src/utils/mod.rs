//! Shared CLI utilities split out of `main.rs` during the module breakup.
//! Public surface (`pub`/`pub(crate)`) is preserved verbatim.

pub(crate) mod agents;
pub(crate) mod detect;
pub(crate) mod evaluate;
pub(crate) mod i18n;
pub(crate) mod patterns;
pub(crate) mod shells;
pub(crate) mod strings;

pub(crate) use agents::{find_bun, spawn_agent, spawn_agent_with_args};
pub(crate) use detect::{
    detect_syntax, parse_schema_auto, strip_json_nulls, typed_parse_error_to_json, InputSyntax,
};
pub use detect::{parse_schema_auto_typed, TypedParseError};
pub(crate) use evaluate::evaluate_sexpr;
pub(crate) use i18n::{copy_locales, detect_language, translate_schema_content};
pub(crate) use patterns::{
    load_extra_patterns, load_project_component_mappings, load_services_registry,
};
pub(crate) use shells::find_shells_dir;
