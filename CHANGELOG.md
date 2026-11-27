# Changelog

All notable changes are recorded here. The JSON output, the exit codes and the MCP tool
names are part of the public interface and follow semantic versioning with the rest.

## 0.1.0 (unreleased)

First version.

- `now`, `prices`, `cheapest` and `grid` commands over Red Eléctrica's public REData API,
  with no API key.
- Exact cheapest-window search at the data's own resolution, across today and tomorrow,
  with an earliest start, a deadline, and a comparison with starting now.
- `--json` output with versioned schemas (`vatio schema`), structured errors on stderr
  and documented exit codes.
- `outlook` command: the cheapest window over the next several days. It searches the
  published prices together with vatio's own estimate for the five days after them (the
  average of the last four days of the same kind, weekdays or weekend days) and labels
  every day as published or estimated. New document `vatio.outlook/v1`.
- Built-in MCP server (`vatio mcp`) with five read-only tools that return the same
  documents as the commands.
- On-disk cache of complete price days. Ended days are kept; today and tomorrow are
  rechecked after an hour. Cached days work offline.
- Retries and a distinct `blocked` error for Red Eléctrica's bot protection.

The Rust library inside the crate is not a stable interface yet.
