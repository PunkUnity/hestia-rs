//! A stream's pictures on their way to the window, without a toolkit.
//!
//! The decoder thread hands each picture to [`UiPresenter`], which keeps the
//! newest one in a mailbox and schedules the UI thread through the port:
//! RGBA pictures go to [`View::show_video_frame`]; native pictures (Android
//! hardware buffers, desktop YUV planes) only ask for a redraw, and the
//! window's GPU renderer — part of the shell, since it hooks into the
//! toolkit's rendering — takes the newest one with [`Presentation::take_gpu`]
//! while the window renders. [`GpuWindow`] tells that renderer which stream
//! is active.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hestia_media::native_picture::{GpuOutput, GpuRendererStatus};
use hestia_media::picture_mailbox::{DisplayUpdate, PictureKind, PictureMailbox};
use hestia_media::{DecodedFrame, FramePresenter, LatencyHistogram, RecoveryLatency};

use crate::ui::{UiHandle, View};

/// Pictures drawn by the window's own GPU renderer (Android hardware
/// buffers, desktop YUV planes); the renderer records them.
#[derive(Default)]
pub struct GpuVideoStats {
    pub redraw_requests: AtomicU64,
    pub drawn: AtomicU64,
    pub superseded: AtomicU64,
    pub ready_to_draw: LatencyHistogram,
    pub receive_to_draw: LatencyHistogram,
    pub recovery: RecoveryLatency,
    pub loss_to_draw: RecoveryLatency,
    #[cfg(target_os = "android")]
    pub(crate) pip: PipDraw,
}

/// Only the first GPU submission after a PiP transition takes the report lock.
#[cfg(any(target_os = "android", test))]
#[derive(Default)]
pub(crate) struct PipDraw {
    armed: std::sync::atomic::AtomicBool,
    pending: Mutex<Option<PipDrawEvent>>,
}

#[cfg(any(target_os = "android", test))]
struct PipDrawEvent {
    mode: &'static str,
    started: Instant,
    report: Arc<Mutex<hestia_core::report::StreamReport>>,
}

#[cfg(any(target_os = "android", test))]
impl PipDraw {
    pub(crate) fn transition(
        &self,
        mode: &'static str,
        await_draw: bool,
        report: &Arc<Mutex<hestia_core::report::StreamReport>>,
    ) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let started = Instant::now();
        report
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .event(format!("Android PiP {mode}"));
        *pending = await_draw.then(|| PipDrawEvent {
            mode,
            started,
            report: report.clone(),
        });
        self.armed.store(await_draw, Ordering::Release);
    }

    fn draw(&self, now: Instant) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        if pending.as_ref().is_some_and(|event| now >= event.started) {
            let event = pending.take().unwrap();
            self.armed.store(false, Ordering::Release);
            let ms = now.duration_since(event.started).as_secs_f64() * 1000.0;
            event.report.lock().unwrap_or_else(|p| p.into_inner()).event(format!(
                "first GPU draw submission after PiP {} ({ms:.2} ms)",
                event.mode
            ));
        }
    }
}

impl GpuVideoStats {
    pub fn record_draw(&self, requested_at: Option<Instant>, loss_at: Option<Instant>, now: Instant) {
        self.recovery.record(requested_at, now);
        self.loss_to_draw.record(loss_at, now);
        self.drawn.fetch_add(1, Ordering::Relaxed);
        #[cfg(target_os = "android")]
        self.pip.draw(now);
    }
}

/// One stream's link to the window and its presentation metrics.
#[derive(Default)]
pub struct Presentation {
    pictures: PictureMailbox,
    pub(crate) delivered: AtomicU64,
    pub(crate) superseded: AtomicU64,
    pub(crate) handoff: LatencyHistogram,
    pub(crate) receive_to_ui: LatencyHistogram,
    pub(crate) recovery: RecoveryLatency,
    pub gpu: GpuVideoStats,
    output: Option<Arc<GpuOutput>>,
    /// The stream this belongs to (see [`GpuWindow::is_active`]).
    pub generation: u64,
}

impl Presentation {
    pub(crate) fn new(generation: u64, output: Option<Arc<GpuOutput>>) -> Self {
        Self {
            generation,
            output,
            ..Self::default()
        }
    }

    pub(crate) fn close(&self) {
        self.pictures.close();
    }

    pub(crate) fn superseded(&self) -> u64 {
        self.superseded.load(Ordering::Relaxed) + self.gpu.superseded.load(Ordering::Relaxed)
    }

    /// Returns whether a callback needs to be scheduled on the UI thread.
    fn enqueue(&self, frame: &mut DecodedFrame) -> bool {
        let Some(update) = self.pictures.push(frame, || self.gpu_available()) else {
            return false;
        };
        match update.superseded {
            Some(PictureKind::Cpu) => {
                self.superseded.fetch_add(1, Ordering::Relaxed);
            }
            Some(PictureKind::Native) => {
                self.gpu.superseded.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
        update.schedule
    }

    /// The window's GPU renderer draws this stream's native pictures now.
    pub fn gpu_available(&self) -> bool {
        !self.pictures.is_closed() && self.output.as_ref().is_some_and(|output| output.available())
    }

    /// Drops a pending native picture (the renderer failed or went away).
    pub fn discard_gpu(&self) {
        self.pictures.discard_native();
    }

    /// The newest native picture, for the renderer to draw now.
    pub fn take_gpu(&self) -> Option<DecodedFrame> {
        let frame = self.pictures.take_native()?;
        self.gpu_available().then_some(frame)
    }
}

/// The window's native renderer knows only which session is active.
#[derive(Default)]
pub struct GpuWindow {
    /// Set by the renderer: ready, paused (no GL context) or failed.
    pub status: Arc<GpuRendererStatus>,
    active: Mutex<std::sync::Weak<Presentation>>,
    active_generation: AtomicU64,
}

impl GpuWindow {
    pub(crate) fn set_active(&self, session: &Arc<Presentation>) {
        let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        self.active_generation.store(session.generation, Ordering::Release);
        *active = Arc::downgrade(session);
    }

    /// The stream being shown, if any.
    pub fn active(&self) -> Option<Arc<Presentation>> {
        self.active.lock().unwrap_or_else(|p| p.into_inner()).upgrade()
    }

    pub fn is_active(&self, generation: u64) -> bool {
        self.active_generation.load(Ordering::Acquire) == generation
    }
}

pub(crate) struct UiPresenter {
    pub(crate) ui: Arc<dyn UiHandle>,
    pub(crate) presentation: Arc<Presentation>,
}

impl FramePresenter for UiPresenter {
    fn present(&mut self, frame: &mut DecodedFrame) {
        if matches!(frame.buffer, hestia_media::FrameBuffer::Direct) {
            // Already on its way to the display (Android direct video).
            let now = Instant::now();
            self.presentation.recovery.record(frame.idr_requested_at, now);
            self.presentation.handoff.record(now.duration_since(frame.decoded_at));
            self.presentation
                .receive_to_ui
                .record(now.duration_since(frame.received_at));
            if self.presentation.delivered.fetch_add(1, Ordering::Relaxed) == 0 {
                self.ui
                    .run(Box::new(|ui: &dyn View| ui.set_stream_status(String::new())));
            }
            frame.discard_native();
            return;
        }
        if !self.presentation.enqueue(frame) {
            return;
        }
        let presentation = self.presentation.clone();
        self.ui.run(Box::new(move |ui: &dyn View| {
            let frame = match presentation.pictures.next_for_display(|| presentation.gpu_available()) {
                Some(DisplayUpdate::Cpu(frame)) => frame,
                Some(DisplayUpdate::RedrawNative) => {
                    presentation.gpu.redraw_requests.fetch_add(1, Ordering::Relaxed);
                    ui.request_redraw();
                    return;
                }
                None => return,
            };
            if frame.rgba().is_none() || presentation.pictures.is_closed() {
                return;
            }
            ui.show_video_frame(&frame);
            let delivered_at = Instant::now();
            presentation.recovery.record(frame.idr_requested_at, delivered_at);
            presentation
                .handoff
                .record(delivered_at.duration_since(frame.decoded_at));
            presentation
                .receive_to_ui
                .record(delivered_at.duration_since(frame.received_at));
            if presentation.delivered.fetch_add(1, Ordering::Relaxed) == 0 {
                ui.set_stream_status(String::new());
            }
            presentation.pictures.recycle_cpu(frame);
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hestia_core::report::StreamReport;

    #[test]
    fn pip_only_counts_draws_after_the_latest_transition() {
        let report = Arc::new(Mutex::new(StreamReport::new("PiP", 0)));
        let pip = PipDraw::default();
        let earlier = Instant::now();
        pip.transition("entered", true, &report);
        pip.draw(earlier - std::time::Duration::from_millis(1));
        pip.transition("expanded", true, &report);
        pip.draw(Instant::now());
        pip.draw(Instant::now());
        pip.transition("entered", true, &report);
        pip.transition("dismissed", false, &report);
        pip.draw(Instant::now());
        let text = report.lock().unwrap().render_with_logs(&[]);
        assert_eq!(text.matches("Android PiP entered").count(), 2);
        assert!(text.contains("Android PiP dismissed"));
        assert!(text.contains("first GPU draw submission after PiP expanded"));
        assert_eq!(text.matches("first GPU draw submission after PiP").count(), 1);
    }
}
