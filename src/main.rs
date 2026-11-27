use std::{io::Write, process::ExitCode};

use clap::{CommandFactory, Parser, Subcommand};
use vatio::{
    CheapestQuery, Error, Options, Result, Vatio,
    error::ErrorReport,
    mcp,
    model::{Cheapest, Grid, Market, Now, Outlook, Prices},
    render, time,
};

const AFTER_HELP: &str = "\
Examples:
  vatio now                          Is electricity cheap right now?
  vatio prices tomorrow              Tomorrow's hourly PVPC (published in the evening)
  vatio cheapest 3h                  Best 3-hour window between now and the end of published prices
  vatio cheapest 90m --until 08:00   ...that finishes by 08:00
  vatio outlook 3h                   Best 3-hour window over the next days, later ones estimated
  vatio grid                         Demand right now and how renewable the mix is
  vatio prices --market spot --json  Quarter-hourly wholesale prices as JSON

Exit codes:
  0  success
  1  internal error
  2  invalid usage
  3  data not available (for example tomorrow's prices before they are published)
  4  no answer from Red Eléctrica: network, upstream error or its bot protection

Environment:
  VATIO_CACHE_DIR   where complete price days are cached
                    (default: ~/.cache/vatio, or %LOCALAPPDATA%\\vatio\\cache on Windows)
  VATIO_NO_CACHE    set to 1 to disable the cache
  VATIO_API_URL     alternative REData base URL (its data is cached only if
                    VATIO_CACHE_DIR is also set)
  NO_COLOR          disable colours

Times are Europe/Madrid. Data: Red Eléctrica de España, REData API (no key needed).";

/// Spain's electricity prices and grid status in your terminal, for people and AI agents.
#[derive(Debug, Parser)]
#[command(name = "vatio", version, about, after_help = AFTER_HELP, propagate_version = true)]
struct Cli {
    /// Print machine-readable JSON with a stable, versioned schema (errors too, on stderr)
    #[arg(long, global = true, help_heading = "Global options")]
    json: bool,

    /// Ignore the local cache and fetch fresh data
    #[arg(long, global = true, help_heading = "Global options")]
    refresh: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// The price right now and how it compares with the rest of today
    Now {
        /// Which price to use
        #[arg(short, long, value_enum, default_value_t)]
        market: Market,
    },
    /// Prices for one day
    Prices {
        /// today, tomorrow, yesterday (or hoy, mañana, ayer) or YYYY-MM-DD, Madrid calendar
        #[arg(default_value = "today")]
        day: String,
        /// Which price to use
        #[arg(short, long, value_enum, default_value_t)]
        market: Market,
    },
    /// The cheapest time to run something for a given duration
    ///
    /// Searches every possible start between now and the end of published prices,
    /// including tomorrow once it is out, and compares the result with starting at
    /// the earliest moment allowed (now, unless --from says later).
    Cheapest {
        /// How long the run lasts: 3h, 90m, 2h30m, PT45M. A bare number means hours
        duration: String,
        /// Earliest start, not in the past: HH:MM (today if still ahead, else tomorrow),
        /// YYYY-MM-DDTHH:MM or RFC 3339 [default: now]
        #[arg(long, value_name = "TIME")]
        from: Option<String>,
        /// The run must finish by this time; same formats, and 24:00 for midnight
        /// [default: end of published prices]
        #[arg(long, value_name = "TIME")]
        until: Option<String>,
        /// How many windows to list: the cheapest, then the cheapest that does not overlap it, and so on
        #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=10))]
        top: u8,
        /// Which price to use
        #[arg(short, long, value_enum, default_value_t)]
        market: Market,
    },
    /// The cheapest time to run something over the next several days (PVPC)
    ///
    /// Searches the published prices (today, and tomorrow once it is out) together
    /// with vatio's own estimate for the five days after them: the average of the
    /// last four days of the same kind, weekdays or weekend days. Every day is
    /// labelled as published or estimated.
    Outlook {
        /// How long the run lasts: 3h, 90m, 2h30m, PT45M. A bare number means hours
        duration: String,
    },
    /// Live demand and the latest generation mix, renewable share and CO2 intensity
    ///
    /// Demand is measured every few minutes. The mix by technology is published per
    /// day, so the output names the day it covers.
    Grid,
    /// Run as an MCP server over stdio, for AI agents
    ///
    /// Tools: get_prices, get_current_price, find_cheapest_window, get_price_outlook,
    /// get_grid_status.
    /// Each returns the same document as the matching command prints with --json.
    Mcp,
    /// Print the JSON Schemas of everything --json and the MCP tools return
    Schema {
        /// One document only
        #[arg(value_enum)]
        name: Option<SchemaName>,
    },
    /// Generate a shell completion script
    Completions { shell: clap_complete::Shell },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum SchemaName {
    Prices,
    Now,
    Cheapest,
    Outlook,
    Grid,
    Error,
}

impl SchemaName {
    /// The schema of the document as it is written out (nullable fields are present
    /// and null, not absent), which is also what the MCP server declares.
    fn schema(self) -> schemars::Schema {
        let generator = schemars::generate::SchemaSettings::default()
            .for_serialize()
            .into_generator();
        match self {
            Self::Prices => generator.into_root_schema_for::<Prices>(),
            Self::Now => generator.into_root_schema_for::<Now>(),
            Self::Cheapest => generator.into_root_schema_for::<Cheapest>(),
            Self::Outlook => generator.into_root_schema_for::<Outlook>(),
            Self::Grid => generator.into_root_schema_for::<Grid>(),
            Self::Error => generator.into_root_schema_for::<ErrorReport>(),
        }
    }
}

fn main() -> ExitCode {
    // `args_os`: an argument that is not valid Unicode must reach clap, which reports
    // it as a usage error, instead of panicking here.
    let json = std::env::args_os().any(|a| a == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Keep clap's own rendering for --help, --version and human usage errors;
        // under --json a usage error becomes the same JSON object as any other.
        Err(err) if err.use_stderr() && json => {
            // First paragraph of clap's message, on one line.
            let rendered = err.render().to_string();
            let message: Vec<&str> = rendered
                .lines()
                .map(str::trim)
                .take_while(|line| !line.is_empty())
                .collect();
            return fail(
                &Error::usage(message.join(" ").trim_start_matches("error: ")),
                true,
            );
        }
        Err(err) => err.exit(),
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return fail(&Error::internal(err), cli.json),
    };
    match runtime.block_on(run(&cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail(&err, cli.json),
    }
}

fn fail(err: &Error, json: bool) -> ExitCode {
    let mut stderr = anstream::stderr();
    if json {
        let _ = writeln!(
            stderr,
            "{}",
            serde_json::to_string(&err.report()).unwrap_or_default()
        );
    } else {
        let red = anstyle::AnsiColor::Red.on_default().bold();
        let _ = writeln!(stderr, "{red}error:{red:#} {}", err.message());
        if let Some(hint) = err.hint() {
            let _ = writeln!(stderr, "  hint: {hint}");
        }
    }
    ExitCode::from(err.exit_code())
}

async fn run(cli: &Cli) -> Result<()> {
    // Commands that need neither the network nor the clock.
    match &cli.command {
        Command::Schema { name } => {
            let value = match name {
                Some(name) => serde_json::to_value(name.schema()),
                None => serde_json::to_value(serde_json::json!({
                    "prices": SchemaName::Prices.schema(),
                    "now": SchemaName::Now.schema(),
                    "cheapest": SchemaName::Cheapest.schema(),
                    "outlook": SchemaName::Outlook.schema(),
                    "grid": SchemaName::Grid.schema(),
                    "error": SchemaName::Error.schema(),
                })),
            };
            return print_json(&value.map_err(Error::internal)?);
        }
        Command::Completions { shell } => {
            let mut script = Vec::new();
            clap_complete::generate(*shell, &mut Cli::command(), "vatio", &mut script);
            return write_stdout(&script);
        }
        _ => {}
    }

    let mut options = Options::from_env()?;
    options.refresh = cli.refresh;
    let vatio = Vatio::new(options)?;
    let now = vatio.now();

    match &cli.command {
        Command::Now { market } => {
            let doc: Now = vatio.now_price(*market).await?;
            emit(cli.json, &doc, || render::now(&doc))
        }
        Command::Prices { day, market } => {
            let doc: Prices = vatio.prices(*market, time::parse_day(day, now)?).await?;
            emit(cli.json, &doc, || render::prices(&doc, now))
        }
        Command::Cheapest {
            duration,
            from,
            until,
            top,
            market,
        } => {
            let from = from
                .as_deref()
                .map(|t| time::parse_when(t, now))
                .transpose()?;
            // A bare --until clock time means its first occurrence after the start.
            let until = until
                .as_deref()
                .map(|t| time::parse_when(t, from.unwrap_or(now)))
                .transpose()?;
            let doc: Cheapest = vatio
                .cheapest(CheapestQuery {
                    market: *market,
                    duration_secs: time::parse_duration(duration)?,
                    from,
                    until,
                    top: usize::from(*top),
                })
                .await?;
            emit(cli.json, &doc, || render::cheapest(&doc))
        }
        Command::Outlook { duration } => {
            let doc: Outlook = vatio.outlook(time::parse_duration(duration)?).await?;
            emit(cli.json, &doc, || render::outlook(&doc))
        }
        Command::Grid => {
            let doc: Grid = vatio.grid().await?;
            emit(cli.json, &doc, || render::grid(&doc))
        }
        Command::Mcp => mcp::serve(vatio).await,
        Command::Schema { .. } | Command::Completions { .. } => unreachable!("handled above"),
    }
}

fn emit<T: serde::Serialize>(json: bool, doc: &T, human: impl FnOnce() -> String) -> Result<()> {
    if json {
        return print_json(doc);
    }
    write_stdout(human().as_bytes())
}

fn print_json<T: serde::Serialize>(doc: &T) -> Result<()> {
    let mut text = serde_json::to_string_pretty(doc).map_err(Error::internal)?;
    text.push('\n');
    write_stdout(text.as_bytes())
}

/// Write to stdout, stripping colours when it is not a terminal. A closed pipe
/// (`vatio prices | head -3`) is a normal way to stop, not an error.
fn write_stdout(bytes: &[u8]) -> Result<()> {
    let mut stdout = anstream::stdout().lock();
    match stdout.write_all(bytes).and_then(|()| stdout.flush()) {
        Err(err) if err.kind() != std::io::ErrorKind::BrokenPipe => Err(Error::internal(err)),
        _ => Ok(()),
    }
}
