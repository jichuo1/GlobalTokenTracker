# Vendored `windows-canvas` 0.100.0

Upstream: <https://crates.io/crates/windows-canvas/0.100.0> (MIT OR Apache-2.0; licenses alongside).
Wired in through `[patch.crates-io]` in the workspace `Cargo.toml`.

## The one change (marked `GTT PATCH` in `src/reactor.rs`)

Demand-driven canvases (`Canvas::invalidated` / `canvas_invalidated`) share **one**
`GpuDevice` per UI thread instead of creating one each.

Why: every `GpuDevice::new_or_warp()` costs ~19MB private memory and ~40 GPU-driver
worker threads. The overview page has five charts: 268 threads / ~192MB private
with per-canvas devices vs ~100 threads / ~117MB with a single device (measured on
Windows 11 + NVIDIA). Upstream already supports sharing for continuous canvases
(`animated_with_device`); there is just no shared-device variant for demand canvases.

Device loss: `rebuild_surface` calls `forget_shared_if` first, so the canvases
rebuilding after a lost device create (and share) a fresh one.

Drop this vendor copy once upstream offers a shared-device demand canvas.
