//! The operations behind every command and MCP tool.

use jiff::{Timestamp, civil::Date};

use crate::{
    cache::{Cache, Entry},
    error::{Error, Result},
    model::{
        AREA_PRICES, Cheapest, Dataset, Level, Market, Now, PricePoint, PriceStats, PriceWindow,
        Prices, SCHEMA_CHEAPEST, SCHEMA_NOW, SCHEMA_PRICES, SearchSpan, Source, Tomorrow, round,
    },
    redata::{self, Client},
    time::{self, LocalTime, ZONE},
    window::{self, Slot},
};

const PRICES_WIDGET: &str = "mercados/precios-mercados-tiempo-real";

/// How long a cached day that has not ended yet is trusted before it is checked again.
/// Published prices are rarely corrected, but it does happen; a day that is over is
/// kept as it is.
const REVALIDATE_AFTER_SECS: i64 = 3600;

/// How to construct the service.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Alternative REData base URL (mirrors, proxies, tests).
    pub base_url: Option<String>,
    /// Where complete price days are cached. `None` disables caching.
    pub cache: Option<Cache>,
    /// Ignore cached entries and overwrite them with fresh data.
    pub refresh: bool,
    /// Pretend the current time is this instant (tests, reproducible runs).
    pub now: Option<Timestamp>,
}

impl Options {
    /// Defaults plus the `VATIO_API_URL`, `VATIO_CACHE_DIR`, `VATIO_NO_CACHE` and
    /// `VATIO_NOW` environment variables.
    pub fn from_env() -> Result<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let now = match var("VATIO_NOW") {
            Some(text) => Some(text.parse::<Timestamp>().map_err(|_| {
                Error::usage(format!(
                    "VATIO_NOW must be an RFC 3339 timestamp, got '{text}'"
                ))
            })?),
            None => None,
        };
        let base_url = var("VATIO_API_URL");
        let disabled = var("VATIO_NO_CACHE").is_some_and(|v| !matches!(v.trim(), "0" | "false"));
        // Data from another server must not end up in the cache the real one reads,
        // so an alternative URL only gets a cache when a directory is named for it.
        let foreign = base_url.is_some() && var("VATIO_CACHE_DIR").is_none();
        Ok(Self {
            base_url,
            cache: if disabled || foreign {
                None
            } else {
                Cache::default_location()
            },
            refresh: false,
            now,
        })
    }
}

/// A day of prices in working form.
#[derive(Debug, Clone)]
struct Day {
    slots: Vec<Slot>,
    complete: bool,
    source: Source,
}

/// A cheapest-window request.
#[derive(Debug, Clone, Copy)]
pub struct CheapestQuery {
    pub market: Market,
    /// Run length in seconds (a whole number of minutes).
    pub duration_secs: i64,
    /// Earliest start. Defaults to now; may not be in the past.
    pub from: Option<Timestamp>,
    /// Latest finish. Defaults to the end of published prices.
    pub until: Option<Timestamp>,
    /// How many non-overlapping windows to return (at least 1).
    pub top: usize,
}

#[derive(Debug, Clone)]
pub struct Vatio {
    pub(crate) client: Client,
    cache: Option<Cache>,
    refresh: bool,
    fixed_now: Option<Timestamp>,
}

impl Vatio {
    pub fn new(options: Options) -> Result<Self> {
        Ok(Self {
            client: Client::new(options.base_url.as_deref())?,
            cache: options.cache,
            refresh: options.refresh,
            fixed_now: options.now,
        })
    }

    pub fn now(&self) -> Timestamp {
        self.fixed_now.unwrap_or_else(Timestamp::now)
    }

    /// Prices for one Madrid calendar day.
    pub async fn prices(&self, market: Market, day: Date) -> Result<Prices> {
        let d = self.day(market, day).await?;
        let points: Vec<PricePoint> = d.slots.iter().map(point).collect();
        Ok(Prices {
            schema: SCHEMA_PRICES,
            market,
            area: AREA_PRICES,
            day: day.to_string(),
            timezone: ZONE,
            resolution_minutes: resolution_minutes(&d.slots),
            complete: d.complete,
            stats: stats(&d.slots),
            points,
            sources: vec![d.source],
        })
    }

    /// The price in force right now.
    pub async fn now_price(&self, market: Market) -> Result<Now> {
        let now = self.now();
        let today = time::today(now);
        let d = self.day(market, today).await?;
        let index = d
            .slots
            .iter()
            .position(|s| s.start <= now && now < s.end)
            .ok_or_else(|| {
                Error::not_yet(
                    format!(
                        "no {} price is published for the current time",
                        market.label()
                    ),
                    None,
                )
            })?;
        let current = d.slots[index];

        let next = match d.slots.get(index + 1) {
            Some(slot) => Some(point(slot)),
            // Last interval of the day: the follower is tomorrow's first, if it is out yet.
            None => match today.tomorrow() {
                Ok(tomorrow) => self
                    .day(market, tomorrow)
                    .await
                    .ok()
                    .and_then(|t| t.slots.first().map(point)),
                Err(_) => None,
            },
        };

        let cheaper = d.slots.iter().filter(|s| s.value < current.value).count();
        let dearer = d.slots.iter().filter(|s| s.value > current.value).count();
        let third = d.slots.len() as f64 / 3.0;
        let level = match ((cheaper as f64) < third, (dearer as f64) < third) {
            (true, false) => Level::Low,
            (false, true) => Level::High,
            // In the middle third, or a plateau so wide that it is both or neither.
            _ => Level::Mid,
        };

        Ok(Now {
            schema: SCHEMA_NOW,
            market,
            area: AREA_PRICES,
            at: LocalTime(now),
            timezone: ZONE,
            current: point(&current),
            next,
            rank: cheaper as u32 + 1,
            rank_of: d.slots.len() as u32,
            level,
            today: stats(&d.slots),
            sources: vec![d.source],
        })
    }

    /// The cheapest time to run something for a fixed duration.
    pub async fn cheapest(&self, q: CheapestQuery) -> Result<Cheapest> {
        // Whole minutes keep every reported window on a minute boundary.
        let now = time::floor_minute(self.now());
        let today = time::today(now);
        let from = q.from.map_or(now, time::floor_minute);
        let length = time::fmt_duration(q.duration_secs);
        if from < now {
            return Err(Error::usage(
                "the earliest start is in the past; this search plans ahead from now",
            ));
        }
        if let Some(until) = q.until {
            if until <= from {
                return Err(Error::usage(
                    "the deadline must be later than the earliest start",
                ));
            }
            if (until - from).as_secs() < q.duration_secs {
                return Err(Error::usage(format!(
                    "a {length} run does not fit between the earliest start and the deadline"
                )));
            }
        }

        // Load the day the search starts on, then following days up to the deadline's
        // day (or tomorrow when there is no deadline), stopping at the first one that
        // cannot be had: prices are published one day ahead, so nothing follows it.
        let first = LocalTime(from).date();
        let last = match q.until {
            Some(until) => {
                let last_instant =
                    Timestamp::from_second(until.as_second() - 1).map_err(Error::internal)?;
                LocalTime(last_instant).date()
            }
            None => today.tomorrow().map_err(Error::internal)?.max(first),
        };
        let mut slots: Vec<Slot> = Vec::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();
        let mut tomorrow = if last > today {
            Tomorrow::NotPublished
        } else {
            Tomorrow::NotNeeded
        };
        let mut day = first;
        while day <= last {
            match self.day(q.market, day).await {
                Ok(d) => {
                    if day > today {
                        tomorrow = Tomorrow::Included;
                    }
                    slots.extend(d.slots);
                    sources.push(d.source);
                }
                // With nothing loaded there is nothing to answer from: say why.
                Err(err) if slots.is_empty() => return Err(err),
                // A later day that is simply not out yet is the normal state of
                // affairs for most of the day, not an error.
                Err(Error::NotAvailable { .. }) => break,
                Err(err) => {
                    tomorrow = Tomorrow::Unavailable;
                    warnings.push(format!(
                        "could not check prices for {day}: {}",
                        err.message()
                    ));
                    break;
                }
            }
            day = day.tomorrow().map_err(Error::internal)?;
        }

        let data_end = slots.last().map_or(from, |s| s.end);
        let until = q.until.map_or(data_end, |u| u.min(data_end));
        if let Some(wanted) = q.until
            && wanted > data_end
        {
            warnings.push(format!(
                "prices are published only until {}; the search stops there, not at the requested {}",
                LocalTime(data_end).rfc3339(),
                LocalTime(wanted).rfc3339()
            ));
        }

        let searchable = window::clip(&slots, from, until);
        let found = window::cheapest(&searchable, q.duration_secs, q.top.max(1));
        let Some(best) = found.first() else {
            return Err(Error::not_yet(
                format!(
                    "published {} prices end at {}, too soon for a {length} run starting from {}",
                    q.market.label(),
                    LocalTime(data_end).rfc3339(),
                    LocalTime(from).rfc3339()
                ),
                Some(publication_hint(q.market)),
            ));
        };

        let as_window = |w: &window::Window| PriceWindow {
            start: LocalTime(w.start),
            end: LocalTime(w.end),
            mean_eur_per_mwh: round(w.mean, 2),
            mean_eur_per_kwh: round(w.mean / 1000.0, 5),
            starts_in_minutes: (w.start - now).as_mins(),
        };
        let earliest = window::mean_over(&searchable, from, q.duration_secs).map(|mean| {
            as_window(&window::Window {
                start: from,
                end: Timestamp::from_second(from.as_second() + q.duration_secs).unwrap_or(from),
                mean,
            })
        });
        let best_out = as_window(best);
        let saving_eur_per_kwh =
            earliest.map(|e| round(e.mean_eur_per_kwh - best_out.mean_eur_per_kwh, 5));
        // A percentage only means something between two positive prices.
        let saving_percent = earliest
            .filter(|e| e.mean_eur_per_mwh > 0.0 && best_out.mean_eur_per_mwh > 0.0)
            .map(|e| {
                let saved = e.mean_eur_per_mwh - best_out.mean_eur_per_mwh;
                round(saved / e.mean_eur_per_mwh * 100.0, 1)
            });

        Ok(Cheapest {
            schema: SCHEMA_CHEAPEST,
            market: q.market,
            area: AREA_PRICES,
            timezone: ZONE,
            duration_minutes: q.duration_secs / 60,
            searched: SearchSpan {
                from: LocalTime(from),
                until: LocalTime(until),
                tomorrow,
            },
            best: best_out,
            alternatives: found[1..].iter().map(as_window).collect(),
            earliest,
            saving_eur_per_kwh,
            saving_percent,
            warnings,
            sources,
        })
    }

    /// Load one day of one market: cache first, then REData.
    async fn day(&self, market: Market, day: Date) -> Result<Day> {
        let now = self.now();
        let today = time::today(now);
        // Prices are set one day ahead, so nothing beyond tomorrow can exist yet.
        if today.tomorrow().is_ok_and(|tomorrow| day > tomorrow) {
            return Err(Error::not_yet(
                format!("{} prices for {day} are not published yet", market.label()),
                Some(
                    "prices are published one day ahead: today is available, and tomorrow from the afternoon or evening",
                ),
            ));
        }
        let (day_start, day_end) = time::day_bounds(day)?;

        let cached = match (&self.cache, self.refresh) {
            (Some(cache), false) => cache
                .get(market, day)
                .and_then(|e| from_cache(e, day_start, day_end)),
            _ => None,
        };
        if let Some((cached_day, fetched_at)) = &cached {
            let fresh = (now - *fetched_at).as_secs() < REVALIDATE_AFTER_SECS;
            if day_end <= now || fresh {
                return Ok(cached_day.clone());
            }
        }
        // With a copy in hand this is only a check for a correction, so it gets one
        // short attempt rather than the full retry budget.
        let quick = cached.is_some();
        match (
            self.fetch_day(market, day, day_start, day_end, quick).await,
            cached,
        ) {
            // The check failed, or came back with less than we already hold: the
            // copy is still the best answer, and `sources[].fetched_at` shows its age.
            (Err(_), Some((cached_day, _))) => Ok(cached_day),
            (Ok(fresh), Some((cached_day, _))) if !fresh.complete => Ok(cached_day),
            (result, _) => result,
        }
    }

    async fn fetch_day(
        &self,
        market: Market,
        day: Date,
        day_start: Timestamp,
        day_end: Timestamp,
        quick: bool,
    ) -> Result<Day> {
        let now = self.now();
        let url = self.client.url(
            PRICES_WIDGET,
            &[
                ("start_date", &format!("{day}T00:00")),
                ("end_date", &format!("{day}T23:59")),
                ("time_trunc", "hour"),
            ],
        );
        let shown_url = redata::redact(&url);
        let not_published = || {
            if day > time::today(now) {
                Error::not_yet(
                    format!("{} prices for {day} are not published yet", market.label()),
                    Some(publication_hint(market)),
                )
            } else {
                Error::no_data(format!(
                    "no {} prices are available for {day}",
                    market.label()
                ))
            }
        };

        let answer = if quick {
            self.client.get_quick(&url).await
        } else {
            self.client.get(&url).await
        };
        let envelope = match answer {
            Ok(envelope) => envelope,
            // "Nothing for that range" is how a day still to come, or one before the
            // records begin, is answered. Today's prices were published yesterday, so
            // for today the same answer is a failure on REData's side.
            Err(failure) if failure.is_no_data() && day != time::today(now) => {
                return Err(not_published());
            }
            Err(failure) => return Err(failure.into_error()),
        };

        // The answer carries both markets. Before a series is published it is simply
        // absent. Keep the market asked for, and cache every series that is complete.
        let mut wanted = None;
        for m in [Market::Pvpc, Market::Spot] {
            let Some(series) = envelope.included.iter().find(|s| s.id == m.series_id()) else {
                continue;
            };
            let observations = match series.observations() {
                Ok(observations) => observations,
                // A fault in the other market's series is not this request's problem.
                Err(_) if m != market => continue,
                Err(err) => return Err(err),
            };
            let slots = to_slots(&observations, m, day_start, day_end);
            let complete = is_complete(&slots, day_start, day_end);
            let published_at = series.last_update();
            if complete && let Some(cache) = &self.cache {
                let stored = slots.iter().map(|s| (s.start, s.end, s.value)).collect();
                cache.put(
                    m,
                    day,
                    &Entry::new(shown_url.clone(), now, published_at, stored),
                );
            }
            if m == market && !slots.is_empty() {
                wanted = Some(Day {
                    slots,
                    complete,
                    source: Source {
                        name: redata::SOURCE_NAME,
                        dataset: Dataset::Prices,
                        url: shown_url.clone(),
                        updated_at: published_at.map(LocalTime),
                        fetched_at: LocalTime(now),
                        cached: false,
                    },
                });
            }
        }
        wanted.ok_or_else(not_published)
    }
}

/// A cache entry as a day, provided it really is a complete series for that day.
/// Anything else (damaged, edited, written for another day) is ignored.
fn from_cache(entry: Entry, day_start: Timestamp, day_end: Timestamp) -> Option<(Day, Timestamp)> {
    let slots: Vec<Slot> = entry
        .slots
        .iter()
        .map(|&(start, end, value)| Slot { start, end, value })
        .collect();
    let sound = slots.iter().all(|s| s.start < s.end && s.value.is_finite());
    if !sound || !is_complete(&slots, day_start, day_end) {
        return None;
    }
    let day = Day {
        slots,
        complete: true,
        source: Source {
            name: redata::SOURCE_NAME,
            dataset: Dataset::Prices,
            url: entry.url,
            updated_at: entry.published_at.map(LocalTime),
            fetched_at: LocalTime(entry.fetched_at),
            cached: true,
        },
    };
    Some((day, entry.fetched_at))
}

/// When next-day prices have been seen to appear, from their update stamps.
fn publication_hint(market: Market) -> &'static str {
    match market {
        Market::Pvpc => {
            "Red Eléctrica publishes next-day PVPC in the evening, around 20:45 to 20:50 Madrid time; ask again after that"
        }
        Market::Spot => {
            "next-day spot prices appear in the afternoon, between about 14:30 and 15:40 Madrid time; ask again after that"
        }
    }
}

/// Turn observations into slots inside `[day_start, day_end)`.
///
/// The slot length is the smallest spacing seen, so a finer series than expected is
/// read correctly, but never longer than the market's own interval: a series with
/// holes must show up as incomplete, not as a day of very long intervals.
fn to_slots(
    observations: &[(Timestamp, f64)],
    market: Market,
    day_start: Timestamp,
    day_end: Timestamp,
) -> Vec<Slot> {
    let mut in_day: Vec<(Timestamp, f64)> = observations
        .iter()
        .copied()
        .filter(|(at, value)| *at >= day_start && *at < day_end && value.is_finite())
        .collect();
    in_day.dedup_by_key(|(at, _)| *at);
    let nominal = market.nominal_slot_secs(LocalTime(day_start).date());
    let step = in_day
        .windows(2)
        .map(|pair| (pair[1].0 - pair[0].0).as_secs())
        .min()
        .map_or(nominal, |seen| seen.min(nominal));
    in_day
        .iter()
        .map(|&(start, value)| {
            let end = Timestamp::from_second(start.as_second() + step).unwrap_or(day_end);
            Slot {
                start,
                end: end.min(day_end),
                value,
            }
        })
        .collect()
}

/// Whether the slots tile the whole day with no gap.
fn is_complete(slots: &[Slot], day_start: Timestamp, day_end: Timestamp) -> bool {
    let (Some(first), Some(last)) = (slots.first(), slots.last()) else {
        return false;
    };
    first.start == day_start
        && last.end == day_end
        && slots.windows(2).all(|pair| pair[0].end == pair[1].start)
}

fn resolution_minutes(slots: &[Slot]) -> u32 {
    slots
        .first()
        .map_or(0, |s| ((s.end - s.start).as_secs() / 60) as u32)
}

fn point(slot: &Slot) -> PricePoint {
    PricePoint::new(slot.start, slot.end, slot.value)
}

/// Min, max and time-weighted mean. `slots` must not be empty.
fn stats(slots: &[Slot]) -> PriceStats {
    let mut min = slots[0];
    let mut max = slots[0];
    let (mut weighted, mut total) = (0.0, 0.0);
    for s in slots {
        if s.value < min.value {
            min = *s;
        }
        if s.value > max.value {
            max = *s;
        }
        let secs = (s.end - s.start).as_secs() as f64;
        weighted += s.value * secs;
        total += secs;
    }
    let mean = if total > 0.0 { weighted / total } else { 0.0 };
    PriceStats {
        min: point(&min),
        max: point(&max),
        mean_eur_per_mwh: round(mean, 2),
        mean_eur_per_kwh: round(mean / 1000.0, 5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn hourly(
        day: &str,
        hours: i64,
        skip: &[i64],
    ) -> (Vec<(Timestamp, f64)>, Timestamp, Timestamp) {
        let (start, end) = time::day_bounds(day.parse().unwrap()).unwrap();
        let obs = (0..hours)
            .filter(|h| !skip.contains(h))
            .map(|h| {
                (
                    Timestamp::from_second(start.as_second() + h * 3600).unwrap(),
                    100.0 + h as f64,
                )
            })
            .collect();
        (obs, start, end)
    }

    #[test]
    fn a_normal_day_is_24_complete_hourly_slots() {
        let (obs, start, end) = hourly("2026-11-25", 24, &[]);
        let slots = to_slots(&obs, Market::Pvpc, start, end);
        assert_eq!(slots.len(), 24);
        assert!(is_complete(&slots, start, end));
        assert_eq!(resolution_minutes(&slots), 60);
    }

    #[test]
    fn clock_change_days_are_complete_at_23_and_25_hours() {
        for (day, hours) in [("2026-03-29", 23), ("2026-10-25", 25)] {
            let (obs, start, end) = hourly(day, hours, &[]);
            let slots = to_slots(&obs, Market::Pvpc, start, end);
            assert_eq!(slots.len() as i64, hours);
            assert!(is_complete(&slots, start, end), "{day}");
        }
        // Assuming 24 hours is wrong on both days: the long day would be left one hour
        // short, and on the short day the 24th value belongs to the next day.
        let (obs, start, end) = hourly("2026-10-25", 24, &[]);
        assert!(!is_complete(
            &to_slots(&obs, Market::Pvpc, start, end),
            start,
            end
        ));
        let (obs, start, end) = hourly("2026-03-29", 24, &[]);
        assert_eq!(to_slots(&obs, Market::Pvpc, start, end).len(), 23);
    }

    #[test]
    fn a_missing_hour_or_a_partial_day_is_not_complete() {
        let (obs, start, end) = hourly("2026-11-25", 24, &[7]);
        let slots = to_slots(&obs, Market::Pvpc, start, end);
        assert_eq!(slots.len(), 23);
        assert!(!is_complete(&slots, start, end));
        let (obs, start, end) = hourly("2026-11-25", 18, &[]);
        assert!(!is_complete(
            &to_slots(&obs, Market::Pvpc, start, end),
            start,
            end
        ));
        assert!(!is_complete(&[], start, end));
    }

    #[test]
    fn a_sparse_series_is_never_stretched_into_a_complete_day() {
        // Every other hour missing, and only two values twelve hours apart: the
        // spacing seen is wider than an hour, but the slots must stay hourly.
        let odd: Vec<i64> = (0..24).filter(|h| h % 2 == 1).collect();
        let (obs, start, end) = hourly("2026-11-25", 24, &odd);
        let slots = to_slots(&obs, Market::Pvpc, start, end);
        assert_eq!((slots.len(), resolution_minutes(&slots)), (12, 60));
        assert!(!is_complete(&slots, start, end));

        let (obs, start, end) =
            hourly("2026-11-25", 24, &(1..12).chain(13..24).collect::<Vec<_>>());
        let slots = to_slots(&obs, Market::Pvpc, start, end);
        assert_eq!((slots.len(), resolution_minutes(&slots)), (2, 60));
        assert!(!is_complete(&slots, start, end));
    }

    #[test]
    fn quarter_hour_series_are_detected_from_the_data() {
        let (start, end) = time::day_bounds("2026-11-25".parse().unwrap()).unwrap();
        let obs: Vec<_> = (0..96)
            .map(|q| {
                (
                    Timestamp::from_second(start.as_second() + q * 900).unwrap(),
                    q as f64,
                )
            })
            .collect();
        let slots = to_slots(&obs, Market::Spot, start, end);
        assert_eq!((slots.len(), resolution_minutes(&slots)), (96, 15));
        assert!(is_complete(&slots, start, end));
        // A market that moves to a finer interval than expected is still read right.
        let slots = to_slots(&obs, Market::Pvpc, start, end);
        assert_eq!(resolution_minutes(&slots), 15);
    }

    #[test]
    fn spot_was_hourly_before_october_2025() {
        // Delivery days up to 30 September 2025 carry 24 spot prices, and that is a
        // complete day, not a quarter-hourly one with three quarters of it missing.
        let (obs, start, end) = hourly("2025-06-01", 24, &[]);
        let slots = to_slots(&obs, Market::Spot, start, end);
        assert_eq!((slots.len(), resolution_minutes(&slots)), (24, 60));
        assert!(is_complete(&slots, start, end));
        // The same shape after the change is a quarter-hourly day full of holes.
        let (obs, start, end) = hourly("2025-10-01", 24, &[]);
        let slots = to_slots(&obs, Market::Spot, start, end);
        assert_eq!(resolution_minutes(&slots), 15);
        assert!(!is_complete(&slots, start, end));
    }

    #[test]
    fn observations_outside_the_day_are_dropped() {
        let (mut obs, start, end) = hourly("2026-11-25", 24, &[]);
        obs.insert(0, (ts("2026-11-24T22:00:00Z"), 1.0));
        obs.push((ts("2026-11-25T23:00:00Z"), 1.0));
        assert_eq!(to_slots(&obs, Market::Pvpc, start, end).len(), 24);
    }

    #[test]
    fn stats_pick_the_earliest_extreme_and_weight_by_time() {
        let slot = |start: i64, end: i64, value| Slot {
            start: Timestamp::from_second(start).unwrap(),
            end: Timestamp::from_second(end).unwrap(),
            value,
        };
        let s = stats(&[
            slot(0, 3600, 50.0),
            slot(3600, 7200, 10.0),
            slot(7200, 9000, 10.0),
            slot(9000, 10800, 50.0),
        ]);
        assert_eq!(s.min.start.0.as_second(), 3600);
        assert_eq!(s.max.start.0.as_second(), 0);
        // 1h30 at 50 and 1h30 at 10.
        assert_eq!((s.mean_eur_per_mwh, s.mean_eur_per_kwh), (30.0, 0.03));
    }

    #[test]
    fn cache_entries_are_only_trusted_when_they_tile_their_day() {
        let (obs, start, end) = hourly("2026-11-25", 24, &[]);
        let slots = to_slots(&obs, Market::Pvpc, start, end);
        let entry = |slots: &[Slot]| {
            Entry::new(
                "u".into(),
                start,
                None,
                slots.iter().map(|s| (s.start, s.end, s.value)).collect(),
            )
        };
        assert!(from_cache(entry(&slots), start, end).is_some());
        // The same entry offered for another day, a day with a hole, and a slot that
        // runs backwards are all refused.
        let (other_start, other_end) = time::day_bounds("2026-11-26".parse().unwrap()).unwrap();
        assert!(from_cache(entry(&slots), other_start, other_end).is_none());
        assert!(from_cache(entry(&slots[1..]), start, end).is_none());
        let mut bad = slots.clone();
        bad[3].value = f64::NAN;
        assert!(from_cache(entry(&bad), start, end).is_none());
    }
}
