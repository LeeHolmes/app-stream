use std::{
    cell::RefCell,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, TryRecvError},
    },
};

use anyhow::{Result, anyhow, bail};
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::{COLOR_BTNFACE, CreateFontIndirectW, DeleteObject, UpdateWindow},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::{
            EM_GETFIRSTVISIBLELINE, EM_GETSEL, EM_LINESCROLL, EM_SCROLLCARET, EM_SETLIMITTEXT,
            EM_SETSEL,
        },
        HiDpi::{
            GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi, SystemParametersInfoForDpi,
        },
        Input::KeyboardAndMouse::{EnableWindow, SetFocus},
        Shell::*,
        WindowsAndMessaging::*,
    },
};
use windows_sys::core::w;

#[derive(Debug)]
pub enum Command {
    ChangeTarget,
    Close,
}

#[derive(Debug)]
pub enum Event {
    Selecting,
    Streaming,
    Stopped,
}

#[derive(Debug)]
pub struct PasswordCancelled;

impl std::fmt::Display for PasswordCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Password entry cancelled")
    }
}

impl std::error::Error for PasswordCancelled {}

const TRAY_MESSAGE: u32 = WM_APP + 1;
const LAYOUT_MESSAGE: u32 = WM_APP + 2;
const TIMER: usize = 1;
const CHANGE: u16 = 100;
const LOG: u16 = 101;
const CLOSE: u16 = 102;
const EXIT: u16 = 103;
const SAVE: u16 = 1;
const CANCEL: u16 = 2;

fn wide(text: &str) -> Vec<u16> {
    text.replace('\0', "\u{fffd}")
        .encode_utf16()
        .chain(Some(0))
        .collect()
}

fn win_error(operation: &str) -> anyhow::Error {
    anyhow!("{operation}: {}", std::io::Error::last_os_error())
}

fn report(error: impl std::fmt::Display) {
    crate::logging::write(format!("GUI: {error}"));
}

pub fn message(title: &str, text: &str, error: bool) {
    let title = wide(title);
    let text = wide(text);
    unsafe {
        if MessageBoxW(
            null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK
                | if error {
                    MB_ICONERROR
                } else {
                    MB_ICONINFORMATION
                },
        ) == 0
        {
            report(win_error("MessageBoxW"));
        }
    }
}

struct Class {
    instance: HINSTANCE,
    name: Vec<u16>,
}

impl Class {
    fn register(name: &str, procedure: WNDPROC) -> Result<Self> {
        let instance = unsafe { GetModuleHandleW(null()) };
        if instance.is_null() {
            return Err(win_error("GetModuleHandleW"));
        }
        let name = wide(name);
        let cursor = unsafe { LoadCursorW(null_mut(), IDC_ARROW) };
        if cursor.is_null() {
            return Err(win_error("LoadCursorW"));
        }
        let icon = unsafe { LoadIconW(instance, std::ptr::without_provenance(1)) };
        if icon.is_null() {
            return Err(win_error("LoadIconW (embedded icon 1)"));
        }
        let description = WNDCLASSW {
            lpfnWndProc: procedure,
            hInstance: instance,
            hCursor: cursor,
            hIcon: icon,
            hbrBackground: (COLOR_BTNFACE + 1) as _,
            lpszClassName: name.as_ptr(),
            ..unsafe { zeroed() }
        };
        if unsafe { RegisterClassW(&description) } == 0 {
            return Err(win_error("RegisterClassW"));
        }
        Ok(Self { instance, name })
    }
}

impl Drop for Class {
    fn drop(&mut self) {
        unsafe {
            if UnregisterClassW(self.name.as_ptr(), self.instance) == 0 {
                report(win_error("UnregisterClassW"));
            }
        }
    }
}

fn apply_appearance(window: HWND) -> Result<()> {
    let dpi = unsafe { GetDpiForWindow(window) }.max(96);
    let mut metrics: NONCLIENTMETRICSW = unsafe { zeroed() };
    metrics.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
    if unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            &mut metrics as *mut _ as *mut _,
            0,
            dpi,
        )
    } == 0
    {
        return Err(win_error("Reading Windows UI font"));
    }
    let font = unsafe { CreateFontIndirectW(&metrics.lfMessageFont) };
    if font.is_null() {
        return Err(win_error("Creating Windows UI font"));
    }
    unsafe {
        let old = GetPropW(window, w!("AppStream.Font"));
        if SetPropW(window, w!("AppStream.Font"), font as _) == 0 {
            let error = win_error("Installing Windows UI font");
            DeleteObject(font as _);
            return Err(error);
        }
        EnumChildWindows(window, Some(update_font), font as isize);
        if !old.is_null() && DeleteObject(old) == 0 {
            report(win_error("Deleting previous Windows UI font"));
        }
        for (kind, width, height, property) in [
            (ICON_BIG, SM_CXICON, SM_CYICON, w!("AppStream.IconBig")),
            (
                ICON_SMALL,
                SM_CXSMICON,
                SM_CYSMICON,
                w!("AppStream.IconSmall"),
            ),
        ] {
            let icon = LoadImageW(
                GetModuleHandleW(null()),
                std::ptr::without_provenance(1),
                IMAGE_ICON,
                GetSystemMetricsForDpi(width, dpi),
                GetSystemMetricsForDpi(height, dpi),
                0,
            );
            if icon.is_null() {
                return Err(win_error("Loading DPI-sized window icon"));
            }
            let old = GetPropW(window, property);
            if SetPropW(window, property, icon) == 0 {
                let error = win_error("Installing DPI-sized window icon");
                DestroyIcon(icon);
                return Err(error);
            }
            SendMessageW(window, WM_SETICON, kind as usize, icon as isize);
            if !old.is_null() && DestroyIcon(old) == 0 {
                report(win_error("Deleting previous window icon"));
            }
        }
    }
    Ok(())
}

unsafe extern "system" fn update_font(window: HWND, font: LPARAM) -> i32 {
    unsafe { SendMessageW(window, WM_SETFONT, font as usize, 1) };
    1
}

fn remove_appearance(window: HWND) {
    let font = unsafe { RemovePropW(window, w!("AppStream.Font")) };
    if !font.is_null() && unsafe { DeleteObject(font) } == 0 {
        report(win_error("Deleting Windows UI font"));
    }
    for property in [w!("AppStream.IconBig"), w!("AppStream.IconSmall")] {
        let icon = unsafe { RemovePropW(window, property) };
        if !icon.is_null() && unsafe { DestroyIcon(icon) } == 0 {
            report(win_error("Deleting window icon"));
        }
    }
}

fn control(parent: HWND, class: &str, text: &str, style: u32, id: u16) -> Result<HWND> {
    let edit = class.eq_ignore_ascii_case("EDIT");
    let class = wide(class);
    let text = wide(text);
    let window = unsafe {
        CreateWindowExW(
            if edit { WS_EX_CLIENTEDGE } else { 0 },
            class.as_ptr(),
            text.as_ptr(),
            WS_CHILD | WS_VISIBLE | if edit { style & !WS_BORDER } else { style },
            0,
            0,
            0,
            0,
            parent,
            id as usize as HMENU,
            GetModuleHandleW(null()),
            null(),
        )
    };
    if window.is_null() {
        return Err(win_error("CreateWindowExW (control)"));
    }
    unsafe {
        SendMessageW(
            window,
            WM_SETFONT,
            GetPropW(parent, w!("AppStream.Font")) as usize,
            1,
        );
    }
    Ok(window)
}

fn set_text(window: HWND, text: &str) {
    if unsafe { SetWindowTextW(window, wide(text).as_ptr()) } == 0 {
        report(win_error("SetWindowTextW"));
    }
}

fn set_user_data(window: HWND, value: isize) -> bool {
    unsafe {
        SetLastError(0);
        if SetWindowLongPtrW(window, GWLP_USERDATA, value) == 0 && GetLastError() != 0 {
            report(win_error("SetWindowLongPtrW"));
            return false;
        }
    }
    true
}

fn show(window: HWND) {
    unsafe {
        ShowWindow(window, SW_RESTORE);
        // Windows may refuse focus stealing; the window is still made visible.
        SetForegroundWindow(window);
    }
}

fn hide(window: HWND) {
    unsafe { ShowWindow(window, SW_HIDE) };
}

fn move_control(window: HWND, x: i32, y: i32, width: i32, height: i32) {
    if !window.is_null() && unsafe { MoveWindow(window, x, y, width.max(1), height.max(1), 1) } == 0
    {
        report(win_error("MoveWindow"));
    }
}

fn scaled(window: HWND, value: i32) -> i32 {
    let dpi = unsafe { GetDpiForWindow(window) }.max(96);
    (i64::from(value) * i64::from(dpi) / 96) as i32
}

struct Shell {
    window: HWND,
    status: HWND,
    log: HWND,
    change: HWND,
    close: HWND,
    icon: HICON,
    tray: bool,
    taskbar_created: u32,
    commands: UnboundedSender<Command>,
    events: Receiver<Event>,
    cancelled: Arc<AtomicBool>,
    closing: bool,
    worker_done: bool,
    can_change: bool,
    revision: Option<u64>,
}

impl Shell {
    fn notification(&self) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.window;
        data.uID = 1;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = TRAY_MESSAGE;
        data.hIcon = self.icon;
        let tooltip = wide("App Stream — connected client log");
        data.szTip[..tooltip.len()].copy_from_slice(&tooltip);
        data
    }

    fn add_tray(&mut self) -> Result<()> {
        let data = self.notification();
        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
            bail!("Could not add the App Stream notification-area icon");
        }
        self.tray = true;
        Ok(())
    }

    fn layout(&self) {
        let mut rect: RECT = unsafe { zeroed() };
        if unsafe { GetClientRect(self.window, &mut rect) } == 0 {
            report(win_error("GetClientRect"));
            return;
        }
        let margin = scaled(self.window, 12);
        let status_height = scaled(self.window, 66);
        let button_height = scaled(self.window, 30);
        let button_width = scaled(self.window, 140);
        let bottom = rect.bottom - margin - button_height;
        move_control(
            self.status,
            margin,
            margin,
            rect.right - 2 * margin,
            status_height,
        );
        move_control(
            self.log,
            margin,
            margin + status_height,
            rect.right - 2 * margin,
            bottom - status_height - 2 * margin,
        );
        move_control(self.change, margin, bottom, button_width, button_height);
        move_control(
            self.close,
            rect.right - margin - button_width,
            bottom,
            button_width,
            button_height,
        );
    }

    fn enable_change(&mut self, enabled: bool) {
        self.can_change = enabled && !self.closing;
        unsafe {
            EnableWindow(self.change, i32::from(self.can_change));
        }
    }

    fn exit(&mut self) {
        if self.closing {
            return;
        }
        self.cancelled.store(true, Ordering::SeqCst);
        self.closing = true;
        self.enable_change(false);
        unsafe {
            EnableWindow(self.close, 0);
        }
        set_text(
            self.status,
            "Closing App Stream... Waiting for the stream worker to finish.",
        );
        if self.commands.send(Command::Close).is_err() {
            report("The stream worker command channel is closed; waiting for worker completion.");
        }
    }

    fn change_target(&mut self) {
        if !self.can_change || self.closing {
            return;
        }
        self.enable_change(false);
        set_text(self.status, "Preparing target selection...");
        show(self.window);
        if self.commands.send(Command::ChangeTarget).is_err() {
            report("The stream worker command channel is closed.");
            set_text(
                self.status,
                "The stream worker is no longer accepting commands. See the log below.",
            );
        }
    }

    fn poll(&mut self) {
        loop {
            match self.events.try_recv() {
                Ok(event) if !self.closing => match event {
                    Event::Selecting => {
                        self.enable_change(false);
                        set_text(
                            self.status,
                            "Move the mouse to highlight an application window, then left-click to select it.\r\nPress Escape to cancel selection. This window stays available for the live log.",
                        );
                        show(self.window);
                    }
                    Event::Streaming => {
                        self.enable_change(true);
                        set_text(
                            self.status,
                            "Streaming the selected application. Use Change target to select another window.\r\nClose this log window to hide it; App Stream continues in the notification area.",
                        );
                        hide(self.window);
                    }
                    Event::Stopped => {
                        self.enable_change(true);
                        set_text(
                            self.status,
                            "Streaming stopped or target selection was cancelled. Check the log for details.\r\nChoose Change target to try again, or right-click the tray icon and choose Exit to quit.",
                        );
                        show(self.window);
                    }
                },
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.worker_done = true;
                    self.refresh_log();
                    unsafe {
                        if DestroyWindow(self.window) == 0 {
                            report(win_error("DestroyWindow"));
                            // The worker has finished, so quitting is now safe.
                            PostQuitMessage(1);
                        }
                    }
                    return;
                }
            }
        }
        self.refresh_log();
    }

    fn refresh_log(&mut self) {
        let (revision, text) = crate::logging::snapshot();
        if self.revision == Some(revision) {
            return;
        }
        self.revision = Some(revision);
        unsafe {
            let first = SendMessageW(self.log, EM_GETFIRSTVISIBLELINE, 0, 0);
            let mut start: u32 = 0;
            let mut end: u32 = 0;
            SendMessageW(
                self.log,
                EM_GETSEL,
                &mut start as *mut _ as usize,
                &mut end as *mut _ as isize,
            );
            let mut scroll: SCROLLINFO = zeroed();
            scroll.cbSize = size_of::<SCROLLINFO>() as u32;
            scroll.fMask = SIF_ALL;
            let has_scroll = GetScrollInfo(self.log, SB_VERT, &mut scroll) != 0;
            let at_bottom = !has_scroll
                || i64::from(scroll.nPos) + i64::from(scroll.nPage) >= i64::from(scroll.nMax);
            set_text(self.log, &text);
            if at_bottom {
                SendMessageW(self.log, EM_SETSEL, usize::MAX, -1);
                SendMessageW(self.log, EM_SCROLLCARET, 0, 0);
            } else {
                SendMessageW(self.log, EM_SETSEL, start as usize, end as isize);
                SendMessageW(self.log, EM_LINESCROLL, 0, first);
            }
        }
    }

    fn menu(window: HWND, can_change: bool, closing: bool) -> u16 {
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            report(win_error("CreatePopupMenu"));
            return 0;
        }
        let menu = Menu(menu);
        let change_flags = MF_STRING | if can_change { MF_ENABLED } else { MF_GRAYED };
        let exit_flags = MF_STRING | if closing { MF_GRAYED } else { MF_ENABLED };
        unsafe {
            if AppendMenuW(
                menu.0,
                change_flags,
                CHANGE as usize,
                wide("Change target").as_ptr(),
            ) == 0
                || AppendMenuW(
                    menu.0,
                    MF_STRING,
                    LOG as usize,
                    wide("Connected client log").as_ptr(),
                ) == 0
                || AppendMenuW(menu.0, MF_SEPARATOR, 0, null()) == 0
                || AppendMenuW(menu.0, exit_flags, EXIT as usize, wide("Exit").as_ptr()) == 0
            {
                report(win_error("AppendMenuW"));
                return 0;
            }
            let mut point: POINT = zeroed();
            if GetCursorPos(&mut point) == 0 {
                report(win_error("GetCursorPos"));
                return 0;
            }
            SetForegroundWindow(window);
            let command = TrackPopupMenu(
                menu.0,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                point.x,
                point.y,
                0,
                window,
                null(),
            );
            // Required to dismiss notification-area menus reliably on the next click.
            if IsWindow(window) != 0 && PostMessageW(window, WM_NULL, 0, 0) == 0 {
                report(win_error("PostMessageW"));
            }
            command as u16
        }
    }

    fn command(&mut self, command: u16) {
        match command {
            CHANGE => self.change_target(),
            CLOSE => hide(self.window),
            EXIT => self.exit(),
            LOG => show(self.window),
            _ => {}
        }
    }
}

struct Menu(HMENU);
impl Drop for Menu {
    fn drop(&mut self) {
        if unsafe { DestroyMenu(self.0) } == 0 {
            report(win_error("DestroyMenu"));
        }
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        unsafe {
            if !self.window.is_null() && IsWindow(self.window) != 0 {
                set_user_data(self.window, 0);
            }
            if self.tray && Shell_NotifyIconW(NIM_DELETE, &self.notification()) == 0 {
                report("Could not remove the notification-area icon.");
            }
            if !self.window.is_null() && IsWindow(self.window) != 0 {
                KillTimer(self.window, TIMER);
                if DestroyWindow(self.window) == 0 {
                    report(win_error("DestroyWindow"));
                }
            }
            if !self.icon.is_null() && DestroyIcon(self.icon) == 0 {
                report(win_error("DestroyIcon"));
            }
        }
    }
}

unsafe extern "system" fn shell_proc(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(l as *const CREATESTRUCTW);
            if !set_user_data(window, create.lpCreateParams as isize) {
                return 0;
            }
            (*(create.lpCreateParams as *const RefCell<Shell>))
                .borrow_mut()
                .window = window;
        }
        match message {
            WM_CREATE => {
                return match apply_appearance(window) {
                    Ok(()) => 0,
                    Err(error) => {
                        report(error);
                        -1
                    }
                };
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return 0;
            }
            WM_NCDESTROY => {
                remove_appearance(window);
                set_user_data(window, 0);
                return DefWindowProcW(window, message, w, l);
            }
            WM_CLOSE => {
                hide(window);
                return 0;
            }
            WM_GETMINMAXINFO => {
                let limits = &mut *(l as *mut MINMAXINFO);
                limits.ptMinTrackSize.x = scaled(window, 480);
                limits.ptMinTrackSize.y = scaled(window, 300);
                return 0;
            }
            WM_DPICHANGED => {
                let rect = &*(l as *const RECT);
                if SetWindowPos(
                    window,
                    null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                ) == 0
                {
                    report(win_error("SetWindowPos"));
                }
                if let Err(error) = apply_appearance(window) {
                    report(error);
                }
                return 0;
            }
            _ => {}
        }
        let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *const RefCell<Shell>;
        if !pointer.is_null()
            && message == TRAY_MESSAGE
            && matches!(l as u32, WM_RBUTTONUP | WM_CONTEXTMENU)
        {
            let settings = (*pointer)
                .try_borrow()
                .map(|shell| (shell.can_change, shell.closing));
            if let Ok((can_change, closing)) = settings {
                // TrackPopupMenu runs a nested message loop. Keep the state
                // unborrowed so worker events and Explorer restarts still work.
                let command = Shell::menu(window, can_change, closing);
                if let Ok(mut shell) = (*pointer).try_borrow_mut() {
                    shell.command(command);
                }
            }
            return 0;
        }
        // Control updates and popup menus synchronously dispatch more messages.
        // Deferring reentrant state updates prevents aliased mutable references.
        if !pointer.is_null()
            && let Ok(mut shell) = (*pointer).try_borrow_mut()
        {
            match message {
                WM_COMMAND => {
                    shell.command((w & 0xffff) as u16);
                    return 0;
                }
                WM_TIMER if w == TIMER => {
                    shell.poll();
                    return 0;
                }
                WM_SIZE | LAYOUT_MESSAGE => {
                    if message != WM_SIZE || w != SIZE_MINIMIZED as usize {
                        shell.layout();
                    }
                    return 0;
                }
                TRAY_MESSAGE => {
                    if matches!(l as u32, WM_LBUTTONUP | WM_LBUTTONDBLCLK) {
                        show(window);
                    }
                    return 0;
                }
                _ if message == shell.taskbar_created => {
                    shell.tray = false;
                    if let Err(error) = shell.add_tray() {
                        report(error);
                        show(window);
                    }
                    return 0;
                }
                _ => {}
            }
        } else if message == WM_SIZE && w != SIZE_MINIMIZED as usize {
            if PostMessageW(window, LAYOUT_MESSAGE, 0, 0) == 0 {
                report(win_error("PostMessageW (layout)"));
            }
            return 0;
        }
        DefWindowProcW(window, message, w, l)
    }
}

pub fn run(
    commands: UnboundedSender<Command>,
    events: Receiver<Event>,
    cancelled: Arc<AtomicBool>,
) -> Result<()> {
    let class = Class::register("AppStream.LogWindow", Some(shell_proc))?;
    let mut shell = Box::new(RefCell::new(Shell {
        window: null_mut(),
        status: null_mut(),
        log: null_mut(),
        change: null_mut(),
        close: null_mut(),
        icon: null_mut(),
        tray: false,
        taskbar_created: 0,
        commands,
        events,
        cancelled,
        closing: false,
        worker_done: false,
        can_change: false,
        revision: None,
    }));
    unsafe {
        shell.get_mut().icon = LoadImageW(
            class.instance,
            std::ptr::without_provenance(1),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE,
        ) as HICON;
        if shell.get_mut().icon.is_null() {
            return Err(win_error("LoadImageW (embedded icon 1)"));
        }
        shell.get_mut().taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        if shell.get_mut().taskbar_created == 0 {
            return Err(win_error("RegisterWindowMessageW"));
        }
        let dpi = GetDpiForSystem().max(96);
        let window = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class.name.as_ptr(),
            wide("App Stream - connected client log").as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            (820 * dpi / 96) as i32,
            (540 * dpi / 96) as i32,
            null_mut(),
            null_mut(),
            class.instance,
            &*shell as *const RefCell<Shell> as *const _,
        );
        if window.is_null() {
            return Err(win_error("CreateWindowExW (log window)"));
        }
        {
            let mut shell = shell.borrow_mut();
            shell.status = control(
                window,
                "STATIC",
                "Move the mouse to highlight an application window, then left-click to select it.\r\nPress Escape to cancel selection. Live connection and stream activity appears below.",
                0,
                0,
            )?;
            shell.log = control(
                window,
                "EDIT",
                "",
                WS_TABSTOP
                    | WS_BORDER
                    | WS_VSCROLL
                    | WS_HSCROLL
                    | ES_MULTILINE as u32
                    | ES_READONLY as u32
                    | ES_AUTOVSCROLL as u32
                    | ES_AUTOHSCROLL as u32,
                0,
            )?;
            SendMessageW(shell.log, EM_SETLIMITTEXT, 8_000_000, 0);
            shell.change = control(
                window,
                "BUTTON",
                "Change target",
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                CHANGE,
            )?;
            shell.close = control(
                window,
                "BUTTON",
                "Close",
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                CLOSE,
            )?;
            shell.enable_change(false);
            shell.layout();
            shell.refresh_log();
            // Never hide the only user interface until its tray replacement exists.
            shell.add_tray()?;
            if SetTimer(window, TIMER, 150, None) == 0 {
                return Err(win_error("SetTimer"));
            }
            ShowWindow(window, SW_SHOW);
            UpdateWindow(window);
        }
        let mut message: MSG = zeroed();
        loop {
            match GetMessageW(&mut message, null_mut(), 0, 0) {
                -1 => return Err(win_error("GetMessageW")),
                0 => {
                    if !shell.borrow().worker_done {
                        bail!("GUI message loop ended before the stream worker completed");
                    }
                    if message.wParam != 0 {
                        bail!("GUI could not destroy its log window");
                    }
                    break;
                }
                _ => {
                    if IsDialogMessageW(window, &message) == 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            }
        }
    }
    Ok(())
}

struct Prompt {
    window: HWND,
    edit: HWND,
    label: HWND,
    password_label: HWND,
    save: HWND,
    cancel: HWND,
    result: Option<String>,
    done: bool,
}

impl Prompt {
    fn layout(&self) {
        let s = |value| scaled(self.window, value);
        move_control(self.label, s(16), s(14), s(465), s(48));
        move_control(self.password_label, s(16), s(76), s(80), s(24));
        move_control(self.edit, s(100), s(72), s(375), s(28));
        move_control(self.save, s(275), s(128), s(96), s(30));
        move_control(self.cancel, s(379), s(128), s(96), s(30));
    }

    fn save(&mut self) {
        unsafe {
            let length = GetWindowTextLengthW(self.edit);
            if length == 0 {
                if MessageBoxW(
                    self.window,
                    wide("Enter a nonempty password.").as_ptr(),
                    wide("Password required").as_ptr(),
                    MB_OK | MB_ICONWARNING,
                ) == 0
                {
                    report(win_error("MessageBoxW (password validation)"));
                }
                SetFocus(self.edit);
                return;
            }
            let mut text = vec![0u16; length as usize + 1];
            let count = GetWindowTextW(self.edit, text.as_mut_ptr(), text.len() as i32);
            if count == 0 {
                report(win_error("GetWindowTextW (password)"));
                text.fill(0);
                return;
            }
            self.result = Some(String::from_utf16_lossy(&text[..count as usize]));
            text.fill(0);
            self.finish();
        }
    }

    fn finish(&mut self) {
        self.done = true;
        // Leave destruction to the owning guard, after the modal loop ends.
        unsafe {
            ShowWindow(self.window, SW_HIDE);
        }
    }
}

impl Drop for Prompt {
    fn drop(&mut self) {
        if !self.window.is_null() && unsafe { IsWindow(self.window) } != 0 {
            set_user_data(self.window, 0);
            if !self.edit.is_null() {
                set_text(self.edit, "");
            }
            if unsafe { DestroyWindow(self.window) } == 0 {
                report(win_error("DestroyWindow (password prompt)"));
            }
        }
    }
}

unsafe extern "system" fn prompt_proc(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(l as *const CREATESTRUCTW);
            if !set_user_data(window, create.lpCreateParams as isize) {
                return 0;
            }
            (*(create.lpCreateParams as *const RefCell<Prompt>))
                .borrow_mut()
                .window = window;
        }
        if message == WM_CREATE {
            return match apply_appearance(window) {
                Ok(()) => 0,
                Err(error) => {
                    report(error);
                    -1
                }
            };
        }
        if message == WM_NCDESTROY {
            remove_appearance(window);
            set_user_data(window, 0);
            return DefWindowProcW(window, message, w, l);
        }
        let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *const RefCell<Prompt>;
        if !pointer.is_null()
            && let Ok(mut prompt) = (*pointer).try_borrow_mut()
        {
            match message {
                WM_COMMAND => match (w & 0xffff) as u16 {
                    SAVE => {
                        prompt.save();
                        return 0;
                    }
                    CANCEL => {
                        prompt.finish();
                        return 0;
                    }
                    _ => {}
                },
                WM_CLOSE => {
                    prompt.finish();
                    return 0;
                }
                WM_DPICHANGED => {
                    let rect = &*(l as *const RECT);
                    if SetWindowPos(
                        window,
                        null_mut(),
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    ) == 0
                    {
                        report(win_error("Resizing password window for DPI"));
                    }
                    if let Err(error) = apply_appearance(window) {
                        report(error);
                    }
                    prompt.layout();
                    return 0;
                }
                _ => {}
            }
        }
        DefWindowProcW(window, message, w, l)
    }
}

pub fn password_prompt(username: &str) -> Result<String> {
    let class = Class::register("AppStream.PasswordWindow", Some(prompt_proc))?;
    let prompt = Box::new(RefCell::new(Prompt {
        window: null_mut(),
        edit: null_mut(),
        label: null_mut(),
        password_label: null_mut(),
        save: null_mut(),
        cancel: null_mut(),
        result: None,
        done: false,
    }));
    unsafe {
        let dpi = GetDpiForSystem().max(96);
        let window = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class.name.as_ptr(),
            wide("App Stream - save password").as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            (510 * dpi / 96) as i32,
            (240 * dpi / 96) as i32,
            null_mut(),
            null_mut(),
            class.instance,
            &*prompt as *const RefCell<Prompt> as *const _,
        );
        if window.is_null() {
            return Err(win_error("CreateWindowExW (password prompt)"));
        }
        {
            let mut prompt = prompt.borrow_mut();
            prompt.label = control(
                window,
                "STATIC",
                &format!(
                    "Enter the stream password for {username}.\r\nSave stores it securely in Windows Credential Manager."
                ),
                0,
                0,
            )?;
            prompt.password_label = control(window, "STATIC", "&Password:", 0, 0)?;
            prompt.edit = control(
                window,
                "EDIT",
                "",
                WS_TABSTOP | WS_BORDER | ES_PASSWORD as u32 | ES_AUTOHSCROLL as u32,
                0,
            )?;
            SendMessageW(prompt.edit, EM_SETLIMITTEXT, 4096, 0);
            prompt.save = control(
                window,
                "BUTTON",
                "&Save",
                WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
                SAVE,
            )?;
            prompt.cancel = control(
                window,
                "BUTTON",
                "Cancel",
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                CANCEL,
            )?;
            prompt.layout();
            SendMessageW(window, DM_SETDEFID, SAVE as usize, 0);
            ShowWindow(window, SW_SHOW);
            UpdateWindow(window);
            SetForegroundWindow(window);
            SetFocus(prompt.edit);
        }
        let mut message: MSG = zeroed();
        while !prompt.borrow().done {
            match GetMessageW(&mut message, null_mut(), 0, 0) {
                -1 => return Err(win_error("GetMessageW (password prompt)")),
                0 => {
                    PostQuitMessage(message.wParam as i32);
                    bail!("Password prompt interrupted");
                }
                _ => {
                    if IsDialogMessageW(window, &message) == 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            }
        }
    }
    let result = prompt.borrow_mut().result.take();
    result.ok_or_else(|| PasswordCancelled.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;

    fn find_test_window(class: &str) -> HWND {
        struct Search {
            class: String,
            window: HWND,
        }
        unsafe extern "system" fn find(window: HWND, parameter: LPARAM) -> i32 {
            unsafe {
                let search = &mut *(parameter as *mut Search);
                let mut pid = 0;
                GetWindowThreadProcessId(window, &mut pid);
                if pid == std::process::id() {
                    let mut class = [0u16; 256];
                    let length = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
                    if String::from_utf16_lossy(&class[..length.max(0) as usize]) == search.class {
                        search.window = window;
                        return 0;
                    }
                }
                1
            }
        }
        let mut search = Search {
            class: class.into(),
            window: null_mut(),
        };
        unsafe { EnumWindows(Some(find), &mut search as *mut Search as isize) };
        search.window
    }

    fn verify_appearance(window: HWND, control: HWND) -> Result<()> {
        use windows_sys::Win32::{
            Graphics::Gdi::{GetObjectW, LOGFONTW},
            UI::Controls::IsAppThemed,
        };
        unsafe {
            anyhow::ensure!(
                IsAppThemed() != 0,
                "Common Controls v6 visual styles are inactive"
            );
            anyhow::ensure!(
                GetWindowLongPtrW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW == 0,
                "Tool-window frame suppresses the title-bar icon"
            );
            for kind in [ICON_SMALL, ICON_BIG] {
                anyhow::ensure!(
                    SendMessageW(window, WM_GETICON, kind as usize, 0) != 0,
                    "Title-bar icon is missing"
                );
            }
            let font = SendMessageW(control, WM_GETFONT, 0, 0);
            anyhow::ensure!(font != 0, "Control font is missing");
            let mut actual: LOGFONTW = zeroed();
            anyhow::ensure!(
                GetObjectW(
                    font as _,
                    size_of::<LOGFONTW>() as i32,
                    &mut actual as *mut _ as _
                ) != 0,
                "Cannot inspect control font"
            );
            let mut expected: NONCLIENTMETRICSW = zeroed();
            expected.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
            anyhow::ensure!(
                SystemParametersInfoForDpi(
                    SPI_GETNONCLIENTMETRICS,
                    expected.cbSize,
                    &mut expected as *mut _ as _,
                    0,
                    GetDpiForWindow(window),
                ) != 0,
                "Cannot inspect system UI font"
            );
            anyhow::ensure!(
                actual.lfFaceName == expected.lfMessageFont.lfFaceName
                    && actual.lfHeight == expected.lfMessageFont.lfHeight,
                "Control does not use the current DPI-scaled Windows UI font"
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop with visual styles"]
    fn password_window_uses_current_visual_styles_font_and_caption_icon() -> Result<()> {
        use std::time::{Duration, Instant};
        let worker = std::thread::spawn(|| -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(5);
            let window = loop {
                let window = find_test_window("AppStream.PasswordWindow");
                if !window.is_null() {
                    let edit = unsafe { FindWindowExW(window, null_mut(), w!("EDIT"), null()) };
                    if !edit.is_null() {
                        break window;
                    }
                }
                anyhow::ensure!(Instant::now() < deadline, "Password window did not appear");
                std::thread::sleep(Duration::from_millis(10));
            };
            let edit = unsafe { FindWindowExW(window, null_mut(), w!("EDIT"), null()) };
            let result = verify_appearance(window, edit);
            unsafe { PostMessageW(window, WM_CLOSE, 0, 0) };
            result
        });
        let result = password_prompt("appearance-test");
        worker
            .join()
            .map_err(|_| anyhow!("Appearance test worker panicked"))??;
        anyhow::ensure!(
            result.is_err_and(|error| error.is::<PasswordCancelled>()),
            "Cancelling the password window did not exit cleanly"
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop and notification area"]
    fn tray_shell_loads_embedded_icon_and_exits_after_worker_shutdown() -> Result<()> {
        use std::time::{Duration, Instant};

        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (events, receiver) = std::sync::mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let worker = std::thread::spawn(move || -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(5);
            let window = loop {
                let window = find_test_window("AppStream.LogWindow");
                if !window.is_null() {
                    break window;
                }
                anyhow::ensure!(Instant::now() < deadline, "Log window did not appear");
                std::thread::sleep(Duration::from_millis(10));
            };
            events.send(Event::Streaming)?;
            while unsafe { IsWindowVisible(window) } != 0 {
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "Streaming did not hide the window"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            // Wait for the first timer tick even if the newly-created window was not yet shown.
            std::thread::sleep(Duration::from_millis(250));
            unsafe { SendMessageW(window, TRAY_MESSAGE, 1, WM_LBUTTONUP as isize) };
            anyhow::ensure!(
                unsafe { IsWindowVisible(window) } != 0,
                "Log did not reopen"
            );
            unsafe { SendMessageW(window, WM_COMMAND, CLOSE as usize, 0) };
            anyhow::ensure!(
                unsafe { IsWindowVisible(window) } == 0,
                "Close did not hide the log"
            );
            anyhow::ensure!(
                !worker_cancelled.load(Ordering::SeqCst),
                "Close stopped the app"
            );
            anyhow::ensure!(
                matches!(
                    received.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                ),
                "Close sent a worker command"
            );
            unsafe { SendMessageW(window, WM_COMMAND, EXIT as usize, 0) };
            anyhow::ensure!(
                matches!(received.blocking_recv(), Some(Command::Close)),
                "Exit was not delivered"
            );
            anyhow::ensure!(
                worker_cancelled.load(Ordering::SeqCst),
                "Exit did not cancel selection"
            );
            anyhow::ensure!(
                unsafe { IsWindow(window) } != 0,
                "GUI exited before worker completion"
            );
            drop(events);
            Ok(())
        });
        let result = run(commands, receiver, cancelled);
        let worker_result = worker
            .join()
            .map_err(|_| anyhow!("GUI test worker panicked"))?;
        result?;
        worker_result
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn desktop_visibility_commands_and_worker_shutdown() -> Result<()> {
        let class = Class::register("AppStream.GuiTest.LogWindow", Some(shell_proc))?;
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (events, receiver) = std::sync::mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let shell = Box::new(RefCell::new(Shell {
            window: null_mut(),
            status: null_mut(),
            log: null_mut(),
            change: null_mut(),
            close: null_mut(),
            icon: null_mut(),
            tray: false,
            taskbar_created: 0,
            commands,
            events: receiver,
            cancelled: cancelled.clone(),
            closing: false,
            worker_done: false,
            can_change: false,
            revision: None,
        }));
        unsafe {
            let window = CreateWindowExW(
                WS_EX_CONTROLPARENT,
                class.name.as_ptr(),
                wide("App Stream GUI desktop test").as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                640,
                400,
                null_mut(),
                null_mut(),
                class.instance,
                &*shell as *const RefCell<Shell> as *const _,
            );
            if window.is_null() {
                return Err(win_error("CreateWindowExW (desktop test)"));
            }
            {
                let mut shell = shell.borrow_mut();
                shell.status = control(window, "STATIC", "Selecting", 0, 0)?;
                shell.log = control(
                    window,
                    "EDIT",
                    "",
                    WS_VSCROLL | ES_MULTILINE as u32 | ES_READONLY as u32,
                    0,
                )?;
                shell.change = control(window, "BUTTON", "Change target", 0, CHANGE)?;
                shell.close = control(window, "BUTTON", "Close", 0, CLOSE)?;
                shell.enable_change(false);
                shell.layout();
            }
            ShowWindow(window, SW_SHOW);
            assert_ne!(IsWindowVisible(window), 0);
            assert_eq!(
                GetWindowLongPtrW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW,
                0
            );

            events.send(Event::Streaming)?;
            shell.borrow_mut().poll();
            assert_eq!(IsWindowVisible(window), 0);
            assert!(shell.borrow().can_change);

            crate::logging::write("GUI desktop test: live log marker".into());
            shell.borrow_mut().poll();
            let log = shell.borrow().log;
            let mut text = vec![0u16; GetWindowTextLengthW(log) as usize + 1];
            let count = GetWindowTextW(log, text.as_mut_ptr(), text.len() as i32);
            assert!(
                String::from_utf16_lossy(&text[..count as usize])
                    .contains("GUI desktop test: live log marker")
            );

            // Use the same WM_COMMAND dispatch as the tray's log menu action.
            SendMessageW(window, WM_COMMAND, LOG as usize, 0);
            assert_ne!(IsWindowVisible(window), 0);
            verify_appearance(window, shell.borrow().change)?;
            SendMessageW(window, WM_CLOSE, 0, 0);
            assert_eq!(IsWindowVisible(window), 0);
            assert!(!cancelled.load(Ordering::SeqCst));
            assert!(!shell.borrow().closing);
            assert!(matches!(
                received.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            SendMessageW(window, TRAY_MESSAGE, 1, WM_LBUTTONUP as isize);
            assert_ne!(IsWindowVisible(window), 0);
            SendMessageW(window, WM_COMMAND, CLOSE as usize, 0);
            assert_eq!(IsWindowVisible(window), 0);
            assert!(!cancelled.load(Ordering::SeqCst));
            assert!(!shell.borrow().closing);
            assert!(matches!(
                received.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            SendMessageW(window, TRAY_MESSAGE, 1, WM_LBUTTONDBLCLK as isize);
            assert_ne!(IsWindowVisible(window), 0);

            events.send(Event::Stopped)?;
            shell.borrow_mut().poll();
            assert_ne!(IsWindowVisible(window), 0);
            SendMessageW(window, WM_COMMAND, CHANGE as usize, 0);
            assert!(matches!(received.try_recv(), Ok(Command::ChangeTarget)));
            assert!(!shell.borrow().can_change);
            assert!(!cancelled.load(Ordering::SeqCst));

            events.send(Event::Streaming)?;
            shell.borrow_mut().poll();
            SendMessageW(window, WM_COMMAND, EXIT as usize, 0);
            assert!(cancelled.load(Ordering::SeqCst));
            assert!(matches!(received.try_recv(), Ok(Command::Close)));
            assert_eq!(IsWindowEnabled(shell.borrow().change), 0);
            assert_eq!(IsWindowEnabled(shell.borrow().close), 0);
            assert_ne!(IsWindow(window), 0);
            assert!(!shell.borrow().worker_done);

            events.send(Event::Stopped)?;
            shell.borrow_mut().poll();
            assert_eq!(IsWindowVisible(window), 0);
            assert_ne!(IsWindow(window), 0);
            drop(events);
            shell.borrow_mut().poll();
            assert!(shell.borrow().worker_done);
            assert_eq!(IsWindow(window), 0);
            let mut quit: MSG = zeroed();
            let mut found_quit = false;
            // PostQuitMessage is low-priority; drain queued layout/paint messages first.
            while PeekMessageW(&mut quit, null_mut(), 0, 0, PM_REMOVE) != 0 {
                if quit.message == WM_QUIT {
                    found_quit = true;
                    break;
                }
                TranslateMessage(&quit);
                DispatchMessageW(&quit);
            }
            assert!(found_quit);
        }
        Ok(())
    }
}
