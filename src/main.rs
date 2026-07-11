#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use clap::builder::styling::{AnsiColor as ClapColor, Styles as ClapStyles};
use clap::{
    Args, ColorChoice as ClapColorChoice, CommandFactory, FromArgMatches, Parser, Subcommand,
    ValueEnum,
};
use serde::Serialize;

use token_ledger::adapters::built_in_adapters;
use token_ledger::adapters::claude::ClaudeAdapter;
use token_ledger::adapters::codex::CodexAdapter;
use token_ledger::config::Config;
use token_ledger::cost::{
    CostBilling, CostPeriodSelection, CostQuery, CostReconciliation, build_cost_document,
    render_cost_with_options, resolve_cost_period, write_cost_html,
};
use token_ledger::db::Ledger;
use token_ledger::html::write_report_html;
use token_ledger::model::{CanonicalEvent, Client, UsageVector, stable_id};
use token_ledger::pricing::{
    CatalogCandidateRelation, CatalogCheckResult, CatalogCollectionDiff, CatalogFreshness,
    CatalogInstallReceipt, MeasureStatus, OfficialCatalogManifest, PricingEngine, RateKind,
    VerificationSeverity,
};
use token_ledger::reconcile::{
    ImportFormat as ReconciliationImportFormat, ReconciliationCounters,
    import_path as import_reconciliation_path, report as reconciliation_report,
    status as reconciliation_status,
};
use token_ledger::report::{
    GroupBy, ReportDocument, aggregate, build_report_document, canonical_model_name,
    local_day_bounds, local_range_bounds, render_report_with_options, write_report_csv,
    write_report_json,
};
use token_ledger::scanner::{ScanOptions, scan};
use token_ledger::terminal::{
    ColorChoice as TerminalColorChoice, TerminalOptions, Tone,
    UnicodeChoice as TerminalUnicodeChoice, current as terminal_ui, display_client_name,
    display_model_name, format_count, format_count_compact, format_decimal, set_current,
};

const CLI_STYLES: ClapStyles = ClapStyles::styled()
    .header(ClapColor::Cyan.on_default().bold())
    .usage(ClapColor::Cyan.on_default().bold())
    .literal(ClapColor::Green.on_default().bold())
    .placeholder(ClapColor::Yellow.on_default());

#[derive(Debug, Parser)]
#[command(
    name = "ledger",
    version,
    about = "Local-first Claude Code and OpenAI Codex token usage ledger",
    styles = CLI_STYLES,
    after_help = "QUICK START:\n  ledger today\n  ledger cost --month\n  ledger doctor\n\nHuman output adapts to terminal width. JSON, CSV, and HTML schemas are unchanged."
)]
struct Cli {
    /// Override the platform config file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Override the SQLite ledger path.
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Override the Claude Code data root.
    #[arg(long, global = true)]
    claude_root: Option<PathBuf>,
    /// Override CODEX_HOME.
    #[arg(long, global = true)]
    codex_home: Option<PathBuf>,
    /// Price reports with one verified immutable catalog revision without
    /// changing the active catalog.
    #[arg(long, global = true)]
    catalog_revision: Option<String>,
    /// Control ANSI color in human output.
    #[arg(long, global = true, value_enum, default_value = "auto")]
    color: CliColorChoice,
    /// Control Unicode rules and status symbols.
    #[arg(long, global = true, value_enum, default_value = "auto")]
    unicode: CliUnicodeChoice,
    /// Emit stable, line-oriented human output without color or Unicode.
    #[arg(long, global = true)]
    plain: bool,
    /// Include catalog, coverage, and accounting evidence in human output.
    #[arg(long, global = true)]
    details: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliColorChoice {
    Auto,
    Always,
    Never,
}

impl From<CliColorChoice> for TerminalColorChoice {
    fn from(value: CliColorChoice) -> Self {
        match value {
            CliColorChoice::Auto => Self::Auto,
            CliColorChoice::Always => Self::Always,
            CliColorChoice::Never => Self::Never,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliUnicodeChoice {
    Auto,
    Always,
    Never,
}

impl From<CliUnicodeChoice> for TerminalUnicodeChoice {
    fn from(value: CliUnicodeChoice) -> Self {
        match value {
            CliUnicodeChoice::Auto => Self::Auto,
            CliUnicodeChoice::Always => Self::Always,
            CliUnicodeChoice::Never => Self::Never,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create configuration and initialize the database.
    Init(InitArgs),
    /// Inspect source discovery, coverage, and catalog health without exposing transcript content.
    Doctor,
    /// Incrementally scan locally persisted sessions.
    Scan(ScanArgs),
    /// Report usage for one local calendar day.
    Day(DayArgs),
    /// Report usage for today in the configured local timezone.
    Today(TodayArgs),
    /// Summarize model costs and combined totals without conflating estimates with cash paid.
    Cost(CostArgs),
    /// Report usage across an inclusive local-date range.
    Range(RangeArgs),
    /// List sessions active on a given local date.
    Sessions(SessionsArgs),
    /// List observed raw models and event counts.
    Models(OutputArgs),
    /// Explain one canonical event and its price match.
    Explain(ExplainArgs),
    /// Inspect or update the effective-dated pricing catalog.
    Prices(PricesArgs),
    /// Import and compare provider-reported usage without changing local observations.
    Reconcile(ReconcileArgs),
    /// Export grouped usage as JSON, CSV, or privacy-safe HTML.
    Export(ExportArgs),
    /// Remove all locally indexed accounting metadata.
    Purge(PurgeArgs),
}

#[derive(Debug, Args)]
struct InitArgs {
    /// IANA timezone stored in the new configuration.
    #[arg(long)]
    tz: Option<String>,
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args, Default)]
struct ScanArgs {
    /// Restrict scanning to claude or codex. Repeatable.
    #[arg(long, value_parser = parse_client)]
    client: Vec<Client>,
    /// Ignore completed usage before an RFC3339 timestamp or YYYY-MM-DD UTC date.
    #[arg(long)]
    since: Option<String>,
    /// Rebuild observations by replaying every selected source from the beginning.
    #[arg(long = "rebuild", visible_alias = "full")]
    full: bool,
    /// Parse and summarize without changing the ledger.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct DayArgs {
    /// YYYY-MM-DD, today, or yesterday in the selected timezone.
    date: String,
    /// Override the configured IANA timezone for this command.
    #[arg(long)]
    tz: Option<String>,
    #[arg(long)]
    no_scan: bool,
    #[command(flatten)]
    filters: ReportFilters,
    #[command(flatten)]
    output: ReportOutputArgs,
}

#[derive(Debug, Args)]
struct TodayArgs {
    /// Override the configured IANA timezone for this command.
    #[arg(long)]
    tz: Option<String>,
    #[arg(long)]
    no_scan: bool,
    #[command(flatten)]
    filters: ReportFilters,
    #[command(flatten)]
    output: ReportOutputArgs,
}

#[derive(Debug, Args)]
struct CostArgs {
    /// Include all matching locally persisted history.
    #[arg(
        long,
        conflicts_with_all = ["today", "yesterday", "month", "start", "end"]
    )]
    all: bool,
    /// Select today in the configured or overridden timezone (the default).
    #[arg(
        long,
        conflicts_with_all = ["all", "yesterday", "month", "start", "end"]
    )]
    today: bool,
    /// Select yesterday in the configured or overridden timezone.
    #[arg(
        long,
        conflicts_with_all = ["all", "today", "month", "start", "end"]
    )]
    yesterday: bool,
    /// Select the current local calendar month through today.
    #[arg(
        long,
        conflicts_with_all = ["all", "today", "yesterday", "start", "end"]
    )]
    month: bool,
    /// First local date, inclusive: YYYY-MM-DD, today, or yesterday.
    #[arg(
        long,
        requires = "end",
        conflicts_with_all = ["all", "today", "yesterday", "month"]
    )]
    start: Option<String>,
    /// Last local date, inclusive: YYYY-MM-DD, today, or yesterday.
    #[arg(
        long,
        requires = "start",
        conflicts_with_all = ["all", "today", "yesterday", "month"]
    )]
    end: Option<String>,
    /// Override the configured IANA timezone for this command.
    #[arg(long)]
    tz: Option<String>,
    /// Read the current ledger without refreshing source sessions first.
    #[arg(long)]
    no_scan: bool,
    #[command(flatten)]
    filters: ReportFilters,
    #[command(flatten)]
    output: ReportOutputArgs,
}

#[derive(Debug, Args)]
struct RangeArgs {
    /// YYYY-MM-DD, today, or yesterday in the selected timezone.
    start: String,
    /// YYYY-MM-DD, today, or yesterday in the selected timezone.
    end: String,
    /// Override the configured IANA timezone for this command.
    #[arg(long)]
    tz: Option<String>,
    #[arg(long, default_value = "day,client,model")]
    group_by: String,
    #[arg(long)]
    no_scan: bool,
    #[command(flatten)]
    filters: ReportFilters,
    #[command(flatten)]
    output: ReportOutputArgs,
}

#[derive(Debug, Args)]
struct SessionsArgs {
    /// YYYY-MM-DD, today, or yesterday in the selected timezone.
    #[arg(long)]
    date: String,
    /// Override the configured IANA timezone for this command.
    #[arg(long)]
    tz: Option<String>,
    #[arg(long)]
    no_scan: bool,
    #[command(flatten)]
    filters: ReportFilters,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Debug, Args, Default)]
struct ReportFilters {
    /// Include only this client (claude or codex). Repeatable.
    #[arg(long, value_parser = parse_client)]
    client: Vec<Client>,
    /// Include only this raw or catalog-canonical model name. Repeatable.
    #[arg(long)]
    model: Vec<String>,
}

#[derive(Debug, Args, Default)]
struct ReportOutputArgs {
    /// Emit the stable machine-readable report envelope.
    #[arg(long, conflicts_with = "html")]
    json: bool,
    /// Emit a self-contained, share-safe HTML report; omit PATH or use - for stdout.
    #[arg(
        long,
        value_name = "PATH",
        num_args = 0..=1,
        default_missing_value = "-",
        conflicts_with = "json"
    )]
    html: Option<PathBuf>,
}

impl ReportOutputArgs {
    fn is_machine(&self) -> bool {
        self.json || self.html.is_some()
    }
}

#[derive(Debug, Args, Default)]
struct OutputArgs {
    /// Emit machine-readable JSON instead of the human table.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ExplainArgs {
    #[arg(long)]
    event: String,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Debug, Args)]
struct PricesArgs {
    #[command(subcommand)]
    command: PriceCommand,
}

#[derive(Debug, Subcommand)]
enum PriceCommand {
    Status(OutputArgs),
    /// List effective price rules in a readable table, or emit the raw catalog with --json.
    List(OutputArgs),
    Verify(OutputArgs),
    /// List verified immutable catalog revisions retained for rollback.
    History(OutputArgs),
    /// Compare a reviewed candidate or configured official feed without installing it.
    Check(PriceCheckArgs),
    Update(PriceUpdateArgs),
    /// Compare two verified active, bundled, or retained revisions.
    Diff(PriceDiffArgs),
    /// Explicitly activate a verified revision, including an older revision.
    Activate(PriceActivateArgs),
    /// Activate the newest retained revision older than the active catalog.
    Rollback(OutputArgs),
}

#[derive(Debug, Args)]
struct PriceUpdateArgs {
    /// HTTPS URL or local JSON file containing a reviewed catalog.
    #[arg(long = "from")]
    source: Option<String>,
    /// Expected SHA-256 digest of the exact JSON bytes; required for HTTPS.
    #[arg(long)]
    sha256: Option<String>,
    /// Use the explicitly configured, checksum-pinned official manifest.
    #[arg(long, conflicts_with = "source")]
    official: bool,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Debug, Args)]
struct PriceCheckArgs {
    /// HTTPS URL or local JSON file containing a reviewed catalog.
    #[arg(long = "from")]
    source: Option<String>,
    /// Expected SHA-256 digest; required for a candidate fetched over HTTPS.
    #[arg(long)]
    sha256: Option<String>,
    /// Check the explicitly configured, checksum-pinned official manifest.
    #[arg(long, conflicts_with = "source")]
    official: bool,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Debug, Args)]
struct PriceDiffArgs {
    /// Baseline verified revision.
    from_revision: String,
    /// Comparison verified revision.
    to_revision: String,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Debug, Args)]
struct PriceActivateArgs {
    revision: String,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Debug, Args)]
struct ReconcileArgs {
    #[command(subcommand)]
    command: ReconcileCommand,
}

#[derive(Debug, Subcommand)]
enum ReconcileCommand {
    /// Import a local provider export as immutable evidence.
    Import(ReconcileImportArgs),
    /// Summarize imported evidence and coverage.
    Status(OutputArgs),
    /// Compare provider buckets with local canonical usage.
    Report(ReconcileReportArgs),
}

#[derive(Debug, Args)]
struct ReconcileImportArgs {
    /// Local canonical JSON/CSV, OpenAI organization, or Anthropic Admin export.
    path: PathBuf,
    /// auto, canonical-json, canonical-csv, openai, or anthropic.
    #[arg(long, default_value = "auto")]
    format: String,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Debug, Args)]
struct ReconcileReportArgs {
    /// Optional first local calendar date (YYYY-MM-DD, today, or yesterday).
    #[arg(long)]
    start: Option<String>,
    /// Optional last local calendar date, inclusive.
    #[arg(long)]
    end: Option<String>,
    /// Override the configured IANA timezone for this command.
    #[arg(long)]
    tz: Option<String>,
    /// Compare the current ledger without scanning source sessions first.
    #[arg(long)]
    no_scan: bool,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ExportFormat {
    Json,
    Csv,
    Html,
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// YYYY-MM-DD, today, or yesterday in the selected timezone.
    #[arg(long)]
    start: String,
    /// YYYY-MM-DD, today, or yesterday in the selected timezone.
    #[arg(long)]
    end: String,
    /// Override the configured IANA timezone for this command.
    #[arg(long)]
    tz: Option<String>,
    #[arg(long, default_value = "day,client,model")]
    group_by: String,
    #[arg(long, value_enum, default_value = "json")]
    format: ExportFormat,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    no_scan: bool,
    #[command(flatten)]
    filters: ReportFilters,
}

#[derive(Debug, Args)]
struct PurgeArgs {
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Serialize)]
struct SessionRow {
    session: String,
    client: String,
    first_event_utc: String,
    last_event_utc: String,
    requests: u64,
    models: Vec<String>,
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Debug, Serialize)]
struct CatalogCheckOutput {
    check: CatalogCheckResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest: Option<OfficialCatalogManifest>,
    trust: String,
}

struct LoadedCatalogCandidate {
    bytes: Vec<u8>,
    expected_sha256: Option<String>,
    manifest: Option<OfficialCatalogManifest>,
    trust: String,
}

fn main() {
    if let Err(error) = run() {
        let terminal = terminal_ui();
        let message = format!(
            "{} {}\n  {error:#}\n",
            terminal.paint(Tone::Error, terminal.status_symbol(Tone::Error)),
            terminal.paint(Tone::Error, "Token Ledger could not complete the command")
        );
        if terminal.emit_stderr(&message).is_err() {
            eprintln!("error: {error:#}");
        }
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let Cli {
        config: config_override,
        db,
        claude_root,
        codex_home,
        catalog_revision,
        color,
        unicode,
        plain,
        details,
        command,
    } = parse_cli();
    let machine_output = command.as_ref().is_some_and(command_is_machine);
    set_current(TerminalOptions::detect(
        color.into(),
        unicode.into(),
        plain,
        details,
        machine_output,
    ));
    let Some(command) = command else {
        return emit_welcome();
    };
    let (mut persisted_config, config_path) = Config::load(config_override.as_deref())?;
    let mut config = persisted_config.clone();
    if let Some(path) = db {
        config.database_path = Some(path);
    }
    config.claude_root_override = claude_root;
    config.codex_home_override = codex_home;
    config.catalog_revision_override = catalog_revision.clone();
    persisted_config.catalog_revision_override = catalog_revision;

    match command {
        Command::Init(args) => command_init(config, &config_path, args),
        Command::Doctor => command_doctor(&config),
        Command::Scan(args) => {
            let mut ledger = open_scan_ledger(&config, args.dry_run)?;
            let spinner = terminal_ui().spinner(if args.dry_run {
                "Dry-running local session scan…"
            } else {
                "Scanning local Claude Code and Codex sessions…"
            });
            let result = run_scan(&mut ledger, &config, &args);
            if let Some(spinner) = spinner {
                spinner.finish_and_clear();
            }
            let summary = result?;
            print_scan_summary(&summary)
        }
        Command::Day(args) => command_day(&config, args),
        Command::Today(args) => command_today(&config, args),
        Command::Cost(args) => command_cost(&config, args),
        Command::Range(args) => command_range(&config, args),
        Command::Sessions(args) => command_sessions(&config, args),
        Command::Models(args) => command_models(&config, args),
        Command::Explain(args) => command_explain(&config, args),
        // A global --db is a one-process override and must never be persisted as
        // an incidental side effect of installing a price catalog.
        Command::Prices(args) => command_prices(&mut persisted_config, &config_path, args),
        Command::Reconcile(args) => command_reconcile(&config, args),
        Command::Export(args) => command_export(&config, args),
        Command::Purge(args) => command_purge(&config, args),
    }
}

fn parse_cli() -> Cli {
    let args = std::env::args_os().collect::<Vec<_>>();
    let mut color = ClapColorChoice::Auto;
    let plain = args.iter().any(|argument| argument == "--plain");
    for (index, argument) in args.iter().enumerate() {
        let argument = argument.to_string_lossy();
        if argument == "--color=never" {
            color = ClapColorChoice::Never;
        } else if argument == "--color=always" {
            color = ClapColorChoice::Always;
        } else if argument == "--color"
            && let Some(value) = args.get(index + 1).map(|value| value.to_string_lossy())
        {
            color = match value.as_ref() {
                "always" => ClapColorChoice::Always,
                "never" => ClapColorChoice::Never,
                _ => ClapColorChoice::Auto,
            };
        }
    }
    if plain {
        color = ClapColorChoice::Never;
    }
    let matches = Cli::command().color(color).get_matches_from(args);
    Cli::from_arg_matches(&matches).expect("clap validated command-line arguments")
}

fn emit_welcome() -> Result<()> {
    let terminal = terminal_ui();
    let mut text = String::new();
    let _ = writeln!(text, "{}", terminal.paint(Tone::Accent, "TOKEN LEDGER"));
    let _ = writeln!(
        text,
        "{}\n",
        terminal.paint(Tone::Muted, "Local Claude Code and OpenAI Codex accounting")
    );
    let _ = writeln!(text, "{}", terminal.paint(Tone::Strong, "QUICK START"));
    let _ = writeln!(text, "  ledger today             Today's usage");
    let _ = writeln!(text, "  ledger cost --month      Month-to-date estimate");
    let _ = writeln!(text, "  ledger doctor            Check local coverage");
    let _ = writeln!(text, "  ledger --help            Every command and option");
    terminal.emit_stdout(&text)?;
    Ok(())
}

fn command_is_machine(command: &Command) -> bool {
    match command {
        Command::Day(args) => args.output.is_machine(),
        Command::Today(args) => args.output.is_machine(),
        Command::Cost(args) => args.output.is_machine(),
        Command::Range(args) => args.output.is_machine(),
        Command::Sessions(args) => args.output.json,
        Command::Models(args) => args.json,
        Command::Explain(args) => args.output.json,
        Command::Prices(args) => match &args.command {
            PriceCommand::Status(output)
            | PriceCommand::Verify(output)
            | PriceCommand::History(output)
            | PriceCommand::Rollback(output) => output.json,
            PriceCommand::Check(args) => args.output.json,
            PriceCommand::Update(args) => args.output.json,
            PriceCommand::Diff(args) => args.output.json,
            PriceCommand::Activate(args) => args.output.json,
            PriceCommand::List(output) => output.json,
        },
        Command::Reconcile(args) => match &args.command {
            ReconcileCommand::Import(args) => args.output.json,
            ReconcileCommand::Status(output) => output.json,
            ReconcileCommand::Report(args) => args.output.json,
        },
        Command::Export(_) => true,
        Command::Init(_) | Command::Doctor | Command::Scan(_) | Command::Purge(_) => false,
    }
}

fn command_init(mut config: Config, path: &Path, args: InitArgs) -> Result<()> {
    if path.exists() && !args.force {
        anyhow::bail!(
            "config {} already exists; use --force to rewrite it",
            path.display()
        );
    }
    if let Some(timezone) = args.tz {
        parse_timezone(&timezone)?;
        config.timezone = timezone;
    }
    config.save(path)?;
    let ledger = open_ledger(&config)?;
    emit_success(
        "INITIALIZED",
        &format!(
            "Database: {}\nConfig: {}\nTimezone: {}",
            ledger.path().display(),
            path.display(),
            config.timezone
        ),
    )?;
    Ok(())
}

fn command_doctor(config: &Config) -> Result<()> {
    let terminal = terminal_ui();
    let adapters = built_in_adapters();
    let claude_root = ClaudeAdapter::resolve_root_with_origin(config)?;
    let codex_home = CodexAdapter::resolve_home_with_origin(config)?;
    let mut source_rows = Vec::new();
    let mut discovery_failed = false;
    for adapter in &adapters {
        match adapter.discover(config) {
            Ok(sources) => {
                let compressed = sources.iter().filter(|source| source.compressed).count();
                source_rows.push(vec![
                    adapter.display_name().to_string(),
                    "OK".to_string(),
                    format_count(sources.len() as u64),
                    if compressed > 0 {
                        format!("{compressed} compressed")
                    } else {
                        "Readable local files".to_string()
                    },
                ]);
            }
            Err(_error) => {
                discovery_failed = true;
                source_rows.push(vec![
                    adapter.display_name().to_string(),
                    "WARN".to_string(),
                    "0".to_string(),
                    "Discovery failed; verify the configured root".to_string(),
                ]);
            }
        }
    }
    let ledger = open_ledger(config)?;
    let stats = ledger.stats()?;
    let coverage = ledger.coverage_snapshot()?;
    let pricing = load_pricing(config)?;
    let unresolved_models: BTreeSet<_> = ledger
        .canonical_events(None, None)?
        .into_iter()
        .filter(|event| pricing.estimate_event(event).pricing_evidence.is_empty())
        .map(|event| (event.client.as_str().to_string(), event.raw_model))
        .collect();
    let status = pricing.status();
    let has_attention = discovery_failed
        || stats.warnings > 0
        || coverage.provisional
        || !unresolved_models.is_empty()
        || status.verification.error_count() > 0;

    let mut output = String::new();
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(Tone::Accent, "TOKEN LEDGER / DOCTOR")
    );
    let _ = writeln!(
        output,
        "{}",
        terminal.paint(
            Tone::Muted,
            format!(
                "Local accounting health{}{}",
                terminal.separator(),
                config.timezone
            )
        )
    );
    let overall_tone = if has_attention {
        Tone::Warning
    } else {
        Tone::Success
    };
    let _ = writeln!(
        output,
        "{} {}\n",
        terminal.paint(overall_tone, terminal.status_symbol(overall_tone)),
        terminal.paint(
            overall_tone,
            if has_attention {
                "Healthy with items to review"
            } else {
                "All checked systems are healthy"
            }
        )
    );

    let _ = writeln!(output, "{}", terminal.paint(Tone::Accent, "LOCAL SOURCES"));
    let _ = writeln!(
        output,
        "{}\n",
        terminal.table(&["CLIENT", "STATUS", "FILES", "DETAIL"], source_rows, &[2])
    );

    let snapshot_state = if coverage.provisional {
        "PROVISIONAL"
    } else {
        "STABLE"
    };
    let scan_detail = coverage.last_scan.as_ref().map_or_else(
        || "No completed scan".to_string(),
        |scan| {
            format!(
                "{} sources, {} observations, {} mode",
                format_count(scan.source_count),
                format_count(scan.observation_count),
                display_scan_mode(&scan.mode)
            )
        },
    );
    let ledger_rows = vec![
        vec![
            "Database".to_string(),
            "OK".to_string(),
            format!(
                "{} sources, {} observations, {} events",
                format_count(stats.sources),
                format_count(stats.observations),
                format_count(stats.canonical_events)
            ),
        ],
        vec![
            "Snapshot".to_string(),
            snapshot_state.to_string(),
            scan_detail,
        ],
        vec![
            "Price catalog".to_string(),
            if status.verification.error_count() == 0 {
                "OK".to_string()
            } else {
                "ERROR".to_string()
            },
            format!(
                "{} · {} · {} source records",
                status.revision,
                catalog_freshness_text(status.freshness),
                format_count(status.source_count as u64)
            ),
        ],
        vec![
            "Warnings".to_string(),
            if stats.warnings == 0 { "OK" } else { "REVIEW" }.to_string(),
            format!("{} sanitized warning(s)", format_count(stats.warnings)),
        ],
    ];
    let _ = writeln!(output, "{}", terminal.paint(Tone::Accent, "LEDGER"));
    let _ = writeln!(
        output,
        "{}\n",
        terminal.table(&["CHECK", "STATUS", "DETAIL"], ledger_rows, &[])
    );

    if !coverage.warning_counts.is_empty() || !unresolved_models.is_empty() {
        let _ = writeln!(output, "{}", terminal.paint(Tone::Warning, "REVIEW"));
        for warning in &coverage.warning_counts {
            let _ = writeln!(
                output,
                "{} {} · {} · {} occurrence(s)",
                terminal.paint(Tone::Warning, terminal.status_symbol(Tone::Warning)),
                warning.client.map_or("Global", |client| match client {
                    Client::ClaudeCode => "Claude Code",
                    Client::OpenaiCodex => "Codex",
                }),
                warning.code,
                format_count(warning.count)
            );
        }
        for (client, model) in unresolved_models.iter().take(20) {
            let _ = writeln!(
                output,
                "{} Unpriced model · {} · {}",
                terminal.paint(Tone::Warning, terminal.status_symbol(Tone::Warning)),
                display_client_name(client),
                display_model_name(model)
            );
        }
        output.push('\n');
    }

    if terminal.details {
        let _ = writeln!(output, "{}", terminal.paint(Tone::Accent, "DETAILS"));
        let _ = writeln!(
            output,
            "Database: {}",
            config.resolved_database_path()?.display()
        );
        let _ = writeln!(
            output,
            "Claude root ({}): {}",
            claude_root.origin,
            claude_root.path.display()
        );
        let _ = writeln!(
            output,
            "Codex home ({}): {}",
            codex_home.origin,
            codex_home.path.display()
        );
        for client in &coverage.clients {
            let window = client
                .earliest_canonical_event
                .as_ref()
                .zip(client.latest_canonical_event.as_ref())
                .map(|(earliest, latest)| {
                    format!("{} .. {}", earliest.occurred_at, latest.occurred_at)
                })
                .unwrap_or_else(|| "unavailable".to_string());
            let _ = writeln!(
                output,
                "Coverage {}: {} events · {}",
                display_client_name(client.client.as_str()),
                format_count(client.canonical_event_count),
                window
            );
        }
        let _ = writeln!(output, "Catalog SHA-256: {}", status.sha256);
    } else {
        let _ = writeln!(
            output,
            "{}",
            terminal.paint(
                Tone::Muted,
                "Paths and coverage windows: rerun with --details"
            )
        );
    }
    terminal.emit_stdout(&output)?;
    Ok(())
}

fn command_day(config: &Config, args: DayArgs) -> Result<()> {
    let timezone = resolve_timezone(config, args.tz.as_deref())?;
    let date = parse_local_date(&args.date, timezone)?;
    command_report_period(
        config,
        date,
        date,
        timezone,
        GroupBy::day_model_client(),
        &args.filters,
        args.no_scan,
        &args.output,
    )
}

fn command_today(config: &Config, args: TodayArgs) -> Result<()> {
    let timezone = resolve_timezone(config, args.tz.as_deref())?;
    let date = date_keyword_at("today", timezone, Utc::now())?;
    command_report_period(
        config,
        date,
        date,
        timezone,
        GroupBy::day_model_client(),
        &args.filters,
        args.no_scan,
        &args.output,
    )
}

fn command_cost(config: &Config, args: CostArgs) -> Result<()> {
    let timezone = resolve_timezone(config, args.tz.as_deref())?;
    let selection = if args.all {
        CostPeriodSelection::AllLocalHistory
    } else if args.today {
        CostPeriodSelection::Today
    } else if args.yesterday {
        CostPeriodSelection::Yesterday
    } else if args.month {
        CostPeriodSelection::CurrentMonth
    } else if let (Some(start), Some(end)) = (args.start.as_deref(), args.end.as_deref()) {
        CostPeriodSelection::ExplicitRange {
            start: parse_local_date(start, timezone)?,
            end: parse_local_date(end, timezone)?,
        }
    } else {
        // `--today` is intentionally optional because today is the useful,
        // safe default. Clap rejects combinations before this point.
        CostPeriodSelection::Today
    };

    let mut ledger = open_ledger(config)?;
    maybe_refresh(&mut ledger, config, args.no_scan, !args.output.is_machine())?;
    let pricing = load_pricing(config)?;
    let mut events = ledger.canonical_events(None, None)?;
    apply_event_filters(&mut events, &args.filters, &pricing);
    let period = resolve_cost_period(selection, timezone, Utc::now(), &events)?;
    if let Some((start, end)) = period.start_utc.zip(period.end_utc_exclusive) {
        events.retain(|event| event.occurred_at >= start && event.occurred_at < end);
    }

    let coverage = ledger.coverage_snapshot()?;
    let providers = cost_provider_scope(&events, &args.filters);
    let billing = match period.start_utc.zip(period.end_utc_exclusive) {
        Some((start, end)) => CostBilling::from_evidence(
            &config.billing_evidence,
            token_ledger::billing::BillingWindow::new(start, end)?,
            &providers,
        )?,
        None => CostBilling::unavailable(
            "No matching local event bounds exist for this all-history query, so no cash billing window was invented.",
        ),
    };

    let selected_models = events
        .iter()
        .map(|event| canonical_model_name(&pricing, event))
        .collect::<BTreeSet<_>>();
    let reconciliation = match period.start_utc.zip(period.end_utc_exclusive) {
        Some((start, end)) => {
            let report = reconciliation_report(&ledger, Some(start), Some(end), timezone)?;
            CostReconciliation::from_report(
                &report,
                &providers,
                &args.filters.model,
                &selected_models,
            )
        }
        None => CostReconciliation::unavailable(
            "No matching local event bounds exist for this all-history query, so reconciliation was not assigned an invented interval.",
        ),
    };

    let query = CostQuery {
        period,
        timezone: timezone.to_string(),
        client_filters: args
            .filters
            .client
            .iter()
            .map(|client| client.as_str().to_string())
            .collect(),
        model_filters: args.filters.model.clone(),
        scope_note: "Usage filters apply to local events and catalog estimates. Cash evidence is restricted by provider and time because account/subscription cash cannot be truthfully allocated to an individual model. Reconciliation remains an independent comparison layer."
            .to_string(),
    };
    let document =
        build_cost_document(&events, query, &pricing, coverage, billing, reconciliation)?;
    if args.output.json {
        println!("{}", serde_json::to_string_pretty(&document)?);
    } else if let Some(path) = args.output.html.as_deref() {
        if path == Path::new("-") {
            print!("{}", write_cost_html(&document, None)?);
        } else {
            write_cost_html(&document, Some(path))?;
            emit_success(
                "HTML REPORT WRITTEN",
                &format!(
                    "{} model row(s){}{}",
                    format_count(document.models.len() as u64),
                    terminal_ui().separator(),
                    path.display()
                ),
            )?;
        }
    } else {
        terminal_ui().emit_stdout(&render_cost_with_options(&document, terminal_ui()))?;
    }
    Ok(())
}

fn cost_provider_scope(events: &[CanonicalEvent], filters: &ReportFilters) -> BTreeSet<String> {
    let mut providers = events
        .iter()
        .map(|event| event.provider.trim().to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    if providers.is_empty() && !filters.client.is_empty() {
        providers.extend(filters.client.iter().map(|client| match client {
            Client::ClaudeCode => "anthropic".to_string(),
            Client::OpenaiCodex => "openai".to_string(),
        }));
    }
    if providers.is_empty() {
        providers.extend(["anthropic".to_string(), "openai".to_string()]);
    }
    providers
}

#[allow(clippy::too_many_arguments)]
fn command_report_period(
    config: &Config,
    start_date: NaiveDate,
    end_date: NaiveDate,
    timezone: Tz,
    group_by: GroupBy,
    filters: &ReportFilters,
    no_scan: bool,
    output: &ReportOutputArgs,
) -> Result<()> {
    let mut ledger = open_ledger(config)?;
    maybe_refresh(&mut ledger, config, no_scan, !output.is_machine())?;
    let (start, end) = local_range_bounds(start_date, end_date, timezone)?;
    let mut events = ledger.canonical_events(Some(start), Some(end))?;
    let coverage = ledger.coverage_snapshot()?;
    let pricing = load_pricing(config)?;
    apply_event_filters(&mut events, filters, &pricing);
    let rows = aggregate(&events, timezone, group_by, &pricing);
    let mut document = build_report_document(
        &events,
        rows,
        start_date,
        end_date,
        timezone,
        start,
        end,
        group_by,
        pricing.status(),
        coverage,
    );
    apply_filter_metadata(&mut document, filters);
    emit_report(&document, output)
}

fn command_range(config: &Config, args: RangeArgs) -> Result<()> {
    let timezone = resolve_timezone(config, args.tz.as_deref())?;
    let start_date = parse_local_date(&args.start, timezone)?;
    let end_date = parse_local_date(&args.end, timezone)?;
    let group_by = GroupBy::parse(&args.group_by)?;
    command_report_period(
        config,
        start_date,
        end_date,
        timezone,
        group_by,
        &args.filters,
        args.no_scan,
        &args.output,
    )
}

fn command_sessions(config: &Config, args: SessionsArgs) -> Result<()> {
    let timezone = resolve_timezone(config, args.tz.as_deref())?;
    let date = parse_local_date(&args.date, timezone)?;
    let mut ledger = open_ledger(config)?;
    maybe_refresh(&mut ledger, config, args.no_scan, !args.output.json)?;
    let (start, end) = local_day_bounds(date, timezone)?;
    let mut events = ledger.canonical_events(Some(start), Some(end))?;
    let pricing = load_pricing(config)?;
    apply_event_filters(&mut events, &args.filters, &pricing);
    let rows = sessions(&events, config.show_raw_ids);
    if args.output.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        let terminal = terminal_ui();
        let requests = rows.iter().map(|row| row.requests).sum::<u64>();
        let input = rows.iter().map(|row| row.input_tokens).sum::<u64>();
        let output_tokens = rows.iter().map(|row| row.output_tokens).sum::<u64>();
        let mut text = String::new();
        let _ = writeln!(
            text,
            "{}",
            terminal.paint(Tone::Accent, "TOKEN LEDGER / SESSIONS")
        );
        let _ = writeln!(
            text,
            "{}",
            terminal.paint(
                Tone::Muted,
                format!("{date}{}{}", terminal.separator(), timezone)
            )
        );
        let _ = writeln!(
            text,
            "\n{} sessions{}{} requests{}{} input{}{} output\n",
            terminal.paint(Tone::Strong, format_count(rows.len() as u64)),
            terminal.separator(),
            format_count(requests),
            terminal.separator(),
            format_count_compact(input),
            terminal.separator(),
            format_count_compact(output_tokens)
        );
        if terminal.layout() == token_ledger::terminal::Layout::Narrow {
            for row in rows {
                let _ = writeln!(text, "{}", terminal.paint(Tone::Strong, row.session));
                let _ = writeln!(text, "  Client:   {}", display_client_name(&row.client));
                let _ = writeln!(text, "  Requests: {}", format_count(row.requests));
                let _ = writeln!(
                    text,
                    "  Tokens:   {}",
                    format_count(row.input_tokens.saturating_add(row.output_tokens))
                );
                let _ = writeln!(
                    text,
                    "  Models:   {}",
                    row.models
                        .iter()
                        .map(|model| display_model_name(model))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        } else {
            let table_rows = rows.into_iter().map(|row| {
                vec![
                    row.session,
                    display_client_name(&row.client),
                    format_count(row.requests),
                    format_count(row.input_tokens.saturating_add(row.output_tokens)),
                    row.models
                        .iter()
                        .map(|model| display_model_name(model))
                        .collect::<Vec<_>>()
                        .join(", "),
                ]
            });
            let _ = writeln!(
                text,
                "{}",
                terminal.table(
                    &["SESSION", "CLIENT", "REQUESTS", "TOKENS", "MODELS"],
                    table_rows,
                    &[2, 3]
                )
            );
        }
        terminal.emit_stdout(&text)?;
    }
    Ok(())
}

fn command_models(config: &Config, args: OutputArgs) -> Result<()> {
    let ledger = open_ledger(config)?;
    let events = ledger.canonical_events(None, None)?;
    let mut models: BTreeMap<(String, String), (u64, UsageVector)> = BTreeMap::new();
    for event in events {
        let entry = models
            .entry((event.client.as_str().to_string(), event.raw_model))
            .or_default();
        entry.0 += 1;
        add_usage(&mut entry.1, &event.usage);
    }
    if args.json {
        let rows: Vec<_> = models
            .into_iter()
            .map(|((client, model), (events, usage))| {
                serde_json::json!({"client":client,"model":model,"events":events,"usage":usage})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        let terminal = terminal_ui();
        let model_count = models.len() as u64;
        let event_count = models.values().map(|(events, _)| events).sum::<u64>();
        let mut text = String::new();
        let _ = writeln!(
            text,
            "{}",
            terminal.paint(Tone::Accent, "TOKEN LEDGER / MODELS")
        );
        let _ = writeln!(
            text,
            "{} model routes{}{} events\n",
            terminal.paint(Tone::Strong, format_count(model_count)),
            terminal.separator(),
            format_count(event_count)
        );
        if terminal.layout() == token_ledger::terminal::Layout::Narrow {
            for ((client, model), (events, usage)) in models {
                let _ = writeln!(text, "{}", display_model_name(&model));
                let _ = writeln!(text, "  Client: {}", display_client_name(&client));
                let _ = writeln!(text, "  Events: {}", format_count(events));
                let _ = writeln!(
                    text,
                    "  Tokens: {}",
                    format_count(
                        usage
                            .input_tokens_total
                            .saturating_add(usage.output_tokens_total)
                    )
                );
            }
        } else {
            let table_rows = models
                .into_iter()
                .map(|((client, model), (events, usage))| {
                    vec![
                        display_model_name(&model),
                        display_client_name(&client),
                        format_count(events),
                        format_count(usage.input_tokens_total),
                        format_count(usage.output_tokens_total),
                    ]
                });
            let _ = writeln!(
                text,
                "{}",
                terminal.table(
                    &["MODEL", "CLIENT", "EVENTS", "INPUT", "OUTPUT"],
                    table_rows,
                    &[2, 3, 4]
                )
            );
        }
        terminal.emit_stdout(&text)?;
    }
    Ok(())
}

fn command_explain(config: &Config, args: ExplainArgs) -> Result<()> {
    let ledger = open_ledger(config)?;
    let event = ledger
        .event_by_id(&args.event)?
        .with_context(|| format!("event {} was not found", args.event))?;
    let provenance = ledger
        .event_provenance(&args.event)?
        .with_context(|| format!("event {} has no stored provenance", args.event))?;
    let pricing = load_pricing(config)?;
    let estimate = pricing.estimate_event(&event);
    let event = event_for_display(event, config.show_raw_ids);
    if args.output.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "event": event,
                "provenance": provenance,
                "estimate": estimate,
                "catalog_version": pricing.catalog().revision(),
            }))?
        );
    } else {
        let terminal = terminal_ui();
        let mut text = String::new();
        let _ = writeln!(
            text,
            "{}",
            terminal.paint(Tone::Accent, "TOKEN LEDGER / EVENT EXPLAIN")
        );
        let _ = writeln!(text, "{}", terminal.paint(Tone::Muted, &event.event_id));
        let _ = writeln!(
            text,
            "{}{}{}{}{}{}{}",
            event.occurred_at.format("%Y-%m-%d %H:%M:%S UTC"),
            terminal.separator(),
            display_client_name(event.client.as_str()),
            terminal.separator(),
            event.provider,
            terminal.separator(),
            display_model_name(&event.raw_model)
        );
        let _ = writeln!(
            text,
            "Quality {}{}coverage {}{}{} contributing sources\n",
            event.quality.as_str(),
            terminal.separator(),
            event.coverage.as_str(),
            terminal.separator(),
            format_count(provenance.source_count)
        );
        let usage_rows = vec![
            vec![
                "Input total".to_string(),
                format_count(event.usage.input_tokens_total),
            ],
            vec![
                "Uncached input".to_string(),
                format_count(event.usage.input_tokens_uncached),
            ],
            vec![
                "Cached input".to_string(),
                format_count(event.usage.input_tokens_cached),
            ],
            vec![
                "Cache writes".to_string(),
                format_count(event.usage.cache_write_tokens()),
            ],
            vec![
                "Output".to_string(),
                format_count(event.usage.output_tokens_total),
            ],
            vec![
                "Reasoning".to_string(),
                format_count(event.usage.reasoning_output_tokens),
            ],
        ];
        let _ = writeln!(text, "{}", terminal.paint(Tone::Accent, "USAGE"));
        let _ = writeln!(
            text,
            "{}\n",
            terminal.table(&["COUNTER", "TOKENS"], usage_rows, &[1])
        );
        let usd = estimate
            .api_equivalent_usd
            .map(|value| format_decimal(value, true))
            .or_else(|| {
                estimate.known_api_equivalent_usd.map(|value| {
                    format!(
                        "{}{}",
                        if terminal.unicode { "≥" } else { ">=" },
                        format_decimal(value, true)
                    )
                })
            })
            .unwrap_or_else(|| "Not priced".to_string());
        let (usd_label, usd_tone) = measure_label_tone(estimate.api_equivalent_usd_measure.status);
        let _ = writeln!(text, "{}", terminal.paint(Tone::Accent, "PRICING"));
        let _ = writeln!(
            text,
            "API equivalent   {usd}  {}",
            terminal.badge(usd_label, usd_tone)
        );
        if let Some(unit) = estimate.provider_unit_name.as_deref() {
            let amount = estimate
                .provider_units
                .map(|value| format_decimal(value, false))
                .or_else(|| {
                    estimate.known_provider_units.map(|value| {
                        format!(
                            "{}{}",
                            if terminal.unicode { "≥" } else { ">=" },
                            format_decimal(value, false)
                        )
                    })
                })
                .unwrap_or_else(|| "Unavailable".to_string());
            let (label, tone) = measure_label_tone(estimate.provider_unit_measure.status);
            let _ = writeln!(
                text,
                "Provider units   {amount} {unit}  {}",
                terminal.badge(label, tone)
            );
        }
        if !estimate.missing_components.is_empty() {
            let _ = writeln!(text, "\n{}", terminal.paint(Tone::Warning, "PRICING GAPS"));
            for missing in &estimate.missing_components {
                let _ = writeln!(
                    text,
                    "{} {}{}{}",
                    terminal.paint(Tone::Warning, terminal.status_symbol(Tone::Warning)),
                    missing.component,
                    terminal.separator(),
                    missing.reason
                );
            }
        }
        if !estimate.explanation.is_empty() {
            let _ = writeln!(text, "\n{}", terminal.paint(Tone::Accent, "PRICE MATH"));
            for explanation in &estimate.explanation {
                let _ = writeln!(text, "  - {explanation}");
            }
        }
        let _ = writeln!(text, "\n{}", terminal.paint(Tone::Accent, "PROVENANCE"));
        let _ = writeln!(
            text,
            "{} observations{}{} duplicate contributions",
            format_count(provenance.observation_count),
            terminal.separator(),
            format_count(provenance.deduplicated_observation_count)
        );
        let rows = provenance.observations.iter().map(|observation| {
            vec![
                observation
                    .occurred_at
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
                observation.source_locator.clone(),
                observation.parser_version.clone(),
                format_count(observation.usage.input_tokens_total),
                format_count(observation.usage.output_tokens_total),
            ]
        });
        let _ = writeln!(
            text,
            "{}",
            terminal.table(
                &["OBSERVED (UTC)", "SOURCE", "PARSER", "INPUT", "OUTPUT"],
                rows,
                &[3, 4]
            )
        );
        let _ = writeln!(
            text,
            "Catalog {}{}{} matching evidence record(s)",
            pricing.catalog().revision(),
            terminal.separator(),
            estimate.pricing_evidence.len()
        );
        terminal.emit_stdout(&text)?;
    }
    Ok(())
}

fn command_prices(config: &mut Config, config_path: &Path, args: PricesArgs) -> Result<()> {
    match args.command {
        PriceCommand::Status(output) => {
            let pricing = load_pricing(config)?;
            let status = pricing.status();
            if output.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                let terminal = terminal_ui();
                let tone = if status.verification.is_valid() {
                    Tone::Success
                } else {
                    Tone::Error
                };
                let mut text = String::new();
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.paint(Tone::Accent, "TOKEN LEDGER / PRICE CATALOG")
                );
                let _ = writeln!(
                    text,
                    "{} {}  {}",
                    terminal.paint(Tone::Strong, &status.revision),
                    terminal.badge(
                        if status.verification.is_valid() {
                            "VERIFIED"
                        } else {
                            "INVALID"
                        },
                        tone
                    ),
                    terminal.badge(
                        catalog_freshness_text(status.freshness)
                            .to_ascii_uppercase()
                            .as_str(),
                        if status.freshness == CatalogFreshness::Fresh {
                            Tone::Success
                        } else {
                            Tone::Warning
                        }
                    )
                );
                let _ = writeln!(
                    text,
                    "\n{} aliases{}{} rates{}{} modifiers{}{} cited sources",
                    format_count(status.alias_count as u64),
                    terminal.separator(),
                    format_count(status.rate_count as u64),
                    terminal.separator(),
                    format_count(status.modifier_count as u64),
                    terminal.separator(),
                    format_count(status.source_count as u64)
                );
                let _ = writeln!(
                    text,
                    "Published {}{}verified {}{}stale after {}",
                    status.published_at.format("%Y-%m-%d"),
                    terminal.separator(),
                    status.verified_at.format("%Y-%m-%d"),
                    terminal.separator(),
                    status.stale_at.format("%Y-%m-%d")
                );
                if terminal.details {
                    let _ = writeln!(text, "SHA-256: {}", status.sha256);
                } else {
                    let _ = writeln!(
                        text,
                        "{}",
                        terminal.paint(Tone::Muted, "Full digest: rerun with --details")
                    );
                }
                terminal.emit_stdout(&text)?;
            }
        }
        PriceCommand::List(output) => {
            let pricing = load_pricing(config)?;
            if output.json {
                println!("{}", String::from_utf8_lossy(pricing.catalog().raw_bytes()));
            } else {
                let terminal = terminal_ui();
                let mut text = String::new();
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.paint(Tone::Accent, "TOKEN LEDGER / PRICE RULES")
                );
                let _ = writeln!(
                    text,
                    "Catalog {}{}{} effective rate rules\n",
                    pricing.catalog().revision(),
                    terminal.separator(),
                    format_count(pricing.catalog().rates().len() as u64)
                );
                let rows = pricing.catalog().rates().iter().map(|rule| {
                    let currency = rule.kind == RateKind::UsdApiEquivalent;
                    let rate = |value: Option<token_ledger::pricing::ExactDecimal>| {
                        value.map_or_else(
                            || "—".to_string(),
                            |value| {
                                let amount = value.0.normalize();
                                if currency {
                                    format!("${amount}")
                                } else {
                                    amount.to_string()
                                }
                            },
                        )
                    };
                    vec![
                        display_model_name(&rule.canonical_model),
                        display_client_name(&rule.provider),
                        rule.kind.unit_name().to_string(),
                        rate(rule.rates.input),
                        rate(rule.rates.cache_read),
                        rate(rule.rates.output),
                        format_count(rule.unit_scale),
                        rule.interval.effective_from.format("%Y-%m-%d").to_string(),
                    ]
                });
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.table(
                        &[
                            "MODEL",
                            "PROVIDER",
                            "MEASURE",
                            "INPUT",
                            "CACHED",
                            "OUTPUT",
                            "PER",
                            "EFFECTIVE"
                        ],
                        rows,
                        &[3, 4, 5, 6]
                    )
                );
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.paint(
                        Tone::Muted,
                        "Use --json for cache-write, tool, selector, and complete rule metadata."
                    )
                );
                terminal.emit_stdout(&text)?;
            }
        }
        PriceCommand::Verify(output) => {
            let pricing = load_pricing(config)?;
            let report = pricing.verify();
            if output.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let terminal = terminal_ui();
                let tone = if report.is_valid() {
                    Tone::Success
                } else {
                    Tone::Error
                };
                let mut text = String::new();
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.paint(Tone::Accent, "TOKEN LEDGER / VERIFY CATALOG")
                );
                let _ = writeln!(
                    text,
                    "{} {} {}{}{} error(s){}{} warning(s)",
                    terminal.paint(tone, terminal.status_symbol(tone)),
                    pricing.catalog().revision(),
                    terminal.badge(
                        if report.is_valid() {
                            "PASSED"
                        } else {
                            "FAILED"
                        },
                        tone
                    ),
                    terminal.separator(),
                    format_count(report.error_count() as u64),
                    terminal.separator(),
                    format_count(report.warning_count() as u64)
                );
                for issue in &report.issues {
                    let issue_tone = match issue.severity {
                        VerificationSeverity::Error => Tone::Error,
                        VerificationSeverity::Warning => Tone::Warning,
                    };
                    let _ = writeln!(
                        text,
                        "{} {}{}{}",
                        terminal.paint(issue_tone, terminal.status_symbol(issue_tone)),
                        issue.code,
                        terminal.separator(),
                        issue.message
                    );
                }
                terminal.emit_stdout(&text)?;
            }
            if !report.is_valid() {
                anyhow::bail!("price catalog verification failed");
            }
        }
        PriceCommand::History(output) => {
            let active_path = active_catalog_path(config)?;
            let retained = PricingEngine::retained_revisions(&active_path)?;
            if output.json {
                println!("{}", serde_json::to_string_pretty(&retained)?);
            } else if retained.is_empty() {
                terminal_ui().emit_stdout(
                    "TOKEN LEDGER / CATALOG HISTORY\nNo retained catalog revisions.\n",
                )?;
            } else {
                let terminal = terminal_ui();
                let mut text = String::new();
                let _ = writeln!(
                    text,
                    "{}\n",
                    terminal.paint(Tone::Accent, "TOKEN LEDGER / CATALOG HISTORY")
                );
                let rows = retained.into_iter().map(|revision| {
                    vec![
                        revision.revision,
                        revision.sha256.chars().take(16).collect::<String>(),
                    ]
                });
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.table(&["REVISION", "SHA-256 PREFIX"], rows, &[])
                );
                terminal.emit_stdout(&text)?;
            }
        }
        PriceCommand::Check(args) => {
            let engine = load_active_pricing(config)?;
            let candidate = load_catalog_candidate(
                config,
                args.source.as_deref(),
                args.sha256.as_deref(),
                args.official,
            )?;
            let catalog = if let Some(manifest) = candidate.manifest.as_ref() {
                manifest.verify_catalog(&candidate.bytes)?
            } else {
                token_ledger::pricing::PriceCatalog::parse(&candidate.bytes)?
            };
            let check =
                engine.check_candidate(&candidate.bytes, candidate.expected_sha256.as_deref())?;
            let output = CatalogCheckOutput {
                check,
                manifest: candidate.manifest,
                trust: candidate.trust,
            };
            // Keep the manifest-to-catalog binding explicit even though
            // check_candidate independently repeats schema verification.
            anyhow::ensure!(
                catalog.sha256() == output.check.candidate_sha256,
                "candidate bytes changed during verification"
            );
            if args.output.json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                print_catalog_check(&output)?;
            }
        }
        PriceCommand::Update(update) => {
            let candidate = load_catalog_candidate(
                config,
                update.source.as_deref(),
                update.sha256.as_deref(),
                update.official,
            )?;
            let engine = load_active_pricing(config)?;
            let destination = active_catalog_path(config)?;
            let receipt = if let Some(manifest) = candidate.manifest.as_ref() {
                let catalog = manifest.verify_catalog(&candidate.bytes)?;
                engine.install_validated_candidate(&catalog, &destination)?
            } else {
                engine.install_candidate(
                    &candidate.bytes,
                    candidate.expected_sha256.as_deref(),
                    &destination,
                )?
            };
            config.price_catalog = Some(destination.clone());
            config.save(config_path)?;
            if update.output.json {
                println!("{}", serde_json::to_string_pretty(&receipt)?);
            } else {
                print_install_receipt("INSTALLED NEWER CATALOG", &receipt, None, None)?;
            }
        }
        PriceCommand::Diff(args) => {
            let active_path = active_catalog_path(config)?;
            let diff = PricingEngine::diff_revisions(
                &active_path,
                &args.from_revision,
                &args.to_revision,
            )?;
            if args.output.json {
                println!("{}", serde_json::to_string_pretty(&diff)?);
            } else {
                let terminal = terminal_ui();
                let mut text = String::new();
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.paint(Tone::Accent, "TOKEN LEDGER / CATALOG DIFF")
                );
                let _ = writeln!(text, "{} -> {}", diff.from_revision, diff.to_revision);
                let _ = writeln!(
                    text,
                    "SHA-256 {} -> {}",
                    &diff.from_sha256[..16],
                    &diff.to_sha256[..16]
                );
                if diff.metadata_changed.is_empty() {
                    let _ = writeln!(text, "Metadata: unchanged");
                } else {
                    let _ = writeln!(
                        text,
                        "Metadata changed: {}",
                        diff.metadata_changed.join(", ")
                    );
                }
                append_collection_diff(&mut text, "Sources", &diff.sources);
                append_collection_diff(&mut text, "Aliases", &diff.aliases);
                append_collection_diff(&mut text, "Rates", &diff.rates);
                append_collection_diff(&mut text, "Modifiers", &diff.modifiers);
                terminal.emit_stdout(&text)?;
            }
        }
        PriceCommand::Activate(args) => {
            let engine = load_active_pricing(config)?;
            let replaced = engine.catalog().revision().to_string();
            let destination = active_catalog_path(config)?;
            let receipt = engine.activate_revision(&args.revision, &destination)?;
            config.price_catalog = Some(destination);
            config.save(config_path)?;
            if args.output.json {
                println!("{}", serde_json::to_string_pretty(&receipt)?);
            } else {
                print_install_receipt(
                    "ACTIVATED HISTORICAL CATALOG",
                    &receipt,
                    Some(&replaced),
                    Some(
                        "Historical activation was explicit; future updates still reject accidental downgrades.",
                    ),
                )?;
            }
        }
        PriceCommand::Rollback(output) => {
            let engine = load_active_pricing(config)?;
            let replaced = engine.catalog().revision().to_string();
            let destination = active_catalog_path(config)?;
            let receipt = engine.rollback(&destination)?;
            config.price_catalog = Some(destination);
            config.save(config_path)?;
            if output.json {
                println!("{}", serde_json::to_string_pretty(&receipt)?);
            } else {
                print_install_receipt(
                    "ROLLED BACK CATALOG",
                    &receipt,
                    Some(&replaced),
                    Some(
                        "Rollback selected the newest verified retained revision older than the replaced catalog.",
                    ),
                )?;
            }
        }
    }
    Ok(())
}

fn command_reconcile(config: &Config, args: ReconcileArgs) -> Result<()> {
    match args.command {
        ReconcileCommand::Import(args) => {
            let format = ReconciliationImportFormat::from_str(&args.format)?;
            let mut ledger = open_ledger(config)?;
            let receipt = import_reconciliation_path(&mut ledger, &args.path, format)?;
            if args.output.json {
                println!("{}", serde_json::to_string_pretty(&receipt)?);
            } else {
                let terminal = terminal_ui();
                let tone = if receipt.imported {
                    Tone::Success
                } else {
                    Tone::Accent
                };
                let mut text = String::new();
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.paint(Tone::Accent, "TOKEN LEDGER / RECONCILIATION IMPORT")
                );
                let _ = writeln!(
                    text,
                    "{} {}{}{} bucket(s){}{}",
                    terminal.paint(tone, terminal.status_symbol(tone)),
                    terminal.badge(
                        if receipt.imported {
                            "IMPORTED"
                        } else {
                            "UNCHANGED"
                        },
                        tone
                    ),
                    terminal.separator(),
                    format_count(receipt.bucket_count),
                    terminal.separator(),
                    receipt.source_kind
                );
                if terminal.details {
                    let _ = writeln!(text, "Evidence SHA-256: {}", receipt.content_digest);
                    let _ = writeln!(text, "{}", receipt.note);
                } else {
                    let _ = writeln!(
                        text,
                        "{}",
                        terminal.paint(Tone::Muted, "Evidence digest: rerun with --details")
                    );
                }
                terminal.emit_stdout(&text)?;
            }
        }
        ReconcileCommand::Status(output) => {
            let ledger = open_ledger(config)?;
            let status = reconciliation_status(&ledger)?;
            if output.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                let terminal = terminal_ui();
                let mut text = String::new();
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.paint(Tone::Accent, "TOKEN LEDGER / RECONCILIATION")
                );
                let _ = writeln!(
                    text,
                    "{} imports{}{} buckets{}providers {}",
                    terminal.paint(Tone::Strong, format_count(status.import_count)),
                    terminal.separator(),
                    format_count(status.bucket_count),
                    terminal.separator(),
                    if status.providers.is_empty() {
                        "none".to_string()
                    } else {
                        status.providers.join(", ")
                    }
                );
                let _ = writeln!(
                    text,
                    "Coverage: {}",
                    match (status.earliest_bucket_start, status.latest_bucket_end) {
                        (Some(start), Some(end)) => format!("{start} through {end}"),
                        _ => "No provider buckets imported".to_string(),
                    }
                );
                if !status.latest_imports.is_empty() {
                    let rows = status.latest_imports.into_iter().map(|import| {
                        vec![
                            import.source_kind,
                            format_count(import.bucket_count),
                            import.imported_at.format("%Y-%m-%d %H:%M UTC").to_string(),
                            import.content_digest.chars().take(16).collect::<String>(),
                        ]
                    });
                    let _ = writeln!(
                        text,
                        "\n{}",
                        terminal.paint(Tone::Accent, "LATEST EVIDENCE")
                    );
                    let _ = writeln!(
                        text,
                        "{}",
                        terminal.table(
                            &["SOURCE KIND", "BUCKETS", "IMPORTED", "SHA-256 PREFIX"],
                            rows,
                            &[1]
                        )
                    );
                }
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.paint(
                        Tone::Muted,
                        "Organization exports do not prove individual subscription usage."
                    )
                );
                terminal.emit_stdout(&text)?;
            }
        }
        ReconcileCommand::Report(args) => {
            let timezone = resolve_timezone(config, args.tz.as_deref())?;
            let start_date = args
                .start
                .as_deref()
                .map(|value| parse_local_date(value, timezone))
                .transpose()?;
            let end_date = args
                .end
                .as_deref()
                .map(|value| parse_local_date(value, timezone))
                .transpose()?;
            if let (Some(start), Some(end)) = (start_date, end_date)
                && end < start
            {
                anyhow::bail!("reconciliation report end date must not precede start date");
            }
            let start = start_date
                .map(|date| local_day_bounds(date, timezone).map(|bounds| bounds.0))
                .transpose()?;
            let end = end_date
                .map(|date| local_day_bounds(date, timezone).map(|bounds| bounds.1))
                .transpose()?;
            let mut ledger = open_ledger(config)?;
            maybe_refresh(&mut ledger, config, args.no_scan, !args.output.json)?;
            let report = reconciliation_report(&ledger, start, end, timezone)?;
            if args.output.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let terminal = terminal_ui();
                let mut text = String::new();
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.paint(Tone::Accent, "TOKEN LEDGER / RECONCILIATION REPORT")
                );
                let _ = writeln!(text, "{}\n", terminal.paint(Tone::Muted, timezone));
                let summary_rows = vec![
                    vec!["Matched".to_string(), format_count(report.summary.matched)],
                    vec![
                        "Counter mismatch".to_string(),
                        format_count(report.summary.counter_mismatch),
                    ],
                    vec![
                        "Local only".to_string(),
                        format_count(report.summary.local_only),
                    ],
                    vec![
                        "Provider only".to_string(),
                        format_count(report.summary.provider_only),
                    ],
                    vec![
                        "Time boundary".to_string(),
                        format_count(report.summary.time_boundary),
                    ],
                    vec![
                        "Route unknown".to_string(),
                        format_count(report.summary.route_unknown),
                    ],
                ];
                let _ = writeln!(
                    text,
                    "{}\n",
                    terminal.table(&["CLASSIFICATION", "COUNT"], summary_rows, &[1])
                );
                if !report.rows.is_empty() {
                    let rows = report.rows.iter().map(|row| {
                        vec![
                            row.bucket_start_utc.format("%Y-%m-%d %H:%M").to_string(),
                            row.provider.clone(),
                            display_model_name(row.model.as_deref().unwrap_or("All models")),
                            row.classification.as_str().to_string(),
                            row.local
                                .as_ref()
                                .map(|side| reconciliation_total_text(&side.counters))
                                .unwrap_or_else(|| "—".to_string()),
                            row.provider_evidence
                                .as_ref()
                                .map(|side| reconciliation_total_text(&side.counters))
                                .unwrap_or_else(|| "—".to_string()),
                        ]
                    });
                    let _ = writeln!(
                        text,
                        "{}",
                        terminal.table(
                            &[
                                "BUCKET (UTC)",
                                "PROVIDER",
                                "MODEL",
                                "CLASSIFICATION",
                                "LOCAL",
                                "PROVIDER"
                            ],
                            rows,
                            &[4, 5]
                        )
                    );
                }
                let _ = writeln!(
                    text,
                    "{}",
                    terminal.paint(
                        Tone::Muted,
                        "Missing provider fields remain unknown, not zero; exports do not establish subscription billing."
                    )
                );
                terminal.emit_stdout(&text)?;
            }
        }
    }
    Ok(())
}

fn reconciliation_total_text(counters: &ReconciliationCounters) -> String {
    let values = [
        counters.input_tokens_uncached,
        counters.input_tokens_cached,
        counters.cache_write_5m_tokens,
        counters.cache_write_1h_tokens,
        counters.cache_write_unknown_tokens,
        counters.output_tokens,
    ];
    if values.iter().all(Option::is_none) {
        "unknown".to_string()
    } else {
        format_count(values.into_iter().flatten().sum::<u64>())
    }
}

fn measure_label_tone(status: MeasureStatus) -> (&'static str, Tone) {
    match status {
        MeasureStatus::Exact => ("EXACT", Tone::Success),
        MeasureStatus::Bounded => ("RANGE", Tone::Warning),
        MeasureStatus::Partial => ("AT LEAST", Tone::Warning),
        MeasureStatus::Unpriced => ("NOT PRICED", Tone::Error),
        MeasureStatus::Unavailable => ("UNAVAILABLE", Tone::Muted),
    }
}

fn command_export(config: &Config, args: ExportArgs) -> Result<()> {
    let timezone = resolve_timezone(config, args.tz.as_deref())?;
    let start_date = parse_local_date(&args.start, timezone)?;
    let end_date = parse_local_date(&args.end, timezone)?;
    let group_by = GroupBy::parse(&args.group_by)?;
    let mut ledger = open_ledger(config)?;
    maybe_refresh(&mut ledger, config, args.no_scan, false)?;
    let (start, end) = local_range_bounds(start_date, end_date, timezone)?;
    let mut events = ledger.canonical_events(Some(start), Some(end))?;
    let coverage = ledger.coverage_snapshot()?;
    let pricing = load_pricing(config)?;
    apply_event_filters(&mut events, &args.filters, &pricing);
    let rows = aggregate(&events, timezone, group_by, &pricing);
    let mut document = build_report_document(
        &events,
        rows,
        start_date,
        end_date,
        timezone,
        start,
        end,
        group_by,
        pricing.status(),
        coverage,
    );
    apply_filter_metadata(&mut document, &args.filters);
    let text = match args.format {
        ExportFormat::Json => write_report_json(&document, args.output.as_deref())?,
        ExportFormat::Csv => write_report_csv(&document, args.output.as_deref())?,
        ExportFormat::Html => write_report_html(&document, args.output.as_deref())?,
    };
    if let Some(path) = args.output {
        emit_success(
            "EXPORT WRITTEN",
            &format!(
                "{} data row(s){}{}",
                format_count(document.rows.len() as u64),
                terminal_ui().separator(),
                path.display()
            ),
        )?;
    } else {
        print!("{text}");
    }
    Ok(())
}

fn emit_report(document: &ReportDocument, output: &ReportOutputArgs) -> Result<()> {
    if output.json {
        println!("{}", write_report_json(document, None)?);
    } else if let Some(path) = output.html.as_deref() {
        if path == Path::new("-") {
            print!("{}", write_report_html(document, None)?);
        } else {
            write_report_html(document, Some(path))?;
            emit_success(
                "HTML REPORT WRITTEN",
                &format!(
                    "{} data row(s){}{}",
                    format_count(document.rows.len() as u64),
                    terminal_ui().separator(),
                    path.display()
                ),
            )?;
        }
    } else {
        terminal_ui().emit_stdout(&render_report_with_options(document, terminal_ui()))?;
    }
    Ok(())
}

fn apply_event_filters(
    events: &mut Vec<CanonicalEvent>,
    filters: &ReportFilters,
    pricing: &PricingEngine,
) {
    events.retain(|event| {
        let client_matches = filters.client.is_empty() || filters.client.contains(&event.client);
        let canonical_model = canonical_model_name(pricing, event);
        let model_matches = filters.model.is_empty()
            || filters.model.iter().any(|value| {
                value.eq_ignore_ascii_case(&event.raw_model)
                    || value.eq_ignore_ascii_case(&canonical_model)
            });
        client_matches && model_matches
    });
}

fn apply_filter_metadata(document: &mut ReportDocument, filters: &ReportFilters) {
    document.query.client_filters = filters
        .client
        .iter()
        .map(|client| client.as_str().to_string())
        .collect();
    document.query.model_filters = filters.model.clone();
}

fn command_purge(config: &Config, args: PurgeArgs) -> Result<()> {
    if !args.yes {
        anyhow::bail!("purge requires --yes; source client files are never affected");
    }
    let mut ledger = open_ledger(config)?;
    ledger.purge()?;
    emit_success(
        "LOCAL ACCOUNTING METADATA PURGED",
        &ledger.path().display().to_string(),
    )?;
    Ok(())
}

fn emit_success(title: &str, detail: &str) -> Result<()> {
    let terminal = terminal_ui();
    let text = format!(
        "{} {}\n{}\n",
        terminal.paint(Tone::Success, terminal.status_symbol(Tone::Success)),
        terminal.paint(Tone::Success, title),
        detail
    );
    terminal.emit_stdout(&text)?;
    Ok(())
}

fn open_ledger(config: &Config) -> Result<Ledger> {
    Ledger::open(&config.resolved_database_path()?)
}

fn open_scan_ledger(config: &Config, dry_run: bool) -> Result<Ledger> {
    if dry_run {
        Ledger::open_in_memory()
    } else {
        open_ledger(config)
    }
}

fn load_pricing(config: &Config) -> Result<PricingEngine> {
    let active = load_active_pricing(config)?;
    let selected_revision = config
        .catalog_revision_override
        .as_deref()
        .or(config.catalog_revision.as_deref());
    let engine = match selected_revision {
        Some(revision) if revision != active.catalog().revision() => {
            PricingEngine::load_revision(&active_catalog_path(config)?, revision)?
        }
        _ => active,
    };
    Ok(engine.with_dimension_overrides(config.pricing_dimension_overrides.clone())?)
}

fn load_active_pricing(config: &Config) -> Result<PricingEngine> {
    match config.price_catalog.as_deref() {
        Some(path) => PricingEngine::load(path)?,
        None => PricingEngine::bundled()?,
    }
    .with_dimension_overrides(config.pricing_dimension_overrides.clone())
    .map_err(Into::into)
}

fn active_catalog_path(config: &Config) -> Result<PathBuf> {
    Ok(config
        .price_catalog
        .clone()
        .unwrap_or(Config::project_dirs()?.config_dir().join("prices.json")))
}

fn run_scan(
    ledger: &mut Ledger,
    config: &Config,
    args: &ScanArgs,
) -> Result<token_ledger::scanner::ScanSummary> {
    let options = ScanOptions {
        clients: args.client.iter().copied().collect::<HashSet<_>>(),
        since: args.since.as_deref().map(parse_since).transpose()?,
        full: args.full,
        dry_run: args.dry_run,
    };
    scan(ledger, config, &built_in_adapters(), &options)
}

fn maybe_refresh(
    ledger: &mut Ledger,
    config: &Config,
    no_scan: bool,
    emit_success: bool,
) -> Result<()> {
    if !no_scan {
        let spinner = terminal_ui().spinner("Refreshing local session ledger…");
        let result = run_scan(ledger, config, &ScanArgs::default());
        if let Some(spinner) = spinner {
            spinner.finish_and_clear();
        }
        let summary = result?;
        if emit_success {
            print_scan_success(&summary)?;
        }
        print_scan_warnings(&summary)?;
    }
    Ok(())
}

fn print_scan_summary(summary: &token_ledger::scanner::ScanSummary) -> Result<()> {
    print_scan_success(summary)?;
    print_scan_warnings(summary)
}

fn print_scan_success(summary: &token_ledger::scanner::ScanSummary) -> Result<()> {
    let terminal = terminal_ui();
    let snapshot_tone = if summary.provisional {
        Tone::Warning
    } else {
        Tone::Success
    };
    let mut output = String::new();
    let _ = writeln!(
        output,
        "{} {}",
        terminal.paint(Tone::Success, terminal.status_symbol(Tone::Success)),
        terminal.paint(
            Tone::Success,
            if summary.dry_run {
                "DRY RUN COMPLETE"
            } else {
                "SCAN COMPLETE"
            }
        )
    );
    let _ = writeln!(
        output,
        "{} sources{}{} scanned{}{} unchanged{}{} observations{}{} replayed",
        format_count(summary.discovered_sources),
        terminal.separator(),
        format_count(summary.scanned_sources),
        terminal.separator(),
        format_count(summary.unchanged_sources),
        terminal.separator(),
        format_count(summary.observations),
        terminal.separator(),
        format_count(summary.reset_sources),
    );
    let _ = writeln!(
        output,
        "{} Snapshot {}{}{} active or volatile sources",
        terminal.paint(snapshot_tone, terminal.status_symbol(snapshot_tone)),
        summary
            .as_of
            .map(|value| value.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "as-of unavailable".to_string()),
        terminal.separator(),
        format_count(summary.active_or_volatile_source_count)
    );
    terminal.emit_stdout(&output)?;
    Ok(())
}

fn print_scan_warnings(summary: &token_ledger::scanner::ScanSummary) -> Result<()> {
    let terminal = terminal_ui();
    let mut output = String::new();
    if summary.warnings > 0 {
        let _ = writeln!(
            output,
            "{} Scan recorded {} warning(s); run `ledger doctor` for sanitized codes.",
            terminal.paint(Tone::Warning, terminal.status_symbol(Tone::Warning)),
            format_count(summary.warnings)
        );
    }
    if summary.provisional {
        let _ = writeln!(
            output,
            "{} Snapshot is provisional because {} source(s) were active or volatile.",
            terminal.paint(Tone::Warning, terminal.status_symbol(Tone::Warning)),
            format_count(summary.active_or_volatile_source_count)
        );
    }
    if !output.is_empty() {
        terminal.emit_stderr(&output)?;
    }
    Ok(())
}

fn sessions(events: &[CanonicalEvent], show_raw_ids: bool) -> Vec<SessionRow> {
    #[derive(Default)]
    struct Acc {
        client: String,
        first: Option<DateTime<Utc>>,
        last: Option<DateTime<Utc>>,
        requests: u64,
        models: BTreeSet<String>,
        input: u64,
        output: u64,
    }
    let mut groups: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for event in events {
        let client = event.client.as_str().to_string();
        let entry = groups
            .entry((client.clone(), event.session_id.clone()))
            .or_default();
        entry.client = client;
        entry.first = Some(
            entry
                .first
                .map_or(event.occurred_at, |value| value.min(event.occurred_at)),
        );
        entry.last = Some(
            entry
                .last
                .map_or(event.occurred_at, |value| value.max(event.occurred_at)),
        );
        entry.requests += 1;
        entry.models.insert(event.raw_model.clone());
        entry.input = entry.input.saturating_add(event.usage.input_tokens_total);
        entry.output = entry.output.saturating_add(event.usage.output_tokens_total);
    }
    groups
        .into_iter()
        .map(|((client, session), acc)| SessionRow {
            session: if show_raw_ids {
                session
            } else {
                stable_id(&["session", &client, &session])
                    .chars()
                    .take(16)
                    .collect()
            },
            client: acc.client,
            first_event_utc: acc.first.unwrap().to_rfc3339(),
            last_event_utc: acc.last.unwrap().to_rfc3339(),
            requests: acc.requests,
            models: acc.models.into_iter().collect(),
            input_tokens: acc.input,
            output_tokens: acc.output,
        })
        .collect()
}

fn event_for_display(mut event: CanonicalEvent, show_raw_ids: bool) -> CanonicalEvent {
    if show_raw_ids {
        return event;
    }

    let client = event.client.as_str();
    event.session_id = stable_id(&["session", client, &event.session_id]);
    event.event_key = stable_id(&["event-key", client, &event.event_key]);
    event.provider_message_id = event
        .provider_message_id
        .as_deref()
        .map(|value| stable_id(&["provider-message", client, value]));
    event.dimensions.provider_request_id = event
        .dimensions
        .provider_request_id
        .as_deref()
        .map(|value| stable_id(&["provider-request", client, value]));
    event
}

fn resolve_timezone(config: &Config, override_value: Option<&str>) -> Result<Tz> {
    parse_timezone(override_value.unwrap_or(&config.timezone))
}

fn parse_timezone(value: &str) -> Result<Tz> {
    Tz::from_str(value).with_context(|| format!("invalid IANA timezone '{value}'"))
}

fn parse_date(value: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .with_context(|| format!("invalid date '{value}'; expected YYYY-MM-DD"))
}

fn parse_local_date(value: &str, timezone: Tz) -> Result<NaiveDate> {
    date_keyword_at(value, timezone, Utc::now())
}

fn date_keyword_at(value: &str, timezone: Tz, now: DateTime<Utc>) -> Result<NaiveDate> {
    match value.trim().to_ascii_lowercase().as_str() {
        "today" => Ok(now.with_timezone(&timezone).date_naive()),
        "yesterday" => now
            .with_timezone(&timezone)
            .date_naive()
            .checked_sub_signed(Duration::days(1))
            .context("yesterday is outside the supported date range"),
        _ => NaiveDate::parse_from_str(value, "%Y-%m-%d").with_context(|| {
            format!("invalid date '{value}'; expected YYYY-MM-DD, today, or yesterday")
        }),
    }
}

fn parse_since(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    let date = parse_date(value)?;
    Ok(date.and_hms_opt(0, 0, 0).unwrap().and_utc())
}

fn parse_client(value: &str) -> Result<Client, String> {
    Client::from_str(value).map_err(|error| error.to_string())
}

fn catalog_freshness_text(value: CatalogFreshness) -> &'static str {
    match value {
        CatalogFreshness::Fresh => "fresh",
        CatalogFreshness::Stale => "stale",
        CatalogFreshness::FutureDated => "future-dated",
    }
}

fn display_scan_mode(value: &str) -> &str {
    match value {
        "full" => "rebuild",
        "dry_run" => "dry run",
        other => other,
    }
}

fn read_update_source(source: &str) -> Result<Vec<u8>> {
    if source.starts_with("https://") {
        let response = reqwest::blocking::get(source)
            .with_context(|| format!("failed to download catalog from {source}"))?
            .error_for_status()?;
        Ok(response.bytes()?.to_vec())
    } else if source.starts_with("http://") {
        anyhow::bail!("catalog updates require HTTPS or a local file")
    } else {
        std::fs::read(source).with_context(|| format!("failed to read catalog {source}"))
    }
}

fn load_catalog_candidate(
    config: &Config,
    source: Option<&str>,
    expected_sha256: Option<&str>,
    official: bool,
) -> Result<LoadedCatalogCandidate> {
    if official {
        anyhow::ensure!(
            source.is_none() && expected_sha256.is_none(),
            "--official cannot be combined with --from or --sha256"
        );
        let manifest_source = config
            .official_price_manifest
            .as_deref()
            .context(
                "--official requires official_price_manifest in the configuration; no network request was made",
            )?;
        let manifest_sha256 = config
            .official_price_manifest_sha256
            .as_deref()
            .context(
                "--official requires official_price_manifest_sha256 in the configuration; no network request was made",
            )?;
        let manifest_bytes = read_update_source(manifest_source)?;
        let manifest = OfficialCatalogManifest::parse_pinned(&manifest_bytes, manifest_sha256)?;
        let catalog_source =
            resolve_manifest_reference(manifest_source, &manifest.catalog_reference)?;
        let bytes = read_update_source(&catalog_source)?;
        manifest.verify_catalog(&bytes)?;
        return Ok(LoadedCatalogCandidate {
            bytes,
            expected_sha256: Some(manifest.catalog_sha256.clone()),
            manifest: Some(manifest),
            trust: format!(
                "checksum-pinned official manifest {} (not a cryptographic signature)",
                manifest_sha256.trim().to_ascii_lowercase()
            ),
        });
    }

    let source = source.context("specify exactly one of --from SOURCE or --official")?;
    if source.starts_with("https://") && expected_sha256.is_none() {
        anyhow::bail!("HTTPS catalog access requires --sha256 with a trusted expected digest");
    }
    let bytes = read_update_source(source)?;
    Ok(LoadedCatalogCandidate {
        bytes,
        expected_sha256: expected_sha256.map(str::to_string),
        manifest: None,
        trust: expected_sha256
            .map(|digest| format!("caller-supplied SHA-256 pin {}", digest.trim()))
            .unwrap_or_else(|| "local file; no transport authenticity claim".to_string()),
    })
}

fn resolve_manifest_reference(manifest_source: &str, reference: &str) -> Result<String> {
    if reference.starts_with("https://") {
        return Ok(reference.to_string());
    }
    if reference.starts_with("http://") {
        anyhow::bail!("official catalog references require HTTPS or a local file");
    }
    if manifest_source.starts_with("https://") {
        let base = reqwest::Url::parse(manifest_source)
            .with_context(|| format!("invalid official manifest URL {manifest_source}"))?;
        let resolved = base
            .join(reference)
            .with_context(|| format!("invalid catalog reference '{reference}'"))?;
        anyhow::ensure!(
            resolved.scheme() == "https",
            "a remote official manifest may reference only HTTPS catalogs"
        );
        return Ok(resolved.to_string());
    }
    let path = Path::new(reference);
    if path.is_absolute() {
        Ok(path.display().to_string())
    } else {
        Ok(Path::new(manifest_source)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
            .display()
            .to_string())
    }
}

fn print_catalog_check(output: &CatalogCheckOutput) -> Result<()> {
    let terminal = terminal_ui();
    let (state, tone) = match output.check.relation {
        CatalogCandidateRelation::Current => ("CURRENT; no update needed", Tone::Success),
        CatalogCandidateRelation::Newer => ("NEWER; eligible for explicit update", Tone::Success),
        CatalogCandidateRelation::Downgrade => (
            "DOWNGRADE REJECTED; use prices activate for an intentional historical selection",
            Tone::Warning,
        ),
        CatalogCandidateRelation::RevisionConflict => {
            ("REJECTED; revision label has different bytes", Tone::Error)
        }
    };
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{}",
        terminal.paint(Tone::Accent, "TOKEN LEDGER / CATALOG CHECK")
    );
    let _ = writeln!(
        text,
        "{} -> {}",
        output.check.active_revision, output.check.candidate_revision
    );
    let _ = writeln!(
        text,
        "{} {}",
        terminal.paint(tone, terminal.status_symbol(tone)),
        terminal.paint(tone, state)
    );
    let _ = writeln!(
        text,
        "Published {} -> {}",
        output.check.active_published_at.format("%Y-%m-%d"),
        output.check.candidate_published_at.format("%Y-%m-%d")
    );
    if terminal.details {
        let _ = writeln!(text, "Candidate SHA-256: {}", output.check.candidate_sha256);
        let _ = writeln!(text, "Trust: {}", output.trust);
    } else {
        let _ = writeln!(
            text,
            "{}",
            terminal.paint(
                Tone::Muted,
                "Digest and trust evidence: rerun with --details"
            )
        );
    }
    terminal.emit_stdout(&text)?;
    Ok(())
}

fn print_install_receipt(
    action: &str,
    receipt: &CatalogInstallReceipt,
    replaced_revision: Option<&str>,
    note: Option<&str>,
) -> Result<()> {
    let terminal = terminal_ui();
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{} {}",
        terminal.paint(Tone::Success, terminal.status_symbol(Tone::Success)),
        terminal.paint(Tone::Success, action)
    );
    let _ = writeln!(text, "Revision {}", receipt.installed_revision);
    if let Some(replaced_revision) = replaced_revision {
        let _ = writeln!(
            text,
            "Activated {} and retained replaced active revision {}.",
            receipt.installed_revision, replaced_revision
        );
    }
    let _ = writeln!(
        text,
        "Immutable history: {} verified revision(s)",
        format_count(receipt.retained_revisions.len() as u64)
    );
    if terminal.details {
        let _ = writeln!(text, "SHA-256: {}", receipt.installed_sha256);
        let _ = writeln!(text, "Active file: {}", receipt.active_path.display());
        let _ = writeln!(text, "History: {}", receipt.history_dir.display());
    }
    if let Some(note) = note {
        let _ = writeln!(text, "{}", terminal.paint(Tone::Muted, note));
    }
    terminal.emit_stdout(&text)?;
    Ok(())
}

fn append_collection_diff(output: &mut String, label: &str, diff: &CatalogCollectionDiff) {
    let _ = writeln!(
        output,
        "{label}: +{} -{} ~{}",
        diff.added.len(),
        diff.removed.len(),
        diff.changed.len()
    );
    for (marker, values) in [
        ("+", &diff.added),
        ("-", &diff.removed),
        ("~", &diff.changed),
    ] {
        if !values.is_empty() {
            let _ = writeln!(output, "  {marker} {}", values.join(", "));
        }
    }
}

fn add_usage(target: &mut UsageVector, source: &UsageVector) {
    target.input_tokens_total = target
        .input_tokens_total
        .saturating_add(source.input_tokens_total);
    target.input_tokens_uncached = target
        .input_tokens_uncached
        .saturating_add(source.input_tokens_uncached);
    target.input_tokens_cached = target
        .input_tokens_cached
        .saturating_add(source.input_tokens_cached);
    target.cache_write_5m_tokens = target
        .cache_write_5m_tokens
        .saturating_add(source.cache_write_5m_tokens);
    target.cache_write_1h_tokens = target
        .cache_write_1h_tokens
        .saturating_add(source.cache_write_1h_tokens);
    target.cache_write_unknown_tokens = target
        .cache_write_unknown_tokens
        .saturating_add(source.cache_write_unknown_tokens);
    target.output_tokens_total = target
        .output_tokens_total
        .saturating_add(source.output_tokens_total);
    target.reasoning_output_tokens = target
        .reasoning_output_tokens
        .saturating_add(source.reasoning_output_tokens);
    target.web_search_requests = target
        .web_search_requests
        .saturating_add(source.web_search_requests);
    target.web_fetch_requests = target
        .web_fetch_requests
        .saturating_add(source.web_fetch_requests);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::tempdir;
    use token_ledger::model::{CoverageStatus, PricingDimensions, UsageQuality};

    fn event(client: Client, session: &str) -> CanonicalEvent {
        CanonicalEvent {
            event_id: stable_id(&[client.as_str(), "event"]),
            event_key: "message:raw-message-id".into(),
            client,
            session_id: session.into(),
            provider_message_id: Some("raw-message-id".into()),
            occurred_at: Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0).unwrap(),
            raw_model: "model".into(),
            provider: "provider".into(),
            usage: UsageVector::default(),
            dimensions: PricingDimensions {
                provider_request_id: Some("raw-request-id".into()),
                ..Default::default()
            },
            quality: UsageQuality::Exact,
            coverage: CoverageStatus::CompleteKnown,
            source_count: 1,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn explain_display_redacts_raw_identifiers_by_default() -> Result<()> {
        let displayed = event_for_display(event(Client::ClaudeCode, "raw-session-id"), false);
        let json = serde_json::to_string(&displayed)?;

        for secret in [
            "raw-session-id",
            "raw-message-id",
            "raw-request-id",
            "message:raw-message-id",
        ] {
            assert!(!json.contains(secret));
        }
        assert_eq!(displayed.event_id, event(Client::ClaudeCode, "x").event_id);
        Ok(())
    }

    #[test]
    fn session_rows_do_not_merge_equal_ids_from_different_clients() {
        let rows = sessions(
            &[
                event(Client::ClaudeCode, "shared-session"),
                event(Client::OpenaiCodex, "shared-session"),
            ],
            false,
        );

        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].session, rows[1].session);
        assert_ne!(rows[0].client, rows[1].client);
    }

    #[test]
    fn explicit_raw_id_mode_preserves_identifiers() {
        let displayed = event_for_display(event(Client::ClaudeCode, "raw-session-id"), true);
        assert_eq!(displayed.session_id, "raw-session-id");
        assert_eq!(
            displayed.provider_message_id.as_deref(),
            Some("raw-message-id")
        );
        assert_eq!(
            displayed.dimensions.provider_request_id.as_deref(),
            Some("raw-request-id")
        );
    }

    #[test]
    fn dry_run_uses_memory_without_creating_configured_database() -> Result<()> {
        let dir = tempdir()?;
        let database = dir.path().join("must-not-exist.sqlite");
        let config = Config {
            database_path: Some(database.clone()),
            ..Default::default()
        };

        let ledger = open_scan_ledger(&config, true)?;

        assert_eq!(ledger.path(), Path::new(":memory:"));
        assert!(!database.exists());
        assert!(!database.with_extension("sqlite-wal").exists());
        Ok(())
    }

    #[test]
    fn natural_dates_resolve_in_the_requested_timezone() -> Result<()> {
        let now = Utc.with_ymd_and_hms(2026, 7, 10, 1, 30, 0).unwrap();
        let new_york: Tz = "America/New_York".parse()?;
        let tokyo: Tz = "Asia/Tokyo".parse()?;

        assert_eq!(
            date_keyword_at("today", new_york, now)?,
            NaiveDate::from_ymd_opt(2026, 7, 9).unwrap()
        );
        assert_eq!(
            date_keyword_at("today", tokyo, now)?,
            NaiveDate::from_ymd_opt(2026, 7, 10).unwrap()
        );
        assert_eq!(
            date_keyword_at("yesterday", tokyo, now)?,
            NaiveDate::from_ymd_opt(2026, 7, 9).unwrap()
        );
        Ok(())
    }

    #[test]
    fn rebuild_flag_keeps_full_alias_compatible() {
        for spelling in ["--rebuild", "--full"] {
            let parsed = Cli::try_parse_from(["ledger", "scan", spelling]).unwrap();
            let Some(Command::Scan(args)) = parsed.command else {
                panic!("expected scan command")
            };
            assert!(args.full);
        }
    }
}
