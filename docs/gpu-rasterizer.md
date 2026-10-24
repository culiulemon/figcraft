# GPU canvas rasterizer (vello)

Plan for moving the canvas from CPU rasterisation (tiny-skia) to a GPU rasterizer, and the
tests that gate it. Exports, the CLI, MCP, tests, the browser build and machines without an
adapter stay on the CPU renderer: `figcraft-render` remains the reference implementation and the
fallback, not a second product.

## Why

Measured on the owner's machine (Intel Arc A750, release build, control channel, 2026-10-24):

| canvas frame | before the layer cache | after the layer cache |
|---|---|---|
| drag a glass pane, 2400 × 1600 | 95–107 ms | 10–13 ms |
| drag a shadowed card under it | ~95 ms | 15–20 ms |
| same scene without glass | ~65 ms | ~1 ms |
| drag a plain rectangle, 2320 × 1440 | 3.6–4.8 ms | 3.6–4.8 ms |
| drag a layer-blurred rectangle | 6.7–7.2 ms | 6.7–7.2 ms |

The last two rows are the ceiling of the CPU pipeline: a plain rectangle and a blurred one cost
almost the same because the frame is dominated by work that does not depend on what is dragged —
rasterising the whole 2320 × 1440 view, converting it to an `egui::ColorImage`, and uploading the
whole texture (~13 MB) every frame. No amount of effect tuning removes that fixed cost. A GPU
rasterizer does: the scene is drawn straight into a texture the UI samples, with no per-frame
pixels crossing the bus.

## Decision

- **vello 0.11** (`wgpu ^30`, `peniko 0.6`, `kurbo 0.13`): the same wgpu major version as
  eframe 0.36 / egui-wgpu 0.36, so it renders with the window's own `Device`/`Queue` and its
  output can be shown by egui without a read-back. `vello_hybrid 0.2` is on wgpu 29 and is not an
  option. `vello_cpu` is a CPU renderer and does not help.
- The GPU path lives in `figcraft-gpu` (L4). It draws a page region into an `Rgba8Unorm` texture
  (`STORAGE_BINDING | TEXTURE_BINDING | COPY_SRC`) sized to the view's device pixels.
- The canvas shows that texture through `egui_wgpu` (`register_native_texture`), not through a
  `ColorImage`. Overlays (selection, handles, snap guides) stay egui shapes on top.
- **All-or-nothing fallback.** For a page with something the GPU path cannot draw *identically*
  (a feature still in a later phase), the canvas renders that frame on the CPU as today. A
  half-GPU frame is never shown. The predicate is one function over the page subtree
  (`gpu_canvas::supported`), extended phase by phase with a test either way.
- Determinism: the GPU texture is read back only for screenshots, exports and tests. The parity
  suite compares GPU and CPU pixel by pixel with a stated tolerance.

Invariants:

- Same `View`, same device-pixel snapping, same LOD decisions as the CPU path.
- Nothing below L6 depends on egui; `figcraft-gpu` depends on wgpu only. wasm stays CPU
  (`vello`'s wasm targets are WebGL2-only and compute is unavailable there).
- Every phase lands with `cargo xtask ci` green, layered (`cargo xtask layers`), and the canvas
  visibly identical in a screenshot comparison.

## Phases

### P0 — feasibility (dependency, device, handoff, read-back)

- Add `vello = { version = "0.11", default-features = false, features = ["wgpu"] }` to
  `figcraft-gpu`; it must resolve against the workspace's wgpu 30.
- `Gpu::canvas()` builds a `CanvasGpu` on the window's device and queue (once, next to the effects).
- Prove the texture can be shown by egui-wgpu without a copy and read back for screenshots.

Exit: a test renders a fixed scene through vello on the machine's adapter, reads the texture back,
and the app builds and runs unchanged (the CPU canvas is still the default in this phase).

### P1 — scene builder and parity

- `figcraft_gpu::scene`: document page + `View` → `vello::Scene`, reusing
  `figcraft_render::node_outline` (shapes, vectors, booleans, text as glyph outlines) and the
  document's paints.
- Covered in P1: rectangles (per-corner radii, corner smoothing), ellipses/arcs, polygons, stars,
  lines, vectors and booleans (via `node_outline`), text in one style (glyph outlines), solid and
  linear/radial/angular gradients, node opacity, node blend modes, clipping frames, hidden layers.
- Not covered (the page falls back to the CPU): effects, image and pattern fills, diamond
  gradients, per-paint blend modes, `PlusDarker`, masks, inside/outside strokes, dashes, width
  profiles, brush strokes, arrow/circle caps, text with styled runs or colour glyphs, and the
  editor's own options (outline mode, `skip`, `solo`) which come with the canvas integration.

Exit: parity tests (below) pass for the P1 feature list; a drag benchmark shows the frame cost
at 2320 × 1440 well under the CPU's fixed 3.6–4.8 ms.

### P2 — canvas integration

- The render thread owns the `CanvasGpu` (the window's device and queue) and draws each job it can
  into one of two textures; the UI thread registers that texture with `egui_wgpu` (once per buffer
  and generation) and paints it in the canvas' rect, with the usual re-projection. No pixels cross
  the bus, and the canvas never uploads an image for a GPU frame.
- Double buffering: the thread draws into one buffer while the window samples the other, so a
  frame on screen is never half-overwritten. A resize (a zoom) reallocates both and bumps a
  generation; the UI frees the textures it registered for the old one.
- The GPU path is chosen per frame: pages in the builder's feature set go to the GPU, everything
  else to the CPU as before, so nothing that worked changes. Outline mode, outlines for the
  selection, skipped layers, the focus view, soft proof and non-sRGB documents stay on the CPU.
- `ui.inspect` reports the choice and the reason for a fallback (`canvas.rasterizer`,
  `canvas.gpuRefusal`), so a scene that stays on the CPU can be explained without a debugger.
  `FIGCRAFT_GPU_CANVAS=0` (or `off`, `false`, `cpu`) turns the GPU canvas off for a run.

Exit: dragging any page in the P1 feature set is GPU-rendered in the real app; screenshots match
the CPU render within the parity tolerance; toggling the setting changes nothing visible.

### P3 — effects on the GPU

- Landed: **layer blur**. A node with one non-progressive layer blur is drawn into its own
  offscreen texture, blurred by the same three box passes as the CPU renderer (on the GPU), and
  composited with the node's opacity and blend mode; the pixels never leave the device. Parity
  (`the_gpu_layer_blur_matches_the_cpu_render`): mean channel difference 0.047/255, worst 33 on a
  blurred antialiased edge.
- Landed: **drop shadows** on rounded, opaque rectangles (the common card): the shadow is vello's
  blurred rounded rect — the card's own outline, offset and blurred — drawn under the card, which
  hides it exactly where the CPU masks it. No offscreen layer, so a page of shadowed cards costs
  almost nothing. Parity (`the_gpu_drop_shadow_matches_the_cpu_render`): mean 0.044/255, worst 31
  (vello's Gaussian against the CPU's three-box approximation). Spread, `showBehind`, a
  translucent card, a stroke, a non-uniform radius or a shadow alongside another effect fall back.
- Next: drop shadows on anything else (spread, groups, translucent covers), inner shadows,
  background blur, glass and grain as passes on the offscreen layer, reusing the existing compute
  pipelines in `figcraft-gpu` and adding what vello lacks (vello has only a blurred rounded rect,
  not a general filter). Each pass is compared to the CPU's before the effect leaves the fallback
  list.

Exit: the 30-shadow + 3-glass + 2-blur page renders entirely on the GPU, and its drag frame at
2400 × 1600 stays at or under the 10–13 ms the CPU path reaches with the layer cache, at
full quality — the target is a single frame budget (≤ 8 ms p95) at that size.

### P4 — default on, cleanup

- GPU canvas by default on desktop; document the fallback and the wasm/CPU policy here and in
  `docs/architecture.md`; refresh the numbers in `docs/hardware-parity.md`, `docs/gaps.md`,
  `ROADMAP.md`.
- Remove CPU-only machinery the GPU path no longer needs (keep what exports and the fallback
  use).

## Test matrix

Run in `cargo xtask ci` unless marked *timing* (release, `--ignored --nocapture`).

| # | test | what it proves |
|---|---|---|
| 1 | `the_gpu_canvas_matches_the_cpu_render` | pixel parity per feature: shapes, radii, gradients, text, opacity, blend, strokes |
| 1b | `the_gpu_text_matches_the_cpu_render` | text is pixel-identical (measured: 0 differing bytes) |
| 1c | `the_gpu_layer_blur_matches_the_cpu_render`, `the_gpu_drop_shadow_matches_the_cpu_render` | effects: mean 0.047 and 0.044/255 |
| 2 | `unsupported_pages_fall_back` | the predicate rejects exactly the features not yet ported |
| 3 | `the_gpu_canvas_reads_back` | screenshot/export path: texture → `Pixmap`, premultiplied, same size |
| 4 | *timing* `canvas_frame_timings` | CPU vs GPU frame at 2320 × 1440, plain and heavy, p50/p95 |
| 5 | *timing* `heavy_scene_timings` (existing) | the current effects comparison stays honest |
| 6 | `cargo xtask wasm` | the browser build still compiles and stays CPU |
| 7 | screenshot comparison | the real app looks the same with the GPU canvas on |

Parity tolerance (what the suite enforces, measured 2026-10-24 on the owner's machine): mean
absolute channel difference ≤ 0.5/255; at most 0.5% of pixels may differ by more than 8 in a
channel; no channel may differ by more than 96 (a half-covered antialiased edge pixel). Text may
differ in hinting but not in position or size. Anything a pass cannot meet is not enabled; it
stays on the fallback list.

## Risks

- **Text.** vello's text stack (`skrifa 0.44`) differs from the workspace's (`0.47`). We do not
  use vello's text: glyphs come from `figcraft_text`'s outlines, so layout, metrics and coverage
  stay ours.
- **Colour.** vello blends in its own precision; Display P3 documents need a shading colour space
  decision before P2 (canvas shows sRGB today; the CPU path converts after rasterising).
- **Blend modes and masks.** vello has layers, blend modes and luminance masks; `PlusDarker` and
  alpha masks need checking one by one in the parity suite.
- **Images.** vello images live in an atlas owned by the `Renderer`; the CPU path caches decoded
  pixmaps. The GPU path registers textures once per image and reuses them, or stays CPU for
  animated GIFs.
- **Start-up cost.** vello builds shader pipelines at first use (~100 ms); build the renderer
  once, when the first GPU frame is requested, and never on a resize.
- **Driver variance.** The parity suite runs on the machine's adapter and skips without one; CI
  without a GPU stays green on the CPU path.

## Status

- 2026-10-24: **P0 and P1 landed** in `crates/gpu` (`scene.rs`, `canvas.rs`, `canvas_tests.rs`),
  with vello 0.11 on the workspace's wgpu 30.0.1 and the same `kurbo`/`peniko` versions the
  renderer uses.
  - Parity, 600 × 300 page (solids, per-corner radii, linear gradient, ellipse, a 50%-opacity
    layer, a multiply layer, a centre stroke): **mean channel difference 0.072/255**, 600 of
    180,000 pixels (0.33%, all on antialiased edges) differ by more than 8, worst single channel
    67 at a half-covered ellipse edge. Both images look identical.
  - Frame cost, release, Intel Arc A750, whole canvas redrawn every frame, gradients and corner
    radii included:

    | canvas | scene | CPU `render_view` | GPU frame (draw + wait) |
    |---|---|---|---|
    | 2320 × 1440 | one card | 5.5–5.7 ms | 0.76–0.86 ms |
    | 2320 × 1440 | 360 cards | 7.5–8.2 ms | 1.4–1.6 ms |
    | 1160 × 720 | one card | 1.5–1.9 ms | 1.1–1.3 ms |
    | 1160 × 720 | 360 cards | 2.3–2.8 ms | 1.45–1.5 ms |

    The GPU frame is nearly flat in pixel count and scene size (~1 ms of it is vello's per-frame
    encode and the submit/wait round trip); the CPU cost scales with both. Pipelined submissions
    (no wait between frames, as the app draws) measured 0.8–2.2 ms a frame.
  - Read-back (screenshots, exports, tests) costs 1.5 ms at 1160 × 720 and 6–9 ms at 2320 × 1440:
    it stays off the canvas path.
  - `cargo xtask layers` passes; the crate is desktop-only (the web app doesn't build it), so the
    browser stays on the CPU renderer.
- 2026-10-24: **P2 landed** — the canvas draws on the GPU for pages in the P1 feature set and
  falls back to the CPU for everything else, in the real app:
  - 60 plain rectangles at a 1200 × 900 canvas (the app's window): **1.1–1.7 ms a frame** on the
    GPU (steady state, measured over 20 drag steps), **5.4–6.7 ms** on the CPU (one drop shadow
    on one layer puts the page back on the CPU, as designed). Occasional 6–8 ms frames after an
    idle gap are the GPU waking up, not scene cost.
  - Zooming (new texture sizes, re-registration), selection overlays and `ui.screenshot` verified
    in the running app; 645 UI tests, the GPU tests and clippy pass.
- 2026-10-24: **P3 slice 1 landed** — layer blur on the GPU. A page whose only effect is one layer
  blur now stays on the GPU: its layer is drawn into an offscreen texture, blurred by the same
  box passes as the CPU, and composited, all on the device.
  - In the app, a 36-rectangle page with one blurred card (radius 20) holds 2.5–4.9 ms a frame
    over 20 drag steps (median ~3.5 ms, occasional GPU wake-ups) where the same page with the
    blur plus a shadow — which still falls back — takes 6.6 ms on the CPU.
  - In isolation (release, 1200 × 900): 1.31 ms a frame against the CPU's 3.33 ms
    (`layer_blur_timings`). The gap to the in-app number is the window's own rendering and the
    control-channel round trips between steps.
  - Shadows, background blur, glass, inner shadows and grain still put the page on the CPU, and a
    node with a blur *and* another effect does too (the CPU applies them in order).
- 2026-10-24: **P3 slice 2 landed** — drop shadows on rounded, opaque cards, straight in the
  scene: no offscreen layer and no blur pass, so a page of 30 shadowed cards is drawn in one
  frame.
  - Release, 30 cards with radius-12 shadows: **0.74–0.82 ms a frame** on the GPU against 24–26 ms
    for a full CPU render (`shadow_timings`); at 2320 × 1440 as well as 904 × 860, since the cost
    is the scene, not the pixels. In the app, dragging a card on that page holds 1.2–2.2 ms a
    frame (occasional GPU wake-ups spike to 7–10 ms, as on plain pages).
  - Parity: mean 0.044/255, worst 31 (the soft edge), against the CPU's three-box blur.
- 2026-10-24: text proved pixel-identical between the two rasterizers (same glyph outlines from
  the renderer's layout: `the_gpu_text_matches_the_cpu_render`, 0 differing bytes at 1×), and
  `FIGCRAFT_GPU_CANVAS=0` turns the GPU canvas off for a run.
- Next: P3's other effects (spread and non-card shadows, inner shadows, backdrop blurs, glass,
  grain), which is what makes the shadow/glass/blur page — the one that still costs 12–15 ms — run
  entirely on the GPU.
