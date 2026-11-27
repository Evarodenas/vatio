# vatio

Spain's electricity prices and grid status in your terminal, for people and for AI agents.
One binary, no API key.

```console
$ vatio cheapest 3h
Cheapest 3h on PVPC · searched Thu 26 Nov 21:10 → Fri 27 Nov 24:00 · €/kWh, energy term before taxes

  ★ Fri 27 Nov 00:00–03:00   0.10881 €/kWh   starts in 2h50m

  Starting now (21:10–00:10) would average 0.15123 €/kWh.
  Waiting saves 0.04242 €/kWh (28.0%).
  Source: Red Eléctrica de España (REData) · updated Wed 25 Nov 20:46
```

(The figures in this README are examples.)

## Commands

| Command | Answers |
|---|---|
| `vatio now` | What does electricity cost right now, and is that low, mid or high for today? |
| `vatio prices [today\|tomorrow\|YYYY-MM-DD]` | Every price of a day, with min, mean and max. |
| `vatio cheapest <duration>` | When should I run something that takes this long? |
| `vatio outlook <duration>` | Which day and hour is cheapest over the next several days? Days not published yet are estimated. |
| `vatio grid` | How much is Spain consuming right now, and how renewable is the mix? |
| `vatio mcp` | The same five answers as tools for an AI agent (MCP over stdio). |
| `vatio schema [name]` | JSON Schemas of everything the tool outputs. |

Add `--json` to any of them for machine-readable output, and `--market spot` for the
wholesale day-ahead price instead of the regulated household price (PVPC).

```console
$ vatio now
PVPC now · Thu 26 Nov 21:10 · €/kWh, energy term before taxes

  0.19000 €/kWh  mid for today  (12 of 24, 1 = cheapest; until 22:00)
  next 22:00  0.15352 ↓

  today: min 0.10426 at 01:00  ·  mean 0.19000  ·  max 0.27574 at 17:00
  Source: Red Eléctrica de España (REData) · updated Wed 25 Nov 20:46

$ vatio grid
Spain's grid · Thu 26 Nov 20:15

  Demand  38,261 MW at 20:05  ·  forecast 38,792  ·  scheduled 38,662
          today so far: peak 38,388 MW at 20:00  ·  low 38,261 MW at 20:05

  Generation on Tue 24 Nov (latest daily figures, provisional)  ·  808.1 GWh
  55.7% renewable  ·  111 g CO₂/kWh

    Wind                   257.7 GWh   31.9%  ██████████████████████
    Nuclear                145.8 GWh   18.0%  ████████████▌
    Combined-cycle gas     145.0 GWh   17.9%  ████████████▍
    Hydro                  122.5 GWh   15.2%  ██████████▌
    Solar PV                54.7 GWh    6.8%  ████▋
    ...
```

### The cheapest window

`vatio cheapest 3h` looks at every possible start between now and the end of published
prices, tomorrow included once it is out, and returns the run with the lowest average
price. It also tells you what starting right now would cost, so the answer is a decision
and not a table to read.

```sh
vatio cheapest 90m                    # durations: 3h, 90m, 2h30m, PT45M; a bare number is hours
vatio cheapest 2h --until 08:00       # must be finished by 08:00
vatio cheapest 4h --from 22:00 --top 3   # not before 22:00; list the three best
```

With `--from`, the comparison is with starting at that time instead of now. `--top`
lists the cheapest window, then the cheapest that does not overlap it, and so on.

The search is exact, not sampled. Prices are constant within each interval, so the best
window always starts or ends on an interval boundary, and those are the candidates
checked. That holds for hourly PVPC, for the quarter-hourly spot market, for durations
that are not a multiple of either, and across the 23- and 25-hour days when the clocks
change. The test suite checks it against trying every possible start.

### The outlook

Prices are published one day ahead, so `cheapest` cannot see past tomorrow.
`vatio outlook 3h` can: it searches the published days (today, and tomorrow once it is
out) together with an estimate for the five days after them, and says which is which.
This is its real output for the evening of 26 November 2026:

```console
$ vatio outlook 3h
Outlook for 3h on PVPC · searched Thu 26 Nov 22:30 → Wed 2 Dec 24:00 · €/kWh, energy term before taxes

    Thu 26 Nov  published  no room left for a 3h run
    Fri 27 Nov  published  03:00–06:00   0.19170 €/kWh
  ★ Sat 28 Nov  estimated  12:00–15:00   0.07969 €/kWh
    Sun 29 Nov  estimated  12:00–15:00   0.07969 €/kWh
    Mon 30 Nov  estimated  03:00–06:00   0.16665 €/kWh
    Tue 1 Dec   estimated  03:00–06:00   0.16665 €/kWh
    Wed 2 Dec   estimated  03:00–06:00   0.16665 €/kWh

  Best: Sat 28 Nov 12:00–15:00, estimated 0.07969 €/kWh, starts in 37h30m.
  Estimated: the average of the last 4 days of the same kind (weekdays or weekend days). It is vatio's own estimate, not Red Eléctrica's; published prices for a day arrive the evening before.
  Source: Red Eléctrica de España (REData) · updated Thu 26 Nov 20:46
```

Each line is the cheapest window inside that day, and the star marks the best of all;
the best window may also cross midnight, in which case only the `Best` line shows it.
Durations are written as for `cheapest`. The outlook is for PVPC only.

The estimate is plain on purpose. A day is a weekday (Monday to Friday) or a weekend
day, with no holiday calendar, and each clock hour of an estimated day is priced at the
average of that hour over the last four published days of the same kind, counting
today and tomorrow when they are published. The figures marked `estimated` are vatio's
own, not Red Eléctrica data: look again once the day is published. The past days it
averages are fetched in one request and cached like any other day.

In a backtest over 508 evenings (July 2025 to November 2026), choosing the day and hour
for a 3-hour run this way captured about 92% of the saving that perfect hindsight would
have, where running at a random time captures 0%. It is an estimate from recent days,
not a forecast of the weather or the market.

## Install

Until the first release is tagged, build from source. It needs a recent Rust toolchain;
the declared minimum is 1.88.

```sh
git clone https://github.com/Evarodenas/vatio && cd vatio
cargo install --path . --locked
```

From the first release on:

```sh
# macOS and Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/Evarodenas/vatio/releases/latest/download/vatio-installer.sh | sh
# Windows
powershell -c "irm https://github.com/Evarodenas/vatio/releases/latest/download/vatio-installer.ps1 | iex"
# with a Rust toolchain
cargo install --locked vatio
```

Shell completions: `vatio completions zsh > ~/.zfunc/_vatio` (also `bash`, `fish`,
`powershell`, `elvish`).

## For AI agents

### As an MCP server

`vatio mcp` serves five read-only tools over stdio: `get_current_price`, `get_prices`,
`find_cheapest_window`, `get_price_outlook` and `get_grid_status`. Each returns, as
structured content, the same document the matching command prints with `--json`, and
declares the same schema that `vatio schema` prints for it.

```sh
claude mcp add --transport stdio --scope user vatio -- vatio mcp
```

Claude Desktop, Cursor and other clients that read an `mcpServers` block:

```json
{
  "mcpServers": {
    "vatio": { "command": "vatio", "args": ["mcp"] }
  }
}
```

Claude Desktop needs the absolute path to the binary (`which vatio`).

### As a command

The JSON output is a contract:

- **One object per run on stdout**, each naming its own version in `schema`
  (`vatio.prices/v1`, `vatio.now/v1`, `vatio.cheapest/v1`, `vatio.outlook/v1`,
  `vatio.grid/v1`).
  The schemas are committed under [`schema/`](schema) and a test fails if code and
  schema drift apart. From the first release on, fields are only ever added within a
  version.
- **Units are in the field names**: `eur_per_kwh`, `eur_per_mwh`, `actual_mw`,
  `co2_g_per_kwh`, `duration_minutes`.
- **Times are RFC 3339 with the Madrid offset** (`2026-10-25T02:00:00+02:00`), which is
  the only form that tells the two 02:00 hours of the autumn clock change apart.
  Intervals are `[start, end)`.
- **Every document lists its `sources`**: who published the data, the request made,
  and when the publisher last updated it.
- **Errors are one JSON object on stderr**, stdout stays empty, and the exit code says
  what kind of failure it was.

```console
$ vatio prices tomorrow --json; echo "exit $?"
{"error":{"code":"not_available","message":"PVPC prices for 2026-11-27 are not published yet","hint":"Red Eléctrica publishes next-day PVPC in the evening, around 20:45 to 20:50 Madrid time; ask again after that","retryable":true}}
exit 3
```

| Exit | `code` | Meaning | Retry? |
|---|---|---|---|
| 0 | | Success | |
| 1 | `internal` | Bug in vatio | no |
| 2 | `usage` | Bad argument: date, duration, flag | after fixing the input |
| 3 | `not_available` | The data is not published (yet) | later, when `retryable` is true |
| 4 | `network`, `upstream`, `blocked` | Could not get an answer from Red Eléctrica | yes, see `retryable` |

## The data

Everything comes from [REData](https://www.ree.es/en/apidatos), the public API of Red
Eléctrica de España, the Spanish grid operator. No registration, no token.

| | What it is | Resolution | Freshness |
|---|---|---|---|
| **PVPC** | The energy term of the regulated household tariff (2.0TD): the cost of energy plus the energy share of tolls and charges. Taxes are applied on top, and the power term is billed separately. | Hourly | Next day appears in the evening, around 20:45 to 20:50 Madrid time |
| **Spot** | Day-ahead wholesale market price for Spain. | 15 minutes (hourly for days before October 2025) | Next day appears between about 14:30 and 15:40 |
| **Demand** | Measured national demand, with the operator's forecast and the market schedule. | 5 minutes | About 10 minutes behind the clock |
| **Generation mix, CO₂** | Energy by technology, renewable share, emissions per kWh. National. | **Daily** | Latest completed day, revised for a couple of days |

Things worth knowing:

- **Prices are for the mainland (peninsular) system.** The Balearics, Canaries, Ceuta and
  Melilla are not covered yet.
- **The generation mix is not live.** REData publishes it per day, so `vatio grid` shows
  the most recent completed day and names it. Intraday mix needs Red Eléctrica's other
  API (ESIOS), which requires a personal token.
- **Red Eléctrica's bot protection refuses some valid requests at random.** vatio
  retries, and if it still cannot get through says `blocked`, so that it is not taken
  for a mistake in your request. Days already cached keep working.
- **Complete price days are cached** (`~/.cache/vatio`; `%LOCALAPPDATA%\vatio\cache` on
  Windows). A day that has ended is kept as it is. Today and tomorrow are checked again
  after an hour, in case Red Eléctrica corrects them, and the cached copy is used if
  that check cannot get through. So repeated calls are instant and cached days work
  offline. `--refresh` forces a new fetch.
- **What PVPC includes** follows the ministry's
  [description of the tariff](https://www.miteco.gob.es/en/energia/energia-electrica/electricidad/contratacion-suministro/precio-voluntario.html).

### Reusing the figures

vatio is an independent project. It is not affiliated with or endorsed by Red Eléctrica
or Redeia, and uses none of their logos.

Red Eléctrica's [legal notice](https://www.ree.es/en/legal-notice) allows its data to be
reused for informational purposes provided the source is named and the date of the
latest update is given, and asks for written permission for commercial use. That is why
every output carries both: the `Source … · updated …` line, and `name` and `updated_at`
in each entry of `sources` in JSON. If you republish a figure, keep them with it.

## Not in scope

Deliberately left out, to keep the tool small: bill simulation and taxes, tariff
comparison, price forecasting from weather or market models (the outlook is a plain
average of recent days), device control, notifications, a hosted service, and
other countries. Candidates for later, in rough order: non-peninsular price zones,
allowed-hours masks for the window search (for example "only at night"), and intraday
generation through an optional ESIOS token.

## Development

```sh
cargo test                                         # unit tests + the real binary against a local stand-in for REData
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
```

Tests never touch the network. `VATIO_API_URL` points the tool at another server and
`VATIO_NOW` pins the clock (RFC 3339), which is how the end-to-end tests are made
deterministic. After changing an output type, regenerate the committed schemas:

```sh
for n in prices now cheapest outlook grid error; do cargo run -q -- schema $n > schema/$n.json; done
```

Releases are built by [dist](https://github.com/axodotdev/cargo-dist); see
[RELEASING.md](RELEASING.md).

## Licence

Code: MIT or Apache-2.0, at your option. The data belongs to Red Eléctrica de España
and is reused under the terms described above.
