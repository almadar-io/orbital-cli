//! CLI subcommand implementations split out of `main.rs`.
//! Public surface (`pub(crate)`) is preserved verbatim.

pub(crate) mod behaviors;
pub(crate) mod compile;
pub(crate) mod convert;
pub(crate) mod dev;
pub(crate) mod engines;
pub(crate) mod engines_cmd;
pub(crate) mod execute;
pub(crate) mod i18n;
pub(crate) mod npm_project;
pub(crate) mod parse;
pub(crate) mod serve;
pub(crate) mod simulate;
pub(crate) mod stamp;
pub(crate) mod test;
pub(crate) mod validate;
pub(crate) mod verify;
pub(crate) mod verify_server;

pub(crate) use behaviors::{run_install, run_list, run_remove, run_update, run_which};
pub(crate) use compile::compile_schema;
pub(crate) use convert::run_convert;
pub(crate) use dev::run_dev_mode;
pub(crate) use engines_cmd::{
    run_install as run_engines_install, run_list as run_engines_list,
    run_remove as run_engines_remove, run_update as run_engines_update,
};
pub(crate) use execute::execute_schema_cli;
pub(crate) use i18n::{run_i18n_check, run_i18n_vocab};
pub(crate) use parse::{format_schema, parse_schema, resolve_schema_cli};
pub(crate) use serve::run_serve;
pub(crate) use simulate::simulate_render;
pub(crate) use stamp::stamp_schema_cli;
pub(crate) use test::run_tests;
pub(crate) use validate::validate_schema;
