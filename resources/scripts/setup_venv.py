#!/usr/bin/env python3
"""
First-run setup script for ESPHome Desktop.

This script:
1. Creates a virtual environment
2. Installs ESPHome and its dependencies
3. Configures PlatformIO for offline usage (if bundled)

Usage:
    python setup_venv.py <data_dir> [--offline]
"""

import argparse
from pathlib import Path
import subprocess
import sys


def log(msg: str) -> None:
    """Print a log message."""
    print(f"[setup] {msg}", flush=True)


def create_venv(venv_dir: Path, python_exe: str) -> None:
    """Create a virtual environment."""
    log(f"Creating virtual environment at {venv_dir}")
    subprocess.run([python_exe, "-m", "venv", str(venv_dir)], check=True)


def get_venv_python(venv_dir: Path) -> str:
    """Get the path to the Python executable in the venv."""
    if sys.platform == "win32":
        return str(venv_dir / "Scripts" / "python.exe")
    return str(venv_dir / "bin" / "python")


def install_esphome(venv_python: str, offline: bool = False) -> None:
    """Install ESPHome in the virtual environment."""
    log("Upgrading pip")
    subprocess.run(
        [venv_python, "-m", "pip", "install", "--upgrade", "pip"], check=True
    )

    log("Installing ESPHome")
    cmd = [venv_python, "-m", "pip", "install", "esphome"]
    if offline:
        # For offline mode, we'd install from bundled wheels
        cmd.extend(["--no-index", "--find-links", "wheels"])
    subprocess.run(cmd, check=True)


def configure_platformio(venv_python: str, data_dir: Path) -> None:
    """Configure PlatformIO to use local cache."""
    log("Configuring PlatformIO")

    # Set PlatformIO home directory
    platformio_home = data_dir / "platformio"
    platformio_home.mkdir(exist_ok=True)

    # PlatformIO reads PLATFORMIO_CORE_DIR environment variable
    # We'll create a config that the app can source
    config_file = data_dir / "platformio_env.sh"
    with open(config_file, "w") as f:
        f.write(f"export PLATFORMIO_CORE_DIR={platformio_home}\n")

    log(f"PlatformIO configured to use {platformio_home}")


def verify_installation(venv_python: str) -> bool:
    """Verify that ESPHome is installed correctly."""
    log("Verifying installation")
    try:
        result = subprocess.run(
            [venv_python, "-m", "esphome", "version"],
            capture_output=True,
            text=True,
            check=True,
        )
        version = result.stdout.strip()
        log(f"ESPHome installed: {version}")
        return True
    except subprocess.CalledProcessError as e:
        log(f"Verification failed: {e}")
        return False


def main() -> int:
    parser = argparse.ArgumentParser(description="Set up ESPHome Desktop environment")
    parser.add_argument("data_dir", type=Path, help="Application data directory")
    parser.add_argument(
        "--offline", action="store_true", help="Use offline installation"
    )
    parser.add_argument(
        "--python", default=sys.executable, help="Python executable to use"
    )
    args = parser.parse_args()

    data_dir = args.data_dir.resolve()
    data_dir.mkdir(parents=True, exist_ok=True)

    venv_dir = data_dir / "venv"

    try:
        # Create virtual environment
        create_venv(venv_dir, args.python)

        # Get venv Python
        venv_python = get_venv_python(venv_dir)

        # Install ESPHome
        install_esphome(venv_python, args.offline)

        # Configure PlatformIO
        configure_platformio(venv_python, data_dir)

        # Verify installation
        if not verify_installation(venv_python):
            log("Installation verification failed!")
            return 1

        log("Setup complete!")
        return 0

    except subprocess.CalledProcessError as e:
        log(f"Setup failed: {e}")
        return 1
    except Exception as e:
        log(f"Unexpected error: {e}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
