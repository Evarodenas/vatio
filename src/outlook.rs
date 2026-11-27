//! The estimate behind `vatio outlook`: prices for days that Red Eléctrica has not
//! published yet, taken from recent days that it has.
//!
//! The rule is deliberately plain, and it is the one that was backtested. A day is a
//! weekday (Monday to Friday), a Saturday or a Sunday, with no holiday calendar. Each
//! local clock hour of an unpublished day is priced at the average of two means of
//! that clock hour over the most recent published days of the same kind: a fresh mean
//! over the last four, and a steady mean over the last twelve, which include the four.

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

/// How many published days of the same kind the fresh mean is over. An estimate needs
/// that many.
pub const FRESH_DAYS: usize = 4;
/// How many published days of the same kind the steady mean is over, the fresh ones
/// included; as many as there are where fewer are found.
pub const STEADY_DAYS: usize = 12;

pub fn kind(day: Date) -> DayKind {
    match day.weekday() {
        Weekday::Saturday => DayKind::Saturday,
        Weekday::Sunday => DayKind::Sunday,
        _ => DayKind::Weekday,
    }
}

/// The days an estimate for `kind` is made from: walking back from `last` to `first`,
/// the first [`STEADY_DAYS`] days of that kind that are `held`, newest first. The
/// second list is the days of that kind passed on the way that are not held: until
/// they have been asked for, the first list may not be the final one.
pub fn source_days(
    kind: DayKind,
    last: Date,
    first: Date,
    held: impl Fn(Date) -> bool,
) -> (Vec<Date>, Vec<Date>) {
    let (mut found, mut missing) = (Vec::new(), Vec::new());
    let mut day = last;
    while day >= first && found.len() < STEADY_DAYS {
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

/// The estimate for each local clock hour, index 0 to 23, from the days
/// [`source_days`] found (the slots of one day each, newest first): the average of
/// the fresh mean, over the first [`FRESH_DAYS`] of them, and the steady mean, over
/// all of them. A clock hour that one of the two means lacks takes the other; `None`
/// for one that both lack.
pub fn estimate(days: &[&[Slot]]) -> [Option<f64>; 24] {
    let fresh = hourly_means(&days[..days.len().min(FRESH_DAYS)]);
    let steady = hourly_means(days);
    std::array::from_fn(|hour| match (fresh[hour], steady[hour]) {
        (Some(fresh), Some(steady)) => Some(0.5 * fresh + 0.5 * steady),
        (fresh, steady) => fresh.or(steady),
    })
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
    use jiff::ToSpan;

    use super::*;
    use crate::redata::Envelope;

    fn date(s: &str) -> Date {
        s.parse().unwrap()
    }

    fn dates(days: &[&str]) -> Vec<Date> {
        days.iter().map(|d| date(d)).collect()
    }

    /// `count` days a week apart, going back from `newest`.
    fn weekly(newest: &str, count: i64) -> Vec<Date> {
        (0..count)
            .map(|week| date(newest).checked_sub((7 * week).days()).unwrap())
            .collect()
    }

    /// The twelve weekdays up to Friday 27 November 2026, newest first.
    const WEEKDAYS_TO_27: [&str; 12] = [
        "2026-11-27",
        "2026-11-26",
        "2026-11-25",
        "2026-11-24",
        "2026-11-23",
        "2026-11-20",
        "2026-11-19",
        "2026-11-18",
        "2026-11-17",
        "2026-11-16",
        "2026-11-13",
        "2026-11-12",
    ];

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
    fn saturday_and_sunday_are_kinds_of_their_own_and_holidays_are_not_special() {
        for (day, expected) in [
            ("2026-11-27", DayKind::Weekday), // Friday
            ("2026-11-28", DayKind::Saturday),
            ("2026-11-29", DayKind::Sunday),
            ("2026-11-30", DayKind::Weekday),
            ("2026-12-25", DayKind::Weekday), // Christmas Day, a Friday
        ] {
            assert_eq!(kind(date(day)), expected, "{day}");
        }
    }

    #[test]
    fn sources_are_the_twelve_latest_days_of_the_kind_up_to_the_last_published_day() {
        // 90 days before Thursday 26 November 2026.
        let first = date("2026-08-28");
        let all = |_| true;
        // Thursday evening with Friday published: tomorrow and today both count.
        let last = date("2026-11-27");
        assert_eq!(
            source_days(DayKind::Weekday, last, first, all),
            (dates(&WEEKDAYS_TO_27), vec![])
        );
        // Twelve Saturdays, or Sundays, reach twelve weeks back.
        assert_eq!(
            source_days(DayKind::Saturday, last, first, all).0,
            weekly("2026-11-21", 12)
        );
        assert_eq!(
            source_days(DayKind::Sunday, last, first, all).0,
            weekly("2026-11-22", 12)
        );
        // Before tomorrow is out, the sources end today.
        let to_today = source_days(DayKind::Weekday, date("2026-11-26"), first, all).0;
        assert_eq!(to_today[..11], dates(&WEEKDAYS_TO_27[1..]));
        assert_eq!(to_today[11], date("2026-11-11"));
        // On a Saturday, today is itself a source for Saturdays.
        assert_eq!(
            source_days(DayKind::Saturday, date("2026-11-28"), first, all).0,
            weekly("2026-11-28", 12)
        );
        // A shorter look-back holds fewer than twelve, and those are what is returned.
        assert_eq!(
            source_days(DayKind::Sunday, last, date("2026-11-10"), all).0,
            weekly("2026-11-22", 2)
        );
    }

    #[test]
    fn days_not_held_are_passed_over_and_reported() {
        let first = date("2026-08-28");
        let last = date("2026-11-27");
        let gap = date("2026-11-25");
        let (found, missing) = source_days(DayKind::Weekday, last, first, |d| d != gap);
        let mut expected = dates(&WEEKDAYS_TO_27);
        expected.retain(|d| *d != gap);
        expected.push(date("2026-11-11"));
        assert_eq!((found, missing), (expected, vec![gap]));
        // Nothing held but today and tomorrow: every earlier day of the kind back to
        // `first` is wanted, and the search goes no further back than that.
        let today = date("2026-11-26");
        let (found, missing) = source_days(DayKind::Weekday, last, first, |d| d >= today);
        assert_eq!(found, dates(&["2026-11-27", "2026-11-26"]));
        assert_eq!(missing.len(), 64);
        assert_eq!(
            (missing.first(), missing.last()),
            (Some(&gap), Some(&first))
        );
        let (found, missing) = source_days(DayKind::Sunday, last, first, |d| d >= today);
        assert_eq!((found, missing), (vec![], weekly("2026-11-22", 13)));
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
    fn the_estimate_is_the_average_of_the_fresh_and_the_steady_mean() {
        // Twelve days, newest first: 130, 110, 90 and 70, then eight at 40.
        let prices = [130.0, 110.0, 90.0, 70.0];
        let days: Vec<Vec<Slot>> = (0..12)
            .map(|i| day("2026-11-24", |_| *prices.get(i).unwrap_or(&40.0)))
            .collect();
        let days: Vec<&[Slot]> = days.iter().map(Vec::as_slice).collect();
        // Fresh mean 400 / 4 = 100; steady mean (400 + 8 × 40) / 12 = 60.
        assert_eq!(estimate(&days), [Some(80.0); 24]);
        // With fewer than twelve days the steady mean is over as many as there are:
        // six give (400 + 2 × 40) / 6 = 80, and four give the fresh mean again.
        assert_eq!(estimate(&days[..6]), [Some(90.0); 24]);
        assert_eq!(estimate(&days[..4]), [Some(100.0); 24]);

        // 29 March 2026 has no 02:00. With it as all four fresh days, 02:00 takes the
        // steady mean alone, and with no other day there is no 02:00 to estimate.
        let short = day("2026-03-29", |_| 100.0);
        let normal = day("2026-03-28", |_| 40.0);
        let days = [&short[..], &short, &short, &short, &normal];
        let hourly = estimate(&days);
        assert_eq!(hourly[1], Some(0.5 * 100.0 + 0.5 * 88.0));
        assert_eq!(hourly[2], Some(40.0));
        assert_eq!(estimate(&days[..4])[2], None);
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

    /// Real PVPC prices for 1 September to 27 November 2026, against the estimate
    /// worked out independently from the rule's definition (in Python, outside this
    /// repository) and rounded to four decimals. The Sundays include 25 October, when
    /// 02:00 happened twice.
    #[test]
    fn the_estimate_matches_an_independent_calculation_on_real_prices() {
        const WEEKDAY: [f64; 24] = [
            175.1933, 162.2512, 152.0217, 149.3687, 148.0113, 151.3025, 173.6871, 224.335,
            257.6137, 211.265, 235.7896, 205.8242, 200.2375, 200.4383, 135.1654, 159.9696,
            204.8567, 264.2304, 358.6838, 358.0917, 364.5554, 352.57, 263.4921, 234.1979,
        ];
        const SATURDAY: [f64; 24] = [
            223.39, 215.9596, 212.8879, 207.3317, 204.5288, 203.5479, 210.9717, 226.8146, 223.4383,
            173.0067, 112.885, 78.7037, 60.935, 56.8779, 59.0746, 70.5767, 115.0321, 172.715,
            224.1217, 250.2458, 259.9412, 247.5083, 221.7262, 201.035,
        ];
        const SUNDAY: [f64; 24] = [
            214.3975, 202.8833, 192.6177, 186.1463, 180.1208, 178.6004, 184.1204, 194.2654,
            180.2208, 112.3092, 67.4813, 52.4175, 51.0675, 51.1737, 55.9196, 63.8225, 91.6092,
            169.6408, 231.7379, 268.9371, 275.2517, 265.0296, 252.4833, 237.9296,
        ];
        let path = format!(
            "{}/tests/fixtures/redata_prices_pvpc_2026-09-01_11-27.json",
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
        let held = |day: Date| {
            let (start, end) = time::day_bounds(day).unwrap();
            slots_of(day).len() as i64 == (end - start).as_secs() / 3600
        };
        assert_eq!(slots_of(date("2026-10-25")).len(), 25);
        let (first, last) = (date("2026-08-28"), date("2026-11-27"));
        for (kind, sources, expected) in [
            (DayKind::Weekday, dates(&WEEKDAYS_TO_27), WEEKDAY),
            (DayKind::Saturday, weekly("2026-11-21", 12), SATURDAY),
            (DayKind::Sunday, weekly("2026-11-22", 12), SUNDAY),
        ] {
            let (from, _) = source_days(kind, last, first, held);
            assert_eq!(from, sources, "{kind:?}");
            let days: Vec<Vec<Slot>> = from.iter().map(|d| slots_of(*d)).collect();
            let days: Vec<&[Slot]> = days.iter().map(Vec::as_slice).collect();
            let hourly = estimate(&days);
            for (hour, want) in expected.into_iter().enumerate() {
                let got = hourly[hour].unwrap();
                assert!(
                    (got - want).abs() < 0.000051,
                    "{kind:?} {hour}: {got} != {want}"
                );
            }
        }
    }
}
