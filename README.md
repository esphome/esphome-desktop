# ESPHome Device Builder

A cross-platform desktop application that bundles ESPHome with Python and runs the ESPHome Device Builder as a background daemon with system tray integration.

## Features

- **System Tray Integration**: Runs in the background with a system tray icon
- **Single-Instance**: Only one instance runs at a time; launching again opens the browser
- **Auto-Updates**: Checks for ESPHome (Python) updates and notifies you
- **Self-Updating App**: macOS DMG, Windows NSIS, and Linux AppImage installs can update themselves in-place from GitHub Releases
- **Cross-Platform**: Native installers for macOS (DMG), Windows (NSIS), and Linux (AppImage/deb)
- **Bundled Python**: Includes a full Python 3.13 runtime - no system Python required

## Installation

### Pre-built Installers

Download the latest release for your platform from [desktop.esphome.io](https://desktop.esphome.io), or grab the files directly from the [GitHub releases](https://github.com/esphome/esphome-desktop/releases/latest) page.

| Platform              | Installer                                                       |
| --------------------- | --------------------------------------------------------------- |
| macOS (Apple Silicon) | `ESPHome.Device.Builder_x.x.x_aarch64.dmg`                      |
| macOS (Intel)         | `ESPHome.Device.Builder_x.x.x_x64.dmg`                          |
| Windows               | `ESPHome.Device.Builder_x.x.x_x64-setup.exe`                    |
| Linux (x86_64)        | `ESPHome.Device.Builder_x.x.x_amd64.AppImage` or `.deb`         |
| Linux (aarch64)       | `ESPHome.Device.Builder_x.x.x_aarch64.AppImage` or `_arm64.deb` |

### First Run

On first launch, ESPHome Device Builder will:
1. Install ESPHome and its dependencies
2. Start the ESPHome Device Builder
3. Open your browser to `http://localhost:6052`

This initial setup may take a few minutes depending on your internet connection.

## Usage

### Starting the App

Simply launch ESPHome Device Builder. It will:
- Start the ESPHome Device Builder in the background
- Show a system tray icon
- Open your browser to the Device Builder

### Tray Menu

Right-click (or left-click on some platforms) the tray icon to access:

- **Open Dashboard** - Open the dashboard in your browser
- **Status** - Shows if the daemon is running
- **Port** - Shows the configured port
- **Backend** - Choose the ESPHome Device Builder channel (stable or beta)
- **Release Channel** - Choose the update channel (Stable, Beta, Dev)
- **Startup** - Choose whether the app launches automatically at login (on by default; see [Running as a remote builder](#running-as-a-remote-builder))
- **Check for Updates** - Check for a new ESPHome Device Builder desktop release, then new ESPHome (Python) and device-builder versions
- **View Logs** - Open the logs folder
- **Open Config Folder** - Open where your ESPHome configs are stored
- **Restart Dashboard** - Restart the ESPHome process
- **Quit ESPHome** - Stop the daemon and exit

### Command Line

The tray menu's actions are also available as `esphome-desktop` subcommands,
which control the running app (useful over SSH, in scripts, or on Linux
desktops without a tray):

```bash
esphome-desktop open             # open the dashboard (starts the app if needed)
esphome-desktop status           # app/backend state, versions, ports, paths (--json for scripts)
esphome-desktop update           # update the desktop app, ESPHome, and the device builder
esphome-desktop restart          # restart the dashboard backend
esphome-desktop logs             # show recent dashboard log output (-f to follow)
esphome-desktop release-channel  # show the ESPHome channel; pass stable|beta|dev to switch
esphome-desktop backend          # show the device-builder channel; pass stable|beta to switch
esphome-desktop startup          # show launch-at-login; pass on|off to change
esphome-desktop quit             # quit the running app
```

Unlike the tray's confirmation dialogs, the CLI applies changes immediately;
running the command is the consent. `logs` and `status` also work when the app
is not running, and `status` prints the config and log directory paths.
Running `esphome-desktop` with no arguments in a terminal prints this command
list instead of launching another app instance; use `open` to start the app.

### Device-builder integration API

The ESPHome Device Builder dashboard (the backend the app runs) can show an
"update available" banner and trigger the update itself through a stable,
versioned JSON interface, separate from the human commands above so their
wording can change without breaking the dashboard. The app sets
`ESPHOME_DESKTOP_BIN` in the backend's environment, pointing at the CLI to call:

```bash
esphome-desktop api version        # {"schema_version":1} (no running app needed)
esphome-desktop api check-update   # one JSON line: per-component update availability
esphome-desktop api update         # trigger the full update; streams JSON, then a terminal line
esphome-desktop api status         # status as one JSON object
```

Every `api` command prints newline-delimited JSON only, one object per line,
valid JSON even on error (`{"type":"err","code":"not_running",...}`). Gate on
`schema_version` before using the others. `check-update` returns
`{"any_available":bool,"app":{...},"esphome":{...},"device_builder":{...}}` where
each component carries `available`, `installed`, `latest`, and `error`. `update`
is fully non-interactive; it stops and restarts the backend without any
confirmation, so an unattended remote builder always comes back on its own even
with no one at the keyboard. Trigger it detached (e.g. Python
`Popen([...], start_new_session=True)`) and poll `check-update`/`status`
afterward; the update completes in the app even if the caller is torn down when
the backend restarts.

On Linux the deb/rpm/AUR packages put `esphome-desktop` on your `PATH`. On
macOS the app installs the command on launch, as a small launcher in
`/opt/homebrew/bin` or `/usr/local/bin` when one is writable; it removes
itself if you later delete the app. If neither directory is writable, call
the binary inside the bundle
(`/Applications/ESPHome Device Builder.app/Contents/MacOS/esphome-desktop`).
On Windows run it from the install directory.

### Running as a remote builder

To keep a machine acting as an always-on builder, leave **Startup → Launch at Login** enabled (the default) so the app relaunches after a reboot. It registers a per-user login item (a macOS LaunchAgent, a Windows `HKCU\...\Run` entry, or a Linux `~/.config/autostart` entry), not a system service, so it starts when a desktop session logs in rather than at boot. For an unattended box that reboots on its own, enable your OS's automatic login so a session starts without someone at the keyboard; otherwise the builder stays offline until someone logs in. The login launch is silent (tray only, no browser).

Turn autostart off with **Startup → Don't Launch at Login**, not the OS's own login-items UI: the app reconciles the login item to its saved preference on every launch, so an entry removed through *System Settings → Login Items* (macOS), *Startup Apps* (Windows), or `~/.config/autostart` (Linux) is re-created on the next start.

### Data Locations

Application data (bundled Python, logs, settings):

| Platform | Location |
|----------|----------|
| macOS | `~/Library/Application Support/io.esphome.builder/` |
| Windows | `%APPDATA%\io.esphome.builder\` |
| Linux | `~/.local/share/io.esphome.builder/` |

This directory contains:
- `python/` - Bundled Python runtime
- `logs/` - Application logs
- `settings.json` - User preferences

Your ESPHome configuration files are stored at `~/esphome/` on all platforms by default (configurable via `config_dir` in `settings.json`).

On Windows, the application itself is installed to `%LOCALAPPDATA%\ESPHome Device Builder\`.

**Windows build data (`C:\esphb\<id>\`).** On Windows the ESPHome Device Builder backend puts its build tree and PlatformIO toolchain under a short folder nested in one `C:\esphb` parent, `C:\esphb\<id>\` (one per config dir), instead of under your profile or config dir. This keeps deep ESP-IDF build paths under Windows' 260-character path limit and clear of spaces in your profile name (e.g. `C:\Users\First Last\…`), both of which otherwise break the build. This folder is **not** removed when you uninstall ESPHome Device Builder, so a reinstall keeps the (multi-GB) toolchain warm and avoids a long re-download. If you want to reclaim the disk space, delete the `C:\esphb` folder by hand after uninstalling. (Only native Windows is affected; running the backend in a Linux container uses the normal data dir.)

## Building from Source

### Prerequisites

- [Rust](https://rustup.rs/) (1.77.2 or later)
- Platform-specific dependencies:
  - **macOS**: Xcode Command Line Tools
  - **Windows**: Visual Studio Build Tools
  - **Linux**: `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev`

### Build Steps

1. Clone the repository:
   ```bash
   git clone https://github.com/esphome/esphome-desktop.git
   cd esphome-desktop
   ```

2. Install Tauri CLI:
   ```bash
   cargo install tauri-cli --locked
   ```

3. Download bundled Python for development:
   ```bash
   ./build-scripts/prepare_bundle.sh
   ```

4. Build:
   ```bash
   cargo tauri build
   ```

The installer will be in `src-tauri/target/release/bundle/`.

### Development

For development with hot-reload:

```bash
cargo tauri dev
```

### Releasing & Self-Update Signing

The desktop app uses `tauri-plugin-updater` to check
`https://github.com/esphome/esphome-desktop/releases/latest/download/latest.json`
for new versions and install them in-place. When a release builds via
`.github/workflows/build.yml`, every supported installer is signed with the
Tauri Ed25519 key and a `latest.json` manifest is uploaded to the draft
release. Once the release is published, existing installs pick it up on their
next update check.

Linux `.deb` / `.rpm` and AUR installs do **not** self-update — those users
update through their system package manager.

## Configuration

Settings are stored in `settings.json`:

```json
{
  "port": 6052,
  "config_dir": null,
  "open_on_start": true,
  "launch_at_startup": true,
  "check_updates": true
}
```

- `port` - Dashboard port (default: 6052)
- `config_dir` - Custom config directory (null = use default)
- `open_on_start` - Open browser when app starts
- `launch_at_startup` - Launch the app automatically at login (default: true; see [Running as a remote builder](#running-as-a-remote-builder))
- `check_updates` - Check for ESPHome updates automatically

## Troubleshooting

### macOS: "App is damaged and can't be opened"

The app is code-signed and notarized. If you still encounter this message (e.g., from a development build), run:

```bash
xattr -c "/Applications/ESPHome Device Builder.app"
```

Then try opening the app again.

### Dashboard won't start

1. Check the logs in the logs folder (accessible via tray menu)
2. Ensure port 6052 (or your configured port) is not in use
3. Try restarting the dashboard from the tray menu

### Serial ports not detected

- **Linux**: You may need to add your user to the `dialout` group:
  ```bash
  sudo usermod -a -G dialout $USER
  ```
  Then log out and back in.

### External components, packages, or builds fail

ESPHome uses **Git** to download external components, remote (`github://`)
packages, dashboard imports, voice models, and other dependencies, so many
configurations won't compile without it. The app bundles Python but not Git,
so these builds fail on machines without Git installed. Install
[Git](https://git-scm.com/downloads), then restart the app so it can detect it
(Git is only checked at startup). The app shows a notification at startup when
Git can't be found on your `PATH`.

### Updates not working

The app uses pip to update ESPHome. If updates fail:
1. Check your internet connection
2. Check the logs for specific error messages

### No system tray icon (Linux)

The tray needs a working `libayatana-appindicator` and a desktop that hosts the
StatusNotifier protocol. The **AppImage** bundles the library and detects it even
when it lives only inside the bundle, so trays now appear on hosts that support
SNI (including KDE Plasma). If your desktop has no StatusNotifier host at all
(some minimal/GNOME setups), the app detects this and falls back to opening the
dashboard in your browser, and update notifications say so instead of pointing at
a tray menu that isn't there.

Without a tray you can still control the app from a terminal — see
[Command Line](#command-line): `esphome-desktop update`, `restart`, `status`,
`logs`, and the rest cover the tray menu's actions.

If the tray still doesn't appear, install the **deb/rpm/AUR** package for your
distribution — it depends on the system `libayatana-appindicator3` package, so
the tray and in-app updater work wherever a StatusNotifier host is present.

## License

Apache License 2.0 - see [LICENSE](LICENSE) for details.

## Contributing

Contributions are welcome! Please see the [Contributing Guide](CONTRIBUTING.md) for details.
