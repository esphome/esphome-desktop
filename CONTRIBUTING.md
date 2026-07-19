# Contributing to ESPHome Device Builder [![Discord Chat](https://img.shields.io/discord/429907082951524364.svg)](https://discord.gg/KhAMKrd) [![GitHub release](https://img.shields.io/github/release/esphome/esphome-desktop.svg)](https://GitHub.com/esphome/esphome-desktop/releases/)

We welcome contributions to the ESPHome suite of code and documentation!

Please read our [contributing guide](https://developers.esphome.io/contributing/code/) if you wish to contribute to the
project and be sure to join us on [Discord](https://discord.gg/KhAMKrd).

**See also:**

[Documentation](https://esphome.io) -- [Issues](https://github.com/esphome/esphome-desktop/issues) -- [Feature requests](https://github.com/orgs/esphome/discussions)

## Code structure policies

**File size cap: 800 lines.** Split a file into submodules before it crosses
the cap; there are no exemptions for new files. `Lint & Test` enforces this
(`.github/scripts/check_file_size.py`), and a pre-commit hook runs the same
check locally.

The cap counts code, not tests. A top-level `#[cfg(test)] mod` block does not
count against it, wherever in the file it sits, so a well tested file is never
pushed over the cap by its own tests. Everything else does count: code that
follows a test module, and a test module nested inside another `mod`.

Six files were already over the cap when it landed and are grandfathered in the
script's `EXEMPT` list. They are allowed to grow, so no in-flight work is
blocked by a rule it predates. The list only shrinks: once one of them drops to
the cap or below, the check fails until its entry is removed, and the cap holds
it there from then on. `src-tauri/src/platform/mod.rs` is the worst of them at
roughly 2300 code lines; see
[#342](https://github.com/esphome/esphome-desktop/issues/342) for the split.

To check before pushing:

```bash
python3 .github/scripts/check_file_size.py   # `python` on Windows
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
by the `Scripts Test` workflow, which lints with `ruff` and runs the `pytest`
suite across macOS, Windows, and Linux on the CPython 3.14 line we bundle:

```bash
python3 -m pip install pytest ruff   # one-time
ruff check .
ruff format --check .
python3 -m pytest tests/ -v
```

On Windows, use `py -m` rather than `python3`; a python.org install never ships a
`python3.exe`, and the `py` launcher is the reliable spelling there (plain
`python` works only if you added it to PATH during install).

---

[![ESPHome - A project from the Open Home Foundation](https://www.openhomefoundation.org/badges/esphome.png)](https://www.openhomefoundation.org/)
