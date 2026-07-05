"""Shared helpers for the script test suite."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path
from types import ModuleType


def load_script_module(path: Path) -> ModuleType:
    """Import a non-package script by path, registered under its file stem.

    The module is registered in ``sys.modules`` before exec so dataclasses can
    resolve the module's annotations (dataclass processing looks the module up
    in ``sys.modules``).
    """
    spec = importlib.util.spec_from_file_location(path.stem, path)
    assert spec is not None and spec.loader is not None, f"cannot load {path}"
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module
