//! CLI subcommand implementations split out of `main.rs`.
//! Public surface (`pub(crate)`) is preserved verbatim.

pub(crate) mod compile;
pub(crate) mod convert;
pub(crate) mod dev;
pub(crate) mod parse;
pub(crate) mod serve;
pub(crate) mod simulate;
pub(crate) mod test;
pub(crate) mod validate;
pub(crate) mod verify;
pub(crate) mod verify_server;

pub(crate) use compile::compile_schema;
pub(crate) use convert::run_convert;
pub(crate) use dev::run_dev_mode;
pub(crate) use parse::{format_schema, parse_schema, resolve_schema_cli};
pub(crate) use serve::run_serve;
pub(crate) use simulate::simulate_render;
pub(crate) use test::run_tests;
pub(crate) use validate::validate_schema;
