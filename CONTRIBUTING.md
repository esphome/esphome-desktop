# Contributing to ESPHome Device Builder [![Discord Chat](https://img.shields.io/discord/429907082951524364.svg)](https://discord.gg/KhAMKrd) [![GitHub release](https://img.shields.io/github/release/esphome/esphome-desktop.svg)](https://GitHub.com/esphome/esphome-desktop/releases/)

We welcome contributions to the ESPHome suite of code and documentation!

Please read our [contributing guide](https://developers.esphome.io/contributing/code/) if you wish to contribute to the
project and be sure to join us on [Discord](https://discord.gg/KhAMKrd).

**See also:**

[Documentation](https://esphome.io) -- [Issues](https://github.com/esphome/esphome-desktop/issues) -- [Feature requests](https://github.com/orgs/esphome/discussions)

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
by the `Scripts Test` workflow, which lints with `ruff` and runs the `pytest`
suite across macOS, Windows, and Linux on the CPython 3.13 line we bundle:

```bash
python3 -m pip install pytest ruff   # one-time
ruff check .
ruff format --check .
python3 -m pytest tests/ -v
```

---

[![ESPHome - A project from the Open Home Foundation](https://www.openhomefoundation.org/badges/esphome.png)](https://www.openhomefoundation.org/)
