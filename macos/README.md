# GlobalTokenTracker for macOS

Native SwiftUI shell over the same Rust `core` the Windows (WinUI 3) app uses —
identical adapters, pricing and ledger, so both platforms report the same numbers.

## Build

Requires Rust (rustup) and Swift 6.2+. The Command Line Tools are enough; Xcode
is not needed.

```bash
macos/scripts/build-app.sh --universal     # target/macos/GlobalTokenTracker.app
macos/scripts/build-dmg.sh v0.8.4          # the designed installer disk image
```

`build-dmg.sh` produces `target/macos/GlobalTokenTracker-<tag>-macos-universal.dmg`,
the asset name the in-app updater looks for (`core::update::asset_name`). The
image opens a fixed Finder window — background `macos/dmg/background.tiff` (1×/2×,
regenerate with `make-background.swift`), the app and an Applications shortcut
in the slots it draws, the app icon as the volume icon — laid out by
[`dmgbuild`](https://github.com/dmgbuild/dmgbuild) from `macos/dmg/settings.py`,
so the result is identical on CI and locally.

Without a signing identity both the app and the image are ad-hoc signed: they
run on the build Mac, but Gatekeeper on other Macs rejects them.

## Release (GitHub Actions)

`stable-release.yml` and `alpha-release.yml` run a **macOS job** (`macos-26`
runner) beside the Windows `verify` job: Rust tests + clippy for the portable
crates on macOS, the universal app, the disk image, then a mount check
(both architectures, strict code signature, layout files). `publish` waits for
both and ships the `.dmg` next to the Windows installer — in `SHA256SUMS.txt`
(which the updater verifies against), `BUILD_INFO.txt` and the release notes.

Optional repository secrets for Developer ID signing + notarization:

| Secret | Value |
|---|---|
| `MACOS_CERT_P12` | Base64 of the "Developer ID Application" certificate + key (.p12) |
| `MACOS_CERT_PASSWORD` | Password of that .p12 |
| `MACOS_SIGN_IDENTITY` | e.g. `Developer ID Application: Name (TEAMID)` |
| `MACOS_NOTARY_APPLE_ID` / `MACOS_NOTARY_TEAM_ID` / `MACOS_NOTARY_PASSWORD` | Apple ID, team ID, app-specific password for `notarytool` |

## Layout

| Path | Role |
|---|---|
| `crates/ffi` | C ABI over `core` (JSON in/out, panic-safe), linked as `libgtt_ffi.a` |
| `macos/Sources/CGTT` | Module map exposing `crates/ffi/include/gtt.h` to Swift |
| `macos/Sources/GlobalTokenTracker` | SwiftUI app |
| `macos/Resources` | `Info.plist`, `en` / `zh-Hans` strings |
| `macos/dmg` | Installer window: background renderer + image, `dmgbuild` layout |

## Platform behaviour

- **Window + menu bar extra** replace the window + tray icon. Closing the window
  keeps the app in the menu bar; *Settings › General* can hide the Dock icon and
  register a login item (`SMAppService`).
- **Refresh cadence** matches Windows: a timer (10 s – 5 min, default 30 s) or
  *Only when files change*, which uses an FSEvents stream over every adapter's
  source roots.
- **Data** lives in `~/.globaltokentracker/ledger.db`, shared with the CLI.
  `GTT_DATA_DIR` relocates it; `GTT_NO_QUOTA=1` disables vendor quota polling.
- **Devices** (LAN multi-device viewing): the *Devices* page lists this Mac,
  paired devices (last sync, errors, include switch, forget), possible double
  counting and devices found over Bonjour; *Add Device…* pairs by address and
  one-time code. Once a device is paired, a *Devices* filter joins Tools /
  Models in the toolbar, the Overview gains a *By Device* card and the Quota
  page lists quotas reported by other devices. Paired devices sync on start,
  on ⌘R and every few minutes (core throttles to one sync per 5 min). All of
  it goes through `gtt_peer` / `gtt_load`'s device filter. Sharing *this* Mac
  still needs `globaltokentracker-cli peer serve`.
- `GTT_OPEN_PAGE=<overview|detail|quota|devices|sources|prices|settings>`
  opens that page at launch (screenshots and UI checks).
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
