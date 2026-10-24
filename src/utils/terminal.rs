//! orb's terminal: the logging flags, live phase spinners and lines, and the timing outputs
//! (`--timings` tree, `--trace-file` Chrome trace). Everything is built on the one logger in
//! `orbital_core::logging`; stdout stays the data channel.

use clap::{ArgAction, Args, FromArgMatches, ValueEnum};
use orbital_core::logging::{self, ConsoleOptions, LogConfig, LogFormat, Timings};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Instant;
use tracing::span::{Attributes, Id, Record};
use tracing::{Level, Subscriber};
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// orb's logging flags. They are orb's wherever they appear before `--`, for orb's own commands
/// and for `cli` programs alike; a program never receives them in its argv.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LogArgs {
    /// More detail on stderr: -v info and each phase's time, -vv debug, -vvv trace
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub(crate) verbose: u8,

    /// Errors only on stderr
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub(crate) quiet: bool,

    /// Log line format on stderr [default: ALMADAR_LOG_FORMAT, else console]
    #[arg(long, value_enum, global = true, value_name = "FORMAT")]
    pub(crate) log_format: Option<LogFormatArg>,

    /// Print a timing tree of every span on stderr when the command ends
    #[arg(long, global = true)]
    pub(crate) timings: bool,

    /// Write every span as a Chrome trace to FILE (open it in ui.perfetto.dev)
    #[arg(long, global = true, value_name = "FILE")]
    pub(crate) trace_file: Option<PathBuf>,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogFormatArg {
    Console,
    Json,
}

fn log_command() -> clap::Command {
    LogArgs::augment_args(
        clap::Command::new("orb")
            .no_binary_name(true)
            .disable_help_flag(true)
            .disable_version_flag(true),
    )
}

/// Split `args` (argv after the binary) into orb's logging flags and everything else, in order.
pub(crate) fn split_log_args(args: &[String]) -> anyhow::Result<(LogArgs, Vec<String>)> {
    let command = log_command();
    let longs: Vec<(String, bool)> = command
        .get_arguments()
        .filter_map(|a| Some((a.get_long()?.to_string(), a.get_action().takes_values())))
        .collect();
    let shorts: Vec<char> = command
        .get_arguments()
        .filter_map(|a| a.get_short())
        .collect();

    let mut taken = Vec::new();
    let mut rest = Vec::new();
    let mut tokens = args.iter();
    while let Some(token) = tokens.next() {
        if token == "--" {
            rest.push(token.clone());
            rest.extend(tokens.by_ref().cloned());
            break;
        }
        if let Some(long) = token.strip_prefix("--") {
            let (name, inline_value) = match long.split_once('=') {
                Some((name, _)) => (name, true),
                None => (long, false),
            };
            if let Some((_, takes_value)) = longs.iter().find(|(l, _)| l == name) {
                taken.push(token.clone());
                if *takes_value && !inline_value {
                    if let Some(value) = tokens.next() {
                        taken.push(value.clone());
                    }
                }
                continue;
            }
        } else if let Some(cluster) = token.strip_prefix('-') {
            if !cluster.is_empty() && cluster.chars().all(|c| shorts.contains(&c)) {
                taken.push(token.clone());
                continue;
            }
        }
        rest.push(token.clone());
    }
    let matches = command.try_get_matches_from(taken)?;
    Ok((LogArgs::from_arg_matches(&matches)?, rest))
}

impl LogArgs {
    fn format(&self) -> LogFormat {
        match self.log_format {
            Some(LogFormatArg::Console) => LogFormat::Console,
            Some(LogFormatArg::Json) => LogFormat::Json,
            None => LogFormat::from_env(),
        }
    }

    /// The env config with the floor these flags set; no flag keeps `LOG_LEVEL`.
    fn config(&self) -> LogConfig {
        let env = LogConfig::from_env();
        match (self.quiet, self.verbose) {
            (true, _) => env.with_floor(Level::ERROR),
            (false, 0) => env,
            (false, 1) => env.with_floor(Level::INFO),
            (false, 2) => env.with_floor(Level::DEBUG),
            (false, _) => env.with_floor(Level::TRACE),
        }
    }

    /// The most verbose span level that gets a finished line: none without -v.
    fn phase_lines(&self) -> Option<Level> {
        match (self.quiet, self.verbose) {
            (true, _) | (false, 0) => None,
            (false, 1) => Some(Level::INFO),
            (false, 2) => Some(Level::DEBUG),
            (false, _) => Some(Level::TRACE),
        }
    }
}

/// What `orb` prints when the command ends.
pub(crate) struct Terminal {
    timings: Option<Timings>,
    tree: bool,
    trace_file: Option<PathBuf>,
}

/// Install orb's subscriber for `args`.
pub(crate) fn init(args: &LogArgs) -> Terminal {
    let config = args.config();
    let format = args.format();
    let ansi = logging::stderr_supports_color();
    let console = ConsoleOptions {
        ansi,
        uptime: true,
        spans: false,
    };
    let progress = format == LogFormat::Console
        && !args.quiet
        && std::io::stderr().is_terminal()
        && std::env::var("TERM").map_or(true, |t| t != "dumb");
    let timings = (args.timings || args.trace_file.is_some()).then(Timings::new);
    let phase_lines = args.phase_lines().filter(|_| format == LogFormat::Console);

    // A shell program or nested `orb` that orb starts logs the way this run does.
    for (key, value) in config.env(format) {
        std::env::set_var(key, value);
    }
    let skipped = config.clone();
    let installed = if progress {
        let spinners = IndicatifLayer::new()
            .with_progress_style(
                indicatif::ProgressStyle::with_template(
                    "{span_child_prefix}{spinner:.cyan} {span_name} {span_fields:.dim} {elapsed:.dim}",
                )
                .expect("valid template"),
            )
            .with_span_child_prefix_symbol("")
            .with_span_child_prefix_indent("  ");
        let writer = spinners.get_stderr_writer();
        tracing_subscriber::registry()
            .with(logging::event_layer(
                format,
                config,
                console,
                writer.clone(),
            ))
            .with(timings.as_ref().map(|t| t.layer()))
            .with(phase_lines.map(|max| PhaseLines::layer(writer, max, ansi)))
            .with(
                spinners.with_filter(tracing_subscriber::filter::filter_fn(|meta| {
                    meta.is_span() && *meta.level() <= Level::INFO
                })),
            )
            .try_init()
    } else {
        tracing_subscriber::registry()
            .with(logging::event_layer(
                format,
                config,
                console,
                std::io::stderr,
            ))
            .with(timings.as_ref().map(|t| t.layer()))
            .with(phase_lines.map(|max| PhaseLines::layer(std::io::stderr, max, ansi)))
            .try_init()
    };
    if installed.is_ok() {
        logging::report_skipped(&skipped);
    }
    Terminal {
        timings,
        tree: args.timings,
        trace_file: args.trace_file.clone(),
    }
}

impl Terminal {
    /// Print the timing tree and write the trace file. Call it once every span is closed.
    pub(crate) fn finish(&self) -> anyhow::Result<()> {
        let Some(timings) = &self.timings else {
            return Ok(());
        };
        if self.tree {
            let tree = timings.tree();
            if !tree.is_empty() {
                let mut stderr = std::io::stderr().lock();
                writeln!(stderr)?;
                write!(stderr, "{tree}")?;
            }
        }
        if let Some(path) = &self.trace_file {
            let trace = serde_json::to_vec(&timings.chrome_trace())?;
            std::fs::write(path, trace).map_err(|e| {
                anyhow::anyhow!("Failed to write the trace file {}: {e}", path.display())
            })?;
            eprintln!(
                "Trace written to {} (open it in https://ui.perfetto.dev)",
                path.display()
            );
        }
        Ok(())
    }
}

struct PhaseStart {
    at: Instant,
    fields: String,
    failed: bool,
}

/// Collects a span's fields as `k=v` text; [`logging::OUTCOME`] marks a failed phase.
#[derive(Default)]
struct PhaseFields {
    text: String,
    failed: Option<bool>,
}

impl tracing::field::Visit for PhaseFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == logging::OUTCOME {
            self.failed = Some(value == "failed");
            return;
        }
        self.push(field, value);
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.push(field, &format!("{value:?}"));
    }
}

impl PhaseFields {
    fn push(&mut self, field: &tracing::field::Field, value: &str) {
        if !self.text.is_empty() {
            self.text.push(' ');
        }
        self.text.push_str(&format!("{}={value}", field.name()));
    }
}

/// One line per finished span at or above `max`: `✓ name fields  41.3ms`, `✗` when its
/// outcome is `failed`, indented by depth.
struct PhaseLines<W> {
    writer: W,
    ansi: bool,
}

impl<W> PhaseLines<W>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    fn layer<S>(writer: W, max: Level, ansi: bool) -> impl Layer<S> + Send + Sync
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        PhaseLines { writer, ansi }.with_filter(tracing_subscriber::filter::filter_fn(
            move |meta| meta.is_span() && *meta.level() <= max,
        ))
    }
}

impl<S, W> Layer<S> for PhaseLines<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + 'static,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = PhaseFields::default();
        attrs.record(&mut fields);
        span.extensions_mut().insert(PhaseStart {
            at: Instant::now(),
            fields: fields.text,
            failed: fields.failed.unwrap_or(false),
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = PhaseFields::default();
        values.record(&mut fields);
        if let Some(start) = span.extensions_mut().get_mut::<PhaseStart>() {
            if let Some(failed) = fields.failed {
                start.failed = failed;
            }
            if !fields.text.is_empty() {
                if !start.fields.is_empty() {
                    start.fields.push(' ');
                }
                start.fields.push_str(&fields.text);
            }
        };
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(start) = span.extensions_mut().remove::<PhaseStart>() else {
            return;
        };
        let depth = span.scope().skip(1).count();
        let line = phase_line(
            depth,
            span.name(),
            &start.fields,
            start.failed,
            &logging::format_duration(start.at.elapsed()),
            self.ansi,
        );
        // A failed write to stderr has nowhere else to be reported.
        let _ = self.writer.make_writer().write_all(line.as_bytes());
    }
}

fn phase_line(
    depth: usize,
    name: &str,
    fields: &str,
    failed: bool,
    elapsed: &str,
    ansi: bool,
) -> String {
    let indent = "  ".repeat(depth);
    let (glyph, color) = if failed { ("✗", "31") } else { ("✓", "32") };
    let sep = if fields.is_empty() { "" } else { " " };
    if ansi {
        format!("{indent}\x1b[{color}m{glyph}\x1b[0m {name}{sep}\x1b[2m{fields}\x1b[0m  \x1b[2m{elapsed}\x1b[0m\n")
    } else {
        format!("{indent}{glyph} {name}{sep}{fields}  {elapsed}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(args: &[&str]) -> (LogArgs, Vec<String>) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        split_log_args(&args).expect("logging flags parse")
    }

    #[test]
    fn logging_flags_are_taken_anywhere_before_the_double_dash() {
        let (log, rest) = split(&["-v", "validate", "app.lolo", "--timings", "-v", "--json"]);
        assert_eq!(log.verbose, 2);
        assert!(log.timings);
        assert_eq!(rest, vec!["validate", "app.lolo", "--json"]);
    }

    #[test]
    fn a_short_cluster_counts_and_a_valued_flag_takes_its_value() {
        let (log, rest) = split(&[
            "test",
            "-vvv",
            "--trace-file",
            "t.json",
            "--log-format=json",
            "x",
        ]);
        assert_eq!(log.verbose, 3);
        assert_eq!(log.trace_file, Some(PathBuf::from("t.json")));
        assert_eq!(log.log_format, Some(LogFormatArg::Json));
        assert_eq!(rest, vec!["test", "x"]);
    }

    #[test]
    fn words_after_the_double_dash_and_other_flags_stay_with_the_command() {
        let (log, rest) = split(&["cascade", "-o", "out", "-vx", "--", "-v", "--timings"]);
        assert_eq!(log, LogArgs::default());
        assert_eq!(
            rest,
            vec!["cascade", "-o", "out", "-vx", "--", "-v", "--timings"]
        );
    }

    #[test]
    fn quiet_and_verbose_together_are_refused() {
        let args: Vec<String> = ["-q", "-v", "validate"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(split_log_args(&args).is_err());
    }

    #[test]
    fn no_flags_leave_every_word_in_place() {
        let (log, rest) = split(&["behaviors", "catalog", "--json"]);
        assert_eq!(log, LogArgs::default());
        assert_eq!(rest, vec!["behaviors", "catalog", "--json"]);
    }

    #[test]
    fn verbosity_sets_the_floor_and_the_phase_lines_together() {
        let (one, _) = split(&["-v"]);
        assert_eq!(one.config().floor(), Level::INFO);
        assert_eq!(one.phase_lines(), Some(Level::INFO));
        let (quiet, _) = split(&["-q"]);
        assert_eq!(quiet.config().floor(), Level::ERROR);
        assert_eq!(quiet.phase_lines(), None);
        assert_eq!(LogArgs::default().phase_lines(), None);
    }

    #[test]
    fn a_phase_line_marks_failure_and_indents_by_depth() {
        assert_eq!(
            phase_line(0, "lower", "file=a.lolo", false, "41.3ms", false),
            "✓ lower file=a.lolo  41.3ms\n"
        );
        assert_eq!(
            phase_line(1, "validate", "", true, "2.00ms", false),
            "  ✗ validate  2.00ms\n"
        );
    }
}
