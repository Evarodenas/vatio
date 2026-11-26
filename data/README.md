# Data for the price-outlook backtest

Collected by `tools/fetch_outlook_data.py` (see the workflow in `.github/workflows/outlook-data.yml`).

| File | What | Source |
|---|---|---|
| `prices_hourly.csv` | PVPC and day-ahead spot price per hour, €/MWh (quarter-hours averaged) | Red Eléctrica de España, REData |
| `mix_daily.csv` | Generation per technology per day, MWh | Red Eléctrica de España, REData |
| `demand_daily.csv` | Demand per day, MWh | Red Eléctrica de España, REData |
| `weather_<model>.csv.gz` | Wind speed at 100 m (m/s), solar radiation (W/m²) and temperature (°C) at a few sites, hourly, UTC. Column `…_d0` is the run current at the time; `…_dN` is what was forecast N×24 hours earlier | Open-Meteo Previous Runs API |

Electricity data: Red Eléctrica de España (REData), reused for information with
attribution; not affiliated. Weather data: [Open-Meteo.com](https://open-meteo.com),
CC BY 4.0, free API for non-commercial use.
