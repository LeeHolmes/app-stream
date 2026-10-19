use std::{
    io::{BufRead, BufReader, Write},
    os::windows::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tokio::sync::{oneshot, watch};

use crate::{
    Args,
    capture::WindowCapture,
    media::{self, Activity, Media, Parameters},
};

pub struct Pipeline {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    activity: Arc<Activity>,
    #[cfg(test)]
    captures: Arc<std::sync::atomic::AtomicU64>,
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.activity.wake();
        if let Some(mut child) = self.child.lock().expect("Encoder mutex poisoned").take() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                _ => {
                    if let Err(error) = child.kill() {
                        eprintln!("Unable to stop FFmpeg: {error}");
                    }
                }
            }
            if let Err(error) = child.wait() {
                eprintln!("Unable to reap FFmpeg: {error}");
            }
        }
    }
}

impl Pipeline {
    pub fn start(
        args: &Args,
        selected: Option<crate::selector::SelectedWindow>,
        media: Media,
        parameters: watch::Sender<Option<Parameters>>,
    ) -> (
        Self,
        oneshot::Receiver<Result<()>>,
        oneshot::Receiver<Result<()>>,
    ) {
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        #[cfg(test)]
        let captures = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pipeline = Self {
            stop: stop.clone(),
            child: child.clone(),
            activity: media.activity.clone(),
            #[cfg(test)]
            captures: captures.clone(),
        };
        let (capture_tx, capture_rx) = oneshot::channel();
        let (encoder_tx, encoder_rx) = oneshot::channel();
        let process = args.process.clone();
        let pid = args.pid;
        let executable = args.ffmpeg.clone();
        let fps = args.fps;
        let crf = args.crf;
        thread::spawn(move || {
            let mut encoder_tx = Some(encoder_tx);
            let result = (|| -> Result<()> {
                let mut capture = match (selected, process.as_deref(), pid) {
                    (Some(window), _, _) => WindowCapture::open_window(window.handle, window.pid)?,
                    (None, Some(process), pid) => WindowCapture::open(process, pid)?,
                    (None, None, Some(pid)) => WindowCapture::open_pid(pid)?,
                    _ => bail!("No capture target was selected"),
                };
                eprintln!("Capturing {}", capture.description());
                let dimensions = format!("{}x{}", capture.width(), capture.height());
                let mut encoder = Command::new(&executable)
                    .creation_flags(0x08000000) // CREATE_NO_WINDOW: FFmpeg must not open a console.
                    .args([
                        "-hide_banner", "-loglevel", "warning", "-nostdin",
                        "-f", "rawvideo", "-pixel_format", "bgra",
                        "-video_size", &dimensions, "-framerate", &fps.to_string(),
                        "-i", "pipe:0", "-an", "-c:v", "libx264",
                        "-preset", "ultrafast", "-tune", "zerolatency",
                        "-crf", &crf.to_string(), "-pix_fmt", "yuv420p",
                        "-g", &fps.to_string(), "-keyint_min", &fps.to_string(),
                        "-sc_threshold", "0", "-bf", "0",
                        "-x264-params", "aud=1:repeat-headers=1",
                        "-flush_packets", "1", "-f", "h264", "pipe:1",
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .with_context(|| format!("Starting FFmpeg at {executable:?}; install an FFmpeg build with libx264 or set --ffmpeg"))?;
                let mut input = encoder.stdin.take().context("Missing FFmpeg stdin")?;
                let output = encoder.stdout.take().context("Missing FFmpeg stdout")?;
                let errors = encoder.stderr.take().context("Missing FFmpeg stderr")?;
                thread::spawn(move || {
                    for line in BufReader::new(errors).lines() {
                        match line {
                            Ok(line) => eprintln!("FFmpeg: {line}"),
                            Err(error) => {
                                eprintln!("Reading FFmpeg diagnostics failed: {error}");
                                break;
                            }
                        }
                    }
                });
                // Hold the lock only for process ownership, never across pipe I/O.
                {
                    let mut slot = child.lock().expect("Encoder mutex poisoned");
                    if stop.load(Ordering::Relaxed) {
                        encoder.kill().context("Stopping FFmpeg during startup")?;
                        encoder.wait().context("Reaping FFmpeg during startup")?;
                        return Ok(());
                    }
                    *slot = Some(encoder);
                }
                let reader_stop = stop.clone();
                let parameters_ready = media.parameters.clone();
                let activity = media.activity.clone();
                let encoder_tx = encoder_tx
                    .take()
                    .context("Encoder reader already started")?;
                thread::spawn(move || {
                    let result = media::read_h264(output, fps, media, parameters);
                    if !reader_stop.load(Ordering::Relaxed) {
                        let _ = encoder_tx.send(result);
                    }
                });
                let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
                let mut deadline = Instant::now();
                let mut idle = false;
                while !stop.load(Ordering::Relaxed) {
                    let warmed_up = parameters_ready.borrow().is_some();
                    if warmed_up && !activity.wait(Duration::ZERO) {
                        idle = true;
                        if !activity.wait(Duration::from_secs(1)) {
                            capture.validate_window()?;
                            continue;
                        }
                    }
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if idle {
                        idle = false;
                        deadline = Instant::now();
                    }
                    let Some(frame) = capture.capture()? else {
                        // Monitor/KVM changes may temporarily minimize or hide the window.
                        thread::sleep(Duration::from_millis(250));
                        deadline = Instant::now();
                        continue;
                    };
                    #[cfg(test)]
                    captures.fetch_add(1, Ordering::Relaxed);
                    input
                        .write_all(&frame)
                        .context("Sending captured frame to FFmpeg (see FFmpeg errors above)")?;
                    deadline += interval;
                    let now = Instant::now();
                    if deadline > now {
                        thread::sleep(deadline - now);
                    } else {
                        // Do not burst duplicate frames after a slow window render.
                        deadline = now;
                    }
                }
                Ok(())
            })();
            if !stop.load(Ordering::Relaxed) {
                let _ = capture_tx.send(result);
            }
            drop(encoder_tx);
        });
        (pipeline, capture_rx, encoder_rx)
    }
}

pub fn unexpected_completion(
    result: std::result::Result<Result<()>, oneshot::error::RecvError>,
    component: &str,
) -> Result<()> {
    result
        .with_context(|| format!("{component} worker stopped without a result"))?
        .with_context(|| format!("{component} failed"))?;
    bail!("{component} stopped unexpectedly")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use tokio::{net::TcpListener, process::Command as AsyncCommand, time::timeout};
    use windows_sys::{Win32::UI::WindowsAndMessaging::*, core::w};

    struct Fixture {
        handle: usize,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            unsafe { PostMessageW(self.handle as _, WM_APP, 0, 0) };
            self.thread.take().unwrap().join().unwrap();
        }
    }

    async fn show_fixture(handle: usize, command: i32) {
        assert_ne!(
            unsafe { PostMessageW(handle as _, WM_APP + 2, command as usize, 0) },
            0
        );
        timeout(Duration::from_secs(2), async {
            loop {
                let matches = unsafe {
                    match command {
                        SW_MINIMIZE => IsIconic(handle as _) != 0,
                        SW_HIDE => IsWindowVisible(handle as _) == 0,
                        _ => IsIconic(handle as _) == 0 && IsWindowVisible(handle as _) != 0,
                    }
                };
                if matches {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Fixture visibility transition timed out");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires FFmpeg/libx264 and an interactive desktop; creates a window"]
    async fn pipeline_idles_and_resumes_for_viewers() {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let resized = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let fixture_resized = resized.clone();
        let fixture_thread = thread::spawn(move || {
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    w!("BUTTON"),
                    w!("App Stream idle pipeline fixture"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    100,
                    100,
                    320,
                    240,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            ready_tx.send(hwnd as usize).unwrap();
            if hwnd.is_null() {
                return;
            }
            let mut message = unsafe { std::mem::zeroed() };
            while unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) } > 0 {
                if message.message == WM_APP {
                    break;
                }
                if message.message == WM_APP + 1 {
                    assert_ne!(
                        unsafe {
                            SetWindowPos(
                                hwnd,
                                std::ptr::null_mut(),
                                0,
                                0,
                                message.wParam as i32,
                                message.lParam as i32,
                                SWP_NOMOVE | SWP_NOZORDER,
                            )
                        },
                        0
                    );
                    fixture_resized.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if message.message == WM_APP + 2 {
                    unsafe { ShowWindow(hwnd, message.wParam as i32) };
                    continue;
                }
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            unsafe { DestroyWindow(hwnd) };
        });
        let handle = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let fixture = Fixture {
            handle,
            thread: Some(fixture_thread),
        };
        assert_ne!(handle, 0, "Creating idle pipeline fixture failed");
        let args =
            Args::try_parse_from(["app-stream", "--username", "viewer", "--fps", "10"]).unwrap();
        let (media, parameters) = media::channel();
        let mut ready = media.parameters.clone();
        let activity = media.activity.clone();
        let (pipeline, mut capture_result, mut encoder_result) = Pipeline::start(
            &args,
            Some(crate::selector::SelectedWindow {
                handle,
                pid: std::process::id(),
            }),
            media.clone(),
            parameters,
        );
        timeout(Duration::from_secs(15), async {
            while ready.borrow().is_none() {
                ready.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (shutdown, rx) = watch::channel(false);
        let server = tokio::spawn(crate::rtsp::serve(
            listener,
            media,
            crate::rtsp::Credentials {
                username: "viewer".into(),
                password: "test-password".into(),
                legacy_digest: false,
            },
            rx,
        ));

        for attempt in 0..2 {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert!(!activity.wait(Duration::ZERO));
            let before = pipeline.captures.load(Ordering::Relaxed);
            show_fixture(handle, if attempt == 0 { SW_MINIMIZE } else { SW_HIDE }).await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert!(
                matches!(
                    capture_result.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ),
                "Minimizing or hiding the idle target must not stop capture"
            );
            show_fixture(handle, SW_RESTORE).await;
            assert_ne!(
                unsafe { PostMessageW(handle as _, WM_APP + 1, 640 + attempt * 100, 360) },
                0
            );
            timeout(Duration::from_secs(2), async {
                while resized.load(Ordering::Relaxed) < (attempt as u64 * 2 + 1) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert_eq!(
                pipeline.captures.load(Ordering::Relaxed),
                before,
                "No PrintWindow calls or encoder input frames are allowed while idle"
            );
            let decoder = timeout(
                Duration::from_secs(20),
                AsyncCommand::new("ffmpeg")
                    .kill_on_drop(true)
                    .args([
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-rtsp_transport",
                        "tcp",
                        "-i",
                        &format!("rtsp://viewer:test-password@127.0.0.1:{port}/app"),
                        "-frames:v",
                        "30",
                        "-an",
                        "-f",
                        "framemd5",
                        "pipe:1",
                    ])
                    .output(),
            );
            let resize_during_play = async {
                timeout(Duration::from_secs(10), async {
                    while pipeline.captures.load(Ordering::Relaxed) < before + 5 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    show_fixture(handle, if attempt == 0 { SW_MINIMIZE } else { SW_HIDE }).await;
                    // Let any in-flight PrintWindow finish before measuring the paused interval.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let paused_count = pipeline.captures.load(Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(700)).await;
                    assert_eq!(
                        pipeline.captures.load(Ordering::Relaxed),
                        paused_count,
                        "Temporarily unavailable windows must not produce frames"
                    );
                    assert!(
                        activity.wait(Duration::ZERO),
                        "The same viewer must remain connected"
                    );
                    show_fixture(handle, SW_RESTORE).await;
                    assert_ne!(
                        unsafe {
                            PostMessageW(handle as _, WM_APP + 1, 241, 401 + attempt as isize * 50)
                        },
                        0
                    );
                    while resized.load(Ordering::Relaxed) < attempt as u64 * 2 + 2 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("Active-stream resize timed out");
            };
            let (decoded, _) = tokio::join!(decoder, resize_during_play);
            let decoded = decoded.expect("RTSP decoder timed out").unwrap();
            assert!(
                decoded.status.success(),
                "{}",
                String::from_utf8_lossy(&decoded.stderr)
            );
            let text = String::from_utf8(decoded.stdout).unwrap();
            assert!(
                text.contains("#dimensions 0: 320x240"),
                "Stream resolution must remain stable: {text}"
            );
            assert_eq!(
                text.lines()
                    .filter(|line| !line.starts_with('#') && !line.is_empty())
                    .count(),
                30,
                "Attempt {attempt} must decode frames after waking from idle: {text}"
            );
            assert!(pipeline.captures.load(Ordering::Relaxed) > before);
            timeout(Duration::from_secs(5), async {
                while activity.wait(Duration::ZERO) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        assert!(matches!(
            capture_result.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            encoder_result.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        shutdown.send(true).unwrap();
        server.await.unwrap().unwrap();
        let child_pid = pipeline.child.lock().unwrap().as_ref().unwrap().id();
        drop(pipeline);
        eprintln!("Stopped idle pipeline and reaped encoder PID {child_pid}");
        drop(fixture);
    }
}
