#![cfg_attr(windows, windows_subsystem = "windows")]

macro_rules! eprintln {
    ($($arg:tt)*) => { crate::logging::write(format!($($arg)*)) };
}

mod logging;
mod media;
mod rtsp;

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod credentials;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod selector;

use anyhow::{Result, ensure};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "App Stream",
    version,
    about = "Stream a Windows process window over authenticated localhost RTSP/TCP"
)]
pub struct Args {
    /// Executable name; omit both process and pid to select a window with the mouse
    #[arg(long)]
    process: Option<String>,

    /// Capture this PID's main window; optionally also verify the process name
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pid: Option<u32>,

    /// IPv4 loopback RTSP port
    #[arg(long, default_value_t = 8554, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,

    /// RTSP viewer username
    #[arg(long, default_value = "viewer")]
    username: String,

    /// Delete this username's saved password and exit (next launch prompts again)
    #[arg(long, alias = "delete-keys")]
    delete_password: bool,

    /// Compatibility for older camera clients: Digest without qop/replay counters
    #[arg(long)]
    legacy_digest: bool,

    /// Capture frames per second
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=60))]
    fps: u32,

    /// x264 quality (lower is better, larger is smaller)
    #[arg(long, default_value_t = 23, value_parser = clap::value_parser!(u8).range(0..=51))]
    crf: u8,

    /// Path to FFmpeg, which must include the libx264 encoder
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
}

fn validate(args: &Args) -> Result<()> {
    ensure!(
        args.process
            .as_ref()
            .is_none_or(|name| !name.trim().is_empty()),
        "--process must not be empty"
    );
    ensure!(
        !args.username.is_empty() && !args.username.contains([':', '\r', '\n']),
        "--username must not be empty or contain a colon or newline"
    );
    Ok(())
}

#[cfg(windows)]
fn main() {
    if let Err(error) = desktop_main() {
        eprintln!("{error:#}");
        gui::message("App Stream - Error", &format!("{error:#}"), true);
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn desktop_main() -> Result<()> {
    use anyhow::Context;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error) => {
            let is_error = error.use_stderr();
            gui::message("App Stream", &error.to_string(), is_error);
            if is_error {
                std::process::exit(2);
            }
            return Ok(());
        }
    };
    validate(&args)?;
    ensure!(
        unsafe {
            windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        } != 0,
        "Setting per-monitor DPI awareness failed: {}",
        std::io::Error::last_os_error()
    );
    if args.delete_password {
        credentials::delete(&args.username)?;
        gui::message("App Stream", &logging::snapshot().1, false);
        return Ok(());
    }
    let password = match credentials::load_or_prompt(&args.username) {
        Ok(password) => password,
        Err(error) if error.is::<gui::PasswordCancelled>() => return Ok(()),
        Err(error) => return Err(error),
    };
    let runtime = tokio::runtime::Runtime::new().context("Creating streaming runtime")?;
    let (commands, commands_rx) = tokio::sync::mpsc::unbounded_channel();
    let (events, events_rx) = std::sync::mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let worker = std::thread::spawn(move || {
        runtime.block_on(run(args, password, commands_rx, events, worker_cancelled))
    });
    let result = gui::run(commands.clone(), events_rx, cancelled.clone());
    cancelled.store(true, Ordering::Relaxed);
    let _ = commands.send(gui::Command::Close);
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("Streaming worker panicked"))??;
    result
}

#[cfg(windows)]
async fn run(
    args: Args,
    password: String,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<gui::Command>,
    events: std::sync::mpsc::Sender<gui::Event>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    use std::sync::atomic::Ordering;

    let mut target = None;
    let mut pick = args.process.is_none() && args.pid.is_none();
    loop {
        if pick {
            let _ = events.send(gui::Event::Selecting);
            match selector::select(cancelled.clone()).await {
                Ok(Some(window)) => target = Some(window),
                Ok(None) => eprintln!("Window selection cancelled."),
                Err(error) => eprintln!("Window selection failed: {error:#}"),
            }
        }
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        if target.is_some() || args.process.is_some() || args.pid.is_some() {
            let result = stream(&args, target, &password, &mut commands, &events).await;
            match result {
                Ok(Some(gui::Command::ChangeTarget)) => {
                    pick = true;
                    continue;
                }
                Ok(Some(gui::Command::Close) | None) => return Ok(()),
                Err(error) => eprintln!("Streaming stopped: {error:#}"),
            }
        }
        let _ = events.send(gui::Event::Stopped);
        match commands.recv().await {
            Some(gui::Command::ChangeTarget) => pick = true,
            Some(gui::Command::Close) | None => return Ok(()),
        }
    }
}

#[cfg(windows)]
async fn stream(
    args: &Args,
    selected: Option<selector::SelectedWindow>,
    password: &str,
    commands: &mut tokio::sync::mpsc::UnboundedReceiver<gui::Command>,
    events: &std::sync::mpsc::Sender<gui::Event>,
) -> Result<Option<gui::Command>> {
    use anyhow::Context;
    use tokio::{net::TcpListener, sync::watch};

    let listener = TcpListener::bind(("127.0.0.1", args.port))
        .await
        .with_context(|| {
            format!(
                "Binding localhost port {} (is it already in use?)",
                args.port
            )
        })?;
    let credentials = rtsp::Credentials {
        username: args.username.clone(),
        password: password.to_owned(),
        legacy_digest: args.legacy_digest,
    };
    if args.legacy_digest {
        eprintln!(
            "Warning: legacy Digest compatibility enabled; nonce-count replay protection is unavailable. Keep this stream localhost-only."
        );
    }
    let (media, parameters) = media::channel();
    let (pipeline, capture_result, encoder_result) =
        encoder::Pipeline::start(args, selected, media.clone(), parameters);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let mut server = tokio::spawn(rtsp::serve(
        listener,
        media.clone(),
        credentials,
        shutdown_rx,
    ));
    let mut server_observed = false;
    let result = async {
        let mut ready = media.parameters.clone();
        tokio::select! {
            biased;
            result = capture_result => encoder::unexpected_completion(result, "Window capture").map(|()| None),
            result = encoder_result => encoder::unexpected_completion(result, "H.264 encoder").map(|()| None),
            result = &mut server => {
                server_observed = true;
                result.context("RTSP server task failed")?.map(|()| None)
            },
            command = commands.recv() => Ok(command),
            result = tokio::signal::ctrl_c() => result.context("Listening for Ctrl+C").map(|()| None),
            result = async {
                tokio::time::timeout(std::time::Duration::from_secs(15), async {
                    while ready.borrow().is_none() {
                        ready.changed().await.context("Encoder stopped during startup")?;
                    }
                    Ok::<_, anyhow::Error>(())
                }).await.context("No video produced within 15 seconds; check the window and FFmpeg")??;
                eprintln!("Ready: rtsp://127.0.0.1:{}/app (RTSP over TCP; authentication required)", args.port);
                let _ = events.send(gui::Event::Streaming);
                std::future::pending::<Result<Option<gui::Command>>>().await
            } => result,
        }
    }.await;
    let _ = shutdown.send(true);
    drop(pipeline);
    if !server_observed {
        server.await.context("Joining RTSP server")??;
    }
    result
}

#[cfg(not(windows))]
fn main() -> Result<()> {
    let args = Args::parse();
    validate(&args)?;
    anyhow::bail!("Window capture is currently supported only on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_allows_picker_pid_only_and_existing_process_modes() {
        let parse = |options: &[&str]| {
            Args::try_parse_from(
                ["app-stream", "--username", "viewer"]
                    .into_iter()
                    .chain(options.iter().copied()),
            )
            .unwrap()
        };
        let picker = parse(&[]);
        assert!(picker.process.is_none() && picker.pid.is_none());
        let pid = parse(&["--pid", "42"]);
        assert_eq!(pid.pid, Some(42));
        assert!(pid.process.is_none());
        let process = parse(&["--process", "notepad", "--pid", "42"]);
        assert_eq!(process.process.as_deref(), Some("notepad"));
        assert_eq!(process.pid, Some(42));
        assert!(
            Args::try_parse_from(["app-stream", "--username", "viewer", "--pid", "0"]).is_err()
        );
    }

    #[test]
    fn cli_can_delete_saved_password_without_starting_capture() {
        for option in ["--delete-password", "--delete-keys"] {
            let args =
                Args::try_parse_from(["app-stream", "--username", "viewer", option]).unwrap();
            assert!(args.delete_password);
            assert!(args.process.is_none() && args.pid.is_none());
        }
    }

    #[test]
    fn cli_rejects_password_parameter() {
        assert!(
            Args::try_parse_from(["app-stream", "--username", "viewer", "--password", "test"])
                .is_err()
        );
    }

    #[test]
    fn double_click_launch_uses_default_viewer_and_picker() {
        let args = Args::try_parse_from(["app-stream"]).unwrap();
        assert_eq!(args.username, "viewer");
        assert!(args.process.is_none() && args.pid.is_none());
        validate(&args).unwrap();
    }

    #[test]
    fn gui_startup_rejects_invalid_usernames_and_empty_process_names() {
        for username in ["", "user:name", "user\rname", "user\nname"] {
            let args = Args::try_parse_from(["app-stream", "--username", username]).unwrap();
            assert!(validate(&args).is_err());
        }
        let args = Args::try_parse_from(["app-stream", "--process", "  "]).unwrap();
        assert!(validate(&args).is_err());
    }

    #[test]
    fn icon_contains_every_requested_size_as_a_valid_png() {
        let icon = include_bytes!("..\\assets\\app-stream.ico");
        assert_eq!(&icon[..4], &[0, 0, 1, 0]);
        let sizes = [16u32, 20, 24, 32, 40, 48, 64, 96, 128, 256];
        assert_eq!(u16::from_le_bytes([icon[4], icon[5]]) as usize, sizes.len());
        for (index, size) in sizes.into_iter().enumerate() {
            let entry = &icon[6 + index * 16..][..16];
            let width = if entry[0] == 0 {
                256
            } else {
                u32::from(entry[0])
            };
            assert_eq!(width, size);
            assert_eq!(entry[0], entry[1]);
            let length = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
            let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
            let png = &icon[offset..offset + length];
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), size);
            assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), size);
        }
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires FFmpeg/libx264 and an interactive desktop; creates two windows"]
    async fn target_restart_reuses_port_disconnects_clients_and_closes_cleanly() {
        use std::{ptr::null_mut, time::Duration};
        use tokio::{
            io::AsyncReadExt,
            net::{TcpListener, TcpStream},
            time::timeout,
        };
        use windows_sys::{Win32::UI::WindowsAndMessaging::*, core::w};

        struct Fixture {
            handles: [usize; 2],
            thread: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                unsafe { PostMessageW(self.handles[0] as _, WM_APP, 0, 0) };
                self.thread.take().unwrap().join().unwrap();
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let handles = [100, 500].map(|x| unsafe {
                CreateWindowExW(
                    0,
                    w!("BUTTON"),
                    w!("App Stream restart fixture"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    x,
                    100,
                    320,
                    240,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                ) as usize
            });
            tx.send(handles).unwrap();
            let mut message = unsafe { std::mem::zeroed() };
            while unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } > 0 {
                if message.message == WM_APP {
                    break;
                }
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            for handle in handles {
                unsafe { DestroyWindow(handle as _) };
            }
        });
        let fixture = Fixture {
            handles: rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            thread: Some(thread),
        };
        assert!(fixture.handles.iter().all(|handle| *handle != 0));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        for (index, handle) in fixture.handles.into_iter().enumerate() {
            let args =
                Args::try_parse_from(["app-stream", "--port", &port.to_string(), "--fps", "10"])
                    .unwrap();
            let (commands, mut commands_rx) = tokio::sync::mpsc::unbounded_channel();
            let (events, events_rx) = std::sync::mpsc::channel();
            let task = tokio::spawn(async move {
                stream(
                    &args,
                    Some(selector::SelectedWindow {
                        handle,
                        pid: std::process::id(),
                    }),
                    "test-password",
                    &mut commands_rx,
                    &events,
                )
                .await
            });
            timeout(Duration::from_secs(20), async {
                loop {
                    if matches!(events_rx.try_recv(), Ok(gui::Event::Streaming)) {
                        break;
                    }
                    assert!(!task.is_finished(), "Stream failed before becoming ready");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            commands
                .send(if index == 0 {
                    gui::Command::ChangeTarget
                } else {
                    gui::Command::Close
                })
                .unwrap();
            let command = timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(matches!(
                (index, command),
                (0, Some(gui::Command::ChangeTarget)) | (1, Some(gui::Command::Close))
            ));
            assert_eq!(
                timeout(Duration::from_secs(5), client.read(&mut [0u8; 1]))
                    .await
                    .unwrap()
                    .unwrap(),
                0,
            );
        }
        let _released = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    }
}
