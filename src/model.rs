//! The output contract. Every type here is what `--json` prints and what the MCP
//! server returns as structured content; `vatio schema` exports their JSON Schemas.
//!
//! Conventions: units live in field names (`eur_per_kwh`, `mw`, `percent`), instants
//! are RFC 3339 with the Europe/Madrid offset, intervals are half-open `[start, end)`,
//! and every document names its own `schema` so a consumer can detect a new version.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::time::LocalTime;

pub const SCHEMA_PRICES: &str = "vatio.prices/v1";
pub const SCHEMA_NOW: &str = "vatio.now/v1";
pub const SCHEMA_CHEAPEST: &str = "vatio.cheapest/v1";
pub const SCHEMA_GRID: &str = "vatio.grid/v1";

/// Prices come from the mainland (peninsular) system.
pub const AREA_PRICES: &str = "peninsula";
/// Demand and generation are national totals.
pub const AREA_GRID: &str = "spain";

/// Which price series to use.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
    clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Market {
    /// PVPC 2.0TD: the energy term of the regulated household tariff. It carries the
    /// cost of energy and the energy share of tolls and charges; taxes come on top. Hourly.
    #[default]
    Pvpc,
    /// Day-ahead wholesale market price for the Spanish zone. Quarter-hourly.
    Spot,
}

impl Market {
    /// Series identifier in Red Eléctrica's data.
    pub(crate) fn series_id(self) -> &'static str {
        match self {
            Self::Pvpc => "1001",
            Self::Spot => "600",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pvpc => "PVPC",
            Self::Spot => "Day-ahead spot",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Pvpc => "pvpc",
            Self::Spot => "spot",
        }
    }

    /// The interval this market was priced at on a given delivery day. The day-ahead
    /// market moved from hourly to quarter-hourly products on 1 October 2025.
    pub(crate) fn nominal_slot_secs(self, day: jiff::civil::Date) -> i64 {
        const QUARTER_HOURLY_SINCE: jiff::civil::Date = jiff::civil::date(2025, 10, 1);
        match self {
            Self::Spot if day >= QUARTER_HOURLY_SINCE => 900,
            Self::Pvpc | Self::Spot => 3600,
        }
    }
}

/// What one upstream request supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Dataset {
    Prices,
    Demand,
    Generation,
    Emissions,
}

impl Dataset {
    pub fn label(self) -> &'static str {
        match self {
            Self::Prices => "prices",
            Self::Demand => "demand",
            Self::Generation => "generation",
            Self::Emissions => "emissions",
        }
    }
}

/// Where a document's data came from. Every document lists its sources.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Source {
    /// Publisher of the data. Cite it when you reuse a figure.
    pub name: &'static str,
    pub dataset: Dataset,
    /// The upstream request that produced it.
    pub url: String,
    /// When the publisher last updated this data. Cite it along with the name.
    pub updated_at: Option<LocalTime>,
    /// When this program retrieved it from upstream.
    pub fetched_at: LocalTime,
    /// True when served from the local cache instead of the network.
    pub cached: bool,
}

/// One priced interval `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
pub struct PricePoint {
    pub start: LocalTime,
    pub end: LocalTime,
    /// Price as published, in euros per megawatt-hour.
    pub eur_per_mwh: f64,
    /// The same price in euros per kilowatt-hour.
    pub eur_per_kwh: f64,
}

impl PricePoint {
    pub fn new(start: jiff::Timestamp, end: jiff::Timestamp, eur_per_mwh: f64) -> Self {
        Self {
            start: LocalTime(start),
            end: LocalTime(end),
            eur_per_mwh: round(eur_per_mwh, 2),
            eur_per_kwh: round(eur_per_mwh / 1000.0, 5),
        }
    }
}

/// Summary of a set of price points.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
pub struct PriceStats {
    /// Cheapest interval (the earliest one on ties).
    pub min: PricePoint,
    /// Most expensive interval (the earliest one on ties).
    pub max: PricePoint,
    /// Time-weighted mean.
    pub mean_eur_per_mwh: f64,
    pub mean_eur_per_kwh: f64,
}

/// Prices for one Europe/Madrid calendar day.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Prices {
    /// Always `vatio.prices/v1`.
    #[schemars(extend("const" = SCHEMA_PRICES))]
    pub schema: &'static str,
    pub market: Market,
    /// Always `peninsula`: prices are those of the mainland (peninsular) system.
    #[schemars(extend("const" = AREA_PRICES))]
    pub area: &'static str,
    /// Calendar day, `YYYY-MM-DD`.
    pub day: String,
    /// Always `Europe/Madrid`.
    #[schemars(extend("const" = "Europe/Madrid"))]
    pub timezone: &'static str,
    /// Length of each interval in minutes: 60 for PVPC; 15 for spot (60 for spot days
    /// before October 2025, when that market was hourly).
    pub resolution_minutes: u32,
    /// True when the points cover the whole day (23, 24 or 25 hours).
    pub complete: bool,
    pub points: Vec<PricePoint>,
    pub stats: PriceStats,
    pub sources: Vec<Source>,
}

/// How the current price compares with the rest of today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Among the cheapest third of today's intervals.
    Low,
    /// In the middle third.
    Mid,
    /// Among the most expensive third.
    High,
}

/// The price in force right now, in the context of today.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Now {
    /// Always `vatio.now/v1`.
    #[schemars(extend("const" = SCHEMA_NOW))]
    pub schema: &'static str,
    pub market: Market,
    /// Always `peninsula`.
    #[schemars(extend("const" = AREA_PRICES))]
    pub area: &'static str,
    /// The instant this answer refers to.
    pub at: LocalTime,
    /// Always `Europe/Madrid`.
    #[schemars(extend("const" = "Europe/Madrid"))]
    pub timezone: &'static str,
    /// The interval containing `at`.
    pub current: PricePoint,
    /// The interval that follows, if published.
    pub next: Option<PricePoint>,
    /// 1 = cheapest interval of today; equal prices share the better rank.
    pub rank: u32,
    /// Number of intervals today (`rank` is out of this).
    pub rank_of: u32,
    pub level: Level,
    /// Today's summary.
    pub today: PriceStats,
    pub sources: Vec<Source>,
}

/// A candidate run window `[start, end)` and what energy costs on average inside it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
pub struct PriceWindow {
    pub start: LocalTime,
    pub end: LocalTime,
    /// Time-weighted mean price over the window.
    pub mean_eur_per_mwh: f64,
    pub mean_eur_per_kwh: f64,
    /// Minutes from now until this window begins (0 = start immediately).
    pub starts_in_minutes: i64,
}

/// What part tomorrow's prices played in a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Tomorrow {
    /// Published and part of the search.
    Included,
    /// The search reached into tomorrow but its prices are not out yet. Asking again
    /// after publication may find a cheaper window.
    NotPublished,
    /// The search reached into tomorrow but its prices could not be checked (see `warnings`).
    Unavailable,
    /// The search ended today, so tomorrow did not matter.
    NotNeeded,
}

/// The time span that was searched.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
pub struct SearchSpan {
    /// Earliest allowed start.
    pub from: LocalTime,
    /// Latest allowed finish: the deadline, or the end of published prices if sooner.
    pub until: LocalTime,
    pub tomorrow: Tomorrow,
}

/// The cheapest time to run something for a fixed duration.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Cheapest {
    /// Always `vatio.cheapest/v1`.
    #[schemars(extend("const" = SCHEMA_CHEAPEST))]
    pub schema: &'static str,
    pub market: Market,
    /// Always `peninsula`.
    #[schemars(extend("const" = AREA_PRICES))]
    pub area: &'static str,
    /// Always `Europe/Madrid`.
    #[schemars(extend("const" = "Europe/Madrid"))]
    pub timezone: &'static str,
    pub duration_minutes: i64,
    pub searched: SearchSpan,
    /// The cheapest window.
    pub best: PriceWindow,
    /// Further windows, each the cheapest that overlaps none of the ones before it.
    pub alternatives: Vec<PriceWindow>,
    /// For comparison: the window that starts at the earliest allowed moment,
    /// `searched.from`, which is now unless a later start was asked for. Null when
    /// published prices do not cover a full run from that moment.
    pub earliest: Option<PriceWindow>,
    /// `earliest` mean minus `best` mean, in euros per kWh. Null without `earliest`.
    pub saving_eur_per_kwh: Option<f64>,
    /// The same saving as a percentage of the `earliest` mean. Null when either mean
    /// is zero or negative, where a percentage is meaningless.
    pub saving_percent: Option<f64>,
    /// Things that limited the answer, in plain words. Empty when nothing did.
    pub warnings: Vec<String>,
    pub sources: Vec<Source>,
}

/// One demand reading.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, JsonSchema)]
pub struct DemandReading {
    pub at: LocalTime,
    pub mw: f64,
}

/// Electricity demand, measured in near real time.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Demand {
    /// Instant of the latest measured value. Usually about 10 minutes behind the clock.
    pub at: LocalTime,
    /// Measured demand at `at`, in megawatts.
    pub actual_mw: f64,
    /// What the operator had forecast for the same instant.
    pub forecast_mw: Option<f64>,
    /// What the market had scheduled for the same instant.
    pub scheduled_mw: Option<f64>,
    /// Spacing of the readings in minutes. Null when only one reading exists yet.
    pub resolution_minutes: Option<u32>,
    /// Highest measured demand so far today.
    pub peak_today: DemandReading,
    /// Lowest measured demand so far today.
    pub low_today: DemandReading,
}

/// One generation technology's output over a day.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Technology {
    /// Identifier for the technology, for example `wind`, `solar_pv`, `nuclear`,
    /// `combined_cycle`. Known technologies keep theirs; one this version does not know
    /// gets an identifier derived from its Spanish name.
    pub id: String,
    /// English display name.
    pub name: String,
    /// Name as published by Red Eléctrica (Spanish).
    pub name_es: String,
    /// Null when Red Eléctrica's answer does not classify the technology.
    pub renewable: Option<bool>,
    /// Energy generated over the day, in megawatt-hours.
    pub mwh: f64,
    /// Share of the day's total generation, 0 to 100.
    pub percent: f64,
}

/// Generation mix for one day. Red Eléctrica publishes this per day, not live, and
/// revises recent days for a while, so treat the latest figures as provisional.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Generation {
    /// The day these figures cover, `YYYY-MM-DD`: the most recent completed day published.
    pub day: String,
    /// Total generation over the day, in megawatt-hours.
    pub total_mwh: f64,
    /// Share of generation from renewable sources, 0 to 100. Null when the answer
    /// does not classify every technology (see `warnings`).
    pub renewable_percent: Option<f64>,
    /// Average emissions per unit generated, in grams of CO2 equivalent per kWh.
    pub co2_g_per_kwh: Option<f64>,
    /// Total emissions from generation over the day, in tonnes of CO2 equivalent.
    pub co2_tonnes: Option<f64>,
    /// Technologies by output, largest first.
    pub technologies: Vec<Technology>,
}

/// The state of Spain's electricity system: live demand plus the latest daily mix.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct Grid {
    /// Always `vatio.grid/v1`.
    #[schemars(extend("const" = SCHEMA_GRID))]
    pub schema: &'static str,
    /// Always `spain`: demand and generation are national totals (mainland and islands).
    #[schemars(extend("const" = AREA_GRID))]
    pub area: &'static str,
    /// The instant this answer was produced.
    pub at: LocalTime,
    /// Always `Europe/Madrid`.
    #[schemars(extend("const" = "Europe/Madrid"))]
    pub timezone: &'static str,
    /// Null when the demand feed could not be read (see `warnings`).
    pub demand: Option<Demand>,
    /// Null when the generation figures could not be read (see `warnings`).
    pub generation: Option<Generation>,
    /// Parts of the answer that are missing, and why. Empty when complete.
    pub warnings: Vec<String>,
    pub sources: Vec<Source>,
}

/// Round half away from zero to `places` decimals.
pub fn round(value: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (value * k).round() / k
}
