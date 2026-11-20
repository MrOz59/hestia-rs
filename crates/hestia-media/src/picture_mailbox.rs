//! Hands the latest decoded picture to a display without depending on its UI.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::DecodedFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureKind {
    Cpu,
    Native,
}

pub struct PictureUpdate {
    /// A display callback must be requested when there is none pending.
    pub schedule: bool,
    pub superseded: Option<PictureKind>,
}

pub enum DisplayUpdate {
    Cpu(DecodedFrame),
    /// Request a redraw. The native renderer takes the picture during rendering.
    RedrawNative,
}

#[derive(Default)]
struct Pending {
    latest: Option<DecodedFrame>,
    spare: Option<DecodedFrame>,
    scheduled: bool,
}

/// One stream's single pending picture, shared by the decoder and display.
/// Keeps one spare CPU buffer for reuse and releases superseded native images.
#[derive(Default)]
pub struct PictureMailbox {
    pending: Mutex<Pending>,
    closed: AtomicBool,
}

impl PictureMailbox {
    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Drops queued pictures and prevents new ones from entering this mailbox.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let (latest, spare) = {
            let mut pending = self.lock();
            (pending.latest.take(), pending.spare.take())
        };
        drop((latest, spare));
    }

    /// Takes the picture and returns a reusable frame to the decoder. The
    /// availability check runs under the lock, and must be fast and nonblocking.
    pub fn push(&self, frame: &mut DecodedFrame, native_available: impl FnOnce() -> bool) -> Option<PictureUpdate> {
        let mut pending = self.lock();
        if self.is_closed() || (frame.rgba().is_none() && !native_available()) {
            return None;
        }
        let (reusable, superseded, release) = match pending.latest.take() {
            Some(older) if older.rgba().is_some() => (older, Some(PictureKind::Cpu), None),
            Some(older) => match pending.spare.take() {
                Some(spare) => (spare, Some(PictureKind::Native), Some(older)),
                None => (older, Some(PictureKind::Native), None),
            },
            None => (pending.spare.take().unwrap_or_default(), None, None),
        };
        pending.latest = Some(std::mem::replace(frame, reusable));
        let schedule = !std::mem::replace(&mut pending.scheduled, true);
        drop(pending);
        frame.discard_native();
        drop(release);
        Some(PictureUpdate { schedule, superseded })
    }

    /// Runs when the display callback fires. A CPU frame is ready for upload.
    /// A native frame stays queued until the renderer draws it.
    pub fn next_for_display(&self, native_available: impl FnOnce() -> bool) -> Option<DisplayUpdate> {
        let mut pending = self.lock();
        pending.scheduled = false;
        if self.is_closed() {
            return None;
        }
        if pending.latest.as_ref()?.rgba().is_some() {
            return pending.latest.take().map(DisplayUpdate::Cpu);
        }
        if native_available() {
            return Some(DisplayUpdate::RedrawNative);
        }
        let discarded = pending.latest.take();
        drop(pending);
        drop(discarded);
        None
    }

    /// Returns a CPU buffer after upload so the decoder can reuse it.
    pub fn recycle_cpu(&self, frame: DecodedFrame) {
        let mut pending = self.lock();
        if !self.is_closed() && frame.rgba().is_some() {
            pending.spare = Some(frame);
        }
    }

    /// Takes a native frame at draw time, leaving any CPU frame queued.
    pub fn take_native(&self) -> Option<DecodedFrame> {
        let mut pending = self.lock();
        if self.is_closed() || pending.latest.as_ref()?.rgba().is_some() {
            return None;
        }
        pending.latest.take()
    }

    /// Drops a pending native frame when the renderer becomes unavailable.
    pub fn discard_native(&self) {
        drop(self.take_native());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_only_the_newest_picture_and_reuses_cpu_buffers() {
        let mailbox = PictureMailbox::default();
        let mut frame = DecodedFrame::default();
        let pixels = frame.reshape(2, 1).as_ptr();
        frame.frame_number = 1;
        assert!(mailbox.push(&mut frame, || false).unwrap().schedule);
        frame.reshape(2, 1);
        frame.frame_number = 2;
        let update = mailbox.push(&mut frame, || false).unwrap();
        assert!(!update.schedule, "the first callback is still pending");
        assert_eq!(update.superseded, Some(PictureKind::Cpu));
        assert_eq!(frame.rgba().unwrap().as_ptr(), pixels, "reuse the superseded buffer");

        assert!(mailbox.take_native().is_none());
        mailbox.discard_native();
        let Some(DisplayUpdate::Cpu(displayed)) = mailbox.next_for_display(|| false) else {
            panic!("missing CPU frame");
        };
        assert_eq!(displayed.frame_number, 2);
        let pixels = displayed.rgba().unwrap().as_ptr();
        mailbox.recycle_cpu(displayed);
        assert!(mailbox.push(&mut frame, || false).unwrap().schedule);
        assert_eq!(frame.rgba().unwrap().as_ptr(), pixels, "reuse the displayed buffer");
    }

    #[test]
    fn a_closed_mailbox_rejects_frames_and_ignores_an_already_scheduled_callback() {
        let previous = PictureMailbox::default();
        let next = PictureMailbox::default();
        let mut frame = DecodedFrame::default();
        frame.reshape(2, 1);
        assert!(previous.push(&mut frame, || false).unwrap().schedule);
        previous.close();
        assert!(previous.next_for_display(|| false).is_none());
        frame.frame_number = 3;
        assert!(previous.push(&mut frame, || false).is_none());
        assert!(next.push(&mut frame, || false).unwrap().schedule);
        let Some(DisplayUpdate::Cpu(displayed)) = next.next_for_display(|| false) else {
            panic!("missing CPU frame");
        };
        assert_eq!(displayed.frame_number, 3);
    }
}
