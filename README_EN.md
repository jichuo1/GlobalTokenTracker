<div align="center">

<img src="assets/icon.png" width="96" height="96" alt="GlobalTokenTracker">

# GlobalTokenTracker

**A desktop app that aggregates usage from every AI coding tool on your machine · Windows (WinUI 3) and macOS (SwiftUI)**

[简体中文](README.md) · **English**

<br>

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-00AEEC?style=flat-square&labelColor=2d3a55)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/Rust-1.90-DEA584?style=flat-square&logo=rust&logoColor=white&labelColor=2d3a55)](https://www.rust-lang.org)
[![Windows](https://img.shields.io/badge/Windows-10%201809%2B-0078D4?style=flat-square&logo=windows&logoColor=white&labelColor=2d3a55)](#requirements)
[![macOS](https://img.shields.io/badge/macOS-26%2B-000000?style=flat-square&logo=apple&logoColor=white&labelColor=2d3a55)](#requirements)
[![UI](https://img.shields.io/badge/UI-WinUI%203%20%C2%B7%20SwiftUI-00AEEC?style=flat-square&labelColor=2d3a55)](#requirements)
[![Release](https://img.shields.io/github/v/release/jichuo1/GlobalTokenTracker?include_prereleases&style=flat-square&label=release&labelColor=2d3a55)](https://github.com/jichuo1/GlobalTokenTracker/releases)
[![Issues](https://img.shields.io/github/issues/jichuo1/GlobalTokenTracker?style=flat-square&labelColor=2d3a55)](https://github.com/jichuo1/GlobalTokenTracker/issues)

</div>

---

## Overview

GlobalTokenTracker (current version 0.8.6, Windows and macOS) unifies local usage stats from AI coding tools: token counts, estimated USD cost from a live price book, subscription quota/credit balances, latency and model distribution. It incrementally scans on-disk logs **read-only** into a local SQLite ledger — nothing is uploaded.

The Windows UI is native WinUI 3 (Rust + windows-reactor); the macOS app is native SwiftUI. Both run the same Rust core (adapters, pricing, ledger), so the same logs give the same numbers on either platform. Built for small binaries, fast cold start and stable tray/menu-bar residency.

## Features

- Five views: Overview / Detail / Quota / Sources / Prices, with persistent tool+model filtering
- Exact bucketing: non-cached input / cache-read / cache-write / output tokens
- **Cross-checked price book**: nine public sources (models.dev, LiteLLM, llmpricing.dev, OpenRouter, Vercel AI Gateway, Helicone, Langfuse, llm-prices.com, Portkey) synced every 12 h; a model is billed at the price **most sources agree on**, so one feed's mistake never reaches the ledger. Offline seed fallback
- Prices page: **search by model** (case- and separator-insensitive: `opus 4.6`, `gpt mini`), a "n/m sources agree" pill per row, per-source quotes on hover, and a "disputed only" filter
- USD estimates kept strictly separate from subscription credits/percentages
- Incremental byte-cursor scanning; truncation/rotation handled; read-only on all sources
- Ledger snapshot backup (two generations) with open-time self-healing
- Periodic or file-watch-only refresh modes; tray-resident
- In-place upgrades: the installer remembers custom dirs and stops running processes
- OTLP/HTTP receiver on `127.0.0.1:4318` for tools that emit telemetry
- **macOS app**: SwiftUI window + menu bar extra (optional today's cost in the menu bar, hide Dock icon, open at login); drum-style page turns that pass through intermediate pages and can be interrupted; custom statistics range with a calendar + 24-hour dial; Settings with text size, theme and accent color, chart font, refresh cadence, update channel and automatic update checks; charts rasterised in one Metal pass (GPU); in-app updates download the new DMG and open it
- **LAN multi-device viewing (preview)**: read-only, off by default, one-time pairing code + TLS 1.3 with pinned certificates, revocation, Bonjour/mDNS discovery plus a subnet check, or add by IP; mutual pairing — viewing, pairing, syncing and a device filter in the Windows and macOS apps' Devices page; sharing from Settings › LAN sharing (Windows) and Devices › Share This Mac (macOS) ([below](#lan-multi-device-viewing-preview))

## Supported sources

| Tool | Form | Level |
|---|---|---|
| Claude Code | Local JSONL logs | ✅ exact + quota (when credentialed) |
| Codex | Local sessions | ✅ exact + quota |
| Devin CLI | Local `devin/cli/sessions.db` | ✅ exact (incl. TTFT) |
| OpenCode / ZCode / Grok / WorkBuddy / MiniMax Code / Kimi Code | Local logs | ✅ exact |
| DeepSeek Harness (CLI / DSH Desktop) | Local sessions `~/.dsh/sessions/…` | ✅ exact |
| Freebuff Desktop | Per-project databases `~/.config/freebuff-desktop/projects/*/desktop-v2.db` (Claude Code / Codex engine threads are counted by those sources) | ✅ exact (free tier counts as US$0) |
| Cline | 4.x / CLI 3.x / desktop: `~/.cline/data/sessions/<id>/<id>.messages.json`; older tasks: `ui_messages.json` + editor globalStorage | ✅ exact (self-reported cost; a $0 BYOK cost is priced from the price book) |
| Command Code | Local JSONL | ✅ exact (self-reported cost) |
| Antigravity (app / IDE / `agy` CLI) | Local conversation databases `~/.gemini/antigravity*/…/*.db` | ✅ exact (incl. thinking tokens; no cache-write data) |
| CodeBuddy IDE / Agent | `CodeBuddyExtension/Data/**/messages/*.json` | 🟡 Request usage snapshots, including streaming updates and history recovery |
| CodeBuddy CLI | `~/.codebuddy/traces/**/*.json` | 🟡 Agent workflow totals; missing breakdowns stay unclassified |
| Cursor / Qoder | Credentials + official quota APIs | 🟡 quota-oriented |

Uninstalled tools are skipped and marked on the Sources page. Deleting a tool's local data does not lose already-ledgered stats.

## Requirements

- **Windows**: Windows 10 **1809+** x64 · Windows App Runtime 1.5+ (auto-installed by the setup on first run; may trigger UAC once) · no admin rights needed · ~20 MB disk.
- **macOS**: macOS **26+** · universal binary (Apple silicon and Intel) · no admin rights needed · ~10 MB disk.

## Install & update

Grab `GlobalTokenTracker-Setup-<ver>-win-x64.exe` from [Releases](https://github.com/jichuo1/GlobalTokenTracker/releases). Double-click for the GUI, or `--quiet` / `--dir <path>` / `--uninstall` for console control. To upgrade, just run the newer installer — it reuses your install location (custom dirs included), stops running processes and preserves all user data under `%USERPROFILE%\.globaltokentracker`.

> [!WARNING]
> The package is currently unsigned — SmartScreen will warn ("More info → Run anyway"). Verify the SHA-256 on the Release page.

**macOS**: download `GlobalTokenTracker-<ver>-macos-universal.dmg` from [Releases](https://github.com/jichuo1/GlobalTokenTracker/releases), open it and drag GlobalTokenTracker to Applications. The ledger lives in `~/.globaltokentracker`, shared with the CLI. Updates: the app downloads the new DMG and opens it — drag to Applications again. Builds without Developer ID signing and notarization are blocked by Gatekeeper on first launch; choose "Open Anyway" in System Settings › Privacy & Security. The DMG contains the app only; build the CLI from source (below).

## LAN multi-device viewing (preview)

Let the computers on one LAN view each other's usage **read-only**, side by side. A shared device serves hourly aggregates only (tokens, cost, quotas, source health) — never raw events, sessions, project names or paths; quotas are per account and never summed across devices.

- **Off by default**: nothing is shared until you run `peer serve`; it listens on loopback `127.0.0.1:4319` unless you bind a LAN address, which requires TLS 1.3.
- **Pairing**: the server prints a one-time 6-digit code (120 s, single use, back-off after failures, burned after five). The code is proven digit by digit, so intercepted traffic cannot brute-force it offline. Viewers pin the server's certificate fingerprint; the server stores only token hashes.
- **Revocation**: `peer revoke` on the server takes effect immediately; `peer forget` on the viewer drops the pin and token.
- **Discovery**: while sharing, the device advertises `_gtt._tcp` on Bonjour/mDNS (name, device id and fingerprint prefix only — no usage) and withdraws it on exit. Where mDNS is blocked (guest Wi-Fi, VLANs, client isolation), add devices by IP (IPv4/IPv6; link-local with `%interface`). Paired devices that changed IP are re-found by device id first.

```bash
# the device being viewed
globaltokentracker-cli peer serve --bind 0.0.0.0:4319 --name "Studio Mac"   # prints a pairing code; Enter for a new one
globaltokentracker-cli peer list
globaltokentracker-cli peer revoke Laptop
# the viewer
globaltokentracker-cli peer discover
globaltokentracker-cli peer pair 192.168.1.20:4319 408036 --name Laptop
globaltokentracker-cli peer pull "Studio Mac" --endpoint summary
globaltokentracker-cli peer sync                       # pull paired devices' aggregates into this ledger
globaltokentracker-cli report week --device all        # this device + peers, with a per-device breakdown
globaltokentracker-cli peer forget "Studio Mac"
```

The Windows and macOS apps' Devices page lists this machine and paired devices (sync status, include switch, forget), possible double counting and nearby devices, and pairs by address + code; once paired, a Devices filter joins the filters, the Overview gains a By Device card and the Quota page shows quotas reported by other devices. The Windows app shares this PC from Settings › LAN sharing and the macOS app from Devices › Share This Mac (address, pairing code — renewed after each pairing so several devices can pair in a row — and the devices that can see this machine, revocable); the Windows installer adds Private-profile-only firewall rules with one administrator approval. Pairing in the apps is mutual: entering the other device's code lets both devices see each other, and turns sharing on here if it was off. "Find devices" uses Bonjour and also checks this machine's own subnet, so devices show up even where Wi-Fi drops multicast. With several devices, all are synced in parallel at their last address; only unreachable ones are looked up again on Bonjour. Synced data is stored separately (never in the local event tables, never re-shared) and appears as a device dimension next to this device; quotas take the freshest report per account and are never summed. macOS needs the Local Network permission; on Windows open TCP 4319 and UDP 5353 for the Private profile only. Protocol and security model: [docs/PEER_API.md](docs/PEER_API.md) (Chinese).

## Build

```powershell
cargo build --release --workspace   # Rust 1.90+, edition 2024 + Windows SDK
cargo test --workspace
powershell -ExecutionPolicy Bypass -File installer\package.ps1   # single-file setup → dist\
```

macOS (Rust + Swift 6.2+; the Command Line Tools are enough):

```bash
macos/scripts/build-app.sh --universal            # target/macos/GlobalTokenTracker.app
macos/scripts/build-dmg.sh v0.8.6                 # installer disk image
cargo build --release -p globaltokentracker-cli   # CLI
```

See [macos/README.md](macos/README.md).

## Privacy

Price sync only issues **GET** requests to the nine public sources above — no usage data, paths or account details are sent (offline seed covers failure). The update check (GitHub Releases) and the optional vendor quota queries are the only other traffic; nothing from your logs is uploaded. By default the only listener is the OTLP receiver on loopback `127.0.0.1:4318`. LAN sharing is opt-in: `peer serve` (default `127.0.0.1:4319`, TLS 1.3 required on LAN addresses) serves read-only aggregates to paired devices only, and its mDNS advertisement (name, device id, fingerprint prefix) exists only while sharing. Source logs are never modified. User data lives in `%USERPROFILE%\.globaltokentracker` (macOS: `~/.globaltokentracker`) and survives updates and uninstalls.

## License

Dual-licensed under **MIT OR Apache-2.0** — [LICENSE-MIT](LICENSE-MIT) · [LICENSE-APACHE](LICENSE-APACHE).

## Acknowledgements

Research references (no code reused): [cc-switch](https://github.com/farion1231/cc-switch) · [TokenTracker](https://github.com/xiufengsun/TokenTracker) · [cursor-usage](https://github.com/chocolatemale/cursor-usage) · [tokcat](https://github.com/handlecusion/tokcat) · [agent-trail](https://github.com/camtrik/agent-trail) · [windows-reactor](https://crates.io/crates/windows-reactor) · pricing data from [models.dev](https://models.dev), [LiteLLM](https://github.com/BerriAI/litellm), [llmpricing.dev](https://llmpricing.dev), [OpenRouter](https://openrouter.ai/models), [Vercel AI Gateway](https://vercel.com/ai-gateway), [Helicone](https://www.helicone.ai/llm-cost), [Langfuse](https://github.com/langfuse/langfuse), [llm-prices.com](https://www.llm-prices.com), [Portkey](https://github.com/Portkey-AI/models).

The Scheduling page supports source controls, a temporary directed model pool, editable workflow nodes and role templates, optional Luna supervision, and configurable call profiles. Deploy the companion model coordinator 0.4.0 or later separately; the GUI installer does not bundle its Python runtime.
