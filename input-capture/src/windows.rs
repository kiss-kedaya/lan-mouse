use async_trait::async_trait;
use core::task::{Context, Poll};
use event_thread::EventThread;
use futures::Stream;
use input_event::Rect;
use std::future::Future;
use std::pin::Pin;

use std::task::ready;
use tokio::sync::mpsc::{Receiver, channel};

use super::{Capture, CaptureError, CaptureEvent, Position};

mod display_util;
mod event_thread;

pub struct WindowsInputCapture {
    event_rx: Receiver<(Position, CaptureEvent)>,
    /// absolute positions of the four virtual hook captures, they are
    /// answered by the window server instead of being queued in `event_rx`
    bind_rx: Receiver<(Position, CaptureEvent)>,
    event_thread: EventThread,
}

#[async_trait]
impl Capture for WindowsInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.event_thread.create(pos)?;
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.event_thread.destroy(pos)?;
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.event_thread.release_capture();
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    fn supports_absolute(&self) -> bool {
        self.event_thread.supports_absolute()
    }

    fn warp(
        &mut self,
        x: f64,
        y: f64,
    ) -> Pin<Box<dyn Future<Output = Result<(), CaptureError>> + '_>> {
        Box::pin(async move {
            event_thread::warp(x, y);
            Ok(())
        })
    }

    fn screenspace(&self) -> Option<Rect> {
        event_thread::screenspace()
    }
}

/// whether Windows reports at least one display for this session
pub(crate) fn displays_available() -> bool {
    event_thread::has_display()
}

impl WindowsInputCapture {
    pub(crate) fn new() -> Self {
        // high-dpi mice generate motion far faster than the event loop drains
        // it; a deep channel keeps every movement instead of dropping events
        // (the hooks only have a few milliseconds before Windows discards them)
        let (event_tx, event_rx) = channel(8192);
        let (bind_tx, bind_rx) = channel(8);
        let event_thread = EventThread::new(event_tx, bind_tx);
        Self {
            event_thread,
            event_rx,
            bind_rx,
        }
    }
}

impl Stream for WindowsInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // hand the virtual hook captures to the service first, it then
        // registers them on whatever side they are missing
        match self.bind_rx.poll_recv(cx) {
            Poll::Ready(Some(e)) => return Poll::Ready(Some(Ok(e))),
            Poll::Ready(None) | Poll::Pending => {}
        }
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}
