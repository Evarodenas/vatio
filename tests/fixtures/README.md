# Test fixtures

Answers from Red Eléctrica's REData API (`https://apidatos.ree.es`), from the live
service on 2026-11-26.

## Raw captures

Saved byte for byte by `curl` from a GitHub runner (headers: `Accept: application/json`).

| File | Status | Request |
|---|---|---|
| `redata_generation_structure_day_2026-11-22_25.json` | 200 | `/es/datos/generacion/estructura-generacion?start_date=2026-11-22T00:00&end_date=2026-11-25T23:59&time_trunc=day` |
| `redata_co2_emissions_day_2026-11-22_25.json` | 200 | `/es/datos/generacion/no-renovables-detalle-emisiones-CO2?start_date=2026-11-22T00:00&end_date=2026-11-25T23:59&time_trunc=day` |
| `redata_error_no_data_502.json` | 502 | `/es/datos/mercados/precios-mercados-tiempo-real?start_date=2026-11-28T00:00&end_date=2026-11-28T23:59&time_trunc=hour` (a day with nothing published yet) |
| `redata_error_bad_request_400.json` | 400 | the same widget with `end_date` before `start_date` |

Things in them that the documentation does not tell you:

- **Emissions, total series (`tCO2 eq.`).** Its `last-update` is a number, not a
  timestamp, and its days are listed out of order (23, 24, 25, 22).
- **Emissions, factor series (`MWh`).** Out of step with the days it is labelled with:
  each value is another day's emissions over that day's generation (for the 25th it gives
  0.0643 t/MWh, which is the emissions of the 22nd over the generation of the 25th; the
  25th's own figures give 0.0756). A two-day answer read earlier the same day had the right
  factors. vatio does not use this series: it divides the day's emissions by the day's
  generation.
- **Errors.** "Nothing for that range" and "bad request" carry the same words and differ
  only in the HTTP status. An unknown widget gets HTTP 500 with an HTML page.

## Transcriptions

Read from the live JSON through a fetch tool rather than saved byte for byte: values,
field names and timestamp strings are the real ones, key order and whitespace are not,
and both cover only part of a day. The first two were later compared with raw captures
of the same days and match value for value.

| File | Request |
|---|---|
| `redata_prices_dst_2026-10-25T0100_0359.json` | `/es/datos/mercados/precios-mercados-tiempo-real?start_date=2026-10-25T01:00&end_date=2026-10-25T03:59&time_trunc=hour` |
| `redata_generation_structure_day_2026-11-24.json` | `/es/datos/generacion/estructura-generacion?start_date=2026-11-24T00:00&end_date=2026-11-24T23:59&time_trunc=day` |
| `redata_demand_realtime_2026-11-26T2000_2030.json` | `/es/datos/demanda/demanda-tiempo-real?start_date=2026-11-26T20:00&end_date=2026-11-26T20:30&time_trunc=hour` (as it stood at 20:12; forecast and schedule were revised later that evening) |

The first is the night the clocks went back: it shows how REData labels the repeated
hour (`02:00+02:00`, then `02:00+01:00`).

Full-day price answers in the end-to-end tests are generated in `tests/cli.rs` in the
same shape, so each test controls exactly which prices it asserts on.
