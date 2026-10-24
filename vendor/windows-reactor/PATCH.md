# Vendored `windows-reactor` 0.100.0

Upstream: <https://crates.io/crates/windows-reactor/0.100.0> (MIT OR Apache-2.0; licenses alongside).
Wired in through `[patch.crates-io]` in the workspace `Cargo.toml`.

## 1. DropDownButton menus open downward (`src/native/winui/mod.rs`)

`Command::SetOwnedMenu` builds the MenuFlyout behind `DropDownButton.menu(..)` in
code with `MenuFlyout::new()` and never sets a placement, so it keeps
`FlyoutBase`'s default, `Top`: every dropdown menu (the scheduling page's
更多 / 添加节点 / 视图) opened upward over its own button even with plenty of room
below. The patch sets `BottomEdgeAlignedLeft` for `DropDownButtonFlyout` only —
the placement a XAML `DropDownButton`/`ComboBox` uses; WinUI still flips to
another side when the preferred one does not fit. Plain `Button.menu(..)` keeps
upstream behaviour.

Upstream's `Menu` has no placement option (`Flyout` does, via `Flyout::placement`).

## 2. Mouse wheel on a Border (`on_pointer_wheel_changed`)

Upstream has no wheel event: `IUIElement.PointerWheelChanged` exists only as an
untyped vtable placeholder and `PointerPointProperties.MouseWheelDelta` is not
bound. The workflow canvas needs the wheel to zoom. Added, all marked
`GTT PATCH`:

- `Border::on_pointer_wheel_changed` — a copy of the PointerMoved plumbing in
  `src/generated.rs` (event id, prop, mount/diff visits, dispatch, descriptor)
  and `src/native/winui/generated.rs` (subscription);
- `PointerEventInfo::wheel_delta` (`src/element.rs`), 120 per notch, 0 for every
  other event, horizontal wheel ignored;
- typed `PointerWheelChanged` / `RemovePointerWheelChanged` slots and methods on
  `IUIElement`, and `IsHorizontalMouseWheel` / `MouseWheelDelta` getters on
  `IPointerPointProperties` (`src/native/winui/bindings.rs`). The
  `MouseWheelDelta` slot follows the interface's alphabetical getter order
  (after the two XButton getters); its neighbours are all 1–4 byte getters, so
  a wrong slot reads 0 or junk, never writes past the i32 — the app only
  accepts 0 < |delta| <= 2400.

Drop this vendor copy once upstream sets a downward default for DropDownButton
menus (or lets `Menu` carry a placement) and exposes a wheel event.
