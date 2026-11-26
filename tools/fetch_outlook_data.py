#!/usr/bin/env python3
"""Collect the history a price-outlook backtest needs, as compact CSV files.

  prices_hourly.csv      PVPC and day-ahead spot, hourly, from Red Eléctrica (REData)
  mix_daily.csv          generation by technology per day (REData)
  demand_daily.csv       demand per day (REData)
  weather_<model>.csv.gz weather forecasts by lead time for a few sites (Open-Meteo
                         Previous Runs API): wind at 100 m, solar radiation, temperature

Standard library only. Run from a machine that can reach both services:
  python3 tools/fetch_outlook_data.py data 2025-01-01 2026-11-26
"""
import csv, gzip, json, sys, time, urllib.error, urllib.parse, urllib.request
from collections import defaultdict
from datetime import date, datetime, timedelta

REDATA = "https://apidatos.ree.es/es/datos"
OPEN_METEO = "https://previous-runs-api.open-meteo.com/v1/forecast"
UA = "vatio-outlook-research/0.1 (+https://github.com/Evarodenas/vatio)"

# Rough centres of the regions that hold most of each resource; proxies, not plants.
SITES = {
    "wind": {"galicia": (43.3, -7.9), "burgos": (42.3, -3.9), "zaragoza": (41.6, -1.0),
             "albacete": (38.9, -1.7), "cadiz": (36.3, -5.9), "navarra": (42.6, -1.6)},
    "solar": {"badajoz": (38.9, -6.3), "ciudadreal": (39.0, -3.9), "sevilla": (37.4, -5.9),
              "murcia": (37.9, -1.3), "teruel": (41.0, -0.9)},
    "temp": {"madrid": (40.42, -3.70), "barcelona": (41.39, 2.17), "valencia": (39.47, -0.38),
             "sevilla": (37.39, -5.99), "bilbao": (43.26, -2.93)},
}
VARIABLE = {"wind": "wind_speed_100m", "solar": "shortwave_radiation", "temp": "temperature_2m"}
# 0 = the run current at the time; N = what was forecast N*24 h before the hour in question.
OFFSETS = [0, 3, 4, 5, 6, 7]
MODELS = ["ecmwf_ifs025"]     # each archive request takes about a minute; one global model is enough to start

LOG_PATH = None
def log(*parts):
    """Print and append to the log file straight away, so a run that is cut short still explains itself."""
    line = " ".join(str(p) for p in parts)
    print(line, flush=True)
    if LOG_PATH:
        with open(LOG_PATH, "a") as f:
            f.write(line + "\n")

def get_json(url, tries=3, pause=5.0):
    """GET and parse JSON. Returns (status, document or None, first bytes of the body)."""
    status, body = 0, b""
    for attempt in range(1, tries + 1):
        try:
            req = urllib.request.Request(url, headers={"Accept": "application/json", "User-Agent": UA})
            with urllib.request.urlopen(req, timeout=60) as res:
                status, body = res.status, res.read()
        except urllib.error.HTTPError as e:
            status, body = e.code, e.read()
        except Exception as e:  # timeouts, resets
            status, body = 0, repr(e).encode()
        try:
            doc = json.loads(body)
        except Exception:
            doc = None
        if doc is not None and status == 200:
            return status, doc, body[:300]
        if status in (400, 404) and doc is not None:
            return status, doc, body[:300]          # a definite answer; retrying will not change it
        if attempt < tries:
            time.sleep(61 if status == 429 else pause * attempt)
    return status, None, body[:300]

def month_ranges(start, end):
    d = start.replace(day=1)
    while d <= end:
        nxt = (d.replace(day=28) + timedelta(days=4)).replace(day=1)
        yield max(d, start), min(nxt - timedelta(days=1), end)
        d = nxt

def chunks(start, end, days):
    d = start
    while d <= end:
        yield d, min(d + timedelta(days=days - 1), end)
        d += timedelta(days=days)

# ---------------------------------------------------------------- REData

def hourly_prices(doc):
    """{(local date, hour, utc offset): {'pvpc': x, 'spot': y}} with sub-hourly values averaged."""
    acc = defaultdict(lambda: defaultdict(list))
    for s in doc.get("included", []):
        name = {"1001": "pvpc", "600": "spot"}.get(str(s.get("id")))
        if not name:
            continue
        for v in s["attributes"]["values"]:
            if v.get("value") is None:
                continue
            t = datetime.fromisoformat(v["datetime"])
            acc[(t.date().isoformat(), t.hour, t.strftime("%z"))][name].append(v["value"])
    return {k: {n: sum(x) / len(x) for n, x in m.items()} for k, m in acc.items()}

def fetch_prices(out, start, end):
    rows = {}
    for a, b in month_ranges(start, end):
        url = (f"{REDATA}/mercados/precios-mercados-tiempo-real?start_date={a}T00:00"
               f"&end_date={b}T23:59&time_trunc=hour")
        status, doc, head = get_json(url)
        got = hourly_prices(doc) if doc and status == 200 else {}
        log("prices", a, b, "http", status, "hours", len(got), "" if got else head)
        rows.update(got)
        time.sleep(1.5)
    with open(f"{out}/prices_hourly.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["date", "hour", "utc_offset", "pvpc_eur_mwh", "spot_eur_mwh"])
        def instant(key):                          # order by the real instant, so clock-change days read in sequence
            d, h, off = key
            return datetime.fromisoformat(f"{d}T{h:02d}:00:00{off[:3]}:{off[3:]}")
        for (d, h, off), m in sorted(rows.items(), key=lambda kv: instant(kv[0])):
            w.writerow([d, h, off, *(f"{m[n]:.2f}" if n in m else "" for n in ("pvpc", "spot"))])
    return len(rows)

def fetch_daily(out, start, end, widget, filename, label):
    table, titles = defaultdict(dict), {}
    for a, b in chunks(start, end, 90):
        url = f"{REDATA}/{widget}?start_date={a}T00:00&end_date={b}T23:59&time_trunc=day"
        status, doc, head = get_json(url)
        n = 0
        if doc and status == 200:
            for s in doc.get("included", []):
                sid = str(s.get("id"))
                titles[sid] = s["attributes"].get("title") or sid
                for v in s["attributes"]["values"]:
                    if v.get("value") is not None:
                        table[v["datetime"][:10]][sid] = v["value"]
                        n += 1
        log(label, a, b, "http", status, "values", n, "" if n else head)
        time.sleep(1.5)
    ids = sorted(titles)
    with open(f"{out}/{filename}", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["date", *(f"{i}:{titles[i]}" for i in ids)])
        for d in sorted(table):
            w.writerow([d, *(f"{table[d][i]:.1f}" if i in table[d] else "" for i in ids)])
    return len(table)

# ---------------------------------------------------------------- Open-Meteo

def write_weather(out, model, columns, data):
    with gzip.open(f"{out}/weather_{model}.csv.gz", "wt", newline="") as f:
        w = csv.writer(f)
        w.writerow(["time_utc", *columns])
        for t in sorted(data):
            w.writerow([t, *(f"{data[t][c]:.1f}" if c in data[t] else "" for c in columns)])

def probe_weather(model):
    """One small request, logged in full, so a refusal is understood before the long loop."""
    q = urllib.parse.urlencode({"latitude": 40.42, "longitude": -3.70, "models": model, "timezone": "GMT",
                                "hourly": "temperature_2m,temperature_2m_previous_day3,temperature_2m_previous_day7",
                                "start_date": "2025-01-01", "end_date": "2025-01-03"}, safe=",")
    status, doc, head = get_json(f"{OPEN_METEO}?{q}", tries=1)
    ok = status == 200 and isinstance(doc, dict) and "hourly" in doc
    filled = {k: sum(v is not None for v in vals) for k, vals in doc["hourly"].items() if k != "time"} if ok else {}
    log("weather probe", model, "http", status, filled if ok else head)
    return ok

def fetch_weather(out, start, end, model):
    columns, data = [], defaultdict(dict)        # data[time_utc][column] = value
    if not probe_weather(model):
        return 0
    failures = 0
    for group, sites in SITES.items():
        base = VARIABLE[group]
        names = [base if n == 0 else f"{base}_previous_day{n}" for n in OFFSETS]
        for site in sites:
            columns += [f"{group}_{site}_d{n}" for n in OFFSETS]
        for a, b in chunks(start, end, 92):
            q = urllib.parse.urlencode({
                "latitude": ",".join(str(p[0]) for p in sites.values()),
                "longitude": ",".join(str(p[1]) for p in sites.values()),
                "hourly": ",".join(names), "models": model, "timezone": "GMT",
                "wind_speed_unit": "ms", "start_date": a.isoformat(), "end_date": b.isoformat(),
            }, safe=",")
            status, doc, head = get_json(f"{OPEN_METEO}?{q}")
            results = doc if isinstance(doc, list) else [doc] if isinstance(doc, dict) else []
            filled = 0
            if status == 200 and results and "hourly" in results[0]:
                for site, res in zip(sites, results):
                    hourly = res["hourly"]
                    for n, name in zip(OFFSETS, names):
                        for t, v in zip(hourly["time"], hourly.get(name) or []):
                            if v is not None:
                                data[t][f"{group}_{site}_d{n}"] = v
                                filled += 1
            log("weather", model, group, a, b, "http", status, "values", filled, "" if filled else head)
            failures = 0 if filled else failures + 1
            if failures >= 3:
                log("weather", model, "giving up after three refusals in a row")
                break
            time.sleep(6)                         # stay well inside the free tier's per-minute allowance
        if data:
            write_weather(out, model, columns, data)   # keep what there is, group by group
        if failures >= 3:
            break
    return len(data)

def main():
    global LOG_PATH
    out, start, end = sys.argv[1], date.fromisoformat(sys.argv[2]), date.fromisoformat(sys.argv[3])
    LOG_PATH = f"{out}/fetch_log.txt"
    only = sys.argv[4].split(",") if len(sys.argv) > 4 else ["prices", "mix", "demand", "weather"]
    if "prices" in only:
        log("prices_hourly rows:", fetch_prices(out, start, end))
    if "mix" in only:
        log("mix_daily rows:", fetch_daily(out, start, end, "generacion/estructura-generacion", "mix_daily.csv", "mix"))
    if "demand" in only:
        log("demand_daily rows:", fetch_daily(out, start, end, "demanda/evolucion", "demand_daily.csv", "demand"))
    if "weather" in only:
        for model in MODELS:
            log(f"weather_{model} hours:", fetch_weather(out, start, end, model))

if __name__ == "__main__":
    main()
