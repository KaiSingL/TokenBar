# TokenBar

AI subscription usage monitor for your terminal.

Tracks quota usage for AI coding assistants — OpenCode Go and ZAI — and
displays live, color-coded usage bars in a TUI dashboard.

![Rust](https://img.shields.io/badge/rust-2021-dea584)
![License](https://img.shields.io/badge/license-MIT-blue)

## Dashboard

```
┌─ TokenBar · last 14:32:05 · every 60s ─────────────── ● live ─┐
│                                                               │
┌─ Personal · opencode_go ──────────────────────────── ready ───┐
│ 5h       ████████████████░░░░░░░░░  62%  resets 3d 12h        │
│ Weekly   █████████████████████████  98%  resets 3d 12h        │
└───────────────────────────────────────────────────────────────┘
│                                                               │
┌─ Work · zai ───────────────────────────────────────── ready ──┐
│ 5h       ████████████░░░░░░░░░░░░  48%  resets 4h 23m         │
│ Weekly   ██████████████░░░░░░░░░░  56%  resets 6d             │
└───────────────────────────────────────────────────────────────┘
│                                                               │
│              [r] refresh   [q] quit                           │
└───────────────────────────────────────────────────────────────┘
```

Bars are coloured **green** (<60%), **yellow** (60–84%), and **red** (≥85%).
When a fetch fails the last known data is shown as **stale** with the error
message and elapsed time.

## Features

- Multi-account TUI dashboard with per-account usage bars
- Automatic refresh on a configurable interval (default 60 s)
- Two providers: **OpenCode Go** (cookie-based) and **ZAI** (API-key-based)
- Embedded browser login for OpenCode Go — no manual cookie hunting
- CLI subcommands for status checks and session management
- Works entirely offline once sessions are saved

## Installation

### Prerequisites

- [Rust toolchain](https://rustup.rs) (stable, edition 2021)
- **Windows**: [WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/)
  (required for the embedded browser login; on Windows 11 it is usually
  pre-installed)

### Build from source

```shell
git clone <repo-url>
cd tokenbar
cargo build --release
cp target/release/tokenbar ~/.local/bin/   # or anywhere on $PATH
```

## Quick start

```shell
# Log in to an OpenCode Go account (opens a browser window)
tokenbar login personal

# Log in to a ZAI account
tokenbar login work --provider zai --api-key sk-abc123...

# Launch the dashboard
tokenbar
```

## CLI reference

| Command | Description |
|---|---|
| `tokenbar` | Launch the TUI dashboard |
| `tokenbar status` | Print account status (session age, workspace IDs) |
| `tokenbar login <name>` | Log in to an OpenCode Go account (opens webview) |
| `tokenbar login <name> --provider zai --api-key <key>` | Save a ZAI API key |
| `tokenbar session set <name> --cookie <str>` | Manually set a session cookie |
| `tokenbar session rm <name>` | Remove a session |
| `tokenbar session status` | List saved sessions |
| `tokenbar session export` | Print sessions as JSON |
| `--config <path>` | Override path to `auth.toml` |
| `--data-dir <path>` | Override data directory |

## Configuration

`auth.toml` (platform config directory — `~/.config/tokenbar/` on Linux,
`%APPDATA%/tokenbar/` on Windows):

```toml
refresh_interval_secs = 60
request_timeout_secs = 15
max_concurrent_fetches = 4

[[accounts]]
name = "Personal"
provider = "opencode_go"

[[accounts]]
name = "Work"
provider = "zai"
api_key = "sk-..."
```

Session cookies are stored in `sessions.json` in the same data directory.

## Providers

### OpenCode Go

Cookie-based provider for [opencode.ai](https://opencode.ai). The login
command opens an embedded WebView2 browser; once authenticated, the session
cookie is captured automatically and saved to `sessions.json`.

### ZAI

API-key-based provider for [z.ai](https://z.ai). Provide the API key via
`--api-key` during login. Usage data is fetched from the ZAI REST quota
endpoint.

## Keybindings (TUI)

| Key | Action |
|---|---|
| `r` | Force refresh now |
| `q` | Quit |

## Project structure

```
src/
├── main.rs              # Entry point, CLI definition
├── config.rs            # auth.toml load/save
├── session.rs           # sessions.json persistence
├── login.rs             # Embedded browser login (wry + tao)
├── model.rs             # Data types (Account, UsageSnapshot, …)
├── error.rs             # Error types
├── app.rs               # App state and background poller
├── api/
│   ├── mod.rs           # Provider dispatch
│   ├── opencodego.rs    # OpenCode Go scraping
│   └── zai.rs           # ZAI API client
└── tui/
    ├── mod.rs           # TUI event loop and layout
    └── widgets.rs       # Account card and meter rendering
```

## Running tests

```shell
cargo test
```

## License

MIT