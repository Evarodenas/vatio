//! Cheapest-window search over a piecewise-constant price series.
//!
//! Prices are constant inside each slot, so the mean price of a fixed-length
//! window is piecewise linear in the window's start time. Its minima can only
//! occur where the window's start or its end sits on a slot boundary, which
//! means checking those candidates is exhaustive — no sampling, and it is exact
//! for hourly slots, quarter-hour slots, and durations that are not a multiple
//! of either (for example 90 minutes over hourly prices).

use jiff::Timestamp;

/// A priced interval `[start, end)`. `value` carries whatever unit the caller uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    pub start: Timestamp,
    pub end: Timestamp,
    pub value: f64,
}

/// A candidate window `[start, end)` with the time-weighted mean of the series over it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub start: Timestamp,
    pub end: Timestamp,
    pub mean: f64,
}

/// Restrict `slots` to `[from, until)`, trimming slots that straddle either edge.
pub fn clip(slots: &[Slot], from: Timestamp, until: Timestamp) -> Vec<Slot> {
    slots
        .iter()
        .filter_map(|s| {
            let start = s.start.max(from);
            let end = s.end.min(until);
            (start < end).then_some(Slot {
                start,
                end,
                value: s.value,
            })
        })
        .collect()
}

/// Time-weighted mean of the series over `[start, start + secs)`, or `None` if the
/// series does not cover that whole span (a gap or the end of published data).
pub fn mean_over(slots: &[Slot], start: Timestamp, secs: i64) -> Option<f64> {
    if secs <= 0 {
        return None;
    }
    let (a, b) = (start.as_second(), start.as_second() + secs);
    let (mut covered, mut weighted) = (0i64, 0f64);
    for s in slots {
        let lo = s.start.as_second().max(a);
        let hi = s.end.as_second().min(b);
        if lo < hi {
            covered += hi - lo;
            weighted += s.value * (hi - lo) as f64;
        }
    }
    (covered == secs).then(|| weighted / secs as f64)
}

/// Up to `top` windows of exactly `secs` seconds: the cheapest one, then the cheapest
/// that does not overlap it, and so on.
///
/// `slots` must be sorted by start and non-overlapping. Windows never span a gap in
/// the data. Ties resolve to the earliest window, so the result is deterministic.
pub fn cheapest(slots: &[Slot], secs: i64, top: usize) -> Vec<Window> {
    if secs <= 0 {
        return Vec::new();
    }
    // Means are compared at one millionth of a unit so that floating-point noise in
    // the sums cannot decide between equally priced windows.
    let rank = |w: &Window| ((w.mean * 1e6).round() as i64, w.start);
    let window_at = |t: i64| -> Option<Window> {
        let start = Timestamp::from_second(t).ok()?;
        let end = Timestamp::from_second(t + secs).ok()?;
        let mean = mean_over(slots, start, secs)?;
        Some(Window { start, end, mean })
    };

    let mut starts: Vec<i64> = slots
        .iter()
        .flat_map(|s| [s.start.as_second(), s.end.as_second() - secs])
        .collect();
    let mut picked: Vec<Window> = Vec::new();
    while picked.len() < top {
        starts.sort_unstable();
        starts.dedup();
        let best = starts
            .iter()
            .filter_map(|&t| window_at(t))
            .filter(|c| picked.iter().all(|p| c.end <= p.start || c.start >= p.end))
            .min_by_key(rank);
        let Some(best) = best else { break };
        // Taking a window bounds the room left around it, so the windows that sit
        // flush against either side of it become candidates for the next pick.
        starts.push(best.end.as_second());
        starts.push(best.start.as_second() - secs);
        picked.push(best);
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3600;

    fn ts(s: i64) -> Timestamp {
        Timestamp::from_second(s).unwrap()
    }

    /// Consecutive slots of `len` seconds starting at t = 0.
    fn series(len: i64, values: &[f64]) -> Vec<Slot> {
        values
            .iter()
            .enumerate()
            .map(|(i, &value)| Slot {
                start: ts(i as i64 * len),
                end: ts((i as i64 + 1) * len),
                value,
            })
            .collect()
    }

    #[test]
    fn aligned_window_picks_cheapest_run() {
        let slots = series(H, &[50.0, 10.0, 20.0, 90.0, 5.0]);
        let w = cheapest(&slots, 2 * H, 1);
        assert_eq!(
            w,
            vec![Window {
                start: ts(H),
                end: ts(3 * H),
                mean: 15.0
            }]
        );
    }

    #[test]
    fn partial_slot_duration_is_time_weighted() {
        // 90 minutes over hourly prices: best is the whole cheap hour plus the
        // cheaper half-hour neighbour, i.e. end-aligned on the 10 -> 20 pair.
        let slots = series(H, &[100.0, 20.0, 10.0, 100.0]);
        let w = cheapest(&slots, 90 * 60, 1)[0];
        assert_eq!((w.start, w.end), (ts(H + 1800), ts(3 * H)));
        assert!((w.mean - (20.0 * 0.5 + 10.0) / 1.5).abs() < 1e-9);
    }

    #[test]
    fn quarter_hour_slots_work_unchanged() {
        let slots = series(900, &[8.0, 8.0, 1.0, 1.0, 1.0, 1.0, 8.0, 8.0]);
        let w = cheapest(&slots, H, 1)[0];
        assert_eq!((w.start, w.end, w.mean), (ts(1800), ts(1800 + H), 1.0));
    }

    #[test]
    fn windows_never_span_a_gap() {
        let mut slots = series(H, &[1.0, 1.0, 50.0, 60.0]);
        slots.remove(1); // hour 1 missing: the two cheap hours are no longer adjacent
        let w = cheapest(&slots, 2 * H, 1);
        assert_eq!(
            w,
            vec![Window {
                start: ts(2 * H),
                end: ts(4 * H),
                mean: 55.0
            }]
        );
    }

    #[test]
    fn top_n_are_non_overlapping_and_sorted() {
        let slots = series(H, &[5.0, 1.0, 1.0, 9.0, 2.0, 2.0, 9.0, 7.0, 8.0]);
        let w = cheapest(&slots, 2 * H, 3);
        assert_eq!(w.len(), 3);
        assert_eq!((w[0].start, w[0].mean), (ts(H), 1.0));
        assert_eq!((w[1].start, w[1].mean), (ts(4 * H), 2.0));
        assert_eq!((w[2].start, w[2].mean), (ts(7 * H), 7.5));
        // Asking for more than can fit returns only what fits.
        assert_eq!(cheapest(&slots, 2 * H, 10).len(), 3);
        assert!(w.windows(2).all(|p| p[0].mean <= p[1].mean));
        for (i, a) in w.iter().enumerate() {
            for b in &w[i + 1..] {
                assert!(a.end <= b.start || a.start >= b.end);
            }
        }
    }

    #[test]
    fn later_picks_may_sit_flush_against_earlier_ones() {
        // 100-minute runs over hourly prices. Once 02:00-03:40 is taken, the cheapest
        // room left starts exactly where it ends, which is not a slot boundary.
        let slots = series(H, &[9.0, 9.0, 1.0, 1.0, 2.0, 2.0, 9.0, 9.0]);
        let w = cheapest(&slots, 100 * 60, 2);
        assert_eq!((w[0].start, w[0].end), (ts(2 * H), ts(2 * H + 6000)));
        assert_eq!(w[1].start, ts(2 * H + 6000));
        // 20 min at 1 and 80 min at 2.
        assert!((w[1].mean - (20.0 + 160.0) / 100.0).abs() < 1e-9);
    }

    #[test]
    fn ties_resolve_to_the_earliest_window() {
        let slots = series(H, &[3.0, 3.0, 3.0, 3.0]);
        assert_eq!(cheapest(&slots, H, 1)[0].start, ts(0));
    }

    #[test]
    fn float_noise_does_not_break_ties() {
        // 0.1 + 0.2 and 0.3 differ in the last bit; the earlier window must still win.
        let slots = series(H, &[0.1, 0.2, 0.15, 0.15]);
        assert_eq!(cheapest(&slots, 2 * H, 1)[0].start, ts(0));
    }

    #[test]
    fn duration_longer_than_data_yields_nothing() {
        let slots = series(H, &[1.0, 2.0]);
        assert!(cheapest(&slots, 3 * H, 1).is_empty());
        assert!(cheapest(&slots, 0, 1).is_empty());
        assert!(cheapest(&[], H, 1).is_empty());
    }

    #[test]
    fn clip_trims_straddling_slots_so_now_is_a_candidate() {
        let slots = series(H, &[10.0, 40.0, 5.0]);
        let from = ts(1200); // 20 minutes into the first hour
        let clipped = clip(&slots, from, ts(3 * H));
        assert_eq!(clipped[0].start, from);
        // "Start right now" for one hour: 40 min at 10 + 20 min at 40.
        let now_mean = mean_over(&clipped, from, H).unwrap();
        assert!((now_mean - 20.0).abs() < 1e-9);
        assert_eq!(cheapest(&clipped, H, 1)[0].start, ts(2 * H));
    }

    /// The claim in the module docs, checked the blunt way: on random series (uneven
    /// slots, gaps, clipped ends, negative and zero prices, ties) every pick must
    /// equal what trying every whole-second start finds.
    #[test]
    fn every_pick_agrees_with_exhaustive_search() {
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut random = |lo: i64, hi: i64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            lo + (seed % (hi - lo + 1) as u64) as i64
        };
        for case in 0..4000 {
            let mut slots = Vec::new();
            let mut t = random(0, 5);
            for _ in 0..random(1, 12) {
                if random(0, 9) == 0 {
                    t += random(1, 5); // a gap
                }
                let len = random(1, 6);
                let value = match random(0, 4) {
                    0 => 0.0,
                    1 => -(random(1, 50) as f64),
                    2 => 10.0,
                    _ => random(0, 300) as f64 + 0.25 * random(0, 3) as f64,
                };
                slots.push(Slot {
                    start: ts(t),
                    end: ts(t + len),
                    value,
                });
                t += len;
            }
            if random(0, 1) == 0 {
                let lo = slots[0].start.as_second() + random(0, 3);
                slots = clip(&slots, ts(lo), ts(t - random(0, 3)));
            }
            let (secs, top) = (random(1, 14), random(1, 4) as usize);

            // Greedy over every start: cheapest, then cheapest clear of the picks so far.
            let (first, last) = (slots.first().map(|s| s.start), slots.last().map(|s| s.end));
            let mut all = Vec::new();
            if let (Some(first), Some(last)) = (first, last) {
                for start in first.as_second()..=last.as_second() - secs {
                    if let Some(mean) = mean_over(&slots, ts(start), secs) {
                        all.push(Window {
                            start: ts(start),
                            end: ts(start + secs),
                            mean,
                        });
                    }
                }
            }
            all.sort_by_key(|w| ((w.mean * 1e6).round() as i64, w.start));
            let mut want: Vec<Window> = Vec::new();
            for c in &all {
                if want.len() < top && want.iter().all(|p| c.end <= p.start || c.start >= p.end) {
                    want.push(*c);
                }
            }

            let got = cheapest(&slots, secs, top);
            let key = |w: &Window| (w.start, w.end, (w.mean * 1e6).round() as i64);
            assert_eq!(
                got.iter().map(key).collect::<Vec<_>>(),
                want.iter().map(key).collect::<Vec<_>>(),
                "case {case}: {secs}s, top {top}, {slots:?}"
            );
        }
    }

    #[test]
    fn negative_prices_are_handled() {
        let slots = series(H, &[5.0, -20.0, -1.0, 30.0]);
        let w = cheapest(&slots, 2 * H, 1)[0];
        assert_eq!((w.start, w.mean), (ts(H), -10.5));
    }
}
