# Releasing

Releases are built by [dist](https://github.com/axodotdev/cargo-dist) from
`dist-workspace.toml`. Pushing a version tag builds binaries for macOS (Apple silicon
and Intel), Linux (x86-64 glibc and musl, arm64) and Windows, plus shell and PowerShell
installers, and attaches them to a GitHub release.

## Before the first release

Nothing has been published anywhere yet. Done so far:

- The repository is at `github.com/Evarodenas/vatio`, and CI has passed on Linux, macOS
  and Windows, on the MSRV (Rust 1.88) and on stable.
- Every command has been run against the live API (2026-11-26, from a GitHub runner).
  That run is where the error answers in `tests/fixtures/` come from, and it found two
  faults that the fixtures now cover; `tests/fixtures/README.md` has the details.

Still to do:

1. Check the name is still free: `cargo search vatio`. It was on 2026-11-26.
2. Run it once from a home connection before midday, when tomorrow's prices are not out
   for either market: `vatio prices tomorrow` should say so and exit with 3. The live run
   was in the evening, so that answer was observed for the day after tomorrow, not for
   tomorrow itself.

## Cutting a release

1. Update the version in `Cargo.toml` and the heading in `CHANGELOG.md`.
2. `cargo test && cargo clippy --all-targets --locked -- -D warnings`
3. Commit, then `git tag v0.1.0 && git push && git push --tags`. The `Release` workflow
   does the rest.
4. `cargo publish` so that `cargo install vatio` works.

## Optional

- **Homebrew.** Create a `homebrew-tap` repository, add `"homebrew"` to `installers` and
  `tap = "<owner>/homebrew-tap"` plus `publish-jobs = ["homebrew"]` in
  `dist-workspace.toml`, add a `HOMEBREW_TAP_TOKEN` secret, and run `dist generate`.
- **MCP Registry.** A `server.json` can point at the crate; the registry verifies
  ownership through a `mcp-name: io.github.<owner>/vatio` line in the README.

After changing `dist-workspace.toml`, run `dist generate` and commit the regenerated
workflow. Do not edit `.github/workflows/release.yml` by hand.
