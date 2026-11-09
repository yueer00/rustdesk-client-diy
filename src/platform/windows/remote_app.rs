use hbb_common::{anyhow::anyhow, bail, ResultType};
use image::{codecs::jpeg::JpegEncoder, ColorType};
use scrap::{window_capture::WindowCapturer, Frame, TraitCapturer, TraitPixelBuffer};
use std::{
    collections::{hash_map::DefaultHasher, HashSet},
    hash::{Hash, Hasher},
    io, mem, ptr,
    time::Duration,
};
use winapi::{
    shared::{
        minwindef::{BOOL, LPARAM, TRUE},
        windef::{HWND, POINT, RECT},
    },
    um::{processthreadsapi::GetCurrentThreadId, winuser::*},
};

const CAPTURE_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_PIXELS: u64 = 16_000_000;
const MAX_DIMENSION: u32 = 8192;

#[link(name = "dwmapi")]
extern "system" {
    fn DwmGetWindowAttribute(
        hwnd: HWND,
        attribute: u32,
        value: *mut std::ffi::c_void,
        size: u32,
    ) -> i32;
}

pub fn supported() -> bool {
    scrap::window_capture::is_supported()
}

pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

pub struct AppFrame {
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
    pub title: String,
    pub layout_id: u64,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct Bounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
impl Bounds {
    fn width(self) -> u32 {
        (self.right as i64 - self.left as i64) as u32
    }
    fn height(self) -> u32 {
        (self.bottom as i64 - self.top as i64) as u32
    }
    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

fn hwnd(id: usize) -> HWND {
    id as HWND
}
fn title(window: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(window);
        if len <= 0 {
            return String::new();
        }
        let mut text = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(window, text.as_mut_ptr(), text.len() as i32);
        String::from_utf16_lossy(&text[..n.max(0) as usize])
    }
}
fn bounds(window: HWND) -> ResultType<Bounds> {
    unsafe {
        let mut rect: RECT = mem::zeroed();
        if IsWindow(window) == 0 || IsWindowVisible(window) == 0 || IsIconic(window) != 0 {
            bail!("Remote application window is closed, hidden or minimized");
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(
            window,
            14,
            &mut cloaked as *mut _ as *mut _,
            mem::size_of_val(&cloaked) as u32,
        ) >= 0
            && cloaked != 0
        {
            bail!("Remote application window is cloaked");
        }
        if GetWindowRect(window, &mut rect) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut visible_rect: RECT = mem::zeroed();
        if DwmGetWindowAttribute(
            window,
            9,
            &mut visible_rect as *mut _ as *mut _,
            mem::size_of_val(&visible_rect) as u32,
        ) >= 0
        {
            rect = visible_rect;
        }
        if rect.right <= rect.left || rect.bottom <= rect.top {
            bail!("Remote application window has empty bounds");
        }
        Ok(Bounds {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        })
    }
}
unsafe extern "system" fn collect_windows(window: HWND, param: LPARAM) -> BOOL {
    let windows = &mut *(param as *mut Vec<usize>);
    windows.push(window as usize);
    TRUE
}
fn all_windows() -> ResultType<Vec<usize>> {
    let mut windows = Vec::new();
    if unsafe { EnumWindows(Some(collect_windows), &mut windows as *mut _ as LPARAM) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(windows)
}
fn belongs_to(window: HWND, root: HWND) -> bool {
    unsafe {
        if window.is_null() {
            return false;
        }
        let mut current = GetAncestor(window, GA_ROOT);
        while !current.is_null() {
            if current == root {
                return true;
            }
            current = GetWindow(current, GW_OWNER);
        }
        false
    }
}

pub fn list_windows() -> ResultType<Vec<WindowInfo>> {
    let mut result = Vec::new();
    for id in all_windows()? {
        let window = hwnd(id);
        let Ok(rect) = bounds(window) else { continue };
        let name = title(window);
        let mut process_id = 0;
        unsafe {
            GetWindowThreadProcessId(window, &mut process_id);
        }
        if name.is_empty()
            || process_id == std::process::id()
            || unsafe {
                !GetWindow(window, GW_OWNER).is_null()
                    || GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0
            }
        {
            continue;
        }
        result.push(WindowInfo {
            id: id as u64,
            title: name,
            width: rect.width(),
            height: rect.height(),
        });
    }
    Ok(result)
}

struct CapturedWindow {
    id: usize,
    capture: WindowCapturer,
    pixels: Vec<u8>,
    width: usize,
    height: usize,
}

pub struct App {
    root: usize,
    windows: Vec<CapturedWindow>,
    layout: Vec<(usize, Bounds)>,
    canvas: Option<Bounds>,
    keys: HashSet<u16>,
    buttons: HashSet<u32>,
}
impl App {
    pub fn new(window_id: u64) -> ResultType<Self> {
        let id = usize::try_from(window_id).map_err(|_| anyhow!("Invalid window ID"))?;
        if !list_windows()?.iter().any(|w| w.id == window_id) {
            bail!("Window is unavailable or unsupported");
        }
        let capture = WindowCapturer::new(id, CAPTURE_TIMEOUT)?;
        Ok(Self {
            root: id,
            windows: vec![CapturedWindow {
                id,
                capture,
                pixels: Vec::new(),
                width: 0,
                height: 0,
            }],
            layout: Vec::new(),
            canvas: None,
            keys: HashSet::new(),
            buttons: HashSet::new(),
        })
    }

    pub fn capture(&mut self) -> ResultType<Option<AppFrame>> {
        bounds(hwnd(self.root))?;
        let mut layout = Vec::new();
        for id in all_windows()? {
            if belongs_to(hwnd(id), hwnd(self.root)) {
                if let Ok(rect) = bounds(hwnd(id)) {
                    layout.push((id, rect));
                }
            }
        }
        if !layout.iter().any(|(id, _)| *id == self.root) {
            bail!("Remote application window is unavailable");
        }
        // EnumWindows returns topmost first; compose the bottom window first.
        layout.reverse();
        self.windows
            .retain(|w| layout.iter().any(|(id, _)| *id == w.id));
        for (id, _) in &layout {
            if !self.windows.iter().any(|w| w.id == *id) {
                let capture = WindowCapturer::new(*id, CAPTURE_TIMEOUT)?;
                self.windows.push(CapturedWindow {
                    id: *id,
                    capture,
                    pixels: Vec::new(),
                    width: 0,
                    height: 0,
                });
            }
        }
        let mut changed = self.layout != layout;
        for window in &mut self.windows {
            match window.capture.frame(Duration::ZERO) {
                Ok(Frame::PixelBuffer(buffer)) => {
                    window.width = buffer.width();
                    window.height = buffer.height();
                    window.pixels.clear();
                    window.pixels.extend_from_slice(buffer.data());
                    changed = true;
                }
                Ok(_) => bail!("Unexpected window capture format"),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
            if window.pixels.is_empty() {
                return Ok(None);
            }
        }
        if !changed {
            return Ok(None);
        }
        let mut area = layout[0].1;
        for (_, r) in &layout {
            area.left = area.left.min(r.left);
            area.top = area.top.min(r.top);
            area.right = area.right.max(r.right);
            area.bottom = area.bottom.max(r.bottom);
        }
        let (width, height) = (area.width(), area.height());
        if width > MAX_DIMENSION
            || height > MAX_DIMENSION
            || u64::from(width) * u64::from(height) > MAX_PIXELS
        {
            bail!("Remote application canvas is too large");
        }
        let mut rgb = vec![0u8; width as usize * height as usize * 3];
        for (id, rect) in &layout {
            let window = self
                .windows
                .iter()
                .find(|w| w.id == *id)
                .ok_or_else(|| anyhow!("Window capture missing"))?;
            let rw = rect.width() as usize;
            let rh = rect.height() as usize;
            if window.width == 0
                || window.height == 0
                || window.pixels.len() != window.width * window.height * 4
            {
                bail!("Invalid captured window frame");
            }
            let offset_x = (rect.left as i64 - area.left as i64) as usize;
            let offset_y = (rect.top as i64 - area.top as i64) as usize;
            for y in 0..rh {
                let sy = y * window.height / rh;
                for x in 0..rw {
                    let sx = x * window.width / rw;
                    let src = (sy * window.width + sx) * 4;
                    let dst = ((offset_y + y) * width as usize + offset_x + x) * 3;
                    rgb[dst] = window.pixels[src + 2];
                    rgb[dst + 1] = window.pixels[src + 1];
                    rgb[dst + 2] = window.pixels[src];
                }
            }
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 75).encode(
            &rgb,
            width,
            height,
            ColorType::Rgb8,
        )?;
        let mut hasher = DefaultHasher::new();
        layout.hash(&mut hasher);
        let layout_id = hasher.finish().max(1);
        self.layout = layout;
        self.canvas = Some(area);
        Ok(Some(AppFrame {
            width,
            height,
            jpeg,
            title: title(hwnd(self.root)),
            layout_id,
        }))
    }

    fn activate(&self, target: HWND) -> ResultType<()> {
        if !belongs_to(target, hwnd(self.root)) {
            bail!("Target is outside the remote application");
        }
        unsafe {
            if GetForegroundWindow() != target {
                let foreground = GetForegroundWindow();
                let foreground_thread = GetWindowThreadProcessId(foreground, ptr::null_mut());
                let current_thread = GetCurrentThreadId();
                let target_thread = GetWindowThreadProcessId(target, ptr::null_mut());
                let mut message: MSG = mem::zeroed();
                PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_NOREMOVE);
                let attached = foreground_thread != 0
                    && foreground_thread != current_thread
                    && AttachThreadInput(current_thread, foreground_thread, 1) != 0;
                let attach_error =
                    if !attached && foreground_thread != 0 && foreground_thread != current_thread {
                        Some(io::Error::last_os_error())
                    } else {
                        None
                    };
                let target_attached = target_thread != 0
                    && target_thread != current_thread
                    && target_thread != foreground_thread
                    && AttachThreadInput(current_thread, target_thread, 1) != 0;
                let target_attach_error = if !target_attached
                    && target_thread != 0
                    && target_thread != current_thread
                    && target_thread != foreground_thread
                {
                    Some(io::Error::last_os_error())
                } else {
                    None
                };
                SetForegroundWindow(target);
                let activated = GetForegroundWindow() == target;
                let target_detach_error = if target_attached
                    && AttachThreadInput(current_thread, target_thread, 0) == 0
                {
                    Some(io::Error::last_os_error())
                } else {
                    None
                };
                if attached && AttachThreadInput(current_thread, foreground_thread, 0) == 0 {
                    bail!(
                        "Cannot detach foreground input queues: {}",
                        io::Error::last_os_error()
                    );
                }
                if let Some(e) = target_detach_error {
                    bail!("Cannot detach target input queues: {e}");
                }
                if !activated || GetForegroundWindow() != target {
                    bail!("Cannot activate remote application (focus or privilege denied; attach errors: {attach_error:?}, {target_attach_error:?})");
                }
            }
        }
        Ok(())
    }
    fn check_layout(&self) -> ResultType<Bounds> {
        let area = self
            .canvas
            .ok_or_else(|| anyhow!("No remote application frame is available"))?;
        bounds(hwnd(self.root))?;
        let current: Vec<_> = all_windows()?
            .into_iter()
            .filter(|id| belongs_to(hwnd(*id), hwnd(self.root)))
            .filter_map(|id| bounds(hwnd(id)).ok().map(|r| (id, r)))
            .rev()
            .collect();
        if current != self.layout {
            bail!("Remote application layout changed; wait for the next frame");
        }
        Ok(area)
    }
    pub fn pointer(
        &mut self,
        kind: u32,
        button: u32,
        x: i32,
        y: i32,
        scroll_x: i32,
        scroll_y: i32,
    ) -> ResultType<()> {
        let area = self.check_layout()?;
        let px = i64::from(area.left) + i64::from(x);
        let py = i64::from(area.top) + i64::from(y);
        if x < 0 || y < 0 || px >= i64::from(area.right) || py >= i64::from(area.bottom) {
            bail!("Pointer is outside the application frame");
        }
        let (px, py) = (px as i32, py as i32);
        let target = self
            .layout
            .iter()
            .rev()
            .find(|(_, r)| r.contains(px, py))
            .map(|(id, _)| hwnd(*id))
            .ok_or_else(|| anyhow!("Pointer is outside application windows"))?;
        if kind == 1 || kind == 3 {
            self.activate(target)?;
        }
        self.check_layout()?;
        let hit = unsafe { WindowFromPoint(POINT { x: px, y: py }) };
        if unsafe { GetAncestor(hit, GA_ROOT) } != target {
            bail!("Pointer is obscured by another window");
        }
        if unsafe { SetCursorPos(px, py) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        match kind {
            0 => {}
            1 | 2 => {
                let (flags, data) = button_flags(button, kind == 1)?;
                send_mouse(flags, data)?;
                if kind == 1 {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
            }
            3 => {
                if scroll_y != 0 {
                    send_mouse(MOUSEEVENTF_WHEEL, scroll_y as u32)?;
                }
                if scroll_x != 0 {
                    send_mouse(MOUSEEVENTF_HWHEEL, scroll_x as u32)?;
                }
            }
            _ => bail!("Unsupported pointer event"),
        }
        Ok(())
    }
    pub fn key(&mut self, virtual_key: u32, down: bool, text: &str) -> ResultType<()> {
        bounds(hwnd(self.root))?;
        let target = unsafe { GetLastActivePopup(hwnd(self.root)) };
        let target = if !target.is_null() && unsafe { IsWindowVisible(target) != 0 } {
            target
        } else {
            hwnd(self.root)
        };
        self.activate(target)?;
        if !text.is_empty() {
            if down {
                for unit in text.encode_utf16() {
                    send_key(0, unit, KEYEVENTF_UNICODE)?;
                    send_key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)?;
                }
            }
        } else {
            if virtual_key == 0 || virtual_key > 255 {
                bail!("Invalid virtual key");
            }
            let vk = virtual_key as u16;
            let extended = matches!(
                vk as i32,
                VK_RMENU
                    | VK_RCONTROL
                    | VK_INSERT
                    | VK_DELETE
                    | VK_HOME
                    | VK_END
                    | VK_PRIOR
                    | VK_NEXT
                    | VK_LEFT
                    | VK_RIGHT
                    | VK_UP
                    | VK_DOWN
                    | VK_DIVIDE
                    | VK_NUMLOCK
                    | VK_SNAPSHOT
            );
            send_key(
                vk,
                0,
                (if down { 0 } else { KEYEVENTF_KEYUP })
                    | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 },
            )?;
            if down {
                self.keys.insert(vk);
            } else {
                self.keys.remove(&vk);
            }
        }
        Ok(())
    }
    pub fn resize(&mut self, width: u32, height: u32) -> ResultType<()> {
        if width == 0
            || height == 0
            || width > MAX_DIMENSION
            || height > MAX_DIMENSION
            || u64::from(width) * u64::from(height) > MAX_PIXELS
        {
            bail!("Invalid application window size");
        }
        let visible = bounds(hwnd(self.root))?;
        let mut outer: RECT = unsafe { mem::zeroed() };
        if unsafe { GetWindowRect(hwnd(self.root), &mut outer) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let outer_width = i64::from(width) + i64::from(outer.right)
            - i64::from(outer.left)
            - i64::from(visible.width());
        let outer_height = i64::from(height) + i64::from(outer.bottom)
            - i64::from(outer.top)
            - i64::from(visible.height());
        if outer_width <= 0
            || outer_height <= 0
            || outer_width > i64::from(i32::MAX)
            || outer_height > i64::from(i32::MAX)
        {
            bail!("Invalid application window size");
        }
        if unsafe {
            SetWindowPos(
                hwnd(self.root),
                ptr::null_mut(),
                0,
                0,
                outer_width as i32,
                outer_height as i32,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
    pub fn release(&mut self) -> ResultType<()> {
        let mut error = None;
        for vk in self.keys.iter().copied().collect::<Vec<_>>() {
            let extended = matches!(
                vk as i32,
                VK_RMENU
                    | VK_RCONTROL
                    | VK_INSERT
                    | VK_DELETE
                    | VK_HOME
                    | VK_END
                    | VK_PRIOR
                    | VK_NEXT
                    | VK_LEFT
                    | VK_RIGHT
                    | VK_UP
                    | VK_DOWN
                    | VK_DIVIDE
                    | VK_NUMLOCK
                    | VK_SNAPSHOT
            );
            match send_key(
                vk,
                0,
                KEYEVENTF_KEYUP | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 },
            ) {
                Ok(()) => {
                    self.keys.remove(&vk);
                }
                Err(e) => error = Some(e),
            }
        }
        for button in self.buttons.iter().copied().collect::<Vec<_>>() {
            match button_flags(button, false).and_then(|(flags, data)| send_mouse(flags, data)) {
                Ok(()) => {
                    self.buttons.remove(&button);
                }
                Err(e) => error = Some(e),
            }
        }
        if let Some(e) = error {
            return Err(e);
        }
        Ok(())
    }
}
fn button_flags(button: u32, down: bool) -> ResultType<(u32, u32)> {
    Ok(match (button, down) {
        (1, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (1, false) => (MOUSEEVENTF_LEFTUP, 0),
        (2, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (2, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (3, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (3, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (4, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
        (4, false) => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
        (5, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
        (5, false) => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
        _ => bail!("Unsupported mouse button"),
    })
}
fn send_input(mut input: INPUT) -> ResultType<()> {
    if unsafe { SendInput(1, &mut input, mem::size_of::<INPUT>() as i32) } != 1 {
        bail!(
            "Windows rejected input (possibly blocked by UIPI): {}",
            io::Error::last_os_error()
        );
    }
    Ok(())
}
fn send_mouse(flags: u32, data: u32) -> ResultType<()> {
    let mut input: INPUT = unsafe { mem::zeroed() };
    input.type_ = INPUT_MOUSE;
    unsafe {
        *input.u.mi_mut() = MOUSEINPUT {
            dx: 0,
            dy: 0,
            mouseData: data,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: 0,
        };
    }
    send_input(input)
}
fn send_key(vk: u16, scan: u16, flags: u32) -> ResultType<()> {
    let mut input: INPUT = unsafe { mem::zeroed() };
    input.type_ = INPUT_KEYBOARD;
    unsafe {
        *input.u.ki_mut() = KEYBDINPUT {
            wVk: vk,
            wScan: scan,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: 0,
        };
    }
    send_input(input)
}
impl Drop for App {
    fn drop(&mut self) {
        if let Err(e) = self.release() {
            hbb_common::throttled_log!(
                Duration::from_secs(30),
                warn,
                "Failed to release remote application input: {e}"
            );
        }
    }
}
