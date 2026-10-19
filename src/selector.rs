use std::{
    cell::Cell,
    mem::{size_of, zeroed},
    ptr::null_mut,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, ensure};
use windows_sys::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{
            BLACK_BRUSH, BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FrameRect,
            GetStockObject, HBRUSH, InvalidateRect, PAINTSTRUCT,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

const TIMER: usize = 1;
const FINISH: u32 = WM_APP + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectedWindow {
    pub handle: usize,
    pub pid: u32,
}

thread_local! {
    // WH_MOUSE_LL callbacks run on the thread that installed the hook.
    static ACTIVE: Cell<*mut State> = const { Cell::new(null_mut()) };
}

struct State {
    overlay: HWND,
    origin: POINT,
    hovered: Option<SelectedWindow>,
    outline: Option<RECT>,
    chosen: Option<SelectedWindow>,
    pressed: bool,
    armed: bool,
    brush: HBRUSH,
    error: Option<anyhow::Error>,
}

pub async fn select(cancelled: Arc<AtomicBool>) -> Result<Option<SelectedWindow>> {
    let worker_cancelled = cancelled.clone();
    eprintln!("Select a window: move the mouse to highlight it, then left-click. Esc cancels.");
    let mut worker = tokio::task::spawn_blocking(move || select_blocking(worker_cancelled));
    tokio::select! {
        result = &mut worker => result.context("Window selector task failed")?,
        result = tokio::signal::ctrl_c() => {
            cancelled.store(true, Ordering::Relaxed);
            let cleanup = worker.await.context("Joining window selector")?;
            result.context("Listening for Ctrl+C")?;
            cleanup?;
            Ok(None)
        }
    }
}

fn select_blocking(cancelled: Arc<AtomicBool>) -> Result<Option<SelectedWindow>> {
    let mut overlay = Overlay::new()?;
    let mut message: MSG = unsafe { zeroed() };
    loop {
        let status = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        ensure!(
            status != -1,
            "Window selector message loop failed: {}",
            std::io::Error::last_os_error()
        );
        if status == 0 || message.message == FINISH {
            break;
        }
        if message.message == WM_TIMER && message.hwnd == overlay.state.overlay {
            if cancelled.load(Ordering::Relaxed)
                || unsafe { GetAsyncKeyState(VK_ESCAPE as i32) } < 0
            {
                break;
            }
            overlay.tick()?;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    if let Some(error) = overlay.state.error.take() {
        return Err(error);
    }
    Ok(overlay.state.chosen)
}

struct Overlay {
    state: Box<State>,
    hook: HHOOK,
}

impl Overlay {
    fn new() -> Result<Self> {
        let mut overlay = Self {
            state: Box::new(State {
                overlay: null_mut(),
                origin: POINT {
                    x: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
                    y: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
                },
                hovered: None,
                outline: None,
                chosen: None,
                pressed: false,
                armed: unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } >= 0,
                brush: unsafe { CreateSolidBrush(0x0000ff) },
                error: None,
            }),
            hook: null_mut(),
        };
        ensure!(
            !overlay.state.brush.is_null(),
            "Creating selector border brush failed"
        );
        let instance = unsafe { GetModuleHandleW(null_mut()) };
        ensure!(!instance.is_null(), "Getting selector module failed");
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("AppStreamWindowSelector"),
            hbrBackground: unsafe { GetStockObject(BLACK_BRUSH) } as HBRUSH,
            ..unsafe { zeroed() }
        };
        // The class remains registered for the process lifetime; subsequent selectors reuse it.
        let atom = unsafe { RegisterClassExW(&class) };
        ensure!(
            atom != 0 || unsafe { windows_sys::Win32::Foundation::GetLastError() } == 1410,
            "Registering selector window class failed: {}",
            std::io::Error::last_os_error()
        );
        overlay.state.overlay = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE,
                class.lpszClassName,
                w!("App Stream - Select a window"),
                WS_POPUP,
                overlay.state.origin.x,
                overlay.state.origin.y,
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
                null_mut(),
                null_mut(),
                instance,
                null_mut(),
            )
        };
        ensure!(
            !overlay.state.overlay.is_null(),
            "Creating selector overlay failed: {}",
            std::io::Error::last_os_error()
        );
        unsafe {
            SetWindowLongPtrW(
                overlay.state.overlay,
                GWLP_USERDATA,
                &mut *overlay.state as *mut State as isize,
            )
        };
        ensure!(
            unsafe { SetLayeredWindowAttributes(overlay.state.overlay, 0, 255, LWA_COLORKEY) } != 0,
            "Making selector background transparent failed"
        );
        ensure!(
            unsafe { SetTimer(overlay.state.overlay, TIMER, 16, None) } != 0,
            "Starting selector timer failed"
        );
        ACTIVE.with(|active| active.set(&mut *overlay.state));
        overlay.hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance, 0) };
        ensure!(
            !overlay.hook.is_null(),
            "Installing selection mouse hook failed: {}",
            std::io::Error::last_os_error()
        );
        overlay.tick()?;
        unsafe { ShowWindow(overlay.state.overlay, SW_SHOWNOACTIVATE) };
        Ok(overlay)
    }

    fn tick(&mut self) -> Result<()> {
        if self.state.pressed {
            return Ok(());
        }
        if unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } >= 0 {
            self.state.armed = true;
        }
        let mut point = POINT { x: 0, y: 0 };
        ensure!(
            unsafe { GetCursorPos(&mut point) } != 0,
            "Reading mouse position failed"
        );
        let hovered = window_at(point, self.state.overlay);
        let outline = hovered.and_then(|window| {
            let mut rect = unsafe { zeroed() };
            (unsafe { GetWindowRect(window.handle as HWND, &mut rect) } != 0).then_some(rect)
        });
        let changed = self.state.hovered != hovered || !same_rect(self.state.outline, outline);
        self.state.hovered = hovered;
        self.state.outline = outline;
        if changed {
            ensure!(
                unsafe { InvalidateRect(self.state.overlay, null_mut(), 1) } != 0,
                "Invalidating selector outline failed"
            );
        }
        Ok(())
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(null_mut()));
        unsafe {
            if !self.hook.is_null() && UnhookWindowsHookEx(self.hook) == 0 {
                eprintln!(
                    "Removing selection mouse hook failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            if !self.state.overlay.is_null() {
                KillTimer(self.state.overlay, TIMER);
                if DestroyWindow(self.state.overlay) == 0 {
                    eprintln!(
                        "Destroying selector overlay failed: {}",
                        std::io::Error::last_os_error()
                    );
                }
            }
            if !self.state.brush.is_null() {
                DeleteObject(self.state.brush);
            }
        }
    }
}

fn window_at(point: POINT, overlay: HWND) -> Option<SelectedWindow> {
    let hwnd = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
    if hwnd.is_null()
        || hwnd == overlay
        || hwnd == unsafe { GetDesktopWindow() }
        || hwnd == unsafe { GetShellWindow() }
        || unsafe { IsWindowVisible(hwnd) } == 0
        || unsafe { IsIconic(hwnd) } != 0
        || unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOOLWINDOW != 0
    {
        return None;
    }
    let mut class = [0u16; 256];
    let length = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    let class = String::from_utf16_lossy(&class[..length.max(0) as usize]);
    if excluded_class(&class) {
        return None;
    }
    let mut rect: RECT = unsafe { zeroed() };
    let mut pid = 0;
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0
        || rect.right < 2
        || rect.bottom < 2
        || unsafe { GetWindowThreadProcessId(hwnd, &mut pid) } == 0
    {
        return None;
    }
    Some(SelectedWindow {
        handle: hwnd as usize,
        pid,
    })
}

fn excluded_class(class: &str) -> bool {
    class.starts_with("AppStream.")
        || matches!(
            class,
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        )
}

unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code >= 0 {
        let consumed = ACTIVE.with(|active| {
            let pointer = active.get();
            if pointer.is_null() {
                return false;
            }
            let state = unsafe { &mut *pointer };
            match message as u32 {
                WM_LBUTTONDOWN if state.armed && !state.pressed => {
                    let point = unsafe { (*(data as *const MSLLHOOKSTRUCT)).pt };
                    if let Some(window) = window_at(point, state.overlay) {
                        state.chosen = Some(window);
                        state.pressed = true;
                        return true;
                    }
                }
                WM_LBUTTONUP if state.pressed => {
                    // Swallow the complete click so selecting does not activate a target control.
                    if unsafe { PostMessageW(state.overlay, FINISH, 0, 0) } == 0 {
                        state.error = Some(anyhow::anyhow!("Finishing window selection failed"));
                        unsafe { PostQuitMessage(0) };
                    }
                    return true;
                }
                _ => {}
            }
            false
        });
        if consumed {
            return 1;
        }
    }
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_PAINT {
        let mut paint: PAINTSTRUCT = unsafe { zeroed() };
        let dc = unsafe { BeginPaint(hwnd, &mut paint) };
        let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const State;
        if !state.is_null() {
            let state = unsafe { &*state };
            if let Some(rect) = state.outline {
                let mut rect = RECT {
                    left: rect.left - state.origin.x,
                    top: rect.top - state.origin.y,
                    right: rect.right - state.origin.x,
                    bottom: rect.bottom - state.origin.y,
                };
                for _ in 0..3 {
                    unsafe { FrameRect(dc, &rect, state.brush) };
                    rect.left += 1;
                    rect.top += 1;
                    rect.right -= 1;
                    rect.bottom -= 1;
                }
            }
        }
        unsafe { EndPaint(hwnd, &paint) };
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn same_rect(first: Option<RECT>, second: Option<RECT>) -> bool {
    match (first, second) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            (a.left, a.top, a.right, a.bottom) == (b.left, b.top, b.right, b.bottom)
        }

        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_app_stream_windows_are_not_capture_targets() {
        assert!(excluded_class("AppStream.LogWindow"));
        assert!(excluded_class("AppStream.PasswordWindow"));
        assert!(!excluded_class("Notepad"));
        assert!(!excluded_class("BUTTON"));
    }

    #[test]
    fn outline_comparison_detects_moves_resizes_and_disappearance() {
        let rect = || RECT {
            left: -100,
            top: 50,
            right: 200,
            bottom: 250,
        };
        assert!(same_rect(None, None));
        assert!(same_rect(Some(rect()), Some(rect())));
        assert!(!same_rect(Some(rect()), None));
        let mut moved = rect();
        moved.left += 1;
        assert!(!same_rect(Some(rect()), Some(moved)));
    }

    #[test]
    #[ignore = "requires an interactive desktop; moves the pointer and creates visible windows"]
    fn highlights_exact_window_consumes_click_and_cleans_up() {
        use windows_sys::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC, UpdateWindow};
        use windows_sys::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            SetThreadDpiAwarenessContext,
        };

        struct RestoreDpi(DPI_AWARENESS_CONTEXT);
        impl Drop for RestoreDpi {
            fn drop(&mut self) {
                unsafe { SetThreadDpiAwarenessContext(self.0) };
            }
        }
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        assert!(!previous.is_null());
        let _dpi = RestoreDpi(previous);

        struct Fixture(HWND);
        impl Drop for Fixture {
            fn drop(&mut self) {
                unsafe { DestroyWindow(self.0) };
            }
        }
        let create = |x, width| {
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOPMOST,
                    w!("BUTTON"),
                    w!("App Stream selector fixture"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    x,
                    120,
                    width,
                    180,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            };
            assert!(!hwnd.is_null());
            Fixture(hwnd)
        };
        let small = create(100, 220);
        let large = create(400, 300);
        unsafe {
            UpdateWindow(small.0);
            UpdateWindow(large.0);
        }
        let mut original = POINT { x: 0, y: 0 };
        assert_ne!(unsafe { GetCursorPos(&mut original) }, 0);
        struct RestoreCursor(POINT);
        impl Drop for RestoreCursor {
            fn drop(&mut self) {
                unsafe { SetCursorPos(self.0.x, self.0.y) };
            }
        }
        let _restore = RestoreCursor(original);
        let mut overlay = Overlay::new().unwrap();
        let hwnd = overlay.state.overlay;
        let mut rect: RECT = unsafe { zeroed() };
        assert_ne!(unsafe { GetWindowRect(small.0, &mut rect) }, 0);
        let point = POINT {
            x: rect.left + 40,
            y: rect.top + 60,
        };
        assert_ne!(unsafe { SetCursorPos(point.x, point.y) }, 0);
        overlay.tick().unwrap();
        unsafe { UpdateWindow(hwnd) };
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(overlay.state.hovered.unwrap().handle, small.0 as usize);
        let dc = unsafe { GetDC(null_mut()) };
        assert!(!dc.is_null());
        let color = unsafe { GetPixel(dc, rect.left + 1, rect.top + 10) };
        unsafe { ReleaseDC(null_mut(), dc) };
        // Desktop color management can slightly alter the brush's exact RGB value.
        assert!(
            color & 0xff >= 220 && (color >> 8) & 0xff < 32 && (color >> 16) & 0xff < 32,
            "Outline must be red, got COLORREF {color:#08x}"
        );

        assert_ne!(unsafe { SetCursorPos(450, 180) }, 0);
        overlay.tick().unwrap();
        assert_eq!(overlay.state.hovered.unwrap().handle, large.0 as usize);
        assert_ne!(unsafe { SetCursorPos(point.x, point.y) }, 0);
        overlay.tick().unwrap();
        overlay.state.armed = true;
        let event = MSLLHOOKSTRUCT {
            pt: point,
            ..unsafe { zeroed() }
        };
        assert_eq!(
            unsafe { mouse_hook(0, WM_LBUTTONDOWN as usize, &event as *const _ as LPARAM) },
            1
        );
        assert_eq!(
            unsafe { mouse_hook(0, WM_LBUTTONUP as usize, &event as *const _ as LPARAM) },
            1
        );
        let chosen = overlay.state.chosen.unwrap();
        assert_eq!(
            chosen.handle, small.0 as usize,
            "Must select exact window, not largest window of its PID"
        );
        drop(overlay);
        assert_eq!(unsafe { IsWindow(hwnd) }, 0);
        ACTIVE.with(|active| assert!(active.get().is_null()));
        let mut capture =
            crate::capture::WindowCapture::open_window(chosen.handle, chosen.pid).unwrap();
        assert_eq!(
            capture.capture().unwrap().unwrap().len(),
            (capture.width() * capture.height() * 4) as usize
        );
        assert!(
            crate::capture::WindowCapture::open_window(chosen.handle, chosen.pid.wrapping_add(1))
                .is_err()
        );
        let by_pid = crate::capture::WindowCapture::open_pid(std::process::id()).unwrap();
        assert!(by_pid.width() > capture.width());
        drop(large);
        drop(small);
        assert!(crate::capture::WindowCapture::open_window(chosen.handle, chosen.pid).is_err());
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn cancelled_selector_removes_overlay() {
        assert!(
            select_blocking(Arc::new(AtomicBool::new(true)))
                .unwrap()
                .is_none()
        );
        assert!(unsafe { FindWindowW(w!("AppStreamWindowSelector"), null_mut()) }.is_null());
        ACTIVE.with(|active| assert!(active.get().is_null()));
    }
}
