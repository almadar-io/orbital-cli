//! Shared CLI utilities split out of `main.rs` during the module breakup.
//! Public surface (`pub`/`pub(crate)`) is preserved verbatim.

pub(crate) mod agents;
pub(crate) mod desktop;
pub(crate) mod detect;
pub(crate) mod diagnostics;
pub(crate) mod evaluate;
pub(crate) mod i18n;
pub(crate) mod patterns;
pub(crate) mod shell_programs;

pub(crate) use agents::{find_bun, spawn_agent, spawn_agent_with_args};
pub(crate) use detect::{
    detect_syntax, parse_schema_auto, parse_schema_auto_with_map, strip_json_nulls,
    typed_parse_error_to_json, InputSyntax,
};
pub use detect::{parse_schema_auto_typed, parse_schema_auto_typed_with_map, TypedParseError};
pub(crate) use diagnostics::merge_post_inline_diagnostics;
pub(crate) use evaluate::evaluate_sexpr;
pub(crate) use i18n::{detect_language, translate_schema_content};
pub(crate) use patterns::{
    load_extra_patterns, load_project_patterns, load_services_registry,
    require_language_data,
};
