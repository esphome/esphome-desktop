#!/usr/bin/env python3
"""Tests for .github/scripts/check_file_size.py.

The cap is only as good as its line count, and every obvious way to write
that count has been wrong. Two of these shapes were found in src-tauri/src
rather than invented, and the other two shipped as bugs in review:

  * `i18n/mod.rs` gates a *function* on `#[cfg(test)]` at column 0, partway up
    the file. Keying on the attribute alone scores the file as 52 lines, and
    a 3000-line file could then pass by opening with a cfg-gated helper.
  * `platform/mod.rs` nests `#[cfg(test)] mod tests` inside its per-OS `mod
    macos` / `mod windows` blocks. Keying on any `mod tests` scores it as
    1722.
  * A test module *mid-file*, with code after it. Truncating at the first one
    scored a 5005-line file as 1.
  * `mod tests {}`, the empty body rustfmt produces. Scanning below the
    declaration for a closing brace finds the next item's, or none, and
    scored a 2004-line file as 0.

Fixtures here use the spelling `cargo fmt` would actually emit. The `{}` bug
survived a first fix precisely because the regression test for it pinned
`{\n}`, which rustfmt rewrites, so no realistic file ever took that path.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest
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


@pytest.mark.parametrize("empty_body", ["mod early {\n}", "mod early {}"])
def test_a_mid_file_test_module_cannot_hide_an_over_cap_file(
    tmp_path: Path, empty_body: str
) -> None:
    """The end-to-end form of the bug: the cap must still fire.

    Both spellings, because rustfmt rewrites the first into the second: an
    empty body is collapsed onto the declaration line, so `mod early {}` is
    the only form that can exist in this tree (`cargo fmt --check` blocks).
    Testing only the `{\\n}` form pins the one spelling rustfmt will not
    produce, which is how the `{}` shape survived the first fix.
    """
    source = f"#[cfg(test)]\n{empty_body}\n" + "".join(
        f"// code {n}\n" for n in range(CAP + 1)
    )
    write(tmp_path, "a.rs", source)
    failures = check_file_size.check(tmp_path, ["a.rs"], set())
    assert len(failures) == 1
    assert str(CAP + 1) in failures[0]


def test_test_module_with_an_empty_body_is_self_contained() -> None:
    """`mod tests {}` closes itself; there is no brace below to scan for.

    Scanning below it swallows everything to the next column-0 `}` or EOF.
    """
    source = "fn a() {}\n#[cfg(test)]\nmod tests {}\nfn b() {}\nfn c() {\n}\n"
    assert check_file_size.code_line_count(source) == 4


@pytest.mark.parametrize(
    "closing",
    ["}", "} // end tests", "} // end tests ", "} /* end tests */"],
)
def test_a_comment_on_the_closing_brace_still_ends_the_block(closing: str) -> None:
    """`} // end tests` is valid Rust and rustfmt keeps it.

    Matching a bare `}` walks past the real terminator to the next one and
    swallows everything between, scoring a 2006-line file as 0.
    """
    source = (
        "#[cfg(test)]\nmod tests {\n    fn t() {}\n"
        f"{closing}\n"
        "fn a() {}\nfn b() {\n}\n"
    )
    assert check_file_size.code_line_count(source) == 3


def test_a_commented_closing_brace_cannot_hide_an_over_cap_file(
    tmp_path: Path,
) -> None:
    source = (
        "#[cfg(test)]\nmod tests {\n    fn t() {}\n} // end tests\n"
        + "".join(f"// code {n}\n" for n in range(CAP + 1))
        + "fn tail() {\n}\n"
    )
    write(tmp_path, "a.rs", source)
    assert len(check_file_size.check(tmp_path, ["a.rs"], set())) == 1


@pytest.mark.parametrize(
    "declaration",
    [
        "mod tests { /* todo */ }",  # truncating at `/*` loses the `}`
        "mod tests { /* todo */\n}",  # what rustfmt rewrites the above into
        "mod tests {} /* todo */",
    ],
)
def test_a_block_comment_does_not_lose_the_closing_brace(declaration: str) -> None:
    """`/* ... */` closes and can be followed by code; it is not to end of line.

    Truncating at the opener leaves `mod tests {`, which fails the
    self-contained check, so the scanner hunts below for a brace, finds the
    next item's, and swallows everything between: a 9-line file scores 0.

    rustfmt splits the first spelling across two lines, so this tree cannot
    hold it, but the helper should not be right only by luck.
    """
    source = f"#[cfg(test)]\n{declaration}\n" + "fn a() {}\nfn b() {\n}\n"
    assert check_file_size.code_line_count(source) == 3


def test_an_unclosed_block_comment_truncates() -> None:
    """No `*/` on the line means the comment really does run on."""
    assert check_file_size._code_before_comment("} /* unfinished") == "}"


@pytest.mark.parametrize(
    ("line", "expected"),
    [
        ("} /* a /* b */ */", "}"),  # nested: one comment, then the brace
        ("} /* a */", "}"),
        ("} // end", "}"),
        ("} /* unfinished", "}"),
        ("mod tests { /* todo */ }", "mod tests {  }"),
        ("mod tests {} /* note */", "mod tests {}"),
        ("#[cfg(test)] /* x */", "#[cfg(test)]"),
        ("#[cfg(test)]", "#[cfg(test)]"),
        ("mod tests;", "mod tests;"),
        ("mod tests {", "mod tests {"),
    ],
)
def test_code_before_comment(line: str, expected: str) -> None:
    """Rust block comments nest, so `/* a /* b */ */` is one comment.

    Closing at the first `*/` leaves `}   */`, which is not a terminator, so
    the block runs on and the file undercounts. rustfmt keeps that line as
    written, so it is a shape this tree can hold, unlike `mod tests { /* todo
    */ }` which it breaks in two.
    """
    assert check_file_size._code_before_comment(line) == expected


def test_a_nested_block_comment_cannot_hide_an_over_cap_file(tmp_path: Path) -> None:
    source = (
        "#[cfg(test)]\nmod tests {\n    fn t() {}\n} /* a /* b */ */\n"
        + "".join(f"// code {n}\n" for n in range(CAP + 1))
        + "fn tail() {\n}\n"
    )
    write(tmp_path, "a.rs", source)
    assert len(check_file_size.check(tmp_path, ["a.rs"], set())) == 1


def test_a_comment_on_the_declaration_still_reads_as_self_contained() -> None:
    """Only `fn a` and `fn b`; the attribute and declaration are both skipped."""
    source = "fn a() {}\n#[cfg(test)]\nmod tests {} // nothing yet\nfn b() {}\n"
    assert check_file_size.code_line_count(source) == 2


def test_a_comment_on_the_attribute_still_matches() -> None:
    source = "fn a() {}\n#[cfg(test)] // unit tests\nmod tests {\n    fn t() {}\n}\n"
    assert check_file_size.code_line_count(source) == 1


def test_test_module_declared_in_another_file_is_skipped() -> None:
    """`mod tests;` has no braces to match."""
    source = "fn a() {}\n#[cfg(test)]\nmod tests;\nfn b() {}\n"
    assert check_file_size.code_line_count(source) == 2


def test_unterminated_test_module_counts_as_code() -> None:
    """No closing brace means the scanner is lost; fail loud, not silent.

    Swallowing to EOF would collapse the count and wave an over-cap file
    through on nothing but odd formatting.
    """
    source = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n"
    assert check_file_size.code_line_count(source) == 4


def test_unterminated_test_module_cannot_hide_an_over_cap_file(
    tmp_path: Path,
) -> None:
    source = "#[cfg(test)]\nmod tests {\n" + "".join(
        f"    // code {n}\n" for n in range(CAP + 1)
    )
    write(tmp_path, "a.rs", source)
    assert len(check_file_size.check(tmp_path, ["a.rs"], set())) == 1


def test_blank_lines_between_attribute_and_mod_are_skipped() -> None:
    source = "fn a() {}\n#[cfg(test)]\n\n\nmod tests {\n    fn t() {}\n}\n"
    assert check_file_size.code_line_count(source) == 1


def test_trailing_whitespace_on_the_attribute_still_matches() -> None:
    source = "fn a() {}\n#[cfg(test)]  \nmod tests {\n    fn t() {}\n}\n"
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


def test_a_git_failure_says_why(tmp_path: Path) -> None:
    """`check=True` + `capture_output` hides git's stderr from the message.

    Without re-raising, a failure here (not a repo, a bad pathspec) reaches
    the CI log as a bare traceback saying only that the exit status was
    non-zero.

    Asserts the prefix and exit code this script controls, plus that *some*
    stderr came through — not git's own wording, which gettext localizes, so
    pinning the English would fail this suite in another locale while the
    behaviour was perfectly correct.
    """
    with pytest.raises(RuntimeError, match=r"git ls-files failed \(128\)") as caught:
        check_file_size.tracked_rust_files(tmp_path)
    _, _, reason = str(caught.value).partition("): ")
    assert reason.strip(), "expected git's stderr to be carried into the message"


def test_a_missing_git_says_why(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A missing git raises FileNotFoundError, not CalledProcessError.

    So it does not go through the branch that handles a git *failure*, and
    without a second handler it is a bare traceback naming an errno.
    """
    monkeypatch.setenv("PATH", str(tmp_path))
    with pytest.raises(RuntimeError, match="could not run git"):
        check_file_size.tracked_rust_files(tmp_path)


def test_no_matching_files_is_an_error_not_a_pass(tmp_path: Path) -> None:
    """An empty match must never be a quiet exit 0.

    `git ls-files` exits 0 and prints nothing when a pathspec matches nothing,
    so a renamed src-tauri/src would otherwise scan zero files and pass: a
    green gate enforcing nothing.
    """
    subprocess.run(["git", "init", "-q"], cwd=tmp_path, check=True)
    with pytest.raises(RuntimeError, match="matched no .rs files"):
        check_file_size.tracked_rust_files(tmp_path)


def test_main_prints_the_diagnostic_rather_than_a_traceback(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """A RuntimeError escaping main() arrives wrapped in the traceback it replaces."""

    def broken(_root: Path) -> list[str]:
        raise RuntimeError("the tree moved")

    monkeypatch.setattr(check_file_size, "tracked_rust_files", broken)
    assert check_file_size.main() == 1
    assert capsys.readouterr().err.strip() == "error: the tree moved"


def test_a_tracked_file_missing_from_the_worktree_says_why(tmp_path: Path) -> None:
    """git ls-files reads the index, which can name a file that is not there."""
    with pytest.raises(RuntimeError, match="missing from the working tree"):
        check_file_size.check(tmp_path, ["src-tauri/src/deleted.rs"], set())


def test_tracked_rust_files_reaches_nested_modules() -> None:
    """`*` crosses `/` in a default git pathspec, so one spec covers the tree.

    That is the opposite of `:(glob)` magic, which sets FNM_PATHNAME and would
    match only the three files sitting directly in src-tauri/src. Asserting
    merely that the list is non-empty would pass either way, while the cap
    silently stopped covering 14 of the 17 files.
    """
    files = check_file_size.tracked_rust_files(REPO_ROOT)
    assert "src-tauri/src/lib.rs" in files, "expected the top-level files"
    assert "src-tauri/src/platform/mod.rs" in files, "expected nested modules"
    nested = [f for f in files if f.count("/") > 2]
    assert len(nested) > 3, f"expected many nested files, found {len(nested)}"
