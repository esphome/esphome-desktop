# Notes for Claude

A short orientation file for an LLM working in this repo.

**Before writing code, read [CONTRIBUTING.md → "Code structure
policies"](CONTRIBUTING.md#code-structure-policies) and ["Running the Rust
checks locally"](CONTRIBUTING.md#running-the-rust-checks-locally).** Those rules
are the authoritative coding standard, set and maintained by the human
maintainers; anything here sits on top of them. When a rule there and a rule
here disagree, CONTRIBUTING.md wins; flag the conflict in the PR so this file
can be brought back into line.

The high-leverage ones to keep in working memory while editing:

- **File size cap: 800 lines**, counting code and not a top-level
  `#[cfg(test)] mod` block, wherever in the file it sits. Code following a
  test module counts, as does a test module nested inside another `mod`.
  Split into submodules before crossing it. Do not
  add an entry to the `EXEMPT` list in `.github/scripts/check_file_size.py`;
  that list is the record of files that predate the cap and it is meant to
  shrink, not grow.
- **These three block the merge.** Run them from `src-tauri/` before pushing,
  with these exact flags; a bare `cargo clippy` lints neither the test targets
  nor the feature-gated code, so it passes on things CI then fails on. CI pins
  the toolchain, so match that version if a local result differs.

  ```bash
  cargo fmt --all --check
  cargo clippy --all-targets --all-features -- -D warnings
  cargo test --all-features
  ```
- **Don't add `Co-Authored-By: Claude` to commits**, and don't mention Claude
  in PR descriptions or commit messages.
