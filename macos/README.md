# GlobalTokenTracker for macOS

Native SwiftUI shell over the same Rust `core` the Windows (WinUI 3) app uses —
identical adapters, pricing and ledger, so both platforms report the same numbers.

## Build

Requires Rust (rustup) and Swift 6.2+. The Command Line Tools are enough; Xcode
is not needed.

```bash
macos/scripts/build-app.sh                     # host architecture
macos/scripts/build-app.sh --universal --zip   # arm64 + x86_64, release zip
```

Output: `target/macos/GlobalTokenTracker.app`. With `--zip` the archive is named
`GlobalTokenTracker-v<version>-macos-universal.zip`, which is the asset the
in-app updater looks for (`core::update::asset_name`).

The bundle is ad-hoc signed and runs on the build machine. Distribution needs a
Developer ID: `CODESIGN_IDENTITY="Developer ID Application: …"` plus notarization.

## Layout

| Path | Role |
|---|---|
| `crates/ffi` | C ABI over `core` (JSON in/out, panic-safe), linked as `libgtt_ffi.a` |
| `macos/Sources/CGTT` | Module map exposing `crates/ffi/include/gtt.h` to Swift |
| `macos/Sources/GlobalTokenTracker` | SwiftUI app |
| `macos/Resources` | `Info.plist`, `en` / `zh-Hans` strings |

## Platform behaviour

- **Window + menu bar extra** replace the window + tray icon. Closing the window
  keeps the app in the menu bar; *Settings › General* can hide the Dock icon and
  register a login item (`SMAppService`).
- **Refresh cadence** matches Windows: a timer (10 s – 5 min, default 30 s) or
  *Only when files change*, which uses an FSEvents stream over every adapter's
  source roots.
- **Data** lives in `~/.globaltokentracker/ledger.db`, shared with the CLI.
  `GTT_DATA_DIR` relocates it; `GTT_NO_QUOTA=1` disables vendor quota polling.
- **Minimum macOS 26** (Liquid Glass); universal binary, so Intel Macs on
  macOS 26 are covered.

## Adapter paths on macOS

`core` resolves platform folders through the `dirs` crate, so
`data_dir()` / `config_dir()` / `data_local_dir()` all map to
`~/Library/Application Support`.

| Source | macOS location | Verified on a Mac |
|---|---|---|
| Claude Code | `~/.claude/projects` | ✅ exact match vs. raw logs |
| Codex | `~/.codex/sessions`, `archived_sessions` | ✅ exact match vs. raw logs |
| Cursor (+ quota) | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` | standard VS Code layout |
| Cline (VS Code ext.) | `~/Library/Application Support/<Code…>/User/globalStorage/saoudrizwan.claude-dev`, `~/.cline/data` | standard VS Code layout |
| OpenCode | `~/.local/share/opencode/opencode.db` (XDG on every OS) | path logic only |
| Antigravity / Gemini | `~/.gemini/antigravity*` | path logic only |
| DeepSeek Harness | `~/.dsh`, `~/Library/Application Support/dsh-desktop/harness` | path logic only |
| ZCode, Grok, WorkBuddy, Qoder, Kimi Code, MiniMax, Command Code | home-directory dot folders | path logic only |
| Devin CLI | `~/Library/Application Support/devin/cli/sessions.db` | ⚠️ mirrors the Windows `%APPDATA%` layout; unconfirmed |
| CodeBuddy IDE | `~/Library/Application Support/CodeBuddyExtension/Data` | ⚠️ mirrors the Windows layout; unconfirmed |
