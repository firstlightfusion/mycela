# mycela

*Named from mycelium — the vast, silent network that binds an ecosystem together.*

Mycela is a Rust framework for distributed control system UIs, combining an Axum web server, multiple industrial SCADA protocols (EPICS, Modbus TCP, etc.) behind an SSE/HTMX front-end with an optional desktop WebView shell. Built in Rust — because speed and memory safety are not optional in industrial control.

### Key Benefits

- **Simple & Fast** — HTMX + SSE for real-time updates
- **Multi-protocol** — EPICS via PVXS, Modbus TCP, ASCII TCP, and ASCII SERIAL adapters supported.
- **Alarm aware** — Full alarm severity display (MAJOR / MINOR / INVALID / OFFLINE).
- **Airgap ready** — All assets (HTMX, fonts, CSS) are embedded in the executable. Simple deployment to isolated networks. **Not to be used as a web service on the public internet.**


## Quick Start

### Prerequisites

- Rust 1.75+ (`rustup update`)
- For EPICS: local `pvxs` crate at `../pvxs-rs`
- For Modbus: `tokio-modbus`
- For ASCII TCP: sibling crate `ascii-tcp`
- For ASCII SERIAL: sibling crate `ascii-serial`

### Demo Server (browser-based)

```bash
# Both protocols (default)
cargo run --example demo_server

# EPICS only
cargo run --example demo_server --no-default-features --features epics

# Modbus only
cargo run --example demo_server --no-default-features --features modbus
```

ASCII protocol examples using the same tagged `protocol` shape:

```json
{
  "id": "flow_ascii_tcp",
  "type": "text_update",
  "label": "Flow (ASCII TCP)",
  "data_type": "double",
  "protocol": {
    "type": "ascii-tcp",
    "host": "127.0.0.1",
    "port": 4000,
    "read_command": "READ FLOW",
    "write_command": "SET FLOW {value}",
    "line_ending": "crlf",
    "response_mode": "number",
    "scale": 1.0,
    "offset": 0.0,
    "min_poll_interval_ms": 500
  }
}
```

```json
{
  "id": "valve_ascii_serial",
  "type": "toggle_button",
  "label": "Valve Cmd (ASCII SERIAL)",
  "data_type": "bool",
  "protocol": {
    "type": "ascii-serial",
    "port_path": "COM3",
    "baud_rate": 9600,
    "data_bits": "eight",
    "parity": "none",
    "stop_bits": "one",
    "read_command": "VALVE?",
    "write_command": "VALVE {value}",
    "line_ending": "lf",
    "response_mode": "bool",
    "min_poll_interval_ms": 500
  }
}
```

Server starts at: **http://127.0.0.1:3000**

### Demo Desktop (self-contained executable)

The desktop app embeds all static assets and config at compile time. A native
window opens automatically — no browser needed.

```bash
cargo run --example demo_desktop --features desktop
```

In `loopback` mode, Axum binds to a random port on `127.0.0.1`; the WebView window opens pointed at
that URL. Logs are written to `logs/mycela.log.<date>` alongside the binary.

### Desktop IPC mode (no localhost listener)

Use IPC transport when you want the desktop app to run without a loopback HTTP
server.

```powershell
$env:MYCELA_DESKTOP_TRANSPORT='ipc'
cargo run --example demo_desktop --features "epics modbus desktop"
```

Transport options:

- `ipc` - Desktop WebView talks to Rust backend via IPC/custom protocol (no Axum bind)
- `loopback` - Backward-compatible mode using localhost HTTP/SSE

If the variable is not set, desktop defaults to `loopback`.

### Adapter Templates

Starter templates are available if you want to build your own app-specific adapters:

- [docs/templates/desktop_adaptor.rs](docs/templates/desktop_adaptor.rs) for IPC desktop apps
- [docs/templates/web_adaptor.rs](docs/templates/web_adaptor.rs) for loopback / browser apps

They are intended to be copied into your app crate and customized with your own routes, assets, screen IDs, and subscription logic. Replace the placeholder values such as `APP_ENTRY_PATH`, `APP_SCREEN_ID`, and `APP_SCREEN_PATH` with app-owned data.

### Deploying an IPC desktop executable

Build a release executable:

```powershell
cargo build --release --example demo_desktop --features "epics modbus desktop"
```

Deploy these artifacts together:

- `target/release/examples/demo_desktop.exe` (or renamed equivalent)
- `logs/` directory (optional but recommended for diagnostics)
- Any required external runtime dependencies (for Windows WebView, install Microsoft Edge WebView2 Runtime)

Set transport at launch (recommended in production):

```powershell
$env:MYCELA_DESKTOP_TRANSPORT='ipc'
./demo_desktop.exe
```

Verification checklist after deploy:

- Startup log shows `Selected desktop transport: ipc`
- No `Axum server bound on port ...` log line in IPC mode
- EPICS/Modbus widgets connect and update normally

## Widgets

| Widget | Description |
|--------|-------------|
| `text_entry` | Editable numeric/string field with write-back |
| `text_update` | Read-only live value display |
| `gauge` | SVG arc gauge with alarm bands |
| `led` | Binary status indicator |
| `slider` | Range control with configurable limits |
| `button` | Momentary command button |
| `toggle_button` | Latching on/off control |
| `select` | Enum drop-down |
| `chart` | Multi-series SVG line chart (up to 6 series) |
| `group` | Layout container for nested widgets |

## Connection Status

All widgets reflect channel state through border colour and status icons:

| State | Indicator |
|-------|-----------|
| Connected, no alarm | No extra border |
| Minor alarm (Hi / Lo) | Orange border + warning icon |
| Major alarm (HiHi / LoLo) | Red border + alarm icon |
| Disconnected / offline | Cyan border, input disabled |
| Invalid / unknown | Grey icon |

## Configuration

Application layout is defined in a JSON file, such as
[examples/demo_app.json](examples/demo_app.json). Applications contain `title`
and `screens`; each screen contains `id`, `title`, and `widgets`. Individual
screens can also be loaded with `ScreenConfig::load`.

### Presets and scoped defaults

Use `presets` to share protocol settings, metadata/alarm bands, styles, or any
other widget options. Use `defaults` for settings common to data widgets in an
application, screen, or group. For a complete, protocol-independent example, see
[examples/compact_app.json](examples/compact_app.json).

```json
{
  "title": "Controls",
  "defaults": { "style": { "width": "100%" } },
  "presets": {
    "motor": {
      "data_type": "double",
      "protocol": { "type": "local", "channel": "app:motor", "initial_value": "0" },
      "metadata": { "control": { "limit_low": 0, "limit_high": 100 } }
    }
  },
  "screens": [{
    "id": "main",
    "title": "Main",
    "widgets": [
      { "id": "entry", "type": "text_entry", "label": "Motor", "preset": "motor" },
      { "id": "readback", "type": "text_update", "label": "Readback", "preset": "motor" }
    ]
  }]
}
```

- Precedence, lowest to highest: app defaults, screen defaults, enclosing group
  defaults (outer to inner), the named preset, explicit widget fields.
- App and screen `presets` are maps of names to widget field objects. A
  screen-local preset replaces an app preset of the same name. Each widget may
  select one preset with `"preset": "name"`. Presets cannot reference other presets.
- Nested objects merge recursively, so a widget can override only
  `metadata.display.units`, a protocol address, or one style dimension.
  Arrays replace rather than concatenate; explicit `null` clears an inherited
  optional field. Changing a nested tagged object's `type` replaces the object
  entirely, preventing connection settings from leaking between protocols.
- Defaults apply to data widgets, not group containers. A group's `defaults`
  apply to its descendants; its own style, protocol, and other explicit settings
  are not inherited. Groups may use presets for their own settings.
- Defaults and presets cannot contain `id`, `children`, `defaults`, `presets`,
  or `preset`. Keep widget identities and layout trees explicit. Group-local
  preset definitions are not supported.
- Expansion happens when deserializing `AppConfig` or `ScreenConfig`, including
  through `load` and `serde_json::from_str` for embedded desktop configurations.
  Rust struct fields and runtime consumers are unchanged. Serialization and
  `ScreenConfig::save` emit fully expanded JSON, not the original preset definitions.
- Existing explicit JSON remains valid. Required fields and feature-gated
  protocol checks still apply after expansion. This shares configuration, not
  live channel connections or a new protocol abstraction.

### Explicit screen configuration

Explicit screen configurations remain supported without presets or defaults:

```json
{
  "id": "demo",
  "title": "Demo Control Screen",
  "description": "...",
  "widgets": [
    {
      "id": "motor_x",
      "type": "text_entry",
      "label": "Motor X Position",
      "data_type": "double",
      "protocol": {
        "type": "epics-pvxs",
        "pv_name": "demo:double"
      },
      "metadata": {
        "display": { "limit_low": 0.0, "limit_high": 100.0, "description": "Motor position", "units": "mm", "precision": 3 },
        "alarm": { "low_alarm_limit": 5.0, "low_warning_limit": 10.0, "high_warning_limit": 90.0, "high_alarm_limit": 95.0, "low_alarm_severity": "MAJOR", "low_warning_severity": "MINOR", "high_warning_severity": "MINOR", "high_alarm_severity": "MAJOR", "hysteresis": 1 }
      }
    },
    {
      "id": "pump_speed",
      "type": "slider",
      "label": "Pump Speed",
      "data_type": "double",
      "protocol": {
        "type": "modbus-tcp",
        "host": "192.168.1.10",
        "port": 502,
        "register": 1000,
        "register_type": "holding_register",
        "scale": 0.1
      }
    }
  ]
}
```

## Technical Stack

| Component | Version |Optional | License |
|-----------|---------|---------|---------|
| [htmx (embedded JavaScript)](https://github.com/bigskysoftware/htmx) | 4.0.0 | No | Zero-Clause BSD |
| [image](https://github.com/image-rs/image) | 0.25.10 | No | Apache-2.0 |
| [Maud](https://github.com/lambda-fairy/maud) | 0.27.0 | No | MIT or Apache-2.0 |
| [tokio](https://github.com/tokio-rs/tokio) | 1.52.4 | No | MIT |
| [tokio-stream](https://github.com/tokio-rs/tokio/releases/tag/tokio-stream-0.1.19) | 0.1.19 | No | MIT |
| [futures](https://github.com/rust-lang/futures-rs) | 0.3.34 | No | MIT or Apache-2.0 |
| [async-stream](https://github.com/tokio-rs/async-stream) | 0.3.6 | No | MIT |
| [chrono](https://github.com/chronotope/chrono) | 0.4.45 | No | MIT or Apache-2.0 |
| [DashMap](https://github.com/xacrimon/dashmap) | 6.2.1 | No | MIT |
| [plotters (SVG)](https://github.com/38/plotters) | 0.3.7 | No | MIT |
| [tracing](https://github.com/tokio-rs/tracing) | 0.1.44 | No | MIT |
| [tracing-subscriber](https://github.com/tokio-rs/tracing) | 0.3.22 | No | MIT |
| [tracing-appender](https://github.com/tokio-rs/tracing) | 0.2.5 | No | MIT |
| [serde](https://github.com/serde-rs/serde) | 1.0.229 | No | MIT or Apache-2.0 |
| [serde_json](https://github.com/serde-rs/json) | 1.0.151 | No | MIT or Apache-2.0 |
| [axum (web and desktop - with loopback)](https://github.com/tokio-rs/axum) | 0.8.9 | Yes | MIT |
| [wry (desktop)](https://github.com/tauri-apps/wry) | 0.55.1 | Yes | MIT |
| [winit (desktop)](https://github.com/rust-windowing/winit) | 0.30.13 | Yes | MIT |
| [tao(desktop)](https://github.com/tauri-apps/tao) | 0.37.1 | Yes | MIT |
| [pvxs (epics protocol)](https://github.com/TineZata/pvxs-rs) | 0.1.1 | Yes | MPL-2.0 |
| [tokio-modbus (modbus protocol)](https://github.com/slowtec/tokio-modbus) | 0.17.0 | Yes | MIT or Apache-2.0 |
| [ascii-tcp (ascii protocol over TCP)](https://github.com/TineZata/ascii-tcp) | 0.1.1 | Yes | MPL-2.0 |
| [ascii-serial (ascii protocol over serial)](https://github.com/TineZata/ascii-serial) | 0.1.1 | Yes | MPL-2.0 |
| [rand (dev-dependency)](https://github.com/rust-random/rand) | 0.8.8 | Yes | MIT or Apache-2.0 |


## Development

```bash
# Debug logging
$env:RUST_LOG="info"; cargo run --example demo_server

# Run tests
cargo test

# Build release (server)
cargo build --release --example demo_server

# Build release (desktop)
cargo build --release --example demo_desktop --features desktop
```

### Environment Variables (EPICS)

```bash
EPICS_PVA_ADDR_LIST=192.168.1.100
EPICS_PVA_AUTO_ADDR_LIST=YES
```

## License

See [LICENSE](LICENSE) file.
