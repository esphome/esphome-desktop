#!/usr/bin/env python3
"""Tests for .github/scripts/check_file_size.py.

The cap is only as good as its line count, and the two obvious ways to write
that count are both wrong on this tree. Every shape below was found in
src-tauri/src, not invented:

  * `i18n/mod.rs` gates a *function* on `#[cfg(test)]` at column 0, partway up
    the file. Keying on the attribute alone scores the file as 52 lines, and
    a 3000-line file could then pass by opening with a cfg-gated helper.
  * `platform/mod.rs` nests `#[cfg(test)] mod tests` inside its per-OS `mod
    macos` / `mod windows` blocks. Keying on any `mod tests` scores it as
    1722.

So `test_cfg_gated_function_is_not_the_test_module` and
`test_nested_test_module_is_not_the_marker` are the regression net for the
whole rule; the rest cover the exempt-list transitions that let the list
shrink but never grow back.
"""

from __future__ import annotations

from pathlib import Path

from script_loader import load_script_module

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "check_file_size.py"

check_file_size = load_script_module(SCRIPT_PATH)

CAP = check_file_size.CAP


def write(root: Path, relative: str, source: str) -> None:
    """Materialise `source` at `relative` under `root`."""
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(source, encoding="utf-8")


def rust_file(code_lines: int, test_lines: int = 0) -> str:
    """A Rust source with `code_lines` of code and a test module."""
    body = "\n".join(f"// code {n}" for n in range(code_lines))
    if not test_lines:
        return body + "\n"
    tests = "\n".join(f"    // test {n}" for n in range(test_lines))
    return f"{body}\n#[cfg(test)]\nmod tests {{\n{tests}\n}}\n"


# --- the line-count rule -------------------------------------------------


def test_counts_whole_file_when_there_is_no_test_module() -> None:
    """tray/mod.rs has no `#[cfg(test)]` at all; it counts whole."""
    assert check_file_size.code_line_count("fn a() {}\nfn b() {}\n") == 2


def test_trailing_test_module_does_not_count() -> None:
    source = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n"
    assert check_file_size.code_line_count(source) == 1


def test_cfg_gated_function_is_not_the_test_module() -> None:
    """The i18n/mod.rs and util/mod.rs shape: `#[cfg(test)] fn`, not `mod`.

    Keying on the attribute alone would return 1 here instead of 5.
    """
    source = (
        "fn a() {}\n"
        "#[cfg(test)]\n"
        "fn test_pinned() {}\n"
        "#[cfg(not(test))]\n"
        "fn pinned() {}\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    fn t() {}\n"
        "}\n"
    )
    assert check_file_size.code_line_count(source) == 5


def test_nested_test_module_is_not_the_marker() -> None:
    """The platform/mod.rs shape: an indented `#[cfg(test)] mod tests`.

    Nested test modules count as code. That overcounts, which is the safe
    direction; the point is that it does not stop at line 1.
    """
    source = (
        "mod macos {\n"
        "    fn a() {}\n"
        "    #[cfg(test)]\n"
        "    mod tests {\n"
        "        fn t() {}\n"
        "    }\n"
        "}\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    fn t() {}\n"
        "}\n"
    )
    assert check_file_size.code_line_count(source) == 7


def test_code_after_a_test_module_still_counts() -> None:
    """A mid-file test module must not exempt the rest of the file.

    Truncating at the first test module instead of skipping over it scores
    this at 1, so a 5000-line file would pass the cap. Grouping tests beside
    the code they cover rather than at the bottom is ordinary, so this is a
    correctness bug before it is ever a way to game the cap.
    """
    source = (
        "fn a() {}\n"
        "#[cfg(test)]\n"
        "mod a_tests {\n"
        "    fn t() {}\n"
        "}\n"
        "fn b() {}\n"
        "fn c() {}\n"
    )
    assert check_file_size.code_line_count(source) == 3


def test_every_test_module_is_skipped_not_just_the_first() -> None:
    source = (
        "fn a() {}\n"
        "#[cfg(test)]\n"
        "mod a_tests {\n"
        "    fn t() {}\n"
        "}\n"
        "fn b() {}\n"
        "#[cfg(test)]\n"
        "mod b_tests {\n"
        "    fn t() {}\n"
        "}\n"
    )
    assert check_file_size.code_line_count(source) == 2


def test_a_mid_file_test_module_cannot_hide_an_over_cap_file(tmp_path: Path) -> None:
    """The end-to-end form of the bug: the cap must still fire."""
    source = "#[cfg(test)]\nmod early {\n}\n" + "".join(
        f"// code {n}\n" for n in range(CAP + 1)
    )
    write(tmp_path, "a.rs", source)
    failures = check_file_size.check(tmp_path, ["a.rs"], set())
    assert len(failures) == 1
    assert str(CAP + 1) in failures[0]


def test_test_module_declared_in_another_file_is_skipped() -> None:
    """`mod tests;` has no braces to match."""
    source = "fn a() {}\n#[cfg(test)]\nmod tests;\nfn b() {}\n"
    assert check_file_size.code_line_count(source) == 2


def test_unterminated_test_module_does_not_run_away() -> None:
    source = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n"
    assert check_file_size.code_line_count(source) == 1


def test_blank_lines_between_attribute_and_mod_are_skipped() -> None:
    source = "fn a() {}\n#[cfg(test)]\n\n\nmod tests {\n}\n"
    assert check_file_size.code_line_count(source) == 1


def test_trailing_whitespace_on_the_attribute_still_matches() -> None:
    source = "fn a() {}\n#[cfg(test)]  \nmod tests {\n}\n"
    assert check_file_size.code_line_count(source) == 1


def test_rust_file_helper_produces_the_line_count_it_claims() -> None:
    """Guard the helper the cap tests below depend on."""
    assert check_file_size.code_line_count(rust_file(10)) == 10
    assert check_file_size.code_line_count(rust_file(10, test_lines=50)) == 10


# --- the cap -------------------------------------------------------------


def test_file_at_the_cap_passes(tmp_path: Path) -> None:
    write(tmp_path, "a.rs", rust_file(CAP))
    assert check_file_size.check(tmp_path, ["a.rs"], set()) == []


def test_file_over_the_cap_fails(tmp_path: Path) -> None:
    write(tmp_path, "a.rs", rust_file(CAP + 1))
    failures = check_file_size.check(tmp_path, ["a.rs"], set())
    assert len(failures) == 1
    assert "a.rs" in failures[0]
    assert str(CAP + 1) in failures[0]


def test_huge_test_module_does_not_push_a_file_over(tmp_path: Path) -> None:
    """The whole point of counting code only."""
    write(tmp_path, "a.rs", rust_file(CAP, test_lines=5000))
    assert check_file_size.check(tmp_path, ["a.rs"], set()) == []


# --- the exempt list -----------------------------------------------------


def test_exempt_file_may_grow(tmp_path: Path) -> None:
    """#331 takes platform/mod.rs to 4320 lines; that must not fail."""
    write(tmp_path, "a.rs", rust_file(CAP * 4))
    assert check_file_size.check(tmp_path, ["a.rs"], {"a.rs"}) == []


def test_exempt_file_under_the_cap_must_be_delisted(tmp_path: Path) -> None:
    write(tmp_path, "a.rs", rust_file(CAP - 1))
    failures = check_file_size.check(tmp_path, ["a.rs"], {"a.rs"})
    assert len(failures) == 1
    assert "EXEMPT" in failures[0]
    assert "a.rs" in failures[0]


def test_exempt_file_exactly_at_the_cap_must_be_delisted(tmp_path: Path) -> None:
    """At the cap is not over it, so the entry has done its job."""
    write(tmp_path, "a.rs", rust_file(CAP))
    failures = check_file_size.check(tmp_path, ["a.rs"], {"a.rs"})
    assert len(failures) == 1
    assert "EXEMPT" in failures[0]


def test_stale_exempt_entry_fails(tmp_path: Path) -> None:
    """A renamed or deleted file must not linger on the list."""
    failures = check_file_size.check(tmp_path, [], {"gone.rs"})
    assert len(failures) == 1
    assert "gone.rs" in failures[0]
    assert "stale" in failures[0]


def test_reports_every_violation_not_just_the_first(tmp_path: Path) -> None:
    write(tmp_path, "a.rs", rust_file(CAP + 1))
    write(tmp_path, "b.rs", rust_file(CAP + 1))
    assert len(check_file_size.check(tmp_path, ["a.rs", "b.rs"], set())) == 2


# --- the real tree -------------------------------------------------------


def test_repo_is_clean() -> None:
    """The committed tree passes, and EXEMPT has no stale entries.

    This is the check CI runs; having it here means a stale entry surfaces
    from `pytest` locally rather than only on a pushed branch.
    """
    files = check_file_size.tracked_rust_files(REPO_ROOT)
    assert files, "expected tracked .rs files under src-tauri/src"
    assert check_file_size.check(REPO_ROOT, files, check_file_size.EXEMPT) == []
