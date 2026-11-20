# Contributing to ESPHome Device Builder [![Discord Chat](https://img.shields.io/discord/429907082951524364.svg)](https://discord.gg/KhAMKrd) [![GitHub release](https://img.shields.io/github/release/esphome/esphome-desktop.svg)](https://GitHub.com/esphome/esphome-desktop/releases/)

We welcome contributions to the ESPHome suite of code and documentation!

Please read our [contributing guide](https://developers.esphome.io/contributing/code/) if you wish to contribute to the
project and be sure to join us on [Discord](https://discord.gg/KhAMKrd).

**See also:**

[Documentation](https://esphome.io) -- [Issues](https://github.com/esphome/esphome-desktop/issues) -- [Feature requests](https://github.com/orgs/esphome/discussions)

## Code structure policies

**File size cap: 800 lines.** Split a file into submodules before it crosses
the cap; there are no exemptions for new files. `Lint & Test` enforces this
(`.github/scripts/check_file_size.py`), and a [prek](#git-hooks-prek) hook runs
the same check locally.

The cap counts code, not tests. A top-level `#[cfg(test)] mod` block does not
count against it, wherever in the file it sits, so a well tested file is never
pushed over the cap by its own tests. Everything else does count: code that
follows a test module, and a test module nested inside another `mod`.

The script's `EXEMPT` list is empty: the six files that were already over the
cap when it landed have all since been brought back under it. The mechanism
stays for the next rule that lands on top of in-flight work — an exempt file is
allowed to grow, so nothing is blocked by a rule it predates — but the list only
shrinks. Once an exempt file drops to the cap or below, the check fails until
its entry is removed, and the cap holds it there from then on. Don't add
entries; split the file instead.

To check before pushing:

```bash
uv run python .github/scripts/check_file_size.py
```

## Running the Rust checks locally

The `src-tauri` crate is gated in CI by a `Lint & Test` workflow. Run the same
checks before opening a PR:

```bash
cd src-tauri
cargo fmt --all --check          # formatting
cargo clippy --all-targets --all-features -- -D warnings   # lints
cargo test --all-features        # unit tests
```

All three are required gates — `fmt`, `clippy`, and `cargo test` each block the
merge on failure. CI pins Rust to a fixed version so a new toolchain release
can't break the lint gates on an unrelated PR; if a clippy/fmt result differs
locally, match that pinned version (see `toolchain:` in `lint-test.yml`).

## Running the Python script tests locally

The first-party Python (the release tooling under `.github/scripts/` and the
runtime helpers under `src-tauri/scripts/` that the Rust binary embeds) is gated
by the `Scripts Test` workflow, which runs the `pytest` suite across macOS,
Windows, and Linux on the CPython 3.14 line we bundle. Ruff lint and format run
as [prek](#git-hooks-prek) hooks.

The dev tools (`pytest`, `ruff`, `prek`) are pinned in `pyproject.toml` and
locked in `uv.lock`; Dependabot bumps them. Use [uv](https://docs.astral.sh/uv/)
so you run the same versions as CI:

```bash
uv sync                          # one-time: creates .venv with the dev tools
uv run pytest tests/ -v
```

## Git hooks (prek)

The repo's hooks are defined in `prek.toml` and run with
[prek](https://prek.j178.dev): file hygiene, `ruff` lint and format, the file
size cap, and `cargo fmt`/`cargo clippy`. The `prek` workflow runs the same
hooks on every PR, except the cargo ones, which `Lint & Test` already gates. The
ruff hooks run the locked ruff through `uv run`, so the hooks need uv on PATH.

```bash
uv run prek install              # run the hooks on every commit
uv run prek run --all-files      # run them by hand
```

---

[![ESPHome - A project from the Open Home Foundation](https://www.openhomefoundation.org/badges/esphome.png)](https://www.openhomefoundation.org/)
