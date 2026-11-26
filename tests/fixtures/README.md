# Test fixtures

Answers from Red Eléctrica's REData API (`https://apidatos.ree.es`), read from the live
service on 2026-11-26.

They were transcribed from the live JSON rather than saved byte for byte: values, field
names and timestamp strings are the real ones, but key order and whitespace are not, and
the two excerpts cover only part of a day. To replace one with a raw capture:

```sh
curl -sS -o tests/fixtures/<name>.json '<url>'
```

| File | Request |
|---|---|
| `redata_prices_dst_2026-10-25T0100_0359.json` | `/es/datos/mercados/precios-mercados-tiempo-real?start_date=2026-10-25T01:00&end_date=2026-10-25T03:59&time_trunc=hour` |
| `redata_demand_realtime_2026-11-26T2000_2030.json` | `/es/datos/demanda/demanda-tiempo-real?start_date=2026-11-26T20:00&end_date=2026-11-26T20:30&time_trunc=hour` |
| `redata_generation_structure_day_2026-11-24.json` | `/es/datos/generacion/estructura-generacion?start_date=2026-11-24T00:00&end_date=2026-11-24T23:59&time_trunc=day` |
| `redata_co2_emissions_day_2026-11-23_24.json` | `/es/datos/generacion/no-renovables-detalle-emisiones-CO2?start_date=2026-11-23T00:00&end_date=2026-11-24T23:59&time_trunc=day` |

The first one is the night the clocks went back: it shows how REData labels the repeated
hour (`02:00+02:00`, then `02:00+01:00`).

Full-day price answers in the end-to-end tests are generated in `tests/cli.rs` in the
same shape, so each test controls exactly which prices it asserts on.
