# Vendored `windows-reactor` 0.100.0

Upstream: <https://crates.io/crates/windows-reactor/0.100.0> (MIT OR Apache-2.0; licenses alongside).
Wired in through `[patch.crates-io]` in the workspace `Cargo.toml`.

## The one change (marked `GTT PATCH` in `src/native/winui/mod.rs`)

`Command::SetOwnedMenu` builds the MenuFlyout behind `DropDownButton.menu(..)` in
code with `MenuFlyout::new()` and never sets a placement, so it keeps
`FlyoutBase`'s default, `Top`: every dropdown menu (the scheduling page's
更多 / 添加节点 / 视图) opened upward over its own button even with plenty of room
below. The patch sets `BottomEdgeAlignedLeft` for `DropDownButtonFlyout` only —
the placement a XAML `DropDownButton`/`ComboBox` uses; WinUI still flips to
another side when the preferred one does not fit. Plain `Button.menu(..)` keeps
upstream behaviour.

Upstream's `Menu` has no placement option (`Flyout` does, via `Flyout::placement`).
Drop this vendor copy once upstream sets a downward default for DropDownButton
menus or lets `Menu` carry a placement.
