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
  (`figcraft_gpu::scene::supported`) plus `scene::plan` for backdrop effects, extended phase by
  phase with a test either way.
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
- Not covered (the page falls back to the CPU): effects that are not a layer blur, a drop shadow or
  a backdrop effect (inner shadows, grain, a node with several effects), a shadow cast by anything
  that isn't one rounded rectangle (a group, an ellipse, a non-uniform radius), image and pattern
  fills, diamond and skewed linear gradients, per-paint blend modes,
  `PlusDarker`, masks, text with a stroke, arrow/circle caps, text with styled runs or colour
  glyphs, and the editor's own options (outline mode, `skip`, `solo`) which come with the canvas
  integration.

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
- Landed: **drop shadows** on rounded rectangles and frames (the common card): the shadow is vello's
  blurred rounded rect — the card's own outline, offset and blurred — drawn under the card, which
  hides it exactly where the CPU masks it. No offscreen layer, so a page of shadowed cards costs
  almost nothing. Parity (`the_gpu_drop_shadow_matches_the_cpu_render`): mean 0.044/255, worst 31
  (vello's Gaussian against the CPU's three-box approximation).
- Landed: **shadows of any number and shape the card itself is**: a shadow that *spreads* grows the
  rounded rect (and its radius) by the spread — the CPU's mitered silhouette dilation; one that
  shows behind is hidden by an opaque card either way; one with a blend mode of its own composites
  as a layer; the card may have any number of shadows, a gradient fill (its stops opaque), an
  opaque stroke (which grows the silhouette by its outer half — inside, centred or outside), rotated
  corners, and children in a clipping frame. `the_gpu_draws_the_shadow_variations` covers all of
  them: mean 0.558/255, worst 59 on the softest shadow edge, 1.5% of pixels off by more than 8 —
  the Gaussian-against-three-boxes difference, largest where two large shadows overlap. A
  **translucent card** (one fill alpha, so one shadow strength) casts a shadow as faint as it paints
  and clipped away where the card covers, as the CPU's cover mask hides it — except when the shadow
  spreads, which the CPU casts from the paints' outline, opaque, so the shadow is full strength.
  Still on the CPU: a non-uniform radius, an ellipse, a group or text with a shadow, a translucent
  fill beside a stroke or fading across the shape, and a shadow alongside another effect (the CPU's
  order matters).
- Landed: **inside, outside and per-side strokes, dashes, width profiles and brush strokes**, as
  the renderer's own band (`figcraft_render::stroke_fill`) filled — vello's native stroke draws the
  centred, plain case. Text with a stroke still falls back.
- A **linear gradient whose width handle isn't square to its axis** (only imported files skew one)
  stays on the CPU: vello's linear gradient is a projection onto its axis, the CPU inverts the full
  handle frame, and the two disagree on such a gradient's bands.
- Landed: **background blur**: the page under the node is drawn into a view-sized texture (the
  node and everything after it left out — the renderer's `backdrop::above`), blurred by the GPU's
  box passes, shown clipped to the node's shape, and the node's content goes over it. Parity mean
  0.049/255, worst 25.
- Landed: **glass**, from the same backdrop: the padded region around the pane is cropped into a
  texture, blurred by the pane's frost and handed to the existing glass shader (the CPU still
  computes the cached distance field and normals — `figcraft_render::glass::pane`), into a texture
  drawn under the pane's content. Parity mean 0.061/255, worst 6, no pixel differs by more than 8
  (`the_gpu_glass_matches_the_cpu_canvas`).
- The single-scene route for a backdrop node (draw the page under it, blur, draw the node over it)
  came first and is still the builder's own path and the parity reference
  (`the_gpu_glass_matches_the_cpu_canvas`, `the_gpu_background_blur_matches_the_cpu_render`); the
  canvas takes the segmented route for every page that has a backdrop node.
- Landed: **the accumulated-backdrop renderer.** A page with a backdrop node is drawn in segments:
  the layers before such a node go into a segment texture, the segment is laid over an
  accumulation texture, the effect is made from the accumulation (a blur of the node's padded
  layer, or a crop + blur + the glass shader) and the node's content goes over it. The page under
  two panes is drawn once, so panes no longer compound.
- **Everything that reads its backdrop now stays on the GPU**, as many panes and blurs as the page
  has. `scene::plan` refuses only a backdrop node nested inside another layer — there is no
  segment boundary inside a layer — and that page stays on the CPU.
- Parity: glass mean 0.061/255, worst 6, no pixel differs by more than 8
  (`the_segmented_page_matches_the_cpu_with_one_glass_pane` — the same numbers as the single-scene
  path); background blur mean 0.049/255, worst 25
  (`the_segmented_page_matches_the_cpu_with_one_background_blur`). The heavy page (30 shadowed
  cards, 3 panes, a background blur and a layer blur) matching to mean 0.725/255, worst 40, all of
  them on antialiased card edges (`the_gpu_page_matches_with_several_backdrop_layers`).
- *Timing* (`segmented_timings`, release, pipelined as the app draws): one pane 904 × 860 CPU
  8.3 ms / GPU 5.7 ms, 2400 × 1600 CPU 14.3 / GPU 3.3; one background blur 4.3 / 3.8 and 11.9 /
  3.2; the heavy page 48.6 / 8.8 and 53.6 / 6.2 — under the 8 ms budget at 2400 × 1600.
- *Timing* (`shadow_timings`, release): 30 cards with radius-12 shadows **0.84–0.90 ms** a frame on
  the GPU (CPU 21–29 ms); with a spread of 6, **0.69–0.91 ms** (CPU 28–34 ms) — a spread shadow is
  still one rounded rect, not a layer.
- In the app (release, control channel, canvas 904 × 860, the same page): the GPU holds
  `canvas.rasterizer = gpu` and drags it in 12–19 ms a frame where the CPU's *full* render takes
  30–34 ms; a single-layer drag is 4.3–4.5 ms on the CPU's retained layers against 12–19 ms on the
  GPU, which re-encodes the page every frame (see "Next slice" below).
- Next: **cross-frame retention for segmented pages** — while one layer moves, the segments and
  effect pixels under it have not changed, and the CPU path re-draws only what moved
  (`figcraft_render::layers`); the GPU path needs the same (a key on the document's subtrees, the
  view and the zoom) before a single-layer drag on a backdrop page beats the CPU's retained
  renderer on render time. Its canvas *latency* is already lower in the app (17.8 ms against the
  CPU's 41 ms in the same session), because no pixels cross the bus.
- Then: inner shadows, grain, spread and translucent shadows, and drop shadows cast from blurred
  layers.

Exit: the 30-shadow + 3-glass + 2-blur page renders entirely on the GPU, and its drag frame at
2400 × 1600 stays at or under the 10–13 ms the CPU path reaches with the layer cache, at
full quality — the target is a single frame budget (≤ 8 ms p95) at that size. **Met** in isolation:
6.2 ms at 2400 × 1600 (`segmented_timings`; the machine's GPU throughput moves 2–3× with what else
is running, so compare runs made together, not across sessions).

#### Straight vs premultiplied alpha (what broke the first attempt)

A first implementation matched the CPU inside every effect (mean 0.06–0.11/255) but failed at
their borders: a pane's rim showed the page background instead of the stripe under it, and a
background blur's edge showed blur where the CPU shows none (worst 123–223 on 250–1600 pixels).

The cause was neither the compose pass nor the image's placement: **vello's targets hold straight
(separated) alpha** — its fine pass stores `rgb / alpha` (`vello_shaders`' `fine.wgsl`, and
`Renderer::register_texture` says its textures are read as unpremultiplied) — while the
accumulation, the box blurs, the glass shader (its `min(rgb, alpha)` clamp) and egui all work on
premultiplied pixels, as tiny-skia's pixmaps do. A segment's antialiased rim therefore held
`(255, 255, 255, 0)`, and a premultiplied-over blit added all of that white to the page.

The canvas now premultiplies once, where vello's output meets the premultiplied side: the compose
pass premultiplies each segment as it lays it over the accumulation, and `render_layer`
premultiplies an effect layer before a blur, the glass shader, or vello's own image brush reads it.
The same conversion removed the differences the layer-blur parity test used to have on its
antialiased edges (mean 0.047 → 0.011/255, worst 33 → 7).

#### Why the segments read only the node's padded layer

The CPU blurs `back`, the node's layer grown by `pad` each way — not the whole view. Cropping to
that rectangle first and blurring the crop is what the segmented canvas does now (`blur_rect`),
which is both the same arithmetic and much cheaper: the background-blur page at 2400 × 1600 went
from 11.6 to 3.2 ms a frame.

### P4 — default on, cleanup

- **Landed.** The GPU canvas is the desktop default (it was from P2; P3's effects and the segmented
  renderer only widen the feature set), `FIGCRAFT_GPU_CANVAS=0` turns it off, and the fallback,
  wasm and export policy are written here and in `docs/architecture.md`; `docs/hardware-parity.md`,
  `docs/gaps.md` and `ROADMAP.md` carry the numbers.
- **Nothing CPU-only was removed, on purpose.** The CPU renderer is still the oracle for the parity
  suite, the source of every export, the browser's canvas, the fallback for refused pages and the
  retainer the GPU lacks (cross-frame retention). The single-scene backdrop route in
  `scene::build_with` is kept as the parity reference for each effect, though the canvas always
  segments a page that has a backdrop node.
- What is left is the full-coverage program below (P5–P11), also listed in `docs/gaps.md` (gap 7).

## Full GPU coverage (P5–P11)

Everything the CPU canvas draws, the GPU canvas should draw too — exports, the parity suite, the
browser and machines without an adapter keep the CPU renderer, but no *page* should fall back
because of what it contains. In the order the work pays off:

### P5 — paints: images, patterns, diamond gradients, per-paint blends

Any file with a photo, a logo or a pattern currently re-renders the whole page on the CPU. The
renderer already decodes, filters (brightness, contrast, saturation, exposure, blur) and caches
every image pixmap; the GPU only has to draw it.

**Landed.** A paint that isn't a solid or a linear/radial/angular gradient is handed to
`figcraft_render::paint::prepare_at`, which returns the renderer's own prepared pixmap (decoded and
filtered image, a pattern's cell, a diamond gradient's 256 × 256 tile), its extend mode, filter
quality, opacity and image-pixel → local transform. The builder uploads that pixmap once (vello's
image atlas keeps it while the same `Blob` is asked for; the `Arc` is cached, so a photo's pixmap —
already cached by the renderer — is the same one every frame) and draws it as vello's image brush:

- **Images**: Fill, Fit, Crop, Tile, rotation and crop, the renderer's padded 1-px border for Fit and
  Crop (so `Pad` spreads transparency, not edge pixels), `Extend::Repeat` for tiles. Filtered and
  translucent images work: the filters are in the pixmap, the paint's opacity is the sampler's
  `alpha`.
- **Pattern fills**: the renderer's cell pixmap, `Extend::Repeat`, bilinear.
- **Diamond gradients**: the renderer's tile through its handle frame, `Extend::Pad`.
- **Per-paint blend modes**: a fill or stroke with its own blend gets a vello layer of its own, as
  the node's blend already did. `PlusDarker` still falls back (P8).

Resampling is the one difference: vello's image sampling is nearest (`ImageQuality::Low`) or
bilinear (`Medium`) — there is no bicubic — so a photo drawn much smaller than its pixels samples
four texels where tiny-skia's bicubic samples sixteen. Measured on a page of 1:1, fitted (2×
downscaled), cropped, tiled, rotated and filtered photos, a diamond gradient, a pattern fill and
multiplied/screened paints: mean 0.066/255, worst 10 on one channel, 5 of 450,000 pixels off by
more than 8 (`the_gpu_draws_the_paint_variations`).
- Animated GIFs (the only animation the renderer plays) stay on the CPU: the canvas refuses a page
  whose images animate, since a frame changes under a document that did not.

Tests: parity per scale mode, per filter, with rotation and crop; a pattern tile; a diamond
gradient; a multiplied fill (`the_gpu_draws_the_paint_variations`). Exit: a page of photos, logos
and pattern fills draws on the GPU within the page tolerance. **Met.**


### P6 — any-outline shadows, inner shadows

A shadow or an inner shadow cast by anything that is not one rounded rectangle still sends the page
to the CPU: an ellipse, a star or vector, text, a card with per-corner radii, a group. The CPU draws
these from the layer's pixels (or the paints' outline) with the box blur; on the GPU they need one
offscreen layer per such node:

- **Outline silhouette** (one outline: ellipse, vector, boolean, text, per-corner rect): draw the
  outline into a layer with the shadow colour (plus a mitred stroke of twice the spread to dilate
  it), blur it with the GPU's box passes, and draw it under the node clipped outside the paints'
  cover — the same arithmetic as the rounded-rect fast path, one layer instead of one draw.
- **Pixels silhouette** (a group, a frame whose children reach past it): the node's content into a
  layer, dilated by the spread (`fx::morph`'s square kernel as a separable max filter), blurred,
  tinted, drawn under it.
- **Inner shadow**: the shape's coverage inverted at the shadow's offset (dilated/shrunk by the
  spread), blurred, tinted and clipped to the shape; for a group, from its pixels' cover.
- `Layers` grows one method (`shadow`/`inner_shadow`) returning the image; the builder draws it and
  the node's own content over it, in the CPU's order (inner shadows over the fills, under children
  and strokes; drop shadows under everything).

Tests: parity for an ellipse, a star, a vector, text, a per-corner card, a group and a frame with
overflowing children; each with and without spread; an inner shadow on a card and on a group.
Timing: 30 such cards must stay under ~10 ms at 2400 × 1600 (they cost one layer each, like a layer
blur). Exit: no shadow of any kind falls back.

### P7 — effect combinations, grain, progressive blur

The CPU applies a node's effects in list order: inner shadows, then the layer blur, grain and
texture, then drop shadows, over the backdrop read. A node with two kinds currently falls back
whole.

- **Combinations**: the builder reads the effect list and drives the same pipeline on the GPU —
  content → layer blur / grain / texture (in order) → drop shadows from that result → backdrop.
  Each stage is a pass the canvas already has, chained through one layer.
- **Grain and texture**: the renderer's noise is deterministic (`grain::noise`/`grains`/`texture`
  take the node, transform and seed), so the CPU can compute the grain tile once and the canvas
  uploads it (cached by node, seed and zoom) — the pixels are the CPU's own. A node-scale change
  regenerates the tile.
- **Progressive blur**: the CPU's radius ramps along an axis; draw it as several band passes of the
  box blur at increasing radii, masked by the ramp, or as one blur with a ramp-weighted mix — the
  second is cheaper and within the effect tolerance.

Tests: parity for shadow + layer blur, two blurs, blur + grain, grain on text, a progressive blur
card. Exit: any single node's effect list draws on the GPU.

### P8 — masks and plus-darker

- **Masks** (alpha, luminance, outline): the mask node's coverage into a texture; the masked
  siblings drawn into a layer; one pass multiplies the layer by the mask (the `compose` pipeline
  with a second input). Layers that a mask hides are not drawn.
- **`PlusDarker`**: no vello blend mode; composite the node's (or paint's) layer with a small pass
  of the CPU's formula.
- The editor's **mask outlines** stay an egui overlay.

Tests: parity for each mask type, a mask over a group, a text mask, and a plus-darker layer and
paint. Exit: masked pages draw on the GPU.

### P9 — the text tail

Text in one style already matches byte for byte. What still falls back: runs with their own fills
and colours, colour glyphs (emoji), text with a stroke, and text as a mask.

- **Styled runs**: layout gives each run its glyphs; build one outline per run and fill it with that
  run's paints, in order.
- **Colour glyphs**: the layout's embedded colour bitmaps go through the P5 image path.
- **Text strokes**: the renderer's stroke band for glyph outlines (`stroke_fill` for text), filled
  as any other band.

Tests: parity for a paragraph with per-run colours, an emoji, stroked text. Exit: no text falls back.

### P10 — structure: nested backdrops, outline mode

- **A backdrop node inside a layer** (a glass pane in a group or frame, a blurred card in a card) is
  the last structural fallback: `scene::plan` refuses it as there is no segment boundary inside a
  layer. Draw the ancestor into a layer (P7's machinery), segment inside it, and composite — or, when
  the ancestor is a plain pass-through container with no opacity, blend or clip, flatten it into the
  page's plan.
- **Outline mode** (the editor's wireframe view): every outline stroked one device pixel, no fills —
  the builder draws it from the same outlines.
- **Soft proof and Display P3** stay on the CPU until the canvas is colour-managed (an sRGB surface
  today, see [`hardware-parity.md`](hardware-parity.md)); they are editor modes, not content.

Tests: parity for a pane inside a frame and a blurred card inside a card; an outline-mode parity
test; the plan's flattening rules. Exit: the fallback predicate answers `Ok` for every page the
canvas can be asked to draw with a device, apart from the colour-managed modes.

### P11 — performance: cross-frame retention

Coverage first, then speed. A segmented page is re-encoded whole every frame, so dragging one layer
costs 12–19 ms against the CPU's retained 4–5 ms (its canvas latency is already lower). Keep the
node's encoded scene and its effect pixels between frames, keyed on the document's subtrees, the
view and the zoom, as `figcraft_render::layers` does; a drag then re-encodes only what moved, and
the segments below it are reused.

Tests: a drag benchmark that must beat the CPU's retained renderer on every page in the corpus
(plain, layer blur, shadows, glass, blurs); a test that a kept segment is dropped when its content
changes. Exit: the GPU is at or under the CPU's retained frame time everywhere, at full quality.

Deliberate exceptions, documented rather than ported: exports (`svg`, `pdf`, `png`/`jpg`), the parity
suite and the browser keep the CPU renderer; soft proof and Display P3 (above); a machine whose
adapter cannot run vello (the canvas refuses and says why in `ui.inspect`).

## Test matrix

Run in `cargo xtask ci` unless marked *timing* (release, `--ignored --nocapture`).

| # | test | what it proves |
|---|---|---|
| 1 | `the_gpu_canvas_matches_the_cpu_render` | pixel parity per feature: shapes, radii, gradients, text, opacity, blend, strokes |
| 1b | `the_gpu_text_matches_the_cpu_render` | text is pixel-identical (measured: 0 differing bytes) |
| 1c | `the_gpu_layer_blur_matches_the_cpu_render`, `the_gpu_drop_shadow_matches_the_cpu_render`, `the_gpu_draws_the_shadow_variations` | effects: mean 0.011, 0.044 and 0.512/255 (the last with spread, two shadows, a stroke, a gradient card, a clipped frame and a rotation) |
| 1d | `the_segmented_page_matches_the_cpu_with_one_glass_pane`, `..._one_background_blur`, `the_gpu_page_matches_with_several_backdrop_layers` | backdrop effects in the accumulated renderer: mean 0.061, 0.049 and 0.725/255 |
| 1e | `the_gpu_draws_the_paint_variations` | paints: a photo 1:1, fitted, cropped, tiled, rotated, filtered and translucent, a pattern fill, a diamond gradient, multiplied and screened paints (mean 0.066/255, worst 10) |
| 2 | `unsupported_pages_fall_back` | the predicate rejects exactly the features not yet ported |
| 3 | `the_gpu_canvas_reads_back` | screenshot/export path: texture → `Pixmap`, premultiplied, same size |
| 4 | *timing* `canvas_frame_timings`, `segmented_timings` | CPU vs GPU frame at 2320 × 1440 (plain and heavy) and 2400 × 1600 (backdrop pages) |
| 5 | *timing* `heavy_scene_timings` (one effect group at a time, GPU against CPU), `heavy_scene_gpu_timings`, `layer_blur_timings`, `shadow_timings`, `segmented_breakdown` | each effect's own cost stays honest |
| 6 | `cargo xtask wasm` | the browser build still compiles and stays CPU |
| 7 | screenshot comparison | the real app looks the same with the GPU canvas on. Measured on the 34-layer page at 34.6%: mean 0.9/255, differences on shape and shadow edges only — the CPU path snaps the view outwards to whole device pixels while the GPU draws the requested view, so content can sit a pixel apart |

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
- 2026-10-24: **P3 slice 3 landed** — backdrop effects. The page under a node is drawn into a
  texture (`backdrop::above` lists what to leave out) and read there:
  - **Background blur** on the GPU: blurred, shown through the node's shape, content over it.
    Parity mean 0.049/255, worst 25; 1.84 ms pipelined against the CPU's 5.07 ms at 904 × 860.
    Pages may have one and stay on the GPU.
  - **Glass** on the GPU: the padded backdrop is cropped, blurred by the pane's frost and run
    through the existing glass shader, from the CPU's cached distance field and normals
    (`figcraft_render::glass::pane` split out of `glass()`). Parity mean 0.061/255, worst 6, no
    pixel off by more than 8. Alone, a pane cost about what the CPU cost (4–11 ms against 5–9 ms),
    so pages with a pane stayed on the CPU until the accumulated renderer landed.
- 2026-10-24: **P3 slice 4 landed — the accumulated-backdrop renderer**, and with it glass and every
  backdrop effect stay on the GPU, however many the page has:
  - A page with a backdrop node is drawn in segments: the layers before it into a segment texture,
    laid over an accumulation, the effect made from the accumulation, its content over it. The page
    under two panes is drawn once.
  - Parity (release tests): glass mean 0.061/255 worst 6 with no pixel off by more than 8;
    background blur mean 0.049/255 worst 25; the heavy 30-card + 3-pane + background-blur +
    layer-blur page mean 0.725/255 worst 40, all on card edges.
  - Release, pipelined: heavy page 8.8 ms at 904 × 860 and **6.2 ms at 2400 × 1600** against the
    CPU's 48.6 and 53.6 (`segmented_timings`); one pane 5.7 / 3.3 ms, one background blur 3.8 /
    3.2 ms.
  - The first attempt failed at effect borders because vello's targets hold straight alpha; see
    "Straight vs premultiplied alpha" above.
- 2026-10-24: **P3 slice 5 landed — wider drop shadows, and the strokes that cast them.** A card's
  shadow is its own rounded rect whatever the card is painted with: spread, any number of shadows,
  `showBehind`, a shadow blend of its own, a gradient fill with opaque stops, a translucent fill
  (one alpha: the shadow is as faint as it paints and clipped away where the card covers), an
  opaque stroke (inside, centred or outside — the silhouette grows by its outer half), rotation,
  and children in a clipping frame. Strokes themselves are no longer limited to plain centred ones: inside, outside,
  per-side, dashed, profiled and brush bands are filled from the renderer's own outline. A linear
  gradient whose width handle isn't square to its axis (only imported files skew one) now stays on
  the CPU, where vello's projection and the CPU's handle frame agree.
  - `the_gpu_draws_the_shadow_variations`: mean 0.558/255, worst 59, 1.5% of pixels off by more
    than 8 — the Gaussian/three-box difference on the soft edges, not a shape error.
  - 30 spread shadows: **0.69–0.91 ms** a frame against the CPU's 28–34 ms (`shadow_timings`).
- 2026-10-24: **the full-coverage program (P5–P11) is written down** in *Full GPU coverage*, in the
  order the work pays off: paints (images, patterns, diamond gradients, per-paint blends), then any-outline and
  inner shadows, effect combinations with grain and progressive blur, masks and plus-darker, the
  text tail, nested backdrops and outline mode, and cross-frame retention. Each phase carries its
  own tests and exit criterion; the deliberately non-ported set (exports, the browser, soft proof
  and Display P3) is named at the end.
- 2026-10-25: **P5 landed — paints.** Images (every scale mode, rotation, crop, filters, opacity),
  pattern fills, diamond gradients and per-paint blend modes draw on the GPU from the renderer's own
  prepared pixmaps; only skew-free linear gradients still use vello's own gradient brush. A page
  with a photo, a logo or a pattern no longer re-renders on the CPU.
  - `the_gpu_draws_the_paint_variations`: mean 0.066/255, worst 10, 5 of 450,000 pixels off by more
    than 8 — the bilinear (vello) against bicubic (tiny-skia) difference on a 2× downscaled photo.
  - Animated GIF pages stay on the CPU (the canvas refuses them in `present`, before any work).
  - `PlusDarker` paints and skewed linear gradients still fall back, with a test each.

