# TokenBar

AI subscription usage monitor for your terminal.

Tracks quota usage for AI coding assistants — OpenCode Go, ZAI, and Grok — and
displays live, color-coded usage bars in a TUI dashboard.

![Rust](https://img.shields.io/badge/rust-2021-dea584)
![License](https://img.shields.io/badge/license-MIT-blue)

## Dashboard

```
┌─ TokenBar · last 14:32:05 · every 60s ─────────────── ● live ─┐
│                                                               │
┌─ Personal · opencode_go ──────────────────────────── synced ──┐
│ 5h       ████████████████░░░░░░░░░  62%  resets 3d 12h        │
│ Weekly   █████████████████████████  98%  resets 3d 12h        │
└───────────────────────────────────────────────────────────────┘
│                                                               │
┌─ Work · zai ───────────────────────────────────────── synced ─┐
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
- Three providers: **OpenCode Go** (cookie), **ZAI** (API key), **Grok** (OAuth)
- Embedded browser login for OpenCode Go — no manual cookie hunting
- Embedded browser login for Grok — captures grok.com session cookies
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
cargo install --path .
```

This builds the binary and installs it to `~/.cargo/bin/tokenbar`
automatically.

## Quick start

```shell
# Log in to an OpenCode Go account (opens a browser window)
tokenbar login personal

# Log in to a ZAI account
tokenbar login work --provider zai --api-key sk-abc123...

# Log in to a Grok account (opens browser)
tokenbar login grokme --provider grok

# Launch the dashboard
tokenbar

# Web dashboard (mobile-friendly; loopback for private tunnel)
tokenbar serve
# open http://127.0.0.1:8790
```

## Web dashboard

```shell
tokenbar serve                  # 127.0.0.1:8790
tokenbar serve -p 8790          # same
tokenbar serve --bind 127.0.0.1 --port 8790
```

| Path | Description |
|---|---|
| `GET /` | Mobile usage UI (auto-refresh) |
| `GET /api/status` | JSON snapshot of all accounts |
| `POST /api/refresh` | Force a poll cycle |
| `GET /healthz` | Liveness |

Binds **loopback only** by default so Cloudflare Access + Tunnel can expose it privately (e.g. `usage.kaising.net`). Do not bind `0.0.0.0` unless you intentionally want LAN access without Access.

## CLI reference

| Command | Description |
|---|---|
| `tokenbar` | Launch the TUI dashboard |
| `tokenbar serve` | Mobile-friendly web dashboard (`127.0.0.1:8790`) |
| `tokenbar status` | Print account status (session age, workspace IDs) |
| `tokenbar login <name>` | Log in to an OpenCode Go account (opens webview) |
| `tokenbar login <name> --provider zai --api-key <key>` | Save a ZAI API key |
| `tokenbar login <name> --provider grok` | Log in to Grok (opens webview) |
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

### Grok

OAuth provider for [xAI Grok](https://grok.com) / SuperGrok (device code,
same idea as `grok login --device-auth`). Login opens **auth.x.ai** in a
webview; approve access (enter the printed user code if asked). Tokens are
stored per account in `sessions.json`.

```shell
tokenbar login alice --provider grok
tokenbar login bob --provider grok
```

Usage is fetched with the access token from
`https://cli-chat-proxy.grok.com/v1/billing?format=credits` (Weekly bar).

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
│   ├── zai.rs           # ZAI API client
│   └── grok/            # Grok OAuth + billing
├── tui/
│   ├── mod.rs           # TUI event loop and layout
│   └── widgets.rs       # Account card and meter rendering
└── web/
    ├── mod.rs           # axum server + /api/status
    └── static/index.html
```
## Running tests

```shell
cargo test
```

## License

MIT