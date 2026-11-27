//! MCP server over stdio. Each tool returns the same document as the matching
//! `--json` command, as structured content with a declared output schema.
//!
//! This module deliberately does not import the crate's `Result` alias: the `rmcp`
//! macros expand to code that names the standard two-parameter `Result`.

use rmcp::{
    Json, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerConfig},
    service::ServerInitializeError,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    error::Error,
    model::{Cheapest, Grid, Market, Now, Outlook, Prices},
    service::{CheapestQuery, Vatio},
    time,
};

const INSTRUCTIONS: &str = "Spain's electricity prices and grid status from Red Eléctrica's public REData API. \
All times are Europe/Madrid and carry their UTC offset; intervals are [start, end). \
Prices are for the mainland (peninsular) system, in EUR/MWh as published and in EUR/kWh. Market \
'pvpc' is the regulated household price (hourly, energy term before taxes); 'spot' is the \
day-ahead wholesale price (quarter-hourly). Next-day PVPC appears in the evening, around 20:45 to \
20:50 Madrid time; before that, requests for tomorrow fail with code 'not_available' and \
retryable=true. To decide when to run an appliance or charge a vehicle, call \
find_cheapest_window instead of working it out from get_prices. To plan further ahead than \
tomorrow, call get_price_outlook: beyond the published days it searches vatio's own estimate (the \
average of recent days of the same kind, not Red Eléctrica data), and every window it returns is \
labelled 'published' or 'estimated'; say which when you report one. When you report a figure, name \
the source and its updated_at from the result's `sources`. When a tool fails, its result text is \
JSON: {\"error\":{\"code\",\"message\",\"hint\",\"retryable\"}}; malformed arguments are rejected \
before the tool runs, with a plain message. Code 'blocked' means the provider's bot protection \
refused a valid request at random: wait a few seconds and call again.";

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PricesArgs {
    /// Day in the Europe/Madrid calendar: "today" (default), "tomorrow", "yesterday" or YYYY-MM-DD.
    #[serde(default)]
    pub day: Option<String>,
    /// "pvpc" (default): regulated household price, hourly. "spot": day-ahead wholesale price, quarter-hourly.
    #[serde(default)]
    pub market: Option<Market>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NowArgs {
    /// "pvpc" (default) or "spot".
    #[serde(default)]
    pub market: Option<Market>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheapestArgs {
    /// How long the run lasts, in whole minutes (1 to 1440). Example: 180 for a 3-hour cycle.
    #[schemars(range(min = 1, max = 1440))]
    pub duration_minutes: u32,
    /// Earliest start, not in the past. HH:MM (Madrid time; today if still ahead, otherwise tomorrow), YYYY-MM-DDTHH:MM (Madrid time) or RFC 3339. Defaults to now.
    #[serde(default)]
    pub from: Option<String>,
    /// Deadline by which the run must have finished, same formats as `from`. Defaults to the end of published prices (end of tomorrow once tomorrow is published).
    #[serde(default)]
    pub until: Option<String>,
    /// Number of non-overlapping windows to return, cheapest first (1 to 10, default 1).
    #[serde(default)]
    #[schemars(range(min = 1, max = 10))]
    pub top: Option<u32>,
    /// "pvpc" (default) or "spot".
    #[serde(default)]
    pub market: Option<Market>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutlookArgs {
    /// How long the run lasts, in whole minutes (1 to 1440). Example: 180 for a 3-hour cycle.
    #[schemars(range(min = 1, max = 1440))]
    pub duration_minutes: u32,
}

#[derive(Debug, Clone)]
pub struct Server {
    vatio: Vatio,
}

/// Tool failures use the same JSON error object as the CLI prints under `--json`.
fn fail(err: Error) -> String {
    serde_json::to_string(&err.report()).unwrap_or_else(|_| err.to_string())
}

#[tool_router]
impl Server {
    pub fn new(vatio: Vatio) -> Self {
        Self { vatio }
    }

    #[tool(
        name = "get_prices",
        description = "Electricity prices in Spain for one day, at the market's native resolution, with min, max and mean. Use for 'what are the prices today/tomorrow/on a date'.",
        annotations(
            title = "Electricity prices for a day",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_prices(
        &self,
        Parameters(args): Parameters<PricesArgs>,
    ) -> Result<Json<Prices>, String> {
        let run = async {
            let day = time::parse_day(args.day.as_deref().unwrap_or("today"), self.vatio.now())?;
            self.vatio
                .prices(args.market.unwrap_or_default(), day)
                .await
        };
        run.await.map(Json).map_err(fail)
    }

    #[tool(
        name = "get_current_price",
        description = "The electricity price in force in Spain right now, the next interval's price, and how now ranks within today (low / mid / high). Use for 'is electricity cheap right now?'.",
        annotations(
            title = "Electricity price right now",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_current_price(
        &self,
        Parameters(args): Parameters<NowArgs>,
    ) -> Result<Json<Now>, String> {
        self.vatio
            .now_price(args.market.unwrap_or_default())
            .await
            .map(Json)
            .map_err(fail)
    }

    #[tool(
        name = "find_cheapest_window",
        description = "The cheapest continuous time window to run something for a given duration in Spain, searching from now (or `from`) to the end of published prices (or `until`), across today and tomorrow. Also returns what starting at the earliest allowed moment (now, unless `from` is given) would cost, and the saving against it. Use for 'when should I run the dishwasher / charge the car?'.",
        annotations(
            title = "Cheapest time to run something",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn find_cheapest_window(
        &self,
        Parameters(args): Parameters<CheapestArgs>,
    ) -> Result<Json<Cheapest>, String> {
        let run = async {
            let now = self.vatio.now();
            let query = cheapest_query(&args, now)?;
            self.vatio.cheapest(query).await
        };
        run.await.map(Json).map_err(fail)
    }

    #[tool(
        name = "get_price_outlook",
        description = "The cheapest continuous time window to run something for a given duration in Spain over the next several days, at the regulated household price (PVPC). Searches the published prices (today, and tomorrow once it is out) together with an estimate for the five days after them, and returns the best window overall plus the cheapest window of each day. Each is labelled 'published' or 'estimated'. Estimated prices are vatio's own estimate, the average of the last 4 days of the same kind (weekdays or weekend days): not Red Eléctrica data and not a forecast, so check again once the day is published. Use for 'which day this week is cheapest to run the washing machine?'; for today and tomorrow only, find_cheapest_window is exact.",
        annotations(
            title = "Cheapest time to run something in the coming days",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_price_outlook(
        &self,
        Parameters(args): Parameters<OutlookArgs>,
    ) -> Result<Json<Outlook>, String> {
        let run = async {
            if !(1..=1440).contains(&args.duration_minutes) {
                return Err(Error::usage("duration_minutes must be between 1 and 1440"));
            }
            self.vatio
                .outlook(i64::from(args.duration_minutes) * 60)
                .await
        };
        run.await.map(Json).map_err(fail)
    }

    #[tool(
        name = "get_grid_status",
        description = "State of Spain's electricity system: demand measured in the last few minutes (with the operator's forecast), and the most recent daily generation mix by technology with renewable share and CO2 intensity. The mix is published per day, so check `generation.day`. Use for 'how green is the grid?' or 'what is demand right now?'.",
        annotations(
            title = "Grid status: demand and generation mix",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_grid_status(&self) -> Result<Json<Grid>, String> {
        self.vatio.grid().await.map(Json).map_err(fail)
    }
}

fn cheapest_query(args: &CheapestArgs, now: jiff::Timestamp) -> Result<CheapestQuery, Error> {
    if !(1..=1440).contains(&args.duration_minutes) {
        return Err(Error::usage("duration_minutes must be between 1 and 1440"));
    }
    let top = args.top.unwrap_or(1);
    if !(1..=10).contains(&top) {
        return Err(Error::usage("top must be between 1 and 10"));
    }
    let from = args
        .from
        .as_deref()
        .map(|t| time::parse_when(t, now))
        .transpose()?;
    let until = args
        .until
        .as_deref()
        .map(|t| time::parse_when(t, from.unwrap_or(now)))
        .transpose()?;
    Ok(CheapestQuery {
        market: args.market.unwrap_or_default(),
        duration_secs: i64::from(args.duration_minutes) * 60,
        from,
        until,
        top: top as usize,
    })
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("vatio", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serve on stdin/stdout until the client disconnects.
pub async fn serve(vatio: Vatio) -> Result<(), Error> {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!(
            "vatio: MCP server waiting for a client on stdin (JSON-RPC). Press Ctrl-C to stop."
        );
    }
    let running = match Server::new(vatio).serve(stdio()).await {
        Ok(running) => running,
        // The client went away before saying anything: a normal way for a session to
        // end, not a failure of this program.
        Err(ServerInitializeError::ConnectionClosed(_)) => return Ok(()),
        Err(err) => return Err(Error::internal(err)),
    };
    running.waiting().await.map_err(Error::internal)?;
    Ok(())
}
