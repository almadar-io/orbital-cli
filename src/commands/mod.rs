//! CLI subcommand implementations split out of `main.rs`.
//! Public surface (`pub(crate)`) is preserved verbatim.

pub(crate) mod behaviors;
pub(crate) mod behaviors_catalog;
pub(crate) mod behaviors_index;
pub(crate) mod behaviors_texts;
pub(crate) mod behaviors_vectors_snapshot;
pub(crate) mod behaviors_vectors;
pub(crate) mod compile;
pub(crate) mod dev;
pub(crate) mod execute;
pub(crate) mod language;
pub(crate) mod npm_project;
pub(crate) mod parse;
pub(crate) mod program_command;
pub(crate) mod program_host;
pub(crate) mod program_inspect;
pub(crate) mod sidecar_fetch;
pub(crate) mod stamp;
pub(crate) mod validate;

pub(crate) use compile::compile_schema;
pub(crate) use behaviors::run_which;
pub(crate) use behaviors_catalog::{run_catalog, run_describe, run_expects, run_source};
pub(crate) use behaviors_texts::run_texts;
pub(crate) use parse::parse_schema;
pub(crate) use stamp::stamp_schema_cli;
pub(crate) use validate::validate_schema;
pub(crate) mod packages;
pub(crate) mod update_notice;
