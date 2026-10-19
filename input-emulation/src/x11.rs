use async_trait::async_trait;
use std::ptr;
use x11::{
    xlib::{
        self, Display, ScreenOfDisplay, XCloseDisplay, XDisplayHeight, XDisplayWidth, XScreenCount,
    },
    xtest,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    Rect,
};

use crate::error::EmulationError;

use super::{Emulation, EmulationHandle, error::X11EmulationCreationError};

pub(crate) struct X11Emulation {
    display: *mut xlib::Display,
}

unsafe impl Send for X11Emulation {}

impl X11Emulation {
    pub(crate) fn new() -> Result<Self, X11EmulationCreationError> {
        let display = unsafe {
            match xlib::XOpenDisplay(ptr::null()) {
                d if std::ptr::eq(d, ptr::null_mut::<xlib::Display>()) => {
                    Err(X11EmulationCreationError::OpenDisplay)
                }
                display => Ok(display),
            }
        }?;
        Ok(Self { display })
    }

    fn warp_pointer(&self, x: i32, y: i32) {
        unsafe {
            xtest::XTestFakeMotionEvent(self.display, -1, x, y, 0);
        }
    }

    fn relative_motion(&self, dx: i32, dy: i32) {
        unsafe {
            xtest::XTestFakeRelativeMotionEvent(self.display, dx, dy, 0, 0);
        }
    }

    fn emulate_mouse_button(&self, button: u32, state: u32) {
        unsafe {
            let x11_button = match button {
                BTN_RIGHT => 3,
                BTN_MIDDLE => 2,
                BTN_BACK => 8,
                BTN_FORWARD => 9,
                BTN_LEFT => 1,
                _ => 1,
            };
            xtest::XTestFakeButtonEvent(self.display, x11_button, state as i32, 0);
        };
    }

    const SCROLL_UP: u32 = 4;
    const SCROLL_DOWN: u32 = 5;
    const SCROLL_LEFT: u32 = 6;
    const SCROLL_RIGHT: u32 = 7;

    fn emulate_scroll(&self, axis: u8, value: f64) {
        let direction = match axis {
            1 => {
                if value < 0.0 {
                    Self::SCROLL_LEFT
                } else {
                    Self::SCROLL_RIGHT
                }
            }
            _ => {
                if value < 0.0 {
                    Self::SCROLL_UP
                } else {
                    Self::SCROLL_DOWN
                }
            }
        };

        unsafe {
            xtest::XTestFakeButtonEvent(self.display, direction, 1, 0);
            xtest::XTestFakeButtonEvent(self.display, direction, 0, 0);
        }
    }

    #[allow(dead_code)]
    fn emulate_key(&self, key: u32, state: u8) {
        let key = key + 8; // xorg keycodes are shifted by 8
        unsafe {
            xtest::XTestFakeKeyEvent(self.display, key, state as i32, 0);
        }
    }
}

impl Drop for X11Emulation {
    fn drop(&mut self) {
        unsafe {
            XCloseDisplay(self.display);
        }
    }
}

#[async_trait]
impl Emulation for X11Emulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    self.relative_motion(dx as i32, dy as i32);
                }
                PointerEvent::Position { time: _, x, y } => {
                    self.warp_pointer(x as i32, y as i32);
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    self.emulate_mouse_button(button, state);
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    self.emulate_scroll(axis, value);
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    self.emulate_scroll(axis, value as f64);
                }
            },
            Event::Keyboard(KeyboardEvent::Key {
                time: _,
                key,
                state,
            }) => {
                self.emulate_key(key, state);
            }
            _ => {}
        }
        unsafe {
            xlib::XFlush(self.display);
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn destroy(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn terminate(&mut self) {
        /* nothing to do */
    }

    fn supports_absolute(&self) -> bool {
        true
    }

    async fn warp(
        &mut self,
        x: f64,
        y: f64,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        self.warp_pointer(x as i32, y as i32);
        Ok(())
    }

    fn screenspace(&self) -> Option<Rect> {
        // bounding box of all X screens (0 = default screen)
        unsafe {
            let display: *mut Display = self.display;
            let n = XScreenCount(display);
            if n <= 0 {
                return None;
            }
            // the default screen is the one of index zero
            let screen = ScreenOfDisplay(display, 0);
            Some(Rect::new(
                0.0,
                0.0,
                XDisplayWidth(display, screen) as f64,
                XDisplayHeight(display, screen) as f64,
            ))
        }
    }
}
