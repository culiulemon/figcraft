//! Canvas rendering off the UI thread (desktop) or inline (web). The latest request wins (queued
//! older ones are skipped); the UI shows each finished image, re-projected to the current view,
//! until the next one arrives.
//! The renderer keeps its last image ([`figcraft_render::Retained`]): a pan renders only the
//! uncovered strips and an edit only the areas of the layers that changed.
//! A new zoom can't reuse that image, and the old one, re-projected, covers little of the view
//! after a zoom out (an eighth of the width from 800% to 100%). When full renders are slow, the
//! whole view is first rendered coarsely ([`PREVIEW_FACTOR`] times fewer pixels each way, kept
//! apart from the full image so pans and edits still build on it), then refined unless a newer
//! request is already waiting.
//! Without a measure of the new render (the first one, another page or file, a zoom after quick
//! renders) nothing is spent up front: on the desktop a watchdog thread renders the preview once
//! the render from scratch has run past [`PREVIEW_AFTER_MS`], beside it (the full render keeps the
//! other cores), and drops it if the full image is there first.

use std::sync::Arc;

use figcraft_color::ColorProfile;
use figcraft_doc::{Document, NodeId};
use figcraft_render::{Pixmap, Retained, View, Work};
// A monotonic clock in the browser too: preview decisions rest on how long renders take there.
use web_time::Instant;

/// A preview has this many times fewer pixels each way than the view it stands in for.
pub const PREVIEW_FACTOR: f64 = 4.0;
/// Full renders quicker than this (about a frame) show without a preview.
pub const PREVIEW_AFTER_MS: f64 = 12.0;
/// The GPU canvas's interaction frames have this many times fewer pixels each way: a quarter of
/// the pixels keeps a drag smooth at any zoom (a 200 × 300 card at 600% takes 25–35 ms a frame at
/// the screen's resolution), and half resolution still shows 300% of the design's pixels — the
/// frame sharpens the moment the interaction stops.
pub const GPU_PREVIEW_FACTOR: f64 = 2.0;
/// Full GPU frames slower than this (about a frame at 75 Hz) are previewed while interacting.
pub const GPU_PREVIEW_AFTER_MS: f64 = 13.0;
/// The quiet time after the last job before the preview on screen is sharpened: a drag that
/// steps slowly must not lose frames to a full render between its steps.
pub const GPU_REFINE_AFTER_MS: f64 = 60.0;

#[derive(Clone)]
pub struct Job {
    pub doc: Arc<Document>,
    pub page: NodeId,
    pub view: View,
    pub outline: bool,
    /// Outline mode's options (hidden layers, object bounds).
    pub outline_extras: figcraft_render::Outline,
    /// Layers drawn as outlines (outlines for the selection).
    pub outlined: Vec<NodeId>,
    pub skip: Vec<NodeId>,
    /// Only this layer (the focus view).
    pub solo: Option<NodeId>,
    /// Soft-proof sRGB: stripe the colours sRGB can't show.
    pub proof: bool,
    /// A board's dot grid under the layers.
    pub dots: Option<figcraft_render::Dots>,
    /// The document's [`figcraft_engine::DocState::uid`]: images of other files are never shown.
    pub uid: u64,
    /// The document's revision: a preview of *newer* content replaces the sharp frame of older
    /// content on screen (a drag), while a preview of the same content never replaces it.
    pub revision: u64,
    /// A coarse preview may come first when the zoom changes (not in pixel preview, whose pixels
    /// are the point).
    pub zoom_preview: bool,
    /// Draw at the view's own resolution even when the last frames were slow: the UI asks for one
    /// when an interaction stops, sharpening the preview it showed while moving.
    pub refine: bool,
    /// When the UI asked for it: how long the image took to reach the screen.
    pub at: Instant,
}

#[derive(Clone)]
pub struct Done {
    /// The image, ready for a texture (converted off the UI thread). Empty when
    /// [`Self::gpu`] drew the frame.
    pub image: egui::ColorImage,
    /// The GPU canvas's texture for this frame (desktop; `None` when the CPU renderer drew it).
    #[cfg(not(target_arch = "wasm32"))]
    pub gpu: Option<figcraft_gpu::canvas::Frame>,
    /// The view the image shows (the requested one snapped outwards to whole device pixels).
    pub view: View,
    pub ms: f64,
    pub work: Work,
    /// What it is an image of ([`Job::uid`], [`Job::page`], [`Job::revision`]).
    pub uid: u64,
    pub page: NodeId,
    pub revision: u64,
    /// A coarse preview, refined by a later image unless a newer request replaced it.
    pub coarse: bool,
    /// [`Job::at`].
    pub at: Instant,
    /// The request it answers, numbered by the worker in order (0 outside a worker).
    pub seq: u64,
}

/// A render of a document in `profile` as a texture image for the screen (sRGB).
pub fn screen_image(pix: &Pixmap, profile: ColorProfile) -> egui::ColorImage {
    let size = [pix.width() as usize, pix.height() as usize];
    if profile.is_srgb() {
        return egui::ColorImage::from_rgba_premultiplied(size, pix.data());
    }
    let mut data = pix.data().to_vec();
    profile.conversion_to(ColorProfile::Srgb).apply_pixels(&mut data);
    egui::ColorImage::from_rgba_premultiplied(size, &data)
}

/// Soft proof: grey diagonal stripes over the pixels whose colours sRGB can't show.
fn mark_outside_srgb(image: &mut egui::ColorImage, pix: &Pixmap, profile: ColorProfile) {
    let w = image.size[0].max(1);
    let clipped = profile.conversion_to(ColorProfile::Srgb).clipped_pixels(pix.data());
    for (i, (px, out)) in image.pixels.iter_mut().zip(clipped).enumerate() {
        if out && ((i % w) + (i / w)) % 8 < 4 {
            *px = egui::Color32::from_gray(128);
        }
    }
}

/// Render `view` of the job (its own view, or the coarse one standing in for it).
fn run(job: &Job, view: &View, retained: &mut Retained, coarse: bool) -> Option<Done> {
    let t0 = Instant::now();
    // The canvas is a preview: what's too small to see in detail at this zoom is approximated.
    let opts = figcraft_render::Options {
        outline_mode: job.outline,
        outline: job.outline_extras,
        outlined: job.outlined.clone(),
        skip: job.skip.clone(),
        solo: job.solo,
        lod: true,
        // The canvas keeps unchanged layers between frames (see `figcraft_render::layers`).
        doc_id: job.uid,
        magnified_image_pixels: true,
        dots: job.dots,
        ..Default::default()
    };
    let (view, pix, work) = retained.render(&job.doc, job.page, view, &opts)?;
    let mut image = screen_image(pix, job.doc.color_profile);
    if job.proof {
        mark_outside_srgb(&mut image, pix, job.doc.color_profile);
    }
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    Some(Done {
        image,
        #[cfg(not(target_arch = "wasm32"))]
        gpu: None,
        view,
        ms,
        work,
        uid: job.uid,
        page: job.page,
        revision: job.revision,
        coarse,
        at: job.at,
        seq: 0,
    })
}

/// `view` with [`PREVIEW_FACTOR`] times fewer pixels each way, showing at least the same area.
pub fn coarse_view(view: &View) -> View {
    let shrink = |n: u32| (f64::from(n) / PREVIEW_FACTOR).ceil().max(1.0) as u32;
    View { origin: view.origin, scale: view.scale / PREVIEW_FACTOR, width: shrink(view.width), height: shrink(view.height) }
}

/// `view` with [`GPU_PREVIEW_FACTOR`] times fewer pixels each way: the same page shown at fewer
/// pixels, drawn into the canvas's top-left corner (see `figcraft_gpu::canvas::present`).
pub fn gpu_preview_view(view: &View) -> View {
    let shrink = |n: u32| (f64::from(n) / GPU_PREVIEW_FACTOR).ceil().max(1.0) as u32;
    View { origin: view.origin, scale: view.scale / GPU_PREVIEW_FACTOR, width: shrink(view.width), height: shrink(view.height) }
}

/// What the render thread keeps between jobs: the canvas image, the previews standing in for it
/// while the zoom changes, and what the last full render drew and took.
pub struct Renders {
    full: Retained,
    preview: Retained,
    /// The last full render's document, page and scale.
    last: Option<(u64, NodeId, f64)>,
    /// How long the last render from scratch took.
    full_ms: f64,
    /// Previews come first only when renders from scratch take at least this long.
    preview_after_ms: f64,
}

impl Default for Renders {
    fn default() -> Self {
        Self::with_threshold(PREVIEW_AFTER_MS)
    }
}

impl Renders {
    /// Previews when renders from scratch take at least `ms` (tests: 0, always).
    pub fn with_threshold(ms: f64) -> Self {
        Self { full: Retained::default(), preview: Retained::default(), last: None, full_ms: 0.0, preview_after_ms: ms }
    }

    /// Whether `job` is a render from scratch: the kept image is of another zoom, page or file, or
    /// there is none.
    fn starts_over(&self, job: &Job) -> bool {
        self.last.is_none_or(|(uid, page, scale)| uid != job.uid || page != job.page || scale.to_bits() != job.view.scale.to_bits())
    }

    /// Whether `job` gets a preview first: it renders from scratch and those have been slow on a
    /// page already drawn (the first render has no measure).
    fn wants_preview(&self, job: &Job) -> bool {
        job.zoom_preview && self.last.is_some() && self.full_ms >= self.preview_after_ms && self.starts_over(job)
    }

    /// Whether `job` should get a preview should its render run long: from scratch, with none first.
    pub fn wants_late_preview(&self, job: &Job) -> bool {
        job.zoom_preview && self.starts_over(job) && !self.wants_preview(job)
    }

    /// The coarse preview of the whole of `job`'s view, when one should come first.
    pub fn preview(&mut self, job: &Job) -> Option<Done> {
        if !self.wants_preview(job) {
            return None;
        }
        preview_with(job, &mut self.preview)
    }

    /// The image of `job`'s view, built on the kept one where it can be.
    pub fn refine(&mut self, job: &Job) -> Option<Done> {
        let done = guarded(&mut self.full, |r| run(job, &job.view, r, false));
        if let Some(d) = &done {
            self.last = Some((job.uid, job.page, job.view.scale));
            if d.work == Work::Full {
                self.full_ms = d.ms;
            }
        }
        done
    }

    /// Whether `job`'s GPU frame should be drawn at fewer pixels: a full frame of *this* view was
    /// slower than a frame's worth, so a drag would lag behind the pointer (see [`Self::gpu`]). A
    /// refine the UI asked for is never drawn small, and a new zoom is drawn full first — that is
    /// what says a zoomed-out view is quick again (a drag there is sharp, and no cheaper).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn gpu_wants_preview(&self, job: &Job) -> bool {
        !job.refine && job.zoom_preview && self.full_ms >= GPU_PREVIEW_AFTER_MS && self.last == Some((job.uid, job.page, job.view.scale))
    }

    /// The frame drawn by the GPU canvas, when the page is in its feature set. `None` leaves the
    /// frame to the CPU renderer (and to the previews it keeps for slow zoom changes).
    ///
    /// With `coarse`, the frame is drawn at [`GPU_PREVIEW_FACTOR`] times fewer pixels each way,
    /// into the canvas's top-left corner; the returned frame says so (`coarse`, and its `view` is
    /// the smaller one), and the screen's `uv` shows just that part. A frame at the screen's own
    /// resolution follows once the interaction stops (see [`Job::refine`]).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn gpu(&mut self, job: &Job, coarse: bool) -> Option<Done> {
        let t0 = Instant::now();
        let view = if coarse { gpu_preview_view(&job.view) } else { job.view.clone() };
        let frame = gpu_frame(job, &view)?;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let d = Done {
            image: egui::ColorImage::default(),
            gpu: Some(frame),
            view,
            ms,
            work: Work::Full,
            uid: job.uid,
            page: job.page,
            revision: job.revision,
            coarse,
            at: job.at,
            seq: 0,
        };
        if !coarse {
            self.last = Some((job.uid, job.page, job.view.scale));
            self.full_ms = ms;
        }
        Some(d)
    }
}

/// Whether the GPU canvas may draw this job: the editor's outline modes, skipped layers, the
/// focus view, soft proof and Display P3 documents are the CPU renderer's. `FIGCRAFT_GPU_CANVAS=0`
/// turns the GPU canvas off (the CPU path is the reference and the fallback).
///
/// Backdrop effects (glass, background blur) are not checked here: the canvas reads the page in
/// segments for those (see [`figcraft_gpu::canvas::segmented_frame`]) and refuses — leaving the
/// frame to the CPU — what it cannot draw exactly.
#[cfg(not(target_arch = "wasm32"))]
fn gpu_job(job: &Job) -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let enabled = *ENABLED.get_or_init(|| match std::env::var("FIGCRAFT_GPU_CANVAS") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "cpu"),
        Err(_) => true,
    });
    enabled && !job.outline && job.outlined.is_empty() && job.skip.is_empty() && job.solo.is_none() && !job.proof && job.doc.color_profile.is_srgb()
}

/// The GPU canvas's frame for `job`, if it can draw the page and this job asks nothing of it that
/// it doesn't do yet.
#[cfg(not(target_arch = "wasm32"))]
fn gpu_frame(job: &Job, view: &View) -> Option<figcraft_gpu::canvas::Frame> {
    if !gpu_job(job) {
        return None;
    }
    // The canvas keeps the window's own size: a preview fills its top-left part (the frame says
    // which), so alternating previews and full frames reallocate nothing.
    figcraft_gpu::canvas::present(&job.doc, job.page, view, (job.view.width, job.view.height))
}

/// The coarse preview of the whole of `job`'s view, kept in `retained`.
pub fn preview_with(job: &Job, retained: &mut Retained) -> Option<Done> {
    let view = coarse_view(&job.view);
    guarded(retained, |r| run(job, &view, r, true))
}

/// `d` numbered as the answer to request `seq`.
fn answering(d: Done, seq: u64) -> Done {
    Done { seq, ..d }
}

/// Of two finished renders, the one to show: the newer request's, and of one request's the refined
/// image (the watchdog's preview can arrive after it).
pub fn better(a: Option<Done>, b: Done) -> Done {
    match a {
        Some(a) if a.seq > b.seq || (a.seq == b.seq && !a.coarse && b.coarse) => a,
        _ => b,
    }
}

/// Run `f` on `retained`; after a panic, forget the image it may have left half drawn.
fn guarded(retained: &mut Retained, f: impl FnOnce(&mut Retained) -> Option<Done>) -> Option<Done> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(retained))) {
        Ok(d) => d,
        Err(_) => {
            retained.clear();
            None
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct Worker {
    tx: std::sync::mpsc::Sender<(u64, Job)>,
    rx: std::sync::mpsc::Receiver<Done>,
    /// Jobs submitted so far.
    submitted: u64,
    /// The newest job the thread is done with: rendered (or skipped for a newer one) and the UI
    /// woken for it.
    finished: Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Worker {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, jobs) = std::sync::mpsc::channel::<(u64, Job)>();
        let (done_tx, rx) = std::sync::mpsc::channel::<Done>();
        let finished = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let done_with = finished.clone();
        // The window that renders with it (each window has its own worker).
        let viewport = ctx.viewport_id();
        // The request being rendered from scratch without a preview, and when it started, for the
        // watchdog; the newest request refined so far.
        let (watch, watched) = std::sync::mpsc::channel::<(u64, Job, std::time::Instant)>();
        let refined = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let refined_seen = refined.clone();
        let (preview_tx, preview_ctx) = (done_tx.clone(), ctx.clone());
        let watchdog = std::thread::Builder::new().name("figcraft-render-preview".into()).spawn(move || {
            let mut retained = Retained::default();
            let after = std::time::Duration::from_secs_f64(PREVIEW_AFTER_MS / 1000.0);
            while let Ok((mut seq, mut job, mut started)) = watched.recv() {
                // Wait out the threshold; a newer arming means that render is over.
                loop {
                    match watched.recv_timeout(after.saturating_sub(started.elapsed())) {
                        Ok(newer) => (seq, job, started) = newer,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                let still_rendering = || refined_seen.load(std::sync::atomic::Ordering::Acquire) < seq;
                if !still_rendering() {
                    continue;
                }
                let Some(d) = preview_with(&job, &mut retained) else { continue };
                if still_rendering() {
                    if preview_tx.send(answering(d, seq)).is_err() {
                        break;
                    }
                    preview_ctx.request_repaint_of(viewport);
                }
            }
        });
        if let Err(e) = watchdog {
            log::error!("render preview thread: {e}");
        }
        let spawned = std::thread::Builder::new().name("figcraft-render".into()).spawn(move || {
            let mut renders = Renders::default();
            let mut next = None;
            loop {
                let Some((mut seq, mut job)) = next.take().or_else(|| jobs.recv().ok()) else { break };
                // Skip to the newest queued job.
                while let Ok(newer) = jobs.try_recv() {
                    (seq, job) = newer;
                }
                // A page the GPU canvas draws goes there. Past a frame's worth a full frame (a
                // drag at a high zoom), the frame is drawn at fewer pixels so it keeps up with the
                // pointer; the UI asks for the sharp one when the interaction stops, and that job
                // (`refine`) is never drawn small.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let preview = renders.gpu_wants_preview(&job);
                    let mut drawn = false;
                    if preview && let Some(d) = renders.gpu(&job, true) {
                        if done_tx.send(d).is_err() {
                            break;
                        }
                        ctx.request_repaint_of(viewport);
                        drawn = true;
                    }
                    // A quick page (and the UI's refine) is drawn at the screen's resolution. A
                    // preview is left alone: the next job supersedes it while the drag goes on.
                    if !preview && let Some(d) = renders.gpu(&job, false) {
                        if done_tx.send(d).is_err() {
                            break;
                        }
                        ctx.request_repaint_of(viewport);
                        done_with.store(seq, std::sync::atomic::Ordering::Release);
                        continue;
                    }
                    if drawn {
                        done_with.store(seq, std::sync::atomic::Ordering::Release);
                        continue;
                    }
                }
                if let Some(d) = renders.preview(&job) {
                    if done_tx.send(answering(d, seq)).is_err() {
                        break;
                    }
                    ctx.request_repaint_of(viewport);
                    // A newer request makes refining this one moot: it gets its own preview.
                    if let Ok(newer) = jobs.try_recv() {
                        next = Some(newer);
                        continue;
                    }
                }
                if renders.wants_late_preview(&job) {
                    // Without the watchdog (it failed to start) there is just no preview.
                    let _ = watch.send((seq, job.clone(), std::time::Instant::now()));
                }
                let done = renders.refine(&job);
                refined.store(seq, std::sync::atomic::Ordering::Release);
                if let Some(d) = done
                    && done_tx.send(answering(d, seq)).is_err()
                {
                    break;
                }
                ctx.request_repaint_of(viewport);
                done_with.store(seq, std::sync::atomic::Ordering::Release);
            }
        });
        if let Err(e) = spawned {
            log::error!("render thread: {e}");
        }
        Self { tx, rx, submitted: 0, finished }
    }

    pub fn submit(&mut self, job: Job) {
        if self.tx.send((self.submitted + 1, job)).is_ok() {
            self.submitted += 1;
        }
    }

    /// Whether a submitted job is still rendering or hasn't woken the UI yet (a finished image
    /// may still wait for [`Self::poll`]).
    pub fn in_flight(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::Acquire) < self.submitted
    }

    /// How many jobs have been submitted (tests: a preview asks for its sharp frame once).
    #[cfg(test)]
    pub(crate) fn submitted_count(&self) -> u64 {
        self.submitted
    }

    /// The best of the renders finished since the last poll ([`better`]).
    pub fn poll(&mut self) -> Option<Done> {
        let mut best = None;
        while let Ok(d) = self.rx.try_recv() {
            best = Some(better(best, d));
        }
        best
    }
}

/// The browser's worker: renders inline, one image a frame: a preview first when one is due, the
/// refined image in the next frame unless a newer request replaced the job. Built everywhere so
/// tests run it.
pub struct Inline {
    ctx: egui::Context,
    pending: Option<Job>,
    /// Requests submitted so far.
    submitted: u64,
    previewed: bool,
    renders: Renders,
}

#[cfg(target_arch = "wasm32")]
pub type Worker = Inline;

impl Inline {
    pub fn new(ctx: egui::Context) -> Self {
        Self { ctx, pending: None, submitted: 0, previewed: false, renders: Renders::default() }
    }
    pub fn submit(&mut self, job: Job) {
        self.pending = Some(job);
        self.submitted += 1;
        self.previewed = false;
    }
    pub fn in_flight(&self) -> bool {
        self.pending.is_some()
    }
    pub fn poll(&mut self) -> Option<Done> {
        if !self.previewed {
            self.previewed = true;
            if let Some(d) = self.pending.as_ref().and_then(|job| self.renders.preview(job)) {
                self.ctx.request_repaint();
                return Some(answering(d, self.submitted));
            }
        }
        let job = self.pending.take()?;
        self.renders.refine(&job).map(|d| answering(d, self.submitted))
    }
}

/// Render synchronously from scratch (tests, screenshots).
pub fn render_now(job: &Job) -> Option<Done> {
    run(job, &job.view, &mut Retained::default(), false)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod gpu_job_tests {
    use super::*;

    fn job(doc: Document, page: NodeId) -> Job {
        Job {
            doc: Arc::new(doc),
            page,
            view: View { origin: (0.0, 0.0), scale: 1.0, width: 400, height: 300 },
            outline: false,
            outline_extras: figcraft_render::Outline::default(),
            outlined: Vec::new(),
            skip: Vec::new(),
            solo: None,
            proof: false,
            dots: None,
            uid: 1,
            revision: 0,
            zoom_preview: true,
            refine: false,
            at: Instant::now(),
        }
    }

    #[test]
    fn a_preview_comes_only_after_a_slow_full_frame_of_the_same_zoom() {
        let d = Document::new("preview");
        let p = d.first_page().unwrap();
        let j = job(d, p);
        let mut r = Renders::with_threshold(0.0);
        assert!(!r.gpu_wants_preview(&j), "no measure yet: the first frame is drawn full");
        r.full_ms = 30.0;
        assert!(!r.gpu_wants_preview(&j), "a slow frame of another zoom says nothing about this one");
        r.last = Some((j.uid, j.page, j.view.scale));
        assert!(r.gpu_wants_preview(&j), "a slow frame of this zoom is previewed while dragging");
        assert!(!r.gpu_wants_preview(&Job { refine: true, ..j.clone() }), "the sharp frame the UI asked for");
        assert!(!r.gpu_wants_preview(&Job { zoom_preview: false, ..j.clone() }), "pixel preview is sharp");
        r.full_ms = 5.0;
        assert!(!r.gpu_wants_preview(&j), "a quick frame is not previewed");
    }

    #[test]
    fn the_gpu_canvas_takes_plain_canvas_jobs() {
        let d = Document::new("gpu job");
        let p = d.first_page().unwrap();
        let mut j = job(d, p);
        assert!(gpu_job(&j));
        j.outline = true;
        assert!(!gpu_job(&j), "outline mode is the CPU renderer's");
        j.outline = false;
        j.outlined = vec![p];
        assert!(!gpu_job(&j), "outlines for the selection are the CPU renderer's");
        j.outlined.clear();
        j.skip = vec![p];
        assert!(!gpu_job(&j), "skipped layers are the CPU renderer's");
        j.skip.clear();
        j.solo = Some(p);
        assert!(!gpu_job(&j), "the focus view is the CPU renderer's");
        j.solo = None;
        j.proof = true;
        assert!(!gpu_job(&j), "soft proof is the CPU renderer's");
        j.proof = false;
        assert!(gpu_job(&j));
    }
}
