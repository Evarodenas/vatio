//! The grid snapshot: live demand plus the most recent daily generation mix.
//!
//! The two halves have different freshness, and the output says so. Demand is measured
//! every few minutes; the mix by technology and its emissions are published per day.

use std::collections::BTreeMap;

use jiff::{Timestamp, ToSpan, civil::Date};

use crate::{
    error::{Error, Result},
    model::{
        AREA_GRID, Dataset, Demand, DemandReading, Generation, Grid, SCHEMA_GRID, Source,
        Technology, round,
    },
    redata::{self, Envelope, Series},
    service::Vatio,
    text,
    time::{self, LocalTime, ZONE},
};

const DEMAND_WIDGET: &str = "demanda/demanda-tiempo-real";
const MIX_WIDGET: &str = "generacion/estructura-generacion";
const CO2_WIDGET: &str = "generacion/no-renovables-detalle-emisiones-CO2";

/// Series identifiers in the real-time demand widget.
const DEMAND_ACTUAL: &str = "2037";
const DEMAND_FORECAST: &str = "2052";
const DEMAND_SCHEDULED: &str = "2053";

/// Series identifiers in the generation and emissions widgets.
const MIX_TOTAL: &str = "10296";
const CO2_TOTAL: &str = "tCO2 eq.";

/// How far back to look for the latest published daily figures.
const MIX_LOOKBACK_DAYS: i64 = 4;

impl Vatio {
    /// Live demand and the latest daily generation mix. Succeeds if either half does;
    /// the missing half is null and explained in `warnings`.
    pub async fn grid(&self) -> Result<Grid> {
        let now = self.now();
        let today = time::today(now);
        // Daily figures: completed days only. The day in progress is an estimate.
        let last = today.yesterday().map_err(Error::internal)?;
        let first = today
            .checked_sub(MIX_LOOKBACK_DAYS.days())
            .map_err(Error::internal)?;

        let demand_url = range_url(&self.client, DEMAND_WIDGET, today, today, "hour");
        let mix_url = range_url(&self.client, MIX_WIDGET, first, last, "day");
        let co2_url = range_url(&self.client, CO2_WIDGET, first, last, "day");
        let (demand_res, mix_res, co2_res) = tokio::join!(
            self.client.get(&demand_url),
            self.client.get(&mix_url),
            self.client.get(&co2_url)
        );

        let mut warnings = Vec::new();
        let mut sources = Vec::new();
        let mut cite = |dataset: Dataset, url: &str, env: &Envelope| {
            sources.push(Source {
                name: redata::SOURCE_NAME,
                dataset,
                url: redata::redact(url),
                updated_at: env.last_update().map(LocalTime),
                fetched_at: LocalTime(now),
                cached: false,
            });
        };
        let read = |res: std::result::Result<Envelope, redata::Failure>| {
            res.map_err(redata::Failure::into_error)
        };

        let demand = read(demand_res).and_then(|env| Ok((demand(&env, now)?, env)));
        let mix = read(mix_res).and_then(|env| Ok((mix(&env)?, env)));
        let (demand, generation) = match (demand, mix) {
            // Nothing to show. Report the failure itself, keeping its kind when both
            // halves failed the same way (no network, the bot wall), so that callers
            // can still tell what happened.
            (Err(d), Err(m)) if d.code() == m.code() => return Err(d),
            (Err(d), Err(m)) => {
                return Err(Error::Upstream {
                    status: None,
                    message: format!(
                        "live demand: {}; generation mix: {}",
                        d.message(),
                        m.message()
                    ),
                });
            }
            (demand, mix) => (demand, mix),
        };

        let demand = match demand {
            Ok((demand, env)) => {
                cite(Dataset::Demand, &demand_url, &env);
                Some(demand)
            }
            Err(err) => {
                warnings.push(format!("live demand is unavailable: {}", err.message()));
                None
            }
        };
        let generation = match generation {
            Ok((found, env)) => {
                cite(Dataset::Generation, &mix_url, &env);
                let Mix {
                    mut generation,
                    day,
                    notes,
                } = found;
                warnings.extend(notes);
                match read(co2_res).and_then(|env| Ok((co2(&env, day)?, env))) {
                    Ok((tonnes, env)) => {
                        generation.co2_g_per_kwh = intensity(tonnes, generation.total_mwh);
                        generation.co2_tonnes = Some(round(tonnes, 1));
                        cite(Dataset::Emissions, &co2_url, &env);
                    }
                    Err(err) => {
                        warnings.push(format!(
                            "emissions figures are unavailable: {}",
                            err.message()
                        ));
                    }
                }
                Some(generation)
            }
            Err(err) => {
                warnings.push(format!(
                    "the generation mix is unavailable: {}",
                    err.message()
                ));
                None
            }
        };

        Ok(Grid {
            schema: SCHEMA_GRID,
            area: AREA_GRID,
            at: LocalTime(now),
            timezone: ZONE,
            demand,
            generation,
            warnings,
            sources,
        })
    }
}

fn range_url(client: &redata::Client, widget: &str, from: Date, to: Date, trunc: &str) -> String {
    let (start, end) = (format!("{from}T00:00"), format!("{to}T23:59"));
    client.url(
        widget,
        &[
            ("start_date", &start),
            ("end_date", &end),
            ("time_trunc", trunc),
        ],
    )
}

fn series<'a>(env: &'a Envelope, id: &str) -> Option<&'a Series> {
    env.included.iter().find(|s| s.id == id)
}

fn demand(env: &Envelope, now: Timestamp) -> Result<Demand> {
    // The forecast and schedule run ahead of the clock; only the measured series
    // says what has happened, so everything is anchored on its latest value.
    let actual: Vec<(Timestamp, f64)> = series(env, DEMAND_ACTUAL)
        .map(Series::observations)
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .filter(|(at, _)| *at <= now)
        .collect();
    let &(at, actual_mw) = actual.last().ok_or_else(|| {
        Error::not_yet(
            "Red Eléctrica has no demand reading for today yet",
            Some("readings trail the clock by about ten minutes; ask again shortly"),
        )
    })?;

    let same_instant = |id: &str| -> Option<f64> {
        let observations = series(env, id)?.observations().ok()?;
        observations.iter().find(|(t, _)| *t == at).map(|(_, v)| *v)
    };
    let step = actual.windows(2).map(|p| (p[1].0 - p[0].0).as_secs()).min();
    let reading = |(at, mw): (Timestamp, f64)| DemandReading {
        at: LocalTime(at),
        mw,
    };
    let peak = actual
        .iter()
        .copied()
        .fold((at, actual_mw), |a, b| if b.1 > a.1 { b } else { a });
    let low = actual
        .iter()
        .copied()
        .fold((at, actual_mw), |a, b| if b.1 < a.1 { b } else { a });

    Ok(Demand {
        at: LocalTime(at),
        actual_mw,
        forecast_mw: same_instant(DEMAND_FORECAST),
        scheduled_mw: same_instant(DEMAND_SCHEDULED),
        resolution_minutes: step.map(|secs| (secs / 60) as u32),
        peak_today: reading(peak),
        low_today: reading(low),
    })
}

/// A day's generation mix, the day it is for, and anything worth telling the reader.
struct Mix {
    generation: Generation,
    day: Date,
    notes: Vec<String>,
}

/// The latest day in the answer whose figures hold together, with its technologies.
///
/// Recent days can be half-loaded. A day is used only if its technologies sum to
/// something positive and, when Red Eléctrica's own total row is present, agree with
/// it; otherwise the search steps back to the day before.
fn mix(env: &Envelope) -> Result<Mix> {
    let mut by_day: BTreeMap<Date, Vec<(&Series, f64)>> = BTreeMap::new();
    let mut totals: BTreeMap<Date, f64> = BTreeMap::new();
    for s in &env.included {
        for (at, value) in s.observations()? {
            let day = LocalTime(at).date();
            if is_total(s) {
                totals.insert(day, value);
            } else {
                by_day.entry(day).or_default().push((s, value));
            }
        }
    }

    // A day holds together when its technologies sum to something positive and match
    // Red Eléctrica's own total for that day. If the answer has no total rows at
    // all, the next best check is that no series is missing compared with other days.
    let most_series = by_day.values().map(Vec::len).max().unwrap_or(0);
    let holds = |day: &Date, rows: &[(&Series, f64)], sum: f64| {
        sum > 0.0
            && match totals.get(day) {
                Some(total) => (sum - total).abs() <= total.abs() * 0.005,
                None => totals.is_empty() && rows.len() == most_series,
            }
    };
    let sum_of = |rows: &[(&Series, f64)]| rows.iter().map(|(_, v)| v).sum::<f64>();

    let mut notes = Vec::new();
    let mut chosen = None;
    for (day, rows) in by_day.iter().rev() {
        let sum = sum_of(rows);
        if holds(day, rows, sum) {
            chosen = Some((*day, rows, sum));
            break;
        }
        notes.push(format!(
            "generation figures for {day} are incomplete; showing an earlier day"
        ));
    }
    if chosen.is_none() {
        // Nothing passes the check. Showing the newest figures with a plain warning
        // serves the reader better than showing nothing.
        chosen = by_day
            .iter()
            .rev()
            .map(|(day, rows)| (*day, rows, sum_of(rows)))
            .find(|(_, _, sum)| *sum > 0.0);
        notes.clear();
        if let Some((day, ..)) = chosen {
            notes.push(format!(
                "generation figures for {day} could not be checked against Red Eléctrica's own total and may be incomplete"
            ));
        }
    }
    let Some((day, rows, total)) = chosen else {
        return Err(Error::not_yet(
            "Red Eléctrica has published no generation figures for the last few days",
            None,
        ));
    };

    let mut technologies: Vec<Technology> = rows
        .iter()
        .map(|(s, value)| {
            let name_es = text::clip(
                s.attributes
                    .title
                    .as_deref()
                    .or(s.kind.as_deref())
                    .unwrap_or(&s.id),
            );
            let (id, name) = technology(&s.id, &name_es);
            Technology {
                id,
                name,
                name_es,
                renewable: is_renewable(s),
                mwh: round(*value, 1),
                percent: round(value / total * 100.0, 1),
            }
        })
        .collect();
    technologies.sort_by(|a, b| b.mwh.total_cmp(&a.mwh).then_with(|| a.id.cmp(&b.id)));

    // The renewable share needs every technology classified. If the answer stops
    // saying which is which, report that instead of a share that looks like zero.
    let renewable_percent = if rows.iter().all(|(s, _)| is_renewable(s).is_some()) {
        let renewable: f64 = rows
            .iter()
            .filter(|(s, _)| is_renewable(s) == Some(true))
            .map(|(_, v)| v)
            .sum();
        Some(round(renewable / total * 100.0, 1))
    } else {
        notes.push(
            "Red Eléctrica's answer does not say which technologies are renewable, so the renewable share is not given"
                .to_owned(),
        );
        None
    };

    Ok(Mix {
        generation: Generation {
            day: day.to_string(),
            total_mwh: round(total, 1),
            renewable_percent,
            co2_g_per_kwh: None,
            co2_tonnes: None,
            technologies,
        },
        day,
        notes,
    })
}

/// Tonnes of CO2 equivalent emitted by generation on `day`.
fn co2(env: &Envelope, day: Date) -> Result<f64> {
    let total = series(env, CO2_TOTAL).or_else(|| {
        env.included.iter().find(|s| {
            s.attributes
                .title
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("Total tCO2 eq."))
        })
    });
    total
        .and_then(|s| s.observations().ok())
        .and_then(|days| {
            days.into_iter()
                .find(|(at, _)| LocalTime(*at).date() == day)
        })
        .map(|(_, tonnes)| tonnes)
        .ok_or_else(|| {
            Error::not_yet(
                format!("Red Eléctrica has published no emissions figures for {day}"),
                None,
            )
        })
}

/// Grams of CO2 equivalent per kWh generated: the day's emissions over the day's
/// generation. REData publishes this factor as a series of its own (id `MWh`),
/// but in an answer spanning several days that series can be out of step with the
/// days it is labelled with (seen on 2026-11-26: each value was another day's
/// emissions over this day's generation), so it is worked out here from the two
/// totals, which is how REData defines it.
fn intensity(tonnes: f64, generated_mwh: f64) -> Option<f64> {
    // Tonnes per MWh is numerically kilograms per kWh.
    (generated_mwh > 0.0).then(|| round(tonnes / generated_mwh * 1000.0, 1))
}

fn is_total(s: &Series) -> bool {
    s.id == MIX_TOTAL
        || s.attributes.composite
        || s.attributes
            .class
            .as_deref()
            .is_some_and(|c| fold(c) == "total")
}

/// Whether the answer classes the series as renewable. `None` when it does not say.
fn is_renewable(s: &Series) -> Option<bool> {
    match fold(s.attributes.class.as_deref()?).as_str() {
        "renovable" => Some(true),
        "no-renovable" | "no renovable" => Some(false),
        _ => None,
    }
}

/// Lower-case with Spanish accents removed, for matching and identifiers.
fn fold(text: &str) -> String {
    text.trim()
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' => 'a',
            'é' => 'e',
            'í' => 'i',
            'ó' => 'o',
            'ú' | 'ü' => 'u',
            'ñ' => 'n',
            other => other,
        })
        .collect()
}

/// Identifier and English name for a technology.
///
/// Matched on REData's series id first and on its Spanish title second, because the
/// titles have been seen to change between publications. A technology this table
/// does not know still gets a usable identifier, so it shows up instead of vanishing.
fn technology(series_id: &str, name_es: &str) -> (String, String) {
    /// (REData id, Spanish titles seen, identifier, English name)
    const KNOWN: [(&str, &[&str], &str, &str); 18] = [
        ("10288", &["hidraulica"], "hydro", "Hydro"),
        (
            "1445",
            &["turbinacion bombeo"],
            "pumped_hydro",
            "Pumped hydro",
        ),
        ("", &["hidroeolica"], "hydro_wind", "Hydro-wind"),
        ("1446", &["nuclear"], "nuclear", "Nuclear"),
        ("10289", &["carbon"], "coal", "Coal"),
        ("", &["fuel + gas"], "fuel_gas", "Fuel and gas"),
        (
            "10344",
            &["motor combustion interna", "motores diesel"],
            "combustion_engines",
            "Combustion engines",
        ),
        ("1450", &["turbina de gas"], "gas_turbine", "Gas turbine"),
        (
            "1451",
            &["turbina de vapor"],
            "steam_turbine",
            "Steam turbine",
        ),
        (
            "1454",
            &["ciclo combinado"],
            "combined_cycle",
            "Combined-cycle gas",
        ),
        ("10291", &["eolica"], "wind", "Wind"),
        ("1458", &["solar fotovoltaica"], "solar_pv", "Solar PV"),
        ("1459", &["solar termica"], "solar_thermal", "Solar thermal"),
        (
            "10292",
            &["otras renovables"],
            "other_renewables",
            "Other renewables",
        ),
        ("10513", &["biocombustibles"], "biofuels", "Biofuels"),
        ("10293", &["cogeneracion"], "cogeneration", "Cogeneration"),
        (
            "10294",
            &["residuos no renovables"],
            "non_renewable_waste",
            "Non-renewable waste",
        ),
        (
            "10295",
            &["residuos renovables"],
            "renewable_waste",
            "Renewable waste",
        ),
    ];
    let folded = fold(name_es);
    let known = KNOWN
        .iter()
        .find(|(id, ..)| !id.is_empty() && *id == series_id)
        .or_else(|| {
            KNOWN
                .iter()
                .find(|(_, titles, ..)| titles.contains(&folded.as_str()))
        });
    if let Some((_, _, id, name)) = known {
        return ((*id).to_owned(), (*name).to_owned());
    }
    let mut id = String::new();
    for c in folded.chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c);
        } else if !id.ends_with('_') && !id.is_empty() {
            id.push('_');
        }
    }
    (
        id.trim_end_matches('_').to_owned(),
        name_es.trim().to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> serde_json::Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
            .unwrap()
    }

    fn envelope(json: serde_json::Value) -> Envelope {
        serde_json::from_value(json).unwrap()
    }

    const MIX_FIXTURE: &str = "redata_generation_structure_day_2026-11-24.json";

    #[test]
    fn mix_reads_the_generation_structure() {
        let found = mix(&envelope(fixture(MIX_FIXTURE))).unwrap();
        let generation = found.generation;
        assert_eq!(found.day.to_string(), "2026-11-24");
        assert!(found.notes.is_empty());
        // The published total row is not a technology, and agrees with their sum.
        assert_eq!(generation.total_mwh, 808_145.3);
        assert_eq!(generation.technologies.len(), 14);
        let wind = &generation.technologies[0];
        assert_eq!(
            (wind.id.as_str(), wind.name_es.as_str()),
            ("wind", "Eólica")
        );
        assert_eq!(
            (wind.renewable, wind.mwh, wind.percent),
            (Some(true), 257_724.1, 31.9)
        );
        assert_eq!(generation.technologies[1].id, "nuclear");
        assert_eq!(generation.technologies[1].renewable, Some(false));
        // Hydro + wind + solar PV + solar thermal + biofuels + renewable waste.
        assert_eq!(generation.renewable_percent, Some(55.7));
        let share: f64 = generation.technologies.iter().map(|t| t.percent).sum();
        assert!((share - 100.0).abs() < 0.5, "{share}");
    }

    /// The fixture with one more day appended to every series, valued by `f(id, value)`.
    fn with_next_day(f: impl Fn(&str, f64) -> Option<f64>) -> Envelope {
        let mut json = fixture(MIX_FIXTURE);
        for s in json["included"].as_array_mut().unwrap() {
            let id = s["id"].as_str().unwrap().to_owned();
            let values = s["attributes"]["values"].as_array_mut().unwrap();
            let base = values[0]["value"].as_f64().unwrap();
            if let Some(value) = f(&id, base) {
                values.push(serde_json::json!({
                    "value": value, "percentage": 0.0, "datetime": "2026-11-25T00:00:00.000+01:00"
                }));
            }
        }
        envelope(json)
    }

    #[test]
    fn a_half_loaded_latest_day_is_passed_over_for_the_one_before() {
        // Only wind and hydro have arrived for the 25th, but the total row is there.
        let partial =
            with_next_day(|id, v| matches!(id, "10291" | "10288" | MIX_TOTAL).then_some(v));
        let found = mix(&partial).unwrap();
        assert_eq!(found.day.to_string(), "2026-11-24");
        assert_eq!(
            found.notes,
            ["generation figures for 2026-11-25 are incomplete; showing an earlier day"]
        );
        // A latest day of zeros is skipped the same way.
        let zeros = with_next_day(|_, _| Some(0.0));
        assert_eq!(mix(&zeros).unwrap().day.to_string(), "2026-11-24");
        // A latest day that lacks a series and has no total row to be checked against
        // is not trusted either, as long as an earlier day can be.
        let no_total = with_next_day(|id, v| (!matches!(id, "1446" | MIX_TOTAL)).then_some(v));
        let found = mix(&no_total).unwrap();
        assert_eq!(
            (found.day.to_string(), found.notes.len()),
            ("2026-11-24".to_owned(), 1)
        );
        // A complete latest day is used.
        let full = with_next_day(|_, v| Some(v * 1.1));
        let found = mix(&full).unwrap();
        assert_eq!(
            (found.day.to_string(), found.notes.len()),
            ("2026-11-25".to_owned(), 0)
        );
        assert_eq!(found.generation.total_mwh, 888_959.9);
    }

    #[test]
    fn when_no_day_can_be_verified_the_newest_is_shown_with_a_warning() {
        // The total row disagrees with the technologies by 1%: nothing checks out.
        let mut json = fixture(MIX_FIXTURE);
        for s in json["included"].as_array_mut().unwrap() {
            if s["id"] == MIX_TOTAL {
                s["attributes"]["values"][0]["value"] = serde_json::json!(816_226.8);
            }
        }
        let found = mix(&envelope(json)).unwrap();
        assert_eq!(found.day.to_string(), "2026-11-24");
        assert_eq!(found.generation.total_mwh, 808_145.3);
        assert_eq!(
            found.notes,
            [
                "generation figures for 2026-11-24 could not be checked against Red Eléctrica's own total and may be incomplete"
            ]
        );
        // And with no positive figures at all there is nothing to show.
        let mut json = fixture(MIX_FIXTURE);
        for s in json["included"].as_array_mut().unwrap() {
            s["attributes"]["values"][0]["value"] = serde_json::json!(0);
        }
        assert!(mix(&envelope(json)).is_err());
    }

    #[test]
    fn without_classification_the_renewable_share_is_withheld_not_zero() {
        let mut json = fixture(MIX_FIXTURE);
        for s in json["included"].as_array_mut().unwrap() {
            if s["id"] != MIX_TOTAL {
                s["attributes"]["type"] = serde_json::Value::Null;
            }
        }
        let found = mix(&envelope(json)).unwrap();
        assert_eq!(found.generation.renewable_percent, None);
        assert!(
            found
                .generation
                .technologies
                .iter()
                .all(|t| t.renewable.is_none())
        );
        assert_eq!(found.notes.len(), 1);
        // The total row is still recognised by its id, so the sum is not doubled.
        assert_eq!(found.generation.total_mwh, 808_145.3);
    }

    #[test]
    fn co2_intensity_is_the_days_emissions_over_the_days_generation() {
        // Raw answers from the live API for 22 to 25 November 2026.
        let emissions = envelope(fixture("redata_co2_emissions_day_2026-11-22_25.json"));
        let found = mix(&envelope(fixture(
            "redata_generation_structure_day_2026-11-22_25.json",
        )))
        .unwrap();
        assert_eq!(found.day.to_string(), "2026-11-25");
        assert_eq!(found.generation.total_mwh, 892_181.3);

        let tonnes = co2(&emissions, found.day).unwrap();
        assert_eq!(round(tonnes, 1), 67_445.0);
        assert_eq!(
            intensity(tonnes, found.generation.total_mwh),
            Some(75.6),
            "67,445 t over 892,181 MWh"
        );

        // The factor series in that same answer says 64.3 for the 25th: the emissions
        // of the 22nd over the generation of the 25th. It is not used.
        let published = series(&emissions, "MWh").unwrap().observations().unwrap();
        let (_, factor) = published.last().unwrap();
        assert_eq!(round(factor * 1000.0, 1), 64.3);
        assert_eq!(
            intensity(
                co2(&emissions, "2026-11-22".parse().unwrap()).unwrap(),
                892_181.3
            ),
            Some(64.3)
        );

        // Every day's total equals the sum of its technologies, whatever order the
        // total series lists its days in.
        for day in ["2026-11-22", "2026-11-23", "2026-11-24", "2026-11-25"] {
            let day: Date = day.parse().unwrap();
            let sum: f64 = emissions
                .included
                .iter()
                .filter(|s| !s.attributes.composite)
                .flat_map(|s| s.observations().unwrap())
                .filter(|(at, _)| LocalTime(*at).date() == day)
                .map(|(_, v)| v)
                .sum();
            assert!((co2(&emissions, day).unwrap() - sum).abs() < 0.01, "{day}");
        }

        assert!(co2(&emissions, "2026-11-20".parse().unwrap()).is_err());
        assert_eq!(intensity(10.0, 0.0), None);
    }

    #[test]
    fn demand_takes_the_latest_measured_value_and_its_companions() {
        let env = envelope(fixture("redata_demand_realtime_2026-11-26T2000_2030.json"));
        let d = demand(&env, "2026-11-26T19:15:00Z".parse().unwrap()).unwrap();
        assert_eq!(d.at.rfc3339(), "2026-11-26T20:05:00+01:00");
        assert_eq!(
            (d.actual_mw, d.forecast_mw, d.scheduled_mw),
            (38_261.0, Some(38_792.0), Some(38_662.0))
        );
        assert_eq!(d.resolution_minutes, Some(5));
        assert_eq!((d.peak_today.mw, d.low_today.mw), (38_388.0, 38_261.0));
        // With a single reading there is no spacing to report.
        let one = demand(&env, "2026-11-26T19:02:00Z".parse().unwrap()).unwrap();
        assert_eq!((one.actual_mw, one.resolution_minutes), (38_388.0, None));
        // Asked a moment before the first reading: nothing measured yet, try again.
        let none = demand(&env, "2026-11-26T18:59:00Z".parse().unwrap()).unwrap_err();
        assert!(none.retryable());
    }

    #[test]
    fn technologies_are_matched_by_id_then_title_and_unknown_ones_survive() {
        assert_eq!(
            technology("10291", "Eólica"),
            ("wind".into(), "Wind".into())
        );
        // Same technology under either of the titles it has been published with.
        assert_eq!(
            technology("10344", "Motor combustión interna").0,
            "combustion_engines"
        );
        assert_eq!(technology("x", "Motores diésel").0, "combustion_engines");
        // A retitled series keeps its identifier through its REData id.
        assert_eq!(technology("1454", "CCGT").0, "combined_cycle");
        assert_eq!(
            technology("x", "Fuel + Gas"),
            ("fuel_gas".into(), "Fuel and gas".into())
        );
        assert_eq!(
            technology("99", "Almacenamiento (baterías)"),
            (
                "almacenamiento_baterias".into(),
                "Almacenamiento (baterías)".into()
            )
        );
    }
}
