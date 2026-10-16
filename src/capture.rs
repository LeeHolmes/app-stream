//! Window-only capture: no desktop/screen fallback is ever used.
//!
//! Among visible, unowned, non-tool windows, the largest client area wins
//! (then full-window area and HWND break ties). Multiple matching PIDs with
//! eligible windows require `--pid`. Capture includes the frame/non-client area;
//! output resolution remains fixed at the initial even dimensions. Resized
//! windows are scaled to fit with letterboxing, without restarting the stream.
//!
//! PrintWindow depends on the target application: GPU surfaces, protected content,
//! and applications that do not implement printing may return blank/incomplete
//! images even on success. It is synchronous and an unresponsive target can stall
//! capture. No attempt is made to bypass these restrictions.

use std::{
    collections::BTreeSet,
    ffi::c_void,
    mem::{size_of, zeroed},
    ptr::{null_mut, write_bytes},
};

use anyhow::{Context, Result, bail, ensure};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_NO_MORE_FILES, GetLastError, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM,
        RECT,
    },
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, COLORONCOLOR, CreateCompatibleDC, CreateDIBSection,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
        SelectObject, SetStretchBltMode, StretchBlt,
    },
    Storage::Xps::PrintWindow,
    System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    },
    UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GWL_EXSTYLE, GetClientRect, GetWindow, GetWindowLongW,
        GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
        IsWindowVisible, PW_RENDERFULLCONTENT, WS_EX_TOOLWINDOW,
    },
};

// Raw Win32 pointers intentionally keep capture thread-affine (!Send / !Sync).
pub struct WindowCapture {
    hwnd: HWND,
    pid: u32,
    title: String,
    source_width: u32,
    source_height: u32,
    width: u32,
    height: u32,
    source: Dib,
    dib: Dib,
}

impl WindowCapture {
    pub fn open(process: &str, pid: Option<u32>) -> Result<Self> {
        Self::open_matching(Some(process), pid)
    }

    pub fn open_pid(pid: u32) -> Result<Self> {
        Self::open_matching(None, Some(pid))
    }

    fn open_matching(process: Option<&str>, pid: Option<u32>) -> Result<Self> {
        ensure!(
            process
                .is_none_or(|name| !normalize_name(name).is_empty() && !name.contains(['\\', '/'])),
            "--process must be an executable name, not a path"
        );
        let pids = matching_processes(process, pid)?;
        ensure!(
            !pids.is_empty(),
            "No running process matches {process:?}{}",
            pid.map(|value| format!(" with PID {value}"))
                .unwrap_or_default()
        );

        let mut enumeration = Enumeration {
            pids: &pids,
            windows: Vec::new(),
        };
        // The callback and its borrowed state live only for this synchronous call.
        ensure!(
            unsafe { EnumWindows(Some(enumerate_window), &mut enumeration as *mut _ as LPARAM) }
                != 0,
            "EnumWindows failed: {}",
            std::io::Error::last_os_error()
        );
        let selected = choose_window(&enumeration.windows)?;
        Self::open_window(selected.hwnd as usize, selected.pid)
    }

    pub fn open_window(handle: usize, pid: u32) -> Result<Self> {
        let hwnd = handle as HWND;
        ensure!(
            unsafe { IsWindow(hwnd) } != 0,
            "Selected window was destroyed"
        );
        let mut actual_pid = 0;
        ensure!(
            unsafe { GetWindowThreadProcessId(hwnd, &mut actual_pid) } != 0 && actual_pid == pid,
            "Selected window no longer belongs to PID {pid}"
        );
        let (source_width, source_height) = window_size(hwnd)?;
        ensure!(
            unsafe { IsIconic(hwnd) } == 0,
            "Selected window (PID {}) is minimized; restore it before capture",
            pid
        );
        let width = source_width & !1;
        let height = source_height & !1;
        ensure!(
            width >= 2 && height >= 2,
            "Selected window is too small to encode"
        );
        let capture = Self {
            hwnd,
            pid,
            title: window_title(hwnd),
            source_width,
            source_height,
            width,
            height,
            source: Dib::new(source_width, source_height)?,
            dib: Dib::new(width, height)?,
        };
        capture.validate_window()?;
        Ok(capture)
    }

    /// None means the live window is temporarily minimized or hidden.
    pub fn capture(&mut self) -> Result<Option<Vec<u8>>> {
        self.validate_window()?;
        if !self.is_available() {
            return Ok(None);
        }
        let (width, height) = window_size(self.hwnd)?;
        if (width, height) != (self.source_width, self.source_height) {
            let source = Dib::new(width, height)?;
            self.source = source;
            self.source_width = width;
            self.source_height = height;
        }
        // Clear the reusable surface so a partial print cannot retain an old frame.
        unsafe { write_bytes(self.source.bits, 0, self.source.byte_len) };
        let printed = unsafe { PrintWindow(self.hwnd, self.source.dc, PW_RENDERFULLCONTENT) };
        self.validate_window()?;
        if !self.is_available() {
            return Ok(None);
        }
        ensure!(
            printed != 0,
            "PrintWindow failed for PID {}; this window may not support window capture",
            self.pid
        );
        ensure!(
            unsafe { GdiFlush() } != 0,
            "GdiFlush failed after PrintWindow"
        );
        // Resizing during PrintWindow is harmless: the next frame uses a new surface.
        self.validate_window()?;
        if !self.is_available() {
            return Ok(None);
        }
        self.source.scale_into(
            self.source_width,
            self.source_height,
            &self.dib,
            self.width,
            self.height,
        )?;
        let mut frame =
            unsafe { std::slice::from_raw_parts(self.dib.bits, self.dib.byte_len) }.to_vec();
        // BI_RGB printing does not define the alpha byte; expose opaque BGRA.
        for pixel in frame.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        Ok(Some(frame))
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn description(&self) -> String {
        format!(
            "PID {} | {} | {}x{} BGRA",
            self.pid, self.title, self.width, self.height
        )
    }

    pub(crate) fn validate_window(&self) -> Result<()> {
        ensure!(
            unsafe { IsWindow(self.hwnd) } != 0,
            "Captured window was destroyed"
        );
        let mut pid = 0;
        ensure!(
            unsafe { GetWindowThreadProcessId(self.hwnd, &mut pid) } != 0 && pid == self.pid,
            "Captured window no longer belongs to PID {}",
            self.pid
        );
        Ok(())
    }

    fn is_available(&self) -> bool {
        unsafe { IsIconic(self.hwnd) == 0 && IsWindowVisible(self.hwnd) != 0 }
    }
}

fn normalize_name(name: &str) -> String {
    let name = name.trim().to_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_owned()
}

fn names_match(requested: &str, executable: &str) -> bool {
    normalize_name(requested) == normalize_name(executable)
}

struct Snapshot(HANDLE);

impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn matching_processes(process: Option<&str>, requested_pid: Option<u32>) -> Result<BTreeSet<u32>> {
    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    ensure!(
        handle != INVALID_HANDLE_VALUE,
        "Process snapshot failed: {}",
        std::io::Error::last_os_error()
    );
    let snapshot = Snapshot(handle);
    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut pids = BTreeSet::new();
    let mut found = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    loop {
        if found == 0 {
            let error = unsafe { GetLastError() };
            ensure!(
                error == ERROR_NO_MORE_FILES,
                "Process enumeration failed: {}",
                std::io::Error::from_raw_os_error(error as i32)
            );
            break;
        }
        let end = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let executable = String::from_utf16_lossy(&entry.szExeFile[..end]);
        if requested_pid.is_none_or(|pid| pid == entry.th32ProcessID)
            && process.is_none_or(|name| names_match(name, &executable))
        {
            pids.insert(entry.th32ProcessID);
        }
        found = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    Ok(pids)
}

struct Candidate {
    hwnd: HWND,
    pid: u32,
    client_area: u64,
    window_area: u64,
}

struct Enumeration<'a> {
    pids: &'a BTreeSet<u32>,
    windows: Vec<Candidate>,
}

unsafe extern "system" fn enumerate_window(hwnd: HWND, parameter: LPARAM) -> i32 {
    // EnumWindows supplies only our stack-owned Enumeration pointer.
    let enumeration = unsafe { &mut *(parameter as *mut Enumeration<'_>) };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if !enumeration.pids.contains(&pid)
        || unsafe { IsWindowVisible(hwnd) } == 0
        || !unsafe { GetWindow(hwnd, GW_OWNER) }.is_null()
        || unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOOLWINDOW != 0
    {
        return 1;
    }
    let mut client: RECT = unsafe { zeroed() };
    let mut window: RECT = unsafe { zeroed() };
    if unsafe { GetClientRect(hwnd, &mut client) } != 0
        && unsafe { GetWindowRect(hwnd, &mut window) } != 0
        && rect_area(&client) > 0
        && rect_area(&window) > 0
    {
        enumeration.windows.push(Candidate {
            hwnd,
            pid,
            client_area: rect_area(&client),
            window_area: rect_area(&window),
        });
    }
    1
}

fn rect_area(rect: &RECT) -> u64 {
    let width = (i64::from(rect.right) - i64::from(rect.left)).max(0) as u64;
    let height = (i64::from(rect.bottom) - i64::from(rect.top)).max(0) as u64;
    width * height
}

fn choose_window(windows: &[Candidate]) -> Result<&Candidate> {
    let pids: BTreeSet<u32> = windows.iter().map(|window| window.pid).collect();
    if pids.len() > 1 {
        let list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("Multiple matching processes have candidate windows (PIDs: {list}); select --pid");
    }
    windows
        .iter()
        .max_by_key(|window| (window.client_area, window.window_area, window.hwnd as usize))
        .context("No visible, unowned, non-tool window with a nonempty client area matches")
}

fn window_size(hwnd: HWND) -> Result<(u32, u32)> {
    let mut rect: RECT = unsafe { zeroed() };
    ensure!(
        unsafe { GetWindowRect(hwnd, &mut rect) } != 0,
        "Cannot read window dimensions: {}",
        std::io::Error::last_os_error()
    );
    let width = i64::from(rect.right) - i64::from(rect.left);
    let height = i64::from(rect.bottom) - i64::from(rect.top);
    ensure!(
        width > 0 && height > 0 && width <= i32::MAX as i64 && height <= i32::MAX as i64,
        "Window has invalid dimensions {width}x{height}"
    );
    Ok((width as u32, height as u32))
}

fn window_title(hwnd: HWND) -> String {
    // Bound allocation and strip terminal control characters from the description.
    let mut buffer = [0u16; 1024];
    let length = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    original: HGDIOBJ,
    bits: *mut u8,
    byte_len: usize,
}

impl Dib {
    fn scale_into(
        &self,
        width: u32,
        height: u32,
        output: &Dib,
        output_width: u32,
        output_height: u32,
    ) -> Result<()> {
        let (scaled_width, scaled_height) = fit_size(width, height, output_width, output_height);
        unsafe { write_bytes(output.bits, 0, output.byte_len) };
        ensure!(
            unsafe { SetStretchBltMode(output.dc, COLORONCOLOR) } != 0,
            "Setting capture scaling mode failed"
        );
        ensure!(
            unsafe {
                StretchBlt(
                    output.dc,
                    ((output_width - scaled_width) / 2) as i32,
                    ((output_height - scaled_height) / 2) as i32,
                    scaled_width as i32,
                    scaled_height as i32,
                    self.dc,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    SRCCOPY,
                )
            } != 0,
            "Scaling resized window into stream frame failed: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            unsafe { GdiFlush() } != 0,
            "GdiFlush failed after scaling window"
        );
        Ok(())
    }

    fn new(width: u32, height: u32) -> Result<Self> {
        let byte_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|&bytes| bytes <= isize::MAX as usize)
            .context("Capture dimensions overflow the addressable BGRA buffer size")?;
        let mut dib = Self {
            dc: unsafe { CreateCompatibleDC(null_mut()) },
            bitmap: null_mut(),
            original: null_mut(),
            bits: null_mut(),
            byte_len,
        };
        ensure!(!dib.dc.is_null(), "CreateCompatibleDC failed");
        let mut info: BITMAPINFO = unsafe { zeroed() };
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..unsafe { zeroed() }
        };
        let mut bits: *mut c_void = null_mut();
        dib.bitmap =
            unsafe { CreateDIBSection(dib.dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0) };
        ensure!(
            !dib.bitmap.is_null() && !bits.is_null(),
            "CreateDIBSection failed"
        );
        dib.bits = bits.cast();
        let original = unsafe { SelectObject(dib.dc, dib.bitmap) };
        ensure!(
            !original.is_null() && original as isize != -1,
            "Selecting capture bitmap into memory DC failed"
        );
        dib.original = original;
        Ok(dib)
    }
}

fn fit_size(width: u32, height: u32, output_width: u32, output_height: u32) -> (u32, u32) {
    if u64::from(width) * u64::from(output_height) > u64::from(height) * u64::from(output_width) {
        (
            output_width,
            (u64::from(height) * u64::from(output_width) / u64::from(width)).max(1) as u32,
        )
    } else {
        (
            (u64::from(width) * u64::from(output_height) / u64::from(height)).max(1) as u32,
            output_height,
        )
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            if !self.original.is_null() {
                SelectObject(self.dc, self.original);
            }
            // Releasing the DC first also unselects the bitmap if restoration fails.
            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(pid: u32, client_area: u64, window_area: u64) -> Candidate {
        Candidate {
            hwnd: null_mut(),
            pid,
            client_area,
            window_area,
        }
    }

    #[test]
    fn executable_matching_is_case_insensitive_with_optional_suffix() {
        assert!(names_match("Notepad", "NOTEPAD.EXE"));
        assert!(names_match("NOTEPAD.exe", "notepad.exe"));
        assert!(names_match("  notepad  ", "notepad.exe"));
        assert!(!names_match("note", "notepad.exe"));
        assert!(!names_match("notepad.exe.exe", "notepad.exe"));
    }

    #[test]
    fn selection_prefers_client_area_then_full_window_area() {
        let windows = [
            candidate(7, 100, 500),
            candidate(7, 200, 300),
            candidate(7, 200, 400),
        ];
        let selected = choose_window(&windows).unwrap();
        assert_eq!(selected.client_area, 200);
        assert_eq!(selected.window_area, 400);
    }

    #[test]
    fn selection_rejects_multiple_pids_even_if_one_window_is_larger() {
        let windows = [candidate(22, 100, 200), candidate(11, 1000, 2000)];
        let error = choose_window(&windows).err().unwrap().to_string();
        assert!(error.contains("11, 22"));
        assert!(error.contains("--pid"));
    }

    #[test]
    fn selection_requires_a_candidate() {
        assert!(choose_window(&[]).is_err());
    }

    #[test]
    fn dib_allocates_a_writable_top_down_bgra_surface() {
        let dib = Dib::new(4, 2).unwrap();
        assert_eq!(dib.byte_len, 32);
        unsafe {
            write_bytes(dib.bits, 0x5a, dib.byte_len);
            assert_eq!(
                std::slice::from_raw_parts(dib.bits, dib.byte_len),
                &[0x5a; 32]
            );
        }
    }

    #[test]
    fn resized_window_fits_without_changing_aspect_ratio() {
        assert_eq!(fit_size(2863, 1483, 2862, 1482), (2861, 1482));
        assert_eq!(fit_size(1918, 1006, 2862, 1482), (2825, 1482));
        assert_eq!(fit_size(400, 200, 320, 240), (320, 160));
        assert_eq!(fit_size(200, 400, 320, 240), (120, 240));
        assert_eq!(fit_size(1, 10_000, 320, 240), (1, 240));
    }

    #[test]
    fn scaling_preserves_full_window_content_and_clears_letterbox_bars() {
        let source = Dib::new(4, 2).unwrap();
        let output = Dib::new(8, 8).unwrap();
        let source_bytes = unsafe { std::slice::from_raw_parts_mut(source.bits, source.byte_len) };
        for row in source_bytes.as_chunks_mut::<16>().0 {
            for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                pixel.copy_from_slice(if x < 2 {
                    &[0, 0, 255, 255]
                } else {
                    &[255, 0, 0, 255]
                });
            }
        }
        unsafe { write_bytes(output.bits, 0xff, output.byte_len) };
        source.scale_into(4, 2, &output, 8, 8).unwrap();
        let bytes = unsafe { std::slice::from_raw_parts(output.bits, output.byte_len) };
        for (y, row) in bytes.as_chunks::<32>().0.iter().enumerate() {
            for (x, pixel) in row.as_chunks::<4>().0.iter().enumerate() {
                let expected = if !(2..6).contains(&y) {
                    [0, 0, 0]
                } else if x < 4 {
                    [0, 0, 255]
                } else {
                    [255, 0, 0]
                };
                assert_eq!(pixel[..3], expected, "Unexpected pixel at {x},{y}");
            }
        }
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop; creates a visible test window"]
    fn captures_owned_window_recovers_from_resize_minimization_and_hiding() {
        use windows_sys::{
            Win32::UI::WindowsAndMessaging::{
                CW_USEDEFAULT, CreateWindowExW, DestroyWindow, SW_HIDE, SW_MINIMIZE, SW_RESTORE,
                SW_SHOW, SWP_NOMOVE, SWP_NOZORDER, SetWindowPos, ShowWindow, WS_OVERLAPPEDWINDOW,
                WS_VISIBLE,
            },
            core::w,
        };

        struct TestWindow(HWND);
        impl Drop for TestWindow {
            fn drop(&mut self) {
                unsafe { DestroyWindow(self.0) };
            }
        }

        let window = TestWindow(unsafe {
            CreateWindowExW(
                0,
                w!("STATIC"),
                w!("App Stream capture integration fixture"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                321,
                241,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        });
        assert!(!window.0.is_null(), "CreateWindowExW failed");
        let executable = std::env::current_exe().unwrap();
        let name = executable.file_name().unwrap().to_str().unwrap();
        let mut capture = WindowCapture::open(name, Some(std::process::id())).unwrap();
        assert_eq!(capture.hwnd, window.0);
        assert_eq!(capture.width() % 2, 0);
        assert_eq!(capture.height() % 2, 0);
        let frame = capture.capture().unwrap().unwrap();
        assert_eq!(
            frame.len(),
            (capture.width() * capture.height() * 4) as usize
        );
        assert!(frame.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255));

        unsafe { ShowWindow(window.0, SW_MINIMIZE) };
        capture.validate_window().unwrap();
        assert!(capture.capture().unwrap().is_none());
        unsafe { ShowWindow(window.0, SW_RESTORE) };
        assert!(capture.capture().unwrap().is_some());
        unsafe { ShowWindow(window.0, SW_HIDE) };
        capture.validate_window().unwrap();
        assert!(capture.capture().unwrap().is_none());
        unsafe { ShowWindow(window.0, SW_SHOW) };
        assert!(capture.capture().unwrap().is_some());
        let output_size = (capture.width(), capture.height());
        for (width, height) in [
            (400, 300),
            (2863, 1483),
            (1918, 1006),
            (241, 401),
            (321, 241),
        ] {
            assert!(
                unsafe {
                    SetWindowPos(
                        window.0,
                        null_mut(),
                        0,
                        0,
                        width,
                        height,
                        SWP_NOMOVE | SWP_NOZORDER,
                    )
                } != 0
            );
            capture.validate_window().unwrap();
            let frame = capture.capture().unwrap().unwrap();
            assert_eq!(
                (capture.source_width, capture.source_height),
                (width as u32, height as u32)
            );
            assert_eq!((capture.width(), capture.height()), output_size);
            assert_eq!(frame.len(), (output_size.0 * output_size.1 * 4) as usize);
            assert!(frame.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255));
        }
        drop(window);
        assert!(
            capture
                .capture()
                .unwrap_err()
                .to_string()
                .contains("destroyed")
        );
    }
}
