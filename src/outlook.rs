//! The estimate behind `vatio outlook`: prices for days that Red Eléctrica has not
//! published yet, taken from recent days that it has.
//!
//! The rule is deliberately plain, and it is the one that was backtested. A day is a
//! weekday (Monday to Friday) or a weekend day, with no holiday calendar. Each local
//! clock hour of an unpublished day is priced at the mean of that clock hour over the
//! four most recent published days of the same kind.

use jiff::{
    Timestamp,
    civil::{Date, Weekday},
};

use crate::{
    error::Result,
    model::DayKind,
    time::{self, LocalTime},
    window::Slot,
};

/// How many published days of the same kind an estimate is the mean of.
pub const DAYS_AVERAGED: usize = 4;

pub fn kind(day: Date) -> DayKind {
    match day.weekday() {
        Weekday::Saturday | Weekday::Sunday => DayKind::Weekend,
        _ => DayKind::Weekday,
    }
}

/// The days an estimate for `kind` is averaged from: walking back from `last` to
/// `first`, the first [`DAYS_AVERAGED`] days of that kind that are `held`, newest
/// first. The second list is the days of that kind passed on the way that are not
/// held: until they have been asked for, the first list may not be the final one.
pub fn source_days(
    kind: DayKind,
    last: Date,
    first: Date,
    held: impl Fn(Date) -> bool,
) -> (Vec<Date>, Vec<Date>) {
    let (mut found, mut missing) = (Vec::new(), Vec::new());
    let mut day = last;
    while day >= first && found.len() < DAYS_AVERAGED {
        if self::kind(day) == kind {
            if held(day) {
                found.push(day);
            } else {
                missing.push(day);
            }
        }
        let Ok(previous) = day.yesterday() else { break };
        day = previous;
    }
    (found, missing)
}

/// The local clock hour (0 to 23) an instant falls in.
fn clock_hour(at: Timestamp) -> usize {
    LocalTime(at).0.to_zoned_datetime(time::madrid()).hour() as usize
}

/// Mean price of each local clock hour, index 0 to 23, over `days` (the slots of one
/// day each). A day counts once per clock hour: where the clocks go back, the hour
/// that happens twice contributes the mean of its two prices, and where they go
/// forward, the hour that does not happen is left out of that hour's mean. `None`
/// for a clock hour that no day has.
pub fn hourly_means(days: &[&[Slot]]) -> [Option<f64>; 24] {
    let mut over_days = [(0.0, 0u32); 24];
    for slots in days {
        let mut in_day = [(0.0, 0u32); 24];
        for slot in *slots {
            let hour = &mut in_day[clock_hour(slot.start)];
            *hour = (hour.0 + slot.value, hour.1 + 1);
        }
        for (hour, (sum, count)) in in_day.into_iter().enumerate() {
            if count > 0 {
                let total = &mut over_days[hour];
                *total = (total.0 + sum / f64::from(count), total.1 + 1);
            }
        }
    }
    over_days.map(|(sum, count)| (count > 0).then(|| sum / f64::from(count)))
}

/// The slots of an estimated day: one per real hour of `day` in Madrid time (23, 24
/// or 25 of them), each priced at the estimate for its clock hour.
pub fn estimated_slots(day: Date, hourly: &[Option<f64>; 24]) -> Result<Vec<Slot>> {
    let (day_start, day_end) = time::day_bounds(day)?;
    Ok((day_start.as_second()..day_end.as_second())
        .step_by(3600)
        .filter_map(|second| {
            let start = Timestamp::from_second(second).ok()?;
            let end = Timestamp::from_second(second + 3600).ok()?;
            let value = hourly[clock_hour(start)]?;
            Some(Slot { start, end, value })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redata::Envelope;

    fn date(s: &str) -> Date {
        s.parse().unwrap()
    }

    fn dates(days: &[&str]) -> Vec<Date> {
        days.iter().map(|d| date(d)).collect()
    }

    /// Every real hour of a day, priced by `f(index of the hour within the day)`.
    fn day(day: &str, f: impl Fn(usize) -> f64) -> Vec<Slot> {
        let (start, end) = time::day_bounds(date(day)).unwrap();
        (start.as_second()..end.as_second())
            .step_by(3600)
            .enumerate()
            .map(|(i, second)| Slot {
                start: Timestamp::from_second(second).unwrap(),
                end: Timestamp::from_second(second + 3600).unwrap(),
                value: f(i),
            })
            .collect()
    }

    #[test]
    fn saturday_and_sunday_are_weekend_and_holidays_are_not_special() {
        for (day, expected) in [
            ("2026-11-27", DayKind::Weekday), // Friday
            ("2026-11-28", DayKind::Weekend),
            ("2026-11-29", DayKind::Weekend),
            ("2026-11-30", DayKind::Weekday),
            ("2026-12-25", DayKind::Weekday), // Christmas Day, a Friday
        ] {
            assert_eq!(kind(date(day)), expected, "{day}");
        }
    }

    #[test]
    fn sources_are_the_four_latest_days_of_the_kind_up_to_the_last_published_day() {
        let first = date("2026-11-10");
        let all = |_| true;
        // Thursday evening with Friday published: tomorrow and today both count.
        let last = date("2026-11-27");
        assert_eq!(
            source_days(DayKind::Weekday, last, first, all),
            (
                dates(&["2026-11-27", "2026-11-26", "2026-11-25", "2026-11-24"]),
                vec![]
            )
        );
        assert_eq!(
            source_days(DayKind::Weekend, last, first, all).0,
            dates(&["2026-11-22", "2026-11-21", "2026-11-15", "2026-11-14"])
        );
        // Before tomorrow is out, the sources end today.
        assert_eq!(
            source_days(DayKind::Weekday, date("2026-11-26"), first, all).0,
            dates(&["2026-11-26", "2026-11-25", "2026-11-24", "2026-11-23"])
        );
        // On a Saturday, today is itself a weekend source.
        assert_eq!(
            source_days(DayKind::Weekend, date("2026-11-28"), first, all).0,
            dates(&["2026-11-28", "2026-11-22", "2026-11-21", "2026-11-15"])
        );
    }

    #[test]
    fn days_not_held_are_passed_over_and_reported() {
        let first = date("2026-11-10");
        let last = date("2026-11-27");
        let gap = date("2026-11-25");
        assert_eq!(
            source_days(DayKind::Weekday, last, first, |d| d != gap),
            (
                dates(&["2026-11-27", "2026-11-26", "2026-11-24", "2026-11-23"]),
                vec![gap]
            )
        );
        // Nothing held but today and tomorrow: every earlier day of the kind back to
        // `first` is wanted, and the search goes no further back than that.
        let today = date("2026-11-26");
        let (found, missing) = source_days(DayKind::Weekday, last, first, |d| d >= today);
        assert_eq!(found, dates(&["2026-11-27", "2026-11-26"]));
        assert_eq!(missing.len(), 12);
        assert_eq!(
            (missing.first(), missing.last()),
            (Some(&gap), Some(&first))
        );
        let (found, missing) = source_days(DayKind::Weekend, last, first, |d| d >= today);
        assert_eq!(
            (found, missing),
            (
                vec![],
                dates(&["2026-11-22", "2026-11-21", "2026-11-15", "2026-11-14"])
            )
        );
    }

    #[test]
    fn a_repeated_hour_counts_once_and_a_missing_hour_not_at_all() {
        // 25 October 2026 has 02:00 twice (indices 2 and 3); the day before is normal.
        let long = day("2026-10-25", |i| 100.0 + i as f64);
        let normal = day("2026-10-24", |_| 200.0);
        assert_eq!((long.len(), normal.len()), (25, 24));
        let means = hourly_means(&[&long, &normal]);
        assert_eq!(means[1], Some((101.0 + 200.0) / 2.0));
        assert_eq!(means[2], Some((102.5 + 200.0) / 2.0));
        assert_eq!(means[3], Some((104.0 + 200.0) / 2.0));
        assert_eq!(means[23], Some((124.0 + 200.0) / 2.0));

        // 29 March 2026 has no 02:00: index 2 is 03:00.
        let short = day("2026-03-29", |i| 100.0 + i as f64);
        let normal = day("2026-03-28", |_| 200.0);
        assert_eq!(short.len(), 23);
        let means = hourly_means(&[&short, &normal]);
        assert_eq!(means[1], Some((101.0 + 200.0) / 2.0));
        assert_eq!(means[2], Some(200.0), "only the day that has a 02:00");
        assert_eq!(means[3], Some((102.0 + 200.0) / 2.0));
        assert_eq!(hourly_means(&[&short])[2], None);
        assert_eq!(hourly_means(&[]), [None; 24]);
    }

    #[test]
    fn an_estimated_day_has_one_slot_per_real_hour() {
        let hourly: [Option<f64>; 24] = std::array::from_fn(|hour| Some(hour as f64));
        let hours = |day: &str| -> Vec<f64> {
            let slots = estimated_slots(date(day), &hourly).unwrap();
            let (start, end) = time::day_bounds(date(day)).unwrap();
            assert_eq!(
                (slots[0].start, slots[slots.len() - 1].end),
                (start, end),
                "{day}"
            );
            assert!(slots.windows(2).all(|pair| pair[0].end == pair[1].start));
            slots.iter().map(|s| s.value).collect()
        };
        let normal: Vec<f64> = (0..24).map(f64::from).collect();
        assert_eq!(hours("2026-11-28"), normal);
        // The clocks go forward: 23 hours, and no 02:00.
        let mut short = normal.clone();
        short.remove(2);
        assert_eq!(hours("2026-03-29"), short);
        // The clocks go back: 25 hours, both 02:00 hours at the 02:00 estimate.
        let mut long = normal.clone();
        long.insert(2, 2.0);
        assert_eq!(hours("2026-10-25"), long);
    }

    /// Real PVPC prices for 12 to 27 November 2026, against the estimate worked out
    /// independently from the rule's definition (in Python, outside this repository).
    #[test]
    fn the_estimate_matches_an_independent_calculation_on_real_prices() {
        const WEEKDAY: [f64; 24] = [
            190.49, 176.77, 168.0125, 167.13, 165.4, 167.41, 188.395, 239.285, 270.9825, 226.6825,
            250.135, 216.2075, 210.3575, 211.6625, 142.8025, 169.6125, 213.5125, 272.46, 362.845,
            363.2825, 372.4675, 362.91, 276.2625, 247.4425,
        ];
        const WEEKEND: [f64; 24] = [
            194.4675, 179.975, 168.415, 162.7975, 157.62, 155.9825, 161.835, 162.455, 162.515,
            138.55, 113.97, 93.97, 80.5975, 76.3225, 82.165, 91.7675, 145.185, 222.125, 249.1425,
            256.53, 251.7875, 238.1175, 221.3875, 198.125,
        ];
        let path = format!(
            "{}/tests/fixtures/redata_prices_pvpc_2026-11-12_27.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let envelope: Envelope = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let observations = envelope.included[0].observations().unwrap();
        let slots_of = |day: Date| -> Vec<Slot> {
            let (start, end) = time::day_bounds(day).unwrap();
            observations
                .iter()
                .filter(|(at, _)| *at >= start && *at < end)
                .map(|&(start, value)| Slot {
                    start,
                    end: Timestamp::from_second(start.as_second() + 3600).unwrap(),
                    value,
                })
                .collect()
        };
        let held = |day: Date| slots_of(day).len() == 24;
        let (first, last) = (date("2026-11-10"), date("2026-11-27"));
        for (kind, expected) in [(DayKind::Weekday, WEEKDAY), (DayKind::Weekend, WEEKEND)] {
            let (from, _) = source_days(kind, last, first, held);
            let days: Vec<Vec<Slot>> = from.iter().map(|d| slots_of(*d)).collect();
            let days: Vec<&[Slot]> = days.iter().map(Vec::as_slice).collect();
            let means = hourly_means(&days);
            for (hour, want) in expected.into_iter().enumerate() {
                let got = means[hour].unwrap();
                assert!(
                    (got - want).abs() < 1e-6,
                    "{kind:?} {hour}: {got} != {want}"
                );
            }
        }
    }
}
