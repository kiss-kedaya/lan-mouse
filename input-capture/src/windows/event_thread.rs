use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ptr::addr_of_mut;

use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::core::{PCWSTR, w};

use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DispatchMessageW, EDD_GET_DEVICE_INTERFACE_NAME, GetMessageW,
    HOOKPROC, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PostThreadMessageW,
    RegisterClassW, SetCursorPos, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WINDOW_STYLE, WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
    WNDPROC,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    Rect,
    scancode::{self, Linux},
};

use super::{CaptureError, CaptureEvent, Position, display_util};

pub(crate) struct EventThread {
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    thread: Option<thread::JoinHandle<()>>,
    /// filled in by the thread once its message queue exists
    thread_id: Arc<(Condvar, Mutex<Option<u32>>)>,
}

impl EventThread {
    pub(crate) fn new(
        event_tx: Sender<(Position, CaptureEvent)>,
        bind_tx: Sender<(Position, CaptureEvent)>,
    ) -> Self {
        let request_buffer = Default::default();
        let thread_id = Arc::new((Condvar::new(), Mutex::new(None)));
        let thread = start(
            event_tx,
            Arc::clone(&request_buffer),
            bind_tx,
            Arc::clone(&thread_id),
        );
        Self {
            request_buffer,
            thread: Some(thread),
            thread_id,
        }
    }

    pub(crate) fn supports_absolute(&self) -> bool {
        true
    }

    pub(crate) fn release_capture(&self) {
        if let Err(e) = self.signal(RequestType::Release) {
            log::debug!("could not release the capture: {e}");
        }
    }

    pub(crate) fn create(&self, pos: Position) -> Result<(), CaptureError> {
        self.client_update(ClientUpdate::Create(pos))
    }

    pub(crate) fn destroy(&self, pos: Position) -> Result<(), CaptureError> {
        self.client_update(ClientUpdate::Destroy(pos))
    }

    fn exit(&self) {
        let _ = self.signal(RequestType::Exit);
    }

    fn client_update(&self, request: ClientUpdate) -> Result<(), CaptureError> {
        {
            let mut requests = self.request_buffer.lock().unwrap();
            requests.push(request);
        }
        self.signal(RequestType::ClientUpdate)
    }

    /// wakes the message loop; the capture thread only exists once it has a
    /// message queue, so a missing id means it could not start
    fn signal(&self, event_type: RequestType) -> Result<(), CaptureError> {
        let Some(id) = *self.thread_id.1.lock().unwrap() else {
            return Err(CaptureError::Other(
                "the Windows input capture thread is not running".to_owned(),
            ));
        };
        unsafe { PostThreadMessageW(id, WM_USER, WPARAM(event_type as usize), LPARAM(0)) }
            .map_err(|e| CaptureError::Other(format!("Windows message queue: {e}")))
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.exit();
        let _ = self.thread.take().expect("thread").join();
    }
}

enum RequestType {
    ClientUpdate = 0,
    Release = 1,
    Exit = 2,
}

enum ClientUpdate {
    Create(Position),
    Destroy(Position),
}

fn blocking_send_event(pos: Position, event: CaptureEvent) {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().blocking_send((pos, event)).unwrap())
}

fn try_send_event(
    pos: Position,
    event: CaptureEvent,
) -> Result<(), TrySendError<(Position, CaptureEvent)>> {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().try_send((pos, event)))
}

thread_local! {
    /// all configured clients
    static CLIENTS: RefCell<HashSet<Position>> = RefCell::new(HashSet::new());
    /// currently active client
    static ACTIVE_CLIENT: Cell<Option<Position>> = const { Cell::new(None) };
    /// input event channel
    static EVENT_TX: RefCell<Option<Sender<(Position, CaptureEvent)>>> = const { RefCell::new(None) };
    /// position of barrier entry
    static ENTRY_POINT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// previous mouse position
    static PREV_POS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    /// displays and generation counter
    static DISPLAYS: RefCell<(Vec<RECT>, i32)> = const { RefCell::new((Vec::new(), 0)) };
    /// whether we left through a top / bottom barrier and are waiting for
    /// the cursor to come back into screenspace
    static VERTICAL_ESCAPE: Cell<bool> = const { Cell::new(false) };
    /// absolute position the hook has to report on the next event
    static PENDING_WARP: Cell<Option<(f64, f64)>> = const { Cell::new(None) };
    /// how many motion events the hook dropped because the queue was full
    static DROPPED_MOTION: Cell<u64> = const { Cell::new(0) };
}

/// Refresh the cached display list if the layout changed and return the
/// bounding box of all displays.
fn refresh_screenspace() -> Option<Rect> {
    DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        display_util::bounding_rect(displays)
    })
}

pub(crate) fn screenspace() -> Option<Rect> {
    refresh_screenspace()
}

/// warp the (hidden) local cursor to a position and report it once to the
/// peer as absolute position. The position is clamped into screenspace.
pub(crate) fn warp(x: f64, y: f64) {
    let Some(space) = refresh_screenspace() else {
        return;
    };
    let max_x = space.right() - 1.0;
    let max_y = space.bottom() - 1.0;
    let x = x.clamp(space.x, max_x);
    let y = y.clamp(space.y, max_y);
    VERTICAL_ESCAPE.set(false);
    PENDING_WARP.set(Some((x, y)));
    unsafe {
        SetCursorPos(x.round() as i32, y.round() as i32).ok();
    }
}

fn get_msg() -> Option<MSG> {
    unsafe {
        let mut msg = std::mem::zeroed();
        let ret = GetMessageW(addr_of_mut!(msg), None, 0, 0);
        match ret.0 {
            0 => None,
            x if x > 0 => Some(msg),
            _ => panic!("error in GetMessageW"),
        }
    }
}

/// runs the message loop in its own thread. The thread id is published by
/// the thread itself right before the loop starts, between reading it here
/// and the first message it would simply be invalid
fn start(
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    bind_tx: Sender<(Position, CaptureEvent)>,
    thread_id: Arc<(Condvar, Mutex<Option<u32>>)>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || start_routine(thread_id, event_tx, request_buffer, bind_tx))
}

fn start_routine(
    ready: Arc<(Condvar, Mutex<Option<u32>>)>,
    event_tx: Sender<(Position, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    bind_tx: Sender<(Position, CaptureEvent)>,
) {
    EVENT_TX.replace(Some(event_tx));
    let mouse_proc: HOOKPROC = Some(mouse_proc);
    let kybrd_proc: HOOKPROC = Some(kybrd_proc);
    let window_proc: WNDPROC = Some(window_proc);

    /* register hooks */
    unsafe {
        let _ = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, None, 0).unwrap();
        let _ = SetWindowsHookExW(WH_KEYBOARD_LL, kybrd_proc, None, 0).unwrap();
    }

    /* one hook-only capture per barrier lets the service activate all four
    positions without sending any real input */
    if let Some(space) = refresh_screenspace() {
        let mid_x = space.x + space.width / 2.0;
        let mid_y = space.y + space.height / 2.0;
        for (pos, x, y) in [
            (Position::Left, space.x, mid_y),
            (Position::Right, space.right() - 1.0, mid_y),
            (Position::Top, mid_x, space.y),
            (Position::Bottom, mid_x, space.bottom() - 1.0),
        ] {
            warp(x, y);
            VERTICAL_ESCAPE.set(false);
            let _ = bind_tx.blocking_send((
                pos,
                CaptureEvent::Input(Event::Pointer(PointerEvent::Position { time: 0, x, y })),
            ));
        }
    }

    let instance = unsafe { GetModuleHandleW(None).unwrap() };
    let instance = instance.into();
    let window_class: WNDCLASSW = WNDCLASSW {
        lpfnWndProc: window_proc,
        hInstance: instance,
        lpszClassName: w!("lan-mouse-message-window-class"),
        ..Default::default()
    };

    static WINDOW_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
    if WINDOW_CLASS_REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        /* register window class if not yet done so */
        unsafe {
            let ret = RegisterClassW(&window_class);
            if ret == 0 {
                panic!("RegisterClassW");
            }
        }
    }

    /* window is used ro receive WM_DISPLAYCHANGE messages */
    unsafe {
        CreateWindowExW(
            Default::default(),
            w!("lan-mouse-message-window-class"),
            w!("lan-mouse-msg-window"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .expect("CreateWindowExW");
    }

    /* the queue exists from the first call into the message api on, publish
    the id now */
    {
        let (cnd, mtx) = &*ready;
        let mut ready = mtx.lock().unwrap();
        *ready = Some(unsafe { GetCurrentThreadId() });
        cnd.notify_one();
    }

    /* run message loop */
    while let Some(msg) = get_msg() {
        // mouse / keybrd proc do not actually return a message
        if msg.hwnd.0.is_null() {
            /* messages sent via PostThreadMessage */
            match msg.wParam.0 {
                x if x == RequestType::Exit as usize => break,
                x if x == RequestType::Release as usize => {
                    ACTIVE_CLIENT.take();
                }
                x if x == RequestType::ClientUpdate as usize => {
                    let requests = {
                        let mut res = vec![];
                        let mut requests = request_buffer.lock().unwrap();
                        for request in requests.drain(..) {
                            res.push(request);
                        }
                        res
                    };

                    for request in requests {
                        update_clients(request)
                    }
                }
                _ => {}
            }
        } else {
            /* other messages for window_procs */
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn check_client_activation(wparam: WPARAM, lparam: LPARAM) -> bool {
    if wparam.0 != WM_MOUSEMOVE as usize {
        return ACTIVE_CLIENT.get().is_some();
    }
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    let curr_pos = (mouse_low_level.pt.x, mouse_low_level.pt.y);

    // the cursor was warped programmatically (crossing devices) or the OS is
    // moving it back into screenspace on its own while an outgoing capture is
    // active: keep the pointer position anchored where we put it and don't
    // treat the jump as user movement.
    if PENDING_WARP.get().is_some() || VERTICAL_ESCAPE.get() {
        PREV_POS.replace(Some(curr_pos));
        return ACTIVE_CLIENT.get().is_some();
    }

    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));

    /* next event is the first actual event */
    let ret = ACTIVE_CLIENT.get().is_some();

    /* client already active, no need to check */
    if ACTIVE_CLIENT.get().is_some() {
        return ret;
    }

    /* check if a client was activated */
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        display_util::entered_barrier(prev_pos, curr_pos, displays)
    });

    let Some(pos) = entered else {
        return ret;
    };

    /* check if a client is registered for the barrier */
    if !CLIENTS.with_borrow(|clients| clients.contains(&pos)) {
        return ret;
    }

    /* update active client and entry point */
    ACTIVE_CLIENT.replace(Some(pos));
    let entry_point = DISPLAYS.with_borrow(|(displays, _)| {
        display_util::clamp_to_display_bounds(displays, prev_pos, curr_pos)
    });
    ENTRY_POINT.replace(entry_point);
    // leaving through a horizontal barrier we anchor the pointer at the
    // entry height and the peer decides where it lands. leaving through a
    // vertical barrier the desktop is merely one screen wide: let the OS
    // move the pointer back into this screen (it keeps x while y is locked).
    VERTICAL_ESCAPE.set(matches!(pos, Position::Top | Position::Bottom));

    /* notify main thread */
    log::debug!("ENTERED @ {prev_pos:?} -> {curr_pos:?}");
    let active = ACTIVE_CLIENT.get().expect("active client");
    blocking_send_event(
        active,
        CaptureEvent::Begin(Some((entry_point.0 as f64, entry_point.1 as f64))),
    );

    ret
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let active = check_client_activation(wparam, lparam);

    /* no client was active */
    if !active {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* get active client if any */
    let Some(pos) = ACTIVE_CLIENT.get() else {
        return LRESULT(1);
    };

    /* replay a pending absolute warp (emitted by warp()) */
    let warped = PENDING_WARP.take().is_some();

    /* convert to lan-mouse event */
    let Some(pointer_event) = to_mouse_event(wparam, lparam) else {
        return LRESULT(1);
    };
    // the absolute position of a warp is emitted from the message loop,
    // swallow the motion event the warp itself produces
    if warped {
        return LRESULT(1);
    }

    /* notify mainthread. a full queue only happens under extreme load; it
    is counted instead of logged per event (the hook has a few ms budget) */
    if let Err(_e) = try_send_event(pos, CaptureEvent::Input(Event::Pointer(pointer_event))) {
        DROPPED_MOTION.with(|c| {
            let n = c.get() + 1;
            c.set(n);
            if n.is_power_of_two() {
                log::debug!("capture queue full, dropped {n} motion events so far");
            }
        });
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn kybrd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    /* get active client if any */
    let Some(client) = ACTIVE_CLIENT.get() else {
        return CallNextHookEx(None, ncode, wparam, lparam);
    };

    /* convert to key event */
    let Some(key_event) = to_key_event(wparam, lparam) else {
        return LRESULT(1);
    };

    if let Err(e) = try_send_event(client, CaptureEvent::Input(Event::Keyboard(key_event))) {
        log::warn!("e: {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn window_proc(
    _hwnd: HWND,
    uint: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    if uint == WM_DISPLAYCHANGE {
        log::debug!("display resolution changed");
        DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
    }
    LRESULT(1)
}

static DISPLAY_RESOLUTION_GENERATION: AtomicI32 = AtomicI32::new(1);

fn update_display_regions(displays: &mut Vec<RECT>, generation: &mut i32) {
    let global_generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
    if *generation != global_generation {
        enumerate_displays(displays);
        log::debug!("displays: {displays:?}");
        *generation = global_generation;
    }
}

fn enumerate_displays(display_rects: &mut Vec<RECT>) {
    display_rects.clear();
    unsafe {
        let mut devices = vec![];
        for i in 0.. {
            let mut device: DISPLAY_DEVICEW = std::mem::zeroed();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let ret = EnumDisplayDevicesW(None, i, &mut device, EDD_GET_DEVICE_INTERFACE_NAME);
            if ret == FALSE {
                break;
            }
            if device
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                devices.push(device.DeviceName);
            }
        }
        for device in devices {
            let mut dev_mode: DEVMODEW = std::mem::zeroed();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ret = EnumDisplaySettingsW(
                PCWSTR::from_raw(&device as *const _),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            );
            if ret == FALSE {
                log::warn!("no display mode");
            }

            let pos = dev_mode.Anonymous1.Anonymous2.dmPosition;
            let (x, y) = (pos.x, pos.y);
            let (width, height) = (dev_mode.dmPelsWidth, dev_mode.dmPelsHeight);

            display_rects.push(RECT {
                left: x,
                right: x + width as i32,
                top: y,
                bottom: y + height as i32,
            });
        }
    }
}

fn update_clients(request: ClientUpdate) {
    match request {
        ClientUpdate::Create(pos) => {
            CLIENTS.with_borrow_mut(|clients| clients.insert(pos));
        }
        ClientUpdate::Destroy(pos) => {
            if let Some(active_pos) = ACTIVE_CLIENT.get() {
                if pos == active_pos {
                    let _ = ACTIVE_CLIENT.take();
                }
            }
            CLIENTS.with_borrow_mut(|clients| clients.remove(&pos));
        }
    }
}

fn to_key_event(wparam: WPARAM, lparam: LPARAM) -> Option<KeyboardEvent> {
    let kybrdllhookstruct: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let mut scan_code = kybrdllhookstruct.scanCode;
    log::trace!("scan_code: {scan_code}");
    if kybrdllhookstruct.flags.contains(LLKHF_EXTENDED) {
        scan_code |= 0xE000;
    }
    let Ok(win_scan_code) = scancode::Windows::try_from(scan_code) else {
        log::warn!("failed to translate to windows scancode: {scan_code}");
        return None;
    };
    log::trace!("windows_scan: {win_scan_code:?}");
    let Ok(linux_scan_code): Result<Linux, ()> = win_scan_code.try_into() else {
        log::warn!("failed to translate into linux scancode: {win_scan_code:?}");
        return None;
    };
    log::trace!("windows_scan: {linux_scan_code:?}");
    let scan_code = linux_scan_code as u32;
    match wparam {
        WPARAM(p) if p == WM_KEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_KEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        WPARAM(p) if p == WM_SYSKEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_SYSKEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        _ => None,
    }
}

fn to_mouse_event(wparam: WPARAM, lparam: LPARAM) -> Option<PointerEvent> {
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    match wparam {
        WPARAM(p) if p == WM_LBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 1,
        }),
        WPARAM(p) if p == WM_MBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 1,
        }),
        WPARAM(p) if p == WM_RBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 1,
        }),
        WPARAM(p) if p == WM_LBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 0,
        }),
        WPARAM(p) if p == WM_RBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MOUSEMOVE as usize => {
            let (x, y) = (mouse_low_level.pt.x, mouse_low_level.pt.y);
            let (ex, ey) = ENTRY_POINT.get();
            // Entry and target devices usually have different sizes, so the
            // OS keeps *moving* the cursor (until it hits the edge) while the
            // pointer is on the peer. Instead of forwarding these raw jumps,
            // clamp the local cursor to the entry row / column and report
            // its position relative to the entry point. The peer turns that
            // into a true absolute position using the exchanged geometry.
            let space = refresh_screenspace()?;
            let mut cx = x;
            let mut cy = y;
            if !space.contains(x as f64, y as f64) || VERTICAL_ESCAPE.get() {
                // the OS is moving the pointer back into screenspace after a
                // vertical barrier: freeze it where it is and anchor on the
                // current column instead of the original entry column
                let space = refresh_screenspace()?;
                cx = x.clamp(space.x as i32, space.right() as i32 - 1);
                cy = y.clamp(space.y as i32, space.bottom() as i32 - 1);
                ENTRY_POINT.replace((cx, cy));
                VERTICAL_ESCAPE.set(false);
                let _ = unsafe { SetCursorPos(cx, cy) };
            } else if cy != ey {
                // different screen heights: keep the cursor on the entry row
                cy = ey;
                let _ = unsafe { SetCursorPos(cx, cy) };
            }
            Some(PointerEvent::Position {
                time: 0,
                x: (cx - ex) as f64,
                y: (cy - ey) as f64,
            })
        }
        WPARAM(p) if p == WM_MOUSEWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: -(mouse_low_level.mouseData as i32 >> 16),
        }),
        WPARAM(p) if p == WM_XBUTTONDOWN as usize || p == WM_XBUTTONUP as usize => {
            let hb = mouse_low_level.mouseData >> 16;
            let button = match hb {
                1 => BTN_BACK,
                2 => BTN_FORWARD,
                _ => {
                    log::warn!("unknown mouse button");
                    return None;
                }
            };
            Some(PointerEvent::Button {
                time: 0,
                button,
                state: if p == WM_XBUTTONDOWN as usize { 1 } else { 0 },
            })
        }
        WPARAM(p) if p == WM_MOUSEHWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 1, // Horizontal
            value: mouse_low_level.mouseData as i32 >> 16,
        }),
        w => {
            log::warn!("unknown mouse event: {w:?}");
            None
        }
    }
}
