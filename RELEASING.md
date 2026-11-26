# Releasing

Releases are built by [dist](https://github.com/axodotdev/cargo-dist) from
`dist-workspace.toml`. Pushing a version tag builds binaries for macOS (Apple silicon
and Intel), Linux (x86-64 glibc and musl, arm64) and Windows, plus shell and PowerShell
installers, and attaches them to a GitHub release.

## Before the first release

These have not been done yet; nothing has been published anywhere.

1. Create the GitHub repository and push. `Cargo.toml` and the README assume
   `github.com/Evarodenas/vatio`; change both if the repository lives elsewhere.
2. Check the name is still free: `cargo search vatio`. It was on 2026-11-26.
3. Run the tool against the live API (the development sandbox could not reach it):
   `vatio now && vatio prices tomorrow; vatio cheapest 3h && vatio grid`
   Then look at the cases that were built from documentation rather than observation:
   what REData answers for a day with nothing published, and for a bad request.

   ```sh
   B=https://apidatos.ree.es/es/datos
   curl -sS -i "$B/mercados/precios-mercados-tiempo-real?start_date=2030-01-01T00:00&end_date=2030-01-01T23:59&time_trunc=hour"
   curl -sS -i "$B/generacion/estructura-generacion?start_date=2026-11-24T00:00&end_date=2026-11-24T23:59&time_trunc=hour"
   curl -sS -i "$B/mercados/no-existe?start_date=2026-11-24T00:00&end_date=2026-11-24T23:59&time_trunc=hour"
   ```

   If the "no data" wording differs from the phrases in `Failure::is_no_data`
   (`src/redata.rs`), add it there.
4. Let CI run on the first push. The Windows and macOS jobs and the MSRV job (Rust 1.88)
   have never run; only Linux with Rust 1.97 has. The clippy job uses the latest stable
   toolchain with warnings as errors, so it may flag lints that 1.97 does not have.
5. Replace the transcribed fixtures in `tests/fixtures/` with raw captures
   (`tests/fixtures/README.md` lists the requests).

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
