"""Shared helpers for the script test suite."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path
from types import ModuleType

REPO_ROOT = Path(__file__).resolve().parent.parent


def load_script_module(path: Path) -> ModuleType:
    """Import a non-package script by path under a repo-unique module name.

    The module name is derived from the script's path relative to the repo
    root (e.g. ``github_scripts_generate_latest_json``) so two scripts that
    share a file name cannot collide in ``sys.modules``. The module is
    registered in ``sys.modules`` before exec so dataclasses can resolve the
    module's annotations (dataclass processing looks the module up in
    ``sys.modules``).
    """
    relative = path.resolve().relative_to(REPO_ROOT).with_suffix("")
    name = "_".join(relative.parts).replace(".", "_").replace("-", "_").lstrip("_")
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise ImportError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module
