#!/usr/bin/env python3
"""Tests for .github/scripts/bump_docs_fallback_version.py.

The bump-docs-fallback-version workflow rewrites the FALLBACK_VERSION constant
in esphome.io's install page after every non-prerelease, fills in esphome.io's
own pull request template, and opens a PR. The FALLBACK_VERSION constant
drifted seven releases behind before anyone noticed (see the script's own
docstring), so the rewrite gets a regression net: the anchor must match the
single declaration line and nothing else in a file that repeats the same
identifier several more times, and a match count other than one must fail the
job loudly instead of silently leaving the fallback stale or corrupting
unrelated version-shaped text. The template fill-in gets the same treatment:
both edits are anchored, and a match count other than one on either anchor
must fail the job rather than open a pull request with an empty description
or no box ticked.

pytest suite (maintainer-requested framework, fully typed, no classes).
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest
from script_loader import load_script_module

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "bump_docs_fallback_version.py"
GHA_PATH = REPO_ROOT / ".github" / "scripts" / "_gha.py"
PR_TEMPLATE_FIXTURE = (
    REPO_ROOT / "tests" / "fixtures" / "esphome_io_pull_request_template.md"
)

bump = load_script_module(SCRIPT_PATH)
gha = load_script_module(GHA_PATH)

# A realistic slice of InstallSelector.astro: the frontmatter that declares
# FALLBACK_VERSION, four more references to it (one plain assignment and three
# inside console.warn template strings), and other version-shaped text (a ${tag}
# download URL, a numeric AbortSignal.timeout) that a looser anchor could
# mistake for the constant.
SAMPLE = """\
---
import { Code, Tabs, TabItem, LinkButton } from "@astrojs/starlight/components";

const FALLBACK_VERSION = "1.0.2";
const LATEST_JSON = "https://github.com/esphome/esphome-desktop/releases/latest/download/latest.json";

const normalizeVersion = (value) => {
  if (typeof value !== "string") return null;
  const trimmed = value.trim().replace(/^v/i, "");
  return /^\\d+\\.\\d+\\.\\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(trimmed) ? trimmed : null;
};

let version = FALLBACK_VERSION;
try {
  const response = await fetch(LATEST_JSON, {
    signal: AbortSignal.timeout(10_000),
  });
  if (!response.ok) {
    console.warn(
      `InstallSelector: ${LATEST_JSON} returned ${response.status}, ` +
        `using ${FALLBACK_VERSION}`,
    );
  } else {
    const latest = await response.json();
    const normalized = normalizeVersion(latest?.version);
    if (normalized) {
      version = normalized;
    } else {
      console.warn(
        `InstallSelector: unusable "version" ` +
          `(${JSON.stringify(latest?.version)}) in ${LATEST_JSON}, ` +
          `using ${FALLBACK_VERSION}`,
      );
    }
  }
} catch (error) {
  console.warn(
    `InstallSelector: could not fetch ${LATEST_JSON}, using ${FALLBACK_VERSION}`,
    error,
  );
}

const tag = `v${version}`;
const base = `https://github.com/esphome/esphome-desktop/releases/download/${tag}/`;
---
"""


# --------------------------------------------------------------------------- #
# bump_fallback_version: pure transform.
# --------------------------------------------------------------------------- #


def test_bump_fallback_version_rewrites_only_the_declaration() -> None:
    # The other FALLBACK_VERSION mentions live in a plain assignment and inside
    # console.warn template strings, not `const FALLBACK_VERSION = "..."`, so
    # the anchored pattern must leave them (and the unrelated ${tag} URL and
    # AbortSignal timeout) alone.
    new, previous = bump.bump_fallback_version(SAMPLE, "1.1.0")
    assert previous == "1.0.2"
    assert new == SAMPLE.replace(
        'const FALLBACK_VERSION = "1.0.2";', 'const FALLBACK_VERSION = "1.1.0";'
    )
    # The identifier appears 5 times total (the declaration plus 4 more inside
    # console.warn template strings); rewriting only the quoted value must
    # leave every occurrence of the identifier itself in place.
    assert new.count("FALLBACK_VERSION") == SAMPLE.count("FALLBACK_VERSION") == 5
    # The other version-shaped text (the ${tag} download URL and the numeric
    # AbortSignal timeout) must be untouched too, not just present somewhere.
    assert (
        "const base = `https://github.com/esphome/esphome-desktop/releases/"
        "download/${tag}/`;" in new
    )
    assert "AbortSignal.timeout(10_000)" in new


def test_bump_fallback_version_raises_on_zero_matches() -> None:
    # The `current` branch of esphome.io does not have the constant yet; a
    # missing declaration must fail loudly rather than silently no-op, which
    # would let a future release believe the bump happened.
    text = SAMPLE.replace('const FALLBACK_VERSION = "1.0.2";\n', "")
    with pytest.raises(ValueError, match="matched 0 times"):
        bump.bump_fallback_version(text, "1.1.0")


def test_bump_fallback_version_raises_on_two_matches() -> None:
    # An over-broad or duplicated declaration must fail instead of picking one
    # match arbitrarily and rewriting the file inconsistently.
    text = SAMPLE + 'const FALLBACK_VERSION = "1.0.2";\n'
    with pytest.raises(ValueError, match="matched 2 times"):
        bump.bump_fallback_version(text, "1.1.0")


def test_bump_fallback_version_writes_replacement_literally() -> None:
    # The replacement is a lambda, not a backreference string, precisely so a
    # version containing regex-replacement specials (backslash-like dots and
    # hyphens here) is written byte-for-byte rather than interpreted as a group
    # reference. There is no VERSION_RE-passing value containing a literal
    # backslash (VERSION_RE forbids it), so this exercises the same lambda path
    # with the most special-character-heavy value the pattern allows. This also
    # covers `_sub_once`'s success path (count == 1, no raise).
    new, previous = bump.bump_fallback_version(SAMPLE, "1.2.3-a.b-c")
    assert previous == "1.0.2"
    assert 'const FALLBACK_VERSION = "1.2.3-a.b-c";' in new


# --------------------------------------------------------------------------- #
# build_title.
# --------------------------------------------------------------------------- #


def test_build_title_format() -> None:
    assert (
        bump.build_title("1.1.0")
        == "[install] Bump Device Builder fallback version to 1.1.0"
    )


# --------------------------------------------------------------------------- #
# build_body: filling in esphome.io's own pull request template.
# --------------------------------------------------------------------------- #


def _expected_description(previous: str, version: str) -> str:
    return bump.DESCRIPTION.format(
        install_selector=bump.INSTALL_SELECTOR,
        previous=previous,
        version=version,
        release_url=bump.RELEASES_URL.format(version=version),
    )


def test_build_body_against_real_fixture() -> None:
    # The point of shipping a real copy of esphome.io's template as a fixture
    # is to test against it, not a hand-rolled stand-in that could drift from
    # what the upstream template actually looks like.
    template = PR_TEMPLATE_FIXTURE.read_text(encoding="utf-8")
    description = _expected_description("1.0.2", "1.1.0")

    body = bump.build_body(template, "1.0.2", "1.1.0")

    # The description lands right after the `## Description` heading and
    # before the `**Related issue` line, not appended somewhere else.
    heading_index = body.index("## Description")
    description_index = body.index(description)
    related_index = body.index("**Related issue")
    assert heading_index < description_index < related_index

    # `current` is ticked; `next` and the unrelated image-link checkbox are
    # untouched.
    assert "- [x] I am merging into `current`" in body
    assert "- [ ] I am merging into `next`" in body
    assert "- [ ] Link added in `/src/content/docs/components/index.mdx`" in body

    # Both "if applicable" placeholders are replaced with N/A verbatim, and
    # none of the original placeholder text survives anywhere in the body.
    assert "**Related issue (if applicable):** N/A" in body
    assert "- N/A" in body
    assert "<link to issue>" not in body
    assert "<esphome PR number goes here>" not in body
    assert "esphome/esphome#" not in body

    # Reversing all four edits must reconstruct the fixture exactly, proving
    # nothing else in the template (headings, checklist wording, the image
    # generation instructions) was touched.
    reconstructed = (
        body.replace("- N/A", "- esphome/esphome#<esphome PR number goes here>", 1)
        .replace(
            "**Related issue (if applicable):** N/A",
            "**Related issue (if applicable):** fixes <link to issue>",
            1,
        )
        .replace(
            "- [x] I am merging into `current`", "- [ ] I am merging into `current`", 1
        )
        .replace(f"## Description\n\n{description}", "## Description", 1)
    )
    assert reconstructed == template


def test_build_body_raises_when_description_heading_missing() -> None:
    # A restructured template (heading renamed) must fail the job rather than
    # open a pull request with no description inserted anywhere.
    template = PR_TEMPLATE_FIXTURE.read_text(encoding="utf-8").replace(
        "## Description\n", "## Overview\n", 1
    )
    with pytest.raises(ValueError, match=r"'## Description' heading matched 0 times"):
        bump.build_body(template, "1.0.2", "1.1.0")


def test_build_body_raises_when_description_heading_duplicated() -> None:
    # An over-broad or duplicated heading must fail instead of picking a match
    # arbitrarily and inserting the description in the wrong place.
    template = PR_TEMPLATE_FIXTURE.read_text(encoding="utf-8").replace(
        "## Description\n", "## Description\n\n## Description\n", 1
    )
    with pytest.raises(ValueError, match=r"'## Description' heading matched 2 times"):
        bump.build_body(template, "1.0.2", "1.1.0")


def test_build_body_raises_when_current_checkbox_missing() -> None:
    # The checklist line itself was removed or reworded; failing loudly beats
    # opening a pull request where no box got ticked.
    template = PR_TEMPLATE_FIXTURE.read_text(encoding="utf-8").replace(
        "- [ ] I am merging into `current`", "I am merging into `current`", 1
    )
    with pytest.raises(ValueError, match=r"checkbox matched 0 times"):
        bump.build_body(template, "1.0.2", "1.1.0")


def test_build_body_raises_when_current_checkbox_already_ticked() -> None:
    # The pattern only matches an unticked box, so a template that arrives
    # pre-ticked (or double-processed) must fail rather than silently leave it
    # as-is while the rest of build_body proceeds as if it had ticked it.
    template = PR_TEMPLATE_FIXTURE.read_text(encoding="utf-8").replace(
        "- [ ] I am merging into `current`", "- [x] I am merging into `current`", 1
    )
    with pytest.raises(ValueError, match=r"checkbox matched 0 times"):
        bump.build_body(template, "1.0.2", "1.1.0")


def test_build_body_related_issue_placeholder_missing_only_warns(
    capsys: pytest.CaptureFixture[str],
) -> None:
    # The "if applicable" placeholders are cosmetic, unlike the heading and
    # checkbox above: a reworded upstream line must not fail the job, only
    # warn and leave the line as the template has it. Exercises the except
    # branch of `_sub_once_optional` for the `RELATED_ISSUE_RE` anchor.
    template = PR_TEMPLATE_FIXTURE.read_text(encoding="utf-8").replace(
        "**Related issue (if applicable):** fixes <link to issue>",
        "**Related issue:** fixes <link to issue>",
        1,
    )

    body = bump.build_body(template, "1.0.2", "1.1.0")

    # The reworded line survives untouched, not mangled by a partial match.
    assert "**Related issue:** fixes <link to issue>" in body
    # The load-bearing edits still happened.
    assert _expected_description("1.0.2", "1.1.0") in body
    assert "- [x] I am merging into `current`" in body
    # The other, unaffected placeholder still gets its cosmetic edit.
    assert "- N/A" in body
    err = capsys.readouterr().err
    assert "::warning::" in err
    assert "'Related issue' placeholder" in err


def test_build_body_esphome_pr_placeholder_missing_only_warns(
    capsys: pytest.CaptureFixture[str],
) -> None:
    # Same guarantee as above for the second cosmetic placeholder, checked
    # independently so a miss on one anchor cannot mask a miss on the other.
    # Exercises the except branch for the `ESPHOME_PR_RE` anchor.
    template = PR_TEMPLATE_FIXTURE.read_text(encoding="utf-8").replace(
        "- esphome/esphome#<esphome PR number goes here>",
        "- esphome/esphome PR link here",
        1,
    )

    body = bump.build_body(template, "1.0.2", "1.1.0")

    # The reworded line survives untouched, not mangled by a partial match.
    assert "- esphome/esphome PR link here" in body
    # The load-bearing edits still happened.
    assert _expected_description("1.0.2", "1.1.0") in body
    assert "- [x] I am merging into `current`" in body
    # The other, unaffected placeholder still gets its cosmetic edit.
    assert "**Related issue (if applicable):** N/A" in body
    err = capsys.readouterr().err
    assert "::warning::" in err
    assert "'esphome/esphome#' placeholder" in err


# --------------------------------------------------------------------------- #
# main: CLI behaviour.
# --------------------------------------------------------------------------- #


def _write_install_page(tmp_path: Path, text: str = SAMPLE) -> Path:
    path = tmp_path / "InstallSelector.astro"
    path.write_text(text, encoding="utf-8")
    return path


def _write_template(tmp_path: Path, text: str | None = None) -> Path:
    path = tmp_path / "PULL_REQUEST_TEMPLATE.md"
    path.write_text(
        text if text is not None else PR_TEMPLATE_FIXTURE.read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    return path


def test_main_happy_path_writes_file_and_outputs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = _write_install_page(tmp_path)
    template = _write_template(tmp_path)
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    rc = bump.main(["1.1.0", "--file", str(path), "--template", str(template)])

    assert rc == 0
    assert 'const FALLBACK_VERSION = "1.1.0";' in path.read_text(encoding="utf-8")

    output = out.read_text(encoding="utf-8")
    assert "changed=true" in output
    assert "title=[install] Bump Device Builder fallback version to 1.1.0" in output
    # The body is multi-line, so it must use the heredoc form, not `body=...`.
    assert "body<<__GHA_EOF_BODY__" in output
    assert "- [x] I am merging into `current`" in output
    assert "- [ ] I am merging into `next`" in output
    assert "**Related issue (if applicable):** N/A" in output
    assert "- N/A" in output
    assert "1.0.2" in output
    assert "1.1.0" in output


def test_main_no_op_when_already_current(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The no-op branch returns before the template is even read, so it needs
    # no --template argument (the default, which does not exist in tmp_path,
    # must never be touched).
    text = SAMPLE.replace(
        'const FALLBACK_VERSION = "1.0.2";', 'const FALLBACK_VERSION = "1.1.0";'
    )
    path = _write_install_page(tmp_path, text)
    original_mtime = path.stat().st_mtime_ns
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    rc = bump.main(["1.1.0", "--file", str(path)])

    assert rc == 0
    # No rewrite happens: content and mtime are untouched.
    assert path.read_text(encoding="utf-8") == text
    assert path.stat().st_mtime_ns == original_mtime
    output = out.read_text(encoding="utf-8")
    assert "changed=false" in output
    assert "changed=true" not in output


@pytest.mark.parametrize("bad_version", ["v1.2.3", "not-a-version"])
def test_main_returns_1_on_invalid_version(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, bad_version: str
) -> None:
    # The install page's own normalizeVersion() would reject these too, so
    # writing them would leave the fallback pointing at a value the page
    # refuses to use. Version validation happens before any file is touched,
    # so no --template argument is needed here either.
    path = _write_install_page(tmp_path)
    original = path.read_text(encoding="utf-8")
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    rc = bump.main([bad_version, "--file", str(path)])

    assert rc == 1
    assert path.read_text(encoding="utf-8") == original
    assert not out.exists()


def test_main_returns_1_when_file_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    missing = tmp_path / "does-not-exist.astro"
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    rc = bump.main(["1.1.0", "--file", str(missing)])

    assert rc == 1
    assert "::error::" in capsys.readouterr().err
    assert not out.exists()


def test_main_returns_1_when_pattern_does_not_match(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A restructured install page (the constant renamed or removed) must fail
    # the job instead of silently leaving the fallback stale.
    text = SAMPLE.replace('const FALLBACK_VERSION = "1.0.2";\n', "")
    path = _write_install_page(tmp_path, text)
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    rc = bump.main(["1.1.0", "--file", str(path)])

    assert rc == 1
    assert path.read_text(encoding="utf-8") == text
    assert not out.exists()


def test_main_returns_1_when_template_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    # The template is read after the bump is computed but before the install
    # page is written, precisely so a missing template leaves the checkout
    # untouched instead of a bumped install page with no pull request body to
    # go with it.
    path = _write_install_page(tmp_path)
    original = path.read_text(encoding="utf-8")
    missing_template = tmp_path / "does-not-exist.md"
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    rc = bump.main(["1.1.0", "--file", str(path), "--template", str(missing_template)])

    assert rc == 1
    assert "::error::" in capsys.readouterr().err
    assert path.read_text(encoding="utf-8") == original
    assert not out.exists()


def test_main_returns_1_when_template_does_not_match(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A restructured upstream template (no `## Description` heading here) must
    # fail the job and leave the install page untouched, the same as a
    # restructured install page does.
    path = _write_install_page(tmp_path)
    original = path.read_text(encoding="utf-8")
    template = _write_template(tmp_path, "no description heading in this template")
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    rc = bump.main(["1.1.0", "--file", str(path), "--template", str(template)])

    assert rc == 1
    assert path.read_text(encoding="utf-8") == original
    assert not out.exists()


def test_main_returns_1_when_write_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A write failure (permissions, read-only filesystem, disk full) must fail
    # the job rather than report success while nothing changed on disk. The
    # template must be valid here so the failure under test is the install
    # page write, not an earlier template-read/build_body failure.
    path = _write_install_page(tmp_path)
    template = _write_template(tmp_path)
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    def _raise(self: Path, *args: object, **kwargs: object) -> int:
        raise OSError("read-only file system")

    monkeypatch.setattr(Path, "write_text", _raise)

    rc = bump.main(["1.1.0", "--file", str(path), "--template", str(template)])

    assert rc == 1
    assert not out.exists()


def test_main_accepts_version_with_surrounding_whitespace(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    path = _write_install_page(tmp_path)
    template = _write_template(tmp_path)
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    rc = bump.main(["  1.1.0  ", "--file", str(path), "--template", str(template)])

    assert rc == 0
    assert 'const FALLBACK_VERSION = "1.1.0";' in path.read_text(encoding="utf-8")


def test_script_entry_point_runs_main(tmp_path: Path) -> None:
    # Executed as a real subprocess to cover the `__main__` guard: SystemExit
    # must carry main()'s return code out to the process exit status, the way
    # the workflow step (which checks $?) depends on.
    path = _write_install_page(tmp_path)
    template = _write_template(tmp_path)
    result = subprocess.run(
        [
            sys.executable,
            str(SCRIPT_PATH),
            "1.1.0",
            "--file",
            str(path),
            "--template",
            str(template),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0
    assert 'const FALLBACK_VERSION = "1.1.0";' in path.read_text(encoding="utf-8")


# --------------------------------------------------------------------------- #
# sys.path setup (importable regardless of caller's cwd).
# --------------------------------------------------------------------------- #


def test_script_dir_sys_path_insertion_is_idempotent() -> None:
    # The script inserts its own directory onto sys.path so the sibling _gha
    # module is importable regardless of the caller's cwd. Re-executing the
    # module (as happens here, and as would happen if a workflow step sourced
    # it twice) must not insert the directory a second time, so the guard's
    # both branches - "not yet on sys.path" and "already there" - need to run.
    script_dir = str(SCRIPT_PATH.resolve().parent)
    original_path = list(sys.path)
    try:
        if script_dir in sys.path:
            sys.path.remove(script_dir)
        load_script_module(SCRIPT_PATH)  # absent -> inserted
        assert sys.path.count(script_dir) == 1
        load_script_module(SCRIPT_PATH)  # already present -> insert skipped
        assert sys.path.count(script_dir) == 1
    finally:
        sys.path[:] = original_path


# --------------------------------------------------------------------------- #
# _gha.emit_outputs.
# --------------------------------------------------------------------------- #


def test_emit_outputs_single_line_value(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    gha.emit_outputs(changed="true", title="hello")

    assert out.read_text(encoding="utf-8") == "changed=true\ntitle=hello\n"


def test_emit_outputs_multiline_value_uses_heredoc_form(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A multi-line value (a PR body) cannot go on a `key=value` line; GitHub's
    # own heredoc syntax is required, with the delimiter derived from the key
    # so it cannot collide with a sibling output's delimiter.
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    gha.emit_outputs(body="line one\nline two")

    content = out.read_text(encoding="utf-8")
    assert content == ("body<<__GHA_EOF_BODY__\nline one\nline two\n__GHA_EOF_BODY__\n")


def test_emit_outputs_widens_delimiter_colliding_with_the_value(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # GitHub ends a heredoc value at the first line equal to the delimiter, so a
    # body containing that delimiter on its own line would be truncated there and
    # the rest read as further step outputs. The PR body is built from
    # esphome.io's template, i.e. content from another repository, so the
    # delimiter has to be widened until it provably cannot occur in the value.
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    gha.emit_outputs(body="before\n__GHA_EOF_BODY__\nafter")

    content = out.read_text(encoding="utf-8")
    assert content == (
        "body<<__GHA_EOF_BODY__1__\n"
        "before\n__GHA_EOF_BODY__\nafter\n"
        "__GHA_EOF_BODY__1__\n"
    )


def test_emit_outputs_widens_delimiter_past_repeated_collisions(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The widened delimiter can itself collide, so the search has to keep going
    # rather than widen once and assume it is now safe.
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    gha.emit_outputs(body="__GHA_EOF_BODY__\n__GHA_EOF_BODY__1__\nreal content")

    content = out.read_text(encoding="utf-8")
    assert content.startswith("body<<__GHA_EOF_BODY__2__\n")
    assert content.endswith("\n__GHA_EOF_BODY__2__\n")
    # The value itself survives intact, collisions and all.
    assert "__GHA_EOF_BODY__\n__GHA_EOF_BODY__1__\nreal content\n" in content


def test_emit_outputs_delimiter_collision_only_matches_whole_lines(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # GitHub compares the delimiter against the whole line, so a value that only
    # mentions it mid-line is not a collision and must not force a wider
    # delimiter - widening on a substring would be needless churn.
    out = tmp_path / "gha_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    gha.emit_outputs(body="see __GHA_EOF_BODY__ inline\nsecond line")

    content = out.read_text(encoding="utf-8")
    assert content.startswith("body<<__GHA_EOF_BODY__\n")


def test_emit_outputs_writes_to_stdout_without_github_output(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    # Running the script locally (no GITHUB_OUTPUT set) must still show what
    # would have been emitted, rather than raising or discarding it.
    monkeypatch.delenv("GITHUB_OUTPUT", raising=False)

    gha.emit_outputs(changed="false")

    assert capsys.readouterr().out == "changed=false\n"
