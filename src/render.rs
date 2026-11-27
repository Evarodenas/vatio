//! Terminal output for people. Styles are emitted unconditionally; the writer in
//! `main` strips them when stdout is not a terminal or `NO_COLOR` is set.

use std::fmt::Write as _;

use anstyle::{AnsiColor, Style};
use jiff::Timestamp;

use crate::{
    model::{
        Basis, Cheapest, Dataset, Grid, Level, Market, Now, Outlook, PricePoint, PriceStats,
        PriceWindow, Prices, Source, Tomorrow,
    },
    time::{self, LocalTime},
};

const BAR_WIDTH: usize = 22;
const GREEN: Style = AnsiColor::Green.on_default();
const RED: Style = AnsiColor::Red.on_default();
const DIM: Style = Style::new().dimmed();
const BOLD: Style = Style::new().bold();

/// A value and its unit as people quote that market: household PVPC per kWh, the
/// wholesale market per MWh.
fn price(market: Market, eur_per_mwh: f64) -> String {
    match market {
        Market::Pvpc => format!("{:.5}", eur_per_mwh / 1000.0),
        Market::Spot => format!("{eur_per_mwh:.2}"),
    }
}

fn unit(market: Market) -> &'static str {
    match market {
        Market::Pvpc => "€/kWh",
        Market::Spot => "€/MWh",
    }
}

fn basis(market: Market) -> &'static str {
    match market {
        Market::Pvpc => "€/kWh, energy term before taxes",
        Market::Spot => "€/MWh, wholesale",
    }
}

fn fmt(at: LocalTime, pattern: &str) -> String {
    at.0.to_zoned_datetime(time::madrid())
        .strftime(pattern)
        .to_string()
}

fn day_label(at: LocalTime) -> String {
    fmt(at, "%a %-d %b")
}

/// `HH:MM`, written as `24:00` when the instant is midnight closing day `of`.
fn clock_end(at: LocalTime, of: LocalTime) -> String {
    if at.hhmm() == "00:00" && at.date() != of.date() {
        "24:00".to_owned()
    } else {
        at.hhmm()
    }
}

fn span(start: LocalTime, end: LocalTime) -> String {
    format!("{}–{}", start.hhmm(), clock_end(end, start))
}

fn bar(value: f64, scale: f64) -> String {
    const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    if scale <= 0.0 || value <= 0.0 {
        return String::new();
    }
    let eighths = ((value / scale).min(1.0) * (BAR_WIDTH * 8) as f64).round() as usize;
    format!("{}{}", "█".repeat(eighths / 8), PARTIAL[eighths % 8])
}

/// The attribution line: who published the data and when they last updated it.
/// Red Eléctrica asks for both whenever its figures are reused.
fn credit(sources: &[Source]) -> String {
    let Some(first) = sources.first() else {
        return String::new();
    };
    // Latest update per dataset, in the order the datasets appear.
    let mut updates: Vec<(Dataset, LocalTime)> = Vec::new();
    for source in sources {
        let Some(at) = source.updated_at else {
            continue;
        };
        match updates
            .iter_mut()
            .find(|(dataset, _)| *dataset == source.dataset)
        {
            Some((_, latest)) => *latest = (*latest).max(at),
            None => updates.push((source.dataset, at)),
        }
    }
    let stamp = |at: LocalTime| format!("{} {}", day_label(at), at.hhmm());
    match updates.as_slice() {
        [] => format!("  {DIM}Source: {}{DIM:#}\n", first.name),
        [(_, at)] => format!(
            "  {DIM}Source: {} · updated {}{DIM:#}\n",
            first.name,
            stamp(*at)
        ),
        many => {
            let parts: Vec<String> = many
                .iter()
                .map(|(dataset, at)| format!("{} {}", dataset.label(), stamp(*at)))
                .collect();
            format!(
                "  {DIM}Source: {}\n  Updated: {}{DIM:#}\n",
                first.name,
                parts.join(" · ")
            )
        }
    }
}

fn stats_line(market: Market, stats: &PriceStats) -> String {
    format!(
        "min {} at {}  ·  mean {}  ·  max {} at {}",
        price(market, stats.min.eur_per_mwh),
        stats.min.start.hhmm(),
        price(market, stats.mean_eur_per_mwh),
        price(market, stats.max.eur_per_mwh),
        stats.max.start.hhmm(),
    )
}

/// Rows to print: the points themselves, or hourly means when the series is finer
/// than an hour (96 quarter-hours do not fit a screen).
fn hourly_rows(points: &[PricePoint]) -> Vec<PricePoint> {
    let mut rows: Vec<(PricePoint, f64, f64)> = Vec::new(); // (row, weighted sum, seconds)
    for p in points {
        let secs = (p.end.0 - p.start.0).as_secs() as f64;
        let hour = p.start.0.as_second().div_euclid(3600);
        match rows.last_mut() {
            Some((row, sum, total)) if row.start.0.as_second().div_euclid(3600) == hour => {
                *sum += p.eur_per_mwh * secs;
                *total += secs;
                row.end = p.end;
            }
            _ => rows.push((*p, p.eur_per_mwh * secs, secs)),
        }
    }
    rows.into_iter()
        .map(|(row, sum, total)| PricePoint::new(row.start.0, row.end.0, sum / total))
        .collect()
}

pub fn prices(doc: &Prices, now: Timestamp) -> String {
    let mut out = String::new();
    let Some(first) = doc.points.first() else {
        return out;
    };
    let _ = writeln!(
        out,
        "{BOLD}{} · {}{BOLD:#} {DIM}· {}{DIM:#}\n",
        doc.market.label(),
        fmt(first.start, "%a %-d %b %Y"),
        basis(doc.market)
    );

    let rows = hourly_rows(&doc.points);
    let scale = rows.iter().map(|r| r.eur_per_mwh).fold(0.0, f64::max);
    let mut sorted: Vec<f64> = rows.iter().map(|r| r.eur_per_mwh).collect();
    sorted.sort_by(f64::total_cmp);
    // Cheapest third green, dearest third red; nothing on a flat or very short series.
    let third = sorted.len() / 3;
    let (low, high) = match third {
        0 => (f64::NEG_INFINITY, f64::INFINITY),
        n => (sorted[n - 1], sorted[sorted.len() - n]),
    };

    // On the autumn clock-change day 02:00 happens twice; show offsets so rows differ.
    let mut labels: Vec<String> = rows.iter().map(|r| r.start.hhmm()).collect();
    let mut seen = labels.clone();
    seen.sort();
    if seen.windows(2).any(|w| w[0] == w[1]) {
        labels = rows.iter().map(|r| fmt(r.start, "%H:%M %:z")).collect();
    }

    for (row, label) in rows.iter().zip(&labels) {
        let current = row.start.0 <= now && now < row.end.0;
        let style = if low < high && row.eur_per_mwh <= low {
            GREEN
        } else if low < high && row.eur_per_mwh >= high {
            RED
        } else {
            Style::new()
        };
        let _ = writeln!(
            out,
            "{} {label}  {style}{:>9}  {}{style:#}{}",
            if current { "▸" } else { " " },
            price(doc.market, row.eur_per_mwh),
            bar(row.eur_per_mwh, scale),
            if current { "  ← now" } else { "" },
        );
    }

    let _ = writeln!(out, "\n  {}", stats_line(doc.market, &doc.stats));
    if rows.len() != doc.points.len() {
        let _ = writeln!(
            out,
            "  {DIM}Hourly means of {} intervals of {} minutes; --json has every interval.{DIM:#}",
            doc.points.len(),
            doc.resolution_minutes
        );
    }
    if !doc.complete {
        let _ = writeln!(
            out,
            "  {DIM}Partial day: Red Eléctrica has not published every interval.{DIM:#}"
        );
    }
    out.push_str(&credit(&doc.sources));
    out
}

pub fn now(doc: &Now) -> String {
    let mut out = String::new();
    let m = doc.market;
    let (word, style) = match doc.level {
        Level::Low => ("low", GREEN),
        Level::Mid => ("mid", Style::new()),
        Level::High => ("high", RED),
    };
    let _ = writeln!(
        out,
        "{BOLD}{} now{BOLD:#} {DIM}· {} {} · {}{DIM:#}\n",
        m.label(),
        day_label(doc.at),
        doc.at.hhmm(),
        basis(m)
    );
    let _ = writeln!(
        out,
        "  {style}{BOLD}{}{BOLD:#}{style:#} {}  {style}{word}{style:#} for today  {DIM}({} of {}, 1 = cheapest; until {}){DIM:#}",
        price(m, doc.current.eur_per_mwh),
        unit(m),
        doc.rank,
        doc.rank_of,
        clock_end(doc.current.end, doc.current.start),
    );
    if let Some(next) = &doc.next {
        let arrow = match next.eur_per_mwh.total_cmp(&doc.current.eur_per_mwh) {
            std::cmp::Ordering::Less => "↓",
            std::cmp::Ordering::Greater => "↑",
            std::cmp::Ordering::Equal => "=",
        };
        let _ = writeln!(
            out,
            "  next {}  {} {arrow}",
            next.start.hhmm(),
            price(m, next.eur_per_mwh)
        );
    }
    let _ = writeln!(out, "\n  today: {}", stats_line(m, &doc.today));
    out.push_str(&credit(&doc.sources));
    out
}

fn wait(minutes: i64) -> String {
    match minutes {
        0 => "start now".to_owned(),
        m => format!("starts in {}", time::fmt_duration(m * 60)),
    }
}

fn window_line(market: Market, w: &PriceWindow) -> String {
    format!(
        "{} {}   {} {}   {}",
        day_label(w.start),
        span(w.start, w.end),
        price(market, w.mean_eur_per_mwh),
        unit(market),
        wait(w.starts_in_minutes)
    )
}

pub fn cheapest(doc: &Cheapest) -> String {
    let mut out = String::new();
    let m = doc.market;
    let _ = writeln!(
        out,
        "{BOLD}Cheapest {} on {}{BOLD:#} {DIM}· searched {} {} → {} {} · {}{DIM:#}\n",
        time::fmt_duration(doc.duration_minutes * 60),
        m.label(),
        day_label(doc.searched.from),
        doc.searched.from.hhmm(),
        day_label(LocalTime(
            Timestamp::from_second(doc.searched.until.0.as_second() - 1)
                .unwrap_or(doc.searched.until.0)
        )),
        clock_end(doc.searched.until, doc.searched.from),
        basis(m),
    );
    let _ = writeln!(out, "  {GREEN}★ {}{GREEN:#}", window_line(m, &doc.best));
    for alt in &doc.alternatives {
        let _ = writeln!(out, "    {}", window_line(m, alt));
    }
    out.push('\n');

    // Compare with the earliest allowed start: now, unless a later one was asked for.
    match &doc.earliest {
        Some(e) if e.start == doc.best.start && e.starts_in_minutes == 0 => {
            let _ = writeln!(out, "  Starting right away is the cheapest option.");
        }
        Some(e) if e.start == doc.best.start => {
            let _ = writeln!(
                out,
                "  The earliest allowed start, {}, is also the cheapest.",
                e.start.hhmm()
            );
        }
        Some(e) => {
            let (when, verdict) = match e.starts_in_minutes {
                0 => ("now".to_owned(), "Waiting saves"),
                _ => (format!("at {}", e.start.hhmm()), "The best window saves"),
            };
            let _ = writeln!(
                out,
                "  Starting {when} ({}) would average {} {}.",
                span(e.start, e.end),
                price(m, e.mean_eur_per_mwh),
                unit(m)
            );
            let percent = doc
                .saving_percent
                .map(|p| format!(" ({p:.1}%)"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "  {verdict} {} {}{percent}.",
                price(m, e.mean_eur_per_mwh - doc.best.mean_eur_per_mwh),
                unit(m)
            );
        }
        None => {
            let _ = writeln!(
                out,
                "  {DIM}Published prices do not cover a full run from the earliest start.{DIM:#}"
            );
        }
    }
    if doc.searched.tomorrow == Tomorrow::NotPublished {
        let _ = writeln!(
            out,
            "  {DIM}Tomorrow's prices are not published yet; a cheaper slot may appear once they are.{DIM:#}"
        );
    }
    for warning in &doc.warnings {
        let _ = writeln!(out, "  {RED}!{RED:#} {warning}");
    }
    out.push_str(&credit(&doc.sources));
    out
}

pub fn outlook(doc: &Outlook) -> String {
    let mut out = String::new();
    let m = doc.market;
    let length = time::fmt_duration(doc.duration_minutes * 60);
    let _ = writeln!(
        out,
        "{BOLD}Outlook for {length} on {}{BOLD:#} {DIM}· searched {} {} → {} {} · {}{DIM:#}\n",
        m.label(),
        day_label(doc.searched.from),
        doc.searched.from.hhmm(),
        day_label(LocalTime(
            Timestamp::from_second(doc.searched.until.0.as_second() - 1)
                .unwrap_or(doc.searched.until.0)
        )),
        clock_end(doc.searched.until, doc.searched.from),
        basis(m),
    );
    let word = |basis: Basis| match basis {
        Basis::Published => "published",
        Basis::Estimated => "estimated",
    };
    for day in &doc.days {
        let label = day
            .day
            .parse::<jiff::civil::Date>()
            .map(|d| d.strftime("%a %-d %b").to_string())
            .unwrap_or_else(|_| day.day.clone());
        let found = match &day.cheapest {
            Some(w) => format!(
                "{}   {} {}",
                span(w.start, w.end),
                price(m, w.mean_eur_per_mwh),
                unit(m)
            ),
            None => format!("no room left for a {length} run"),
        };
        let row = format!("{label:<10}  {}  {found}", word(day.basis));
        // The best window may cross midnight, and then it is no single day's own.
        if day.cheapest == Some(doc.best.window) {
            let _ = writeln!(out, "  {GREEN}★ {row}{GREEN:#}");
        } else {
            let _ = writeln!(out, "    {row}");
        }
    }

    let best = &doc.best.window;
    let _ = writeln!(
        out,
        "\n  Best: {} {}, {} {} {}, {}.",
        day_label(best.start),
        span(best.start, best.end),
        word(doc.best.basis),
        price(m, best.mean_eur_per_mwh),
        unit(m),
        wait(best.starts_in_minutes)
    );
    let _ = writeln!(
        out,
        "  {DIM}Estimated: the average of the last {} days of the same kind (weekdays or weekend \
         days). It is vatio's own estimate, not Red Eléctrica's; published prices for a day \
         arrive the evening before.{DIM:#}",
        doc.method.days_averaged
    );
    for warning in &doc.warnings {
        let _ = writeln!(out, "  {RED}!{RED:#} {warning}");
    }
    out.push_str(&credit(&doc.sources));
    out
}

/// `38261.4` as `38,261`.
fn thousands(value: f64) -> String {
    let digits = format!("{:.0}", value.abs());
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if value < 0.0 { format!("-{out}") } else { out }
}

pub fn grid(doc: &Grid) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{BOLD}Spain's grid{BOLD:#} {DIM}· {} {}{DIM:#}",
        day_label(doc.at),
        doc.at.hhmm()
    );

    if let Some(d) = &doc.demand {
        let extra = |label: &str, mw: Option<f64>| {
            mw.map(|v| format!("  ·  {label} {}", thousands(v)))
                .unwrap_or_default()
        };
        let _ = writeln!(
            out,
            "\n  {BOLD}Demand{BOLD:#}  {BOLD}{} MW{BOLD:#} at {}{}{}",
            thousands(d.actual_mw),
            d.at.hhmm(),
            extra("forecast", d.forecast_mw),
            extra("scheduled", d.scheduled_mw),
        );
        let _ = writeln!(
            out,
            "  {DIM}        today so far: peak {} MW at {}  ·  low {} MW at {}{DIM:#}",
            thousands(d.peak_today.mw),
            d.peak_today.at.hhmm(),
            thousands(d.low_today.mw),
            d.low_today.at.hhmm(),
        );
    }

    if let Some(g) = &doc.generation {
        let day = g
            .day
            .parse::<jiff::civil::Date>()
            .ok()
            .and_then(|d| time::day_bounds(d).ok())
            .map(|(start, _)| day_label(LocalTime(start)))
            .unwrap_or_else(|| g.day.clone());
        let _ = writeln!(
            out,
            "\n  {BOLD}Generation{BOLD:#} on {day} {DIM}(latest daily figures, provisional){DIM:#}  ·  {:.1} GWh",
            g.total_mwh / 1000.0
        );
        let headline: Vec<String> = [
            g.renewable_percent
                .map(|p| format!("{GREEN}{p:.1}% renewable{GREEN:#}")),
            g.co2_g_per_kwh.map(|v| format!("{v:.0} g CO₂/kWh")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !headline.is_empty() {
            let _ = writeln!(out, "  {}", headline.join("  ·  "));
        }
        out.push('\n');
        let scale = g.technologies.iter().map(|t| t.mwh).fold(0.0, f64::max);
        let width = g
            .technologies
            .iter()
            .map(|t| t.name.chars().count())
            .max()
            .unwrap_or(0);
        for t in &g.technologies {
            let style = if t.renewable == Some(true) {
                GREEN
            } else {
                Style::new()
            };
            let _ = writeln!(
                out,
                "    {:<width$}  {:>7.1} GWh  {:>5.1}%  {style}{}{style:#}",
                t.name,
                t.mwh / 1000.0,
                t.percent,
                bar(t.mwh, scale),
            );
        }
    }

    out.push('\n');
    for warning in &doc.warnings {
        let _ = writeln!(out, "  {RED}!{RED:#} {warning}");
    }
    out.push_str(&credit(&doc.sources));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_scale_with_eighth_block_precision() {
        assert_eq!(bar(10.0, 10.0).chars().count(), BAR_WIDTH);
        assert_eq!(bar(5.0, 10.0), "█".repeat(BAR_WIDTH / 2));
        assert_eq!(bar(0.0, 10.0), "");
        assert_eq!(bar(-3.0, 10.0), "");
        assert_eq!(bar(1.0, 0.0), "");
        assert!(bar(10.0 / (BAR_WIDTH * 8) as f64, 10.0) == "▏");
    }

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(thousands(38261.4), "38,261");
        assert_eq!(thousands(999.0), "999");
        assert_eq!(thousands(1_234_567.0), "1,234,567");
        assert_eq!(thousands(-1500.0), "-1,500");
    }

    #[test]
    fn quarter_hours_collapse_to_hourly_means() {
        let p = |q: i64, v: f64| {
            PricePoint::new(
                Timestamp::from_second(q * 900).unwrap(),
                Timestamp::from_second((q + 1) * 900).unwrap(),
                v,
            )
        };
        let rows = hourly_rows(&[
            p(0, 10.0),
            p(1, 20.0),
            p(2, 30.0),
            p(3, 40.0),
            p(4, 8.0),
            p(5, 8.0),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].eur_per_mwh, rows[0].end.0.as_second()),
            (25.0, 3600)
        );
        assert_eq!(
            (rows[1].eur_per_mwh, rows[1].end.0.as_second()),
            (8.0, 5400)
        );
    }

    #[test]
    fn midnight_closing_a_day_reads_as_24() {
        let start = LocalTime("2026-11-26T21:00:00Z".parse().unwrap()); // 22:00 Madrid
        let end = LocalTime("2026-11-26T23:00:00Z".parse().unwrap()); // 00:00 next day
        assert_eq!(span(start, end), "22:00–24:00");
        let early = LocalTime("2026-11-26T23:00:00Z".parse().unwrap());
        let later = LocalTime("2026-11-27T02:00:00Z".parse().unwrap());
        assert_eq!(span(early, later), "00:00–03:00");
    }
}
