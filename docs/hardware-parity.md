# Hardware and platform parity

> **Last reviewed:** 2026-10-10 · **Last updated:** 2026-10-25 · **Change:** minor (GPU canvas: page segments kept between frames and nested-backdrop pages given a segment of their own; hardware row 5–8 h → 4–6 h) · **Target:** Figma Design (desktop app 126.10.7)

How FigCraft uses hardware and which platforms it runs on, against Figma. Figma's desktop app is
Electron with a GPU helper process; its canvas renders on the GPU (WebGL, with WebGPU rolling out
since 2025). FigCraft's desktop canvas renders on the GPU too (vello, on the window's wgpu device),
keeping the CPU renderer as its reference, its fallback, its exports and the browser's canvas.

**Summary (estimated):** hardware ~60%, performance ~60%, platforms ~75%.

## Hardware

| Feature | Figma | FigCraft (macOS / Windows / Linux / web) | Hours |
|---|---|---|---|
| Canvas rendering | GPU | desktop: a GPU rasterizer (vello on the window's wgpu device) draws the page straight into a texture egui samples, with no per-frame pixel copy; images, patterns, diamond gradients, portraits, styled and stroked text, colour glyphs, any-outline shadows, inner shadows, every effect list, masks, page-level plus-darker layers and backdrop effects nested in containers are drawn there. A page with backdrop effects is drawn in segments, each segment is kept between frames (keyed on its subtrees, the view and the zoom), and each page node's encoded scene is kept too, so a drag on the corpus pages costs 0.9–10.9 ms a frame. Very little falls back whole to the CPU renderer (tiny-skia, banded across cores, retained, levels of detail below 50% zoom): texture, a group's inner shadow, a paint's plus-darker (and a plus-darker layer under a backdrop effect), and the editor's own modes (outlines, skipped layers, soft proof, P3). In the browser the canvas stays on the CPU (`docs/gpu-rasterizer.md`) | 4–6 left: damage-tracked assembly on backdrop pages (the GPU re-assembles those whole each frame, 2–3× the CPU's damaged-pixel frame) |
| Effects (blurs, glass, noise) | GPU | GPU for layer blur (uniform and progressive), drop and inner shadows of any shape, grain, background blur and glass (the last two read the page in segments, and work inside groups and frames too); two effects on one layer draw in the CPU's order in one layer | in the above |
| HiDPI / Retina | yes | yes (all); a canvas image rendered at the screen's density is placed on whole device pixels, so fractional pans, zooms and display scales don't blur it (#359; geometry tests, the snapping not judged by eye) | — |
| Wide-gamut display (Display P3) | yes, on capable displays | colours stored and exported in P3; the screen shows sRGB: eframe/egui-wgpu 0.36 configure an 8-bit sRGB surface with no colour space option, although wgpu 30 can present Display P3 or extended sRGB on Metal, DX12, Vulkan and WebGPU (see gaps #18). The colour picker warns about colours outside sRGB and View ▸ Soft-proof sRGB stripes them on the canvas | 2–5 |
| Trackpad pinch zoom and two-finger pan | yes | yes (egui zoom delta, smooth scroll) | — |
| Pen tablet pressure (pencil, Figma Draw brushes) | yes (forum reports of regressions in 2025) | Windows pens (winit pointer input) and browser styluses that send touch events with a force (an Apple Pencil in Safari); the Pencil records it as the stroke's width profile. A mouse, macOS and Linux tablets and pens sending only pointer events in a browser report none: constant width | 1–3 |
| Several windows / multi-monitor | yes (a window per file; presenter in its own window) | desktop: File ▸ New window, a tab's Open in new window or a tab dragged off the strip; each window has its own tabs, panels and canvas, draws only when it needs to and asks about unsaved changes when it closes. Only the main window has a screen-reader tree (eframe 0.36), so files stay tabs while a screen reader runs; positions aren't restored at the next start; the presenter stays inside its window; one window in the browser | 1–2 |
| Screen readers | partial | AccessKit tree on desktop (names, roles, states), never tested with a real screen reader; none in the browser | 4–8 + human QA |
| Keyboard layouts (non-US) | yes | keymap uses logical keys; not checked on AZERTY/QWERTZ/JIS | 1–2 |

## Performance (measured by `cargo xtask bench`, synthetic files, owner's Mac)

| Case | FigCraft | Figma |
|---|---|---|
| 50k-layer page, zoomed out, full render | ~8 ms (levels of detail) | not timed side by side |
| 10k layers: full 1440×900 render | 7–15 ms | not timed |
| 10k layers: move one layer, re-render | ~0.2 ms | not timed |
| Design system of ~300 components × 2,000 instances: recolour a main used ~900 times | 4–18 ms | not timed |
| `.figcraft` at 50k layers: save / load | ~0.09 s / ~0.16 s | n/a |
| Idle window | 0 frames, ~0% CPU | — |

One case on the owner's Windows PC (Intel Arc A750, release build, canvas 904 × 860): dragging a
page of 30 shadowed cards, 3 glass panes and 2 blurred cards took ~95 ms a frame on the CPU before
its layer cache; the CPU's retained renderer brings a one-layer drag to 4–5 ms and a whole-page
drag to 30–34 ms. The GPU canvas (which now draws that page, glass included, in one accumulated
pass per segment) reports 12–19 ms a frame in the app for both, with a lower canvas latency (17.8 ms
against the CPU's 41 ms in the same session) as no pixels cross the bus; in isolation the same page
is 6.2 ms at 2400 × 1600 (`cargo test -p figcraft-gpu --release segmented_timings -- --ignored`).
The GPU re-encodes the page each frame — retention like the CPU's is the next slice — so a
single-layer drag on such a page is where the CPU still wins on render time.

Nothing has been timed against Figma on the same document (needs `.fig` import). 

## Platforms

| Platform | Figma | FigCraft | Notes |
|---|---|---|---|
| macOS | desktop app (Intel and Apple silicon) | universal `.dmg`, signed and notarized (v0.2.0) | where nearly all QA happens. Quitting ends the process once the preferences, recovery copies and live sessions are taken care of: the window's teardown crashed on Macs with a Touch Bar (#781); Quit from the menu bar, which asks nothing, keeps unsaved files' recovery copies for the next start |
| Windows x64 / arm64 | desktop app | `.msi` and portable zip for x64, x86 and arm64 | built in CI; Windows: QA pass done 2026-10-11, findings in PR |
| Linux | none (browser only) | AppImage, `.deb`, `.rpm`, Flatpak, tarball, x86_64 and aarch64; riscv64 tarball | CI tests run here. Clipboard (also the BSDs): reads run on a helper thread that gives up after 2 s, and images copied in other apps paste under Wayland through the data-control protocol (#693). Under Wayland the app can't tell its window is minimised, so control requests that need a drawn frame may still wait there (#720). None of this run on a Linux desktop here. On a Wayland compositor without server-side decorations (GNOME) the window gets winit's Adwaita controls (#592: checked by its author in a GNOME Wayland session; KDE not run) |
| FreeBSD | none | tarball | |
| Web | primary (figma.com) | `figcraft-web` (WASM, WebGPU/WebGL2), web zip | no accessibility tree; file access through the browser; copy and paste through the system clipboard (copies written from the copy event, pastes read from the paste event; no pasting images from other apps) |
| iOS / Android | apps: view, comment, mirror prototypes | none (the web build is not touch-first) | 20–40 h for a viewer |

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-26 | minor | GPU canvas: a page-level plus-darker layer draws on the GPU as a step of its own (its pixels built with the blend left off, then composited over a copy of the page by `plus.wgsl`, W3C's `max(0, backdrop + source − 1)`); `the_gpu_draws_a_plus_darker_layer` is mean 0.004/255, worst 6. A paint's plus-darker and a plus-darker layer under a backdrop effect remain on the CPU, with texture and a group's inner shadow. Numbers unchanged |
| 2026-10-26 | minor | GPU canvas: luminance masks draw on the GPU (a compose pass multiplying the run by the mask's `ceil(255 × luma × alpha) / 255`, tiny-skia's formula), kept between frames like the other masked layers; parity is within 2/255 at the mask's ends and the mask test's mean stays 0.052/255. Only texture, a group's inner shadow and plus-darker remain on the CPU. Numbers unchanged |
| 2026-10-25 | minor | GPU canvas, P11 (gap 7): a page with backdrop effects keeps each ordinary step in a canvas-sized texture between frames and each page node's encoded scene too (keyed on its subtrees, the view and the zoom), so a drag encodes only the layers it changed; a step or node that reads the page under it is never kept, and on a segmented page a container with a nested backdrop effect gets a step of its own. A single-layer drag on the corpus pages is 0.9–10.9 ms (at or under the CPU's retained renderer on plain, shadow and effect-heavy pages, 2–3× it on backdrop pages, which are re-assembled whole each frame). Hardware row 5–8 h → 4–6 h (damage-tracked assembly left); percentages unchanged |
| 2026-10-25 | major | GPU canvas coverage (gap 7): images (every scale mode, rotation, crop, filters), pattern fills, diamond gradients and per-paint blends; shadows and inner shadows of any outline (ellipse, star, vector, text, per-corner, groups, overflowing frames) drawn from a layer the GPU blurs with the CPU's own box passes; a node's whole effect list in one layer (inner shadows, layer blur — progressive included —, grain, then the drop shadows cast from that result); styled runs, colour glyphs and glyph strokes (pixel-identical); masks (the mask's coverage cut into the run it clips); and backdrop effects nested in a group or frame (pixel-identical) all draw on the GPU. Parity: 0.066, 0.130, 0.052 and 0.000/255 mean channel difference. Still on the CPU: texture, a group's inner shadow, plus-darker, luminance masks, and the editor's own modes. Hardware row estimate 35–60 h → 5–8 h (P11 retention), since narrowed to 4–6 h |
| 2026-10-18 | minor | macOS quit crash (#781): on Macs with a Touch Bar the app could crash as it quit (an exception in AppKit's Touch Bar support while the window's content view, which the accessibility adapter subclasses, was torn down). Quitting now keeps everything (preferences, this run's recovery copies, live sessions ended) and then ends the process before that teardown. Quit from the menu bar ends the app without the unsaved-changes question, so unsaved files' recovery copies now stay and reopen at the next start instead of being removed. Numbers unchanged |
| 2026-10-17 | minor | Linux row: clipboard reads on Linux and the BSDs time out after 2 s on a helper thread, images copied in other apps paste under Wayland (arboard's data-control feature, #693), a minimised window can't be detected under Wayland, so frame-only control requests may still wait there (#720), and Wayland compositors without server-side decorations give the window Adwaita controls (#592, checked by its author on GNOME, not by the maintainers; KDE not run); compiled in CI, not run on a Linux desktop. HiDPI row: native-density canvas images placed on whole device pixels (#359, geometry tests; not judged by eye). Numbers unchanged |
| 2026-10-14 | minor | Copy and paste in the browser: the web build never wrote to the system clipboard, so Cmd+V after Cmd+C pasted whatever text the clipboard held before (as a text layer) instead of the copied layers. Copies (layers, Copy as SVG/CSS/text/PNG, links) now go out through egui's output, which eframe writes during the copy event (`web_clipboard_tests`). Numbers unchanged. |
| 2026-10-11 | minor | Windows QA pass (gap 8): everyday workflows on Windows 11 through the desktop app and MCP; fixed: unsaved changes lost when the app was killed or crashed (recovery copies every few seconds, reopened at the next start), Ctrl+arrows/Backspace/Delete acting on the whole line in text (now by word; Ctrl+Home/End to the text's ends), scrubbed fields snapping back on release, typing on a field Tab reached, Escape not closing dialogs, skewed or vanishing linear gradients, hints cut off at the panel edge, the window-close question in English in the Spanish interface. Percentages and hours unchanged |
| 2026-10-11 | minor | Display P3 honest minimum (gap 18): the colour picker of a Display P3 file warns when a colour is outside sRGB (Outside sRGB, with what the canvas and exports do), and View ▸ Soft-proof sRGB (`view.proofSrgb`) stripes the canvas pixels sRGB can't show. Why the canvas still shows sRGB: wgpu 30 can present Display P3 or extended-range sRGB surfaces (Metal, DX12, Vulkan, WebGPU canvases), but eframe/egui-wgpu 0.36 configure an 8-bit sRGB surface with no option for the format or colour space, and egui's renderer writes sRGB-encoded UI colours; showing P3 needs those two upstream (or a patched egui-wgpu). Hardware 13–27 h → 12–26 h; gap 18 6–12 h → 5–11 h; percentages, feature breadth (~79%) and ready for real work (~47%) unchanged at this rounding; full parity ~363–697 h → ~362–696 h |
| 2026-10-11 | minor | Several windows (gap 18): File ▸ New window, a tab's Open in new window or a tab dragged off the tab strip opens files in another window with its own tabs, panels and canvas, sharing the files, preferences, libraries and clipboard; each window draws only when it needs to (idle: ~0% CPU), closing one closes its files after asking about unsaved changes (the main window too, which takes over another window instead of quitting; the last window quits), and the control channel and MCP open, list, focus and capture windows (`window.new`, `window.list`, `window.focus`, `window` on any request); the screen-reader tree covers the main window only (eframe 0.36), so with a screen reader running files stay tabs; one window in the browser. Hardware 55% → 60%, 13–27 h → 10–21 h; gap 18 6–12 h → 4–8 h (P3 on screen left); feature breadth (~79%) and ready for real work (~47%) unchanged at this rounding; full parity ~363–697 h → ~360–691 h |
| 2026-10-11 | minor | Stretch and scatter brushes and pen pressure landed (gap 9): eight brushes drawn in code (four stretched along the path, four scattered with spacing, size, rotation and scatter jitter from a seed) in the stroke settings, drawn everywhere strokes are and flattened by outline stroke; the Pencil records pen pressure as the stroke's width profile (Windows pens, browser styluses; macOS and Linux tablets report none) and can draw with a brush; Figma Draw extras breadth 45% → 65%, ready 30% → 45%, 10–18 h → 5–9 h; Strokes 2–4 h → 1–3 h; Hardware 50% → 55%, 15–30 h → 13–27 h; feature breadth (~79%) and ready for real work (~47%) unchanged at this rounding; full parity ~380–725 h → ~375–715 h |
| 2026-10-10 | major | Created from the 2026-10-10 re-measure |
