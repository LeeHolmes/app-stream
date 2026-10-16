mod media;
mod rtsp;

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod credentials;
#[cfg(windows)]
mod encoder;
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

    /// Required RTSP viewer username
    #[arg(long)]
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
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
    run(args).await
}

#[cfg(windows)]
async fn run(args: Args) -> Result<()> {
    use anyhow::Context;
    use tokio::{net::TcpListener, sync::watch};

    if args.delete_password {
        credentials::delete(&args.username)?;
        return Ok(());
    }
    let password = credentials::load_or_prompt(&args.username)?;

    ensure!(
        unsafe {
            windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        } != 0,
        "Setting per-monitor DPI awareness failed: {}",
        std::io::Error::last_os_error()
    );
    let listener = TcpListener::bind(("127.0.0.1", args.port))
        .await
        .with_context(|| {
            format!(
                "Binding localhost port {} (is it already in use?)",
                args.port
            )
        })?;
    let selected = if args.process.is_none() && args.pid.is_none() {
        selector::select().await?
    } else {
        None
    };
    if args.process.is_none() && args.pid.is_none() && selected.is_none() {
        eprintln!("Window selection cancelled.");
        return Ok(());
    }
    let credentials = rtsp::Credentials {
        username: args.username.clone(),
        password,
        legacy_digest: args.legacy_digest,
    };
    if args.legacy_digest {
        eprintln!(
            "Warning: legacy Digest compatibility enabled; nonce-count replay protection is unavailable. Keep this stream localhost-only."
        );
    }
    let (media, parameters) = media::channel();
    let (pipeline, capture_result, encoder_result) =
        encoder::Pipeline::start(&args, selected, media.clone(), parameters);
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
            result = capture_result => encoder::unexpected_completion(result, "Window capture"),
            result = encoder_result => encoder::unexpected_completion(result, "H.264 encoder"),
            result = &mut server => {
                server_observed = true;
                result.context("RTSP server task failed")?
            },
            result = tokio::signal::ctrl_c() => result.context("Listening for Ctrl+C"),
            result = async {
                tokio::time::timeout(std::time::Duration::from_secs(15), async {
                    while ready.borrow().is_none() {
                        ready.changed().await.context("Encoder stopped during startup")?;
                    }
                    Ok::<_, anyhow::Error>(())
                }).await.context("No video produced within 15 seconds; check the window and FFmpeg")??;
                eprintln!("Ready: rtsp://127.0.0.1:{}/app (RTSP over TCP; authentication required)", args.port);
                std::future::pending::<Result<()>>().await
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
async fn run(_args: Args) -> Result<()> {
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
}
