# ESPHome Desktop

A cross-platform desktop application that bundles ESPHome with Python and runs the dashboard as a background daemon with system tray integration.

## Features

- **System Tray Integration**: Runs in the background with a system tray icon
- **Single-Instance**: Only one instance runs at a time; launching again opens the browser
- **Auto-Updates**: Checks for ESPHome updates and notifies you
- **Cross-Platform**: Native installers for macOS (DMG), Windows (MSI/NSIS), and Linux (AppImage/deb)
- **Bundled Python**: Includes a full Python 3.13 runtime - no system Python required

## Installation

### Pre-built Installers

Download the latest release for your platform from the [Releases](https://github.com/esphome/esphome-desktop/releases) page.

| Platform | Installer |
|----------|-----------|
| macOS (Apple Silicon) | `ESPHome Desktop_x.x.x_aarch64.dmg` |
| Windows | `ESPHome Desktop_x.x.x_x64-setup.exe` or `.msi` |
| Linux | `esphome-desktop_x.x.x_amd64.AppImage` or `.deb` |

### First Run

On first launch, ESPHome Desktop will:
1. Create a virtual environment with the bundled Python
2. Install ESPHome and its dependencies
3. Start the dashboard
4. Open your browser to `http://localhost:6052`

This initial setup may take a few minutes depending on your internet connection.

## Usage

### Starting the App

Simply launch ESPHome Desktop. It will:
- Start the ESPHome dashboard in the background
- Show a system tray icon
- Open your browser to the dashboard

### Tray Menu

Right-click (or left-click on some platforms) the tray icon to access:

- **Open Dashboard** - Open the dashboard in your browser
- **Status** - Shows if the daemon is running
- **Port** - Shows the configured port
- **Check for Updates** - Check for new ESPHome versions
- **View Logs** - Open the logs folder
- **Open Config Folder** - Open where your ESPHome configs are stored
- **Restart Dashboard** - Restart the ESPHome process
- **Quit** - Stop the daemon and exit

### Data Locations

| Platform | Location |
|----------|----------|
| macOS | `~/Library/Application Support/ESPHome Desktop/` |
| Windows | `%APPDATA%\ESPHome Desktop\` |
| Linux | `~/.local/share/esphome-desktop/` |

This directory contains:
- `venv/` - Python virtual environment with ESPHome
- `config/` - Your ESPHome configuration files (default location)
- `logs/` - Application logs
- `settings.json` - User preferences

## Building from Source

### Prerequisites

- [Rust](https://rustup.rs/) (1.77.2 or later)
- [Node.js](https://nodejs.org/) (20.x or later)
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

## Configuration

Settings are stored in `settings.json`:

```json
{
  "port": 6052,
  "config_dir": null,
  "open_on_start": true,
  "check_updates": true
}
```

- `port` - Dashboard port (default: 6052)
- `config_dir` - Custom config directory (null = use default)
- `open_on_start` - Open browser when app starts
- `check_updates` - Check for ESPHome updates automatically

## Troubleshooting

### macOS: "App is damaged and can't be opened"

The app is code-signed and notarized. If you still encounter this message (e.g., from a development build), run:

```bash
xattr -c "/Applications/ESPHome Builder.app"
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

### Updates not working

The app uses pip to update ESPHome. If updates fail:
1. Check your internet connection
2. Check the logs for specific error messages

## License

Apache License 2.0 - see [LICENSE](LICENSE) for details.

## Contributing

Contributions are welcome! Please see the [Contributing Guide](CONTRIBUTING.md) for details.
