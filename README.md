# App Stream

A Windows Rust GUI application that captures a process's main window
and publishes H.264 video through an authenticated, localhost-only RTSP server.
Each instance streams one window at `rtsp://127.0.0.1:PORT/app`. App Stream and
its FFmpeg encoder run without console windows.

## Requirements

- Windows 10/11 with an interactive desktop session.
- A current stable Rust toolchain and the Visual Studio C++ build tools/Windows
  SDK to build the application (`rustup`'s Windows installer can guide setup).
- FFmpeg on `PATH`, built with the `libx264` encoder. Check with
  `ffmpeg -hide_banner -encoders`. Alternatively pass `--ffmpeg` with its full path.
- An RTSP client supporting Digest authentication and RTP interleaved over TCP,
  such as VLC, FFplay, or FFprobe.

## Build and run

```powershell
cargo build --release
.\target\release\app-stream.exe --process notepad.exe --port 8554 --username viewer
```

You can also double-click the executable: it uses username `viewer`, port `8554`,
and the visual window picker. Command-line options remain available for custom
settings and process/PID selection. Help and argument errors appear in dialogs.

On first launch for a username, enter the password in the masked GUI dialog. App
Stream saves it in **Windows Credential Manager** and reuses it on later launches
without prompting. The stream becomes available when the log reports `Ready`,
and the app hides in the system tray.

### System tray and client log

Right-click the App Stream tray icon to:

- **Change target**: select a different window using the red-outline picker.
  This restarts capture and the RTSP listener on the same port; existing viewers
  are disconnected and must reconnect. Pressing Esc keeps the previous target
  (and restarts its stream), if one was selected.
- **Connected client log**: open the live log, including authenticated client
  usernames, IP addresses/source ports, connection/disconnection events, and
  capture, FFmpeg, and server errors. A single left-click on the tray icon opens
  the log too (double-click also works).
- **Exit**: stop the listener, disconnect viewers, terminate/reap FFmpeg,
  remove the tray icon, and exit.

The log window's **Close** button and title-bar **X** both hide it to the tray;
neither stops streaming. To quit, right-click the tray icon and choose **Exit**. Startup
or capture failures show the log window rather than disappearing silently. Use
**Change target** to retry or the tray's **Exit** to quit. Cancelling the initial picker
also leaves the app available for retry. The log holds the latest 1,000 lines
in memory, with UTC times, and is not written to disk. Diagnostics never contain
passwords or authorization headers.

### Saved passwords

Passwords are stored in the current Windows user's Credential Manager using
the `App Stream` service and the RTSP username, using the operating system keyring. They are not saved in a plaintext configuration file.
Different RTSP usernames have separate entries; different ports and processes
using the same username share that username's saved password.

App Stream reads the saved password for `--username`. If none is saved, it uses a
masked GUI dialog and saves the new password for future launches. Passwords cannot
be supplied through command-line parameters or environment variables.

To delete a saved password and exit without selecting/capturing a window:

```powershell
.\target\release\app-stream.exe --username viewer --delete-password
```

The next normal launch prompts for a new password. `--delete-keys` is an alias,
deleting the specified username's App Stream
password. To change a password, delete it and run normally to enter a new one.
Credential-store read/write failures are reported rather than silently falling
back to a prompt or continuing without saving. Password spaces are preserved.

To choose a window visually, omit both `--process` and `--pid`:

```powershell
.\target\release\app-stream.exe --port 8554 --username viewer
```

After entering the password, move the mouse over a window. A thin red border
follows the window under the pointer. Left-click to select that exact window
(even if its process has other, larger windows). The selection click is consumed,
so it does not activate a button or other control in the target. The border
disappears before streaming starts. Press **Esc** to cancel.
Desktop backgrounds, taskbars, and tool windows are not selectable. The outline
supports multiple monitors, including monitors with negative screen coordinates.

To select a process's main window by PID alone:

```powershell
.\target\release\app-stream.exe --pid 1234 --username viewer
```

For unattended use, first run interactively under the same Windows account to
save the password. Subsequent launches reuse the saved password without prompting.

### Watch the stream

In VLC, enable **RTP over RTSP (TCP)** in the input/codec preferences (or launch
`vlc --rtsp-tcp rtsp://127.0.0.1:8554/app`), then enter the username and password
when prompted.

With FFplay:

```powershell
ffplay -rtsp_transport tcp 'rtsp://viewer:YOUR_PASSWORD@127.0.0.1:8554/app'
```

Percent-encode reserved characters in credentials when putting them in a URL.
Credentials in viewer URLs are visible to local process inspection; prefer
VLC's interactive prompt for normal use.

### Options

| Option | Purpose |
| --- | --- |
| `--process NAME` | Optional executable name, case-insensitive; `.exe` is optional |
| `--pid PID` | Select a process by PID; if `--process` is also supplied, both must match |
| `--port PORT` | Local IPv4 TCP port, default `8554` |
| `--username NAME` | Viewer username, default `viewer` |
| `--delete-password` | Delete saved password for `--username` and exit; alias `--delete-keys` |
| `--legacy-digest` | Opt-in Digest compatibility for older camera clients without `qop`/nonce counters |
| `--fps N` | Capture rate, 1-60, default `30` |
| `--crf N` | x264 quality, 0-51, default `23`; lower is better/larger |
| `--ffmpeg PATH` | FFmpeg executable, default `ffmpeg` |

Use different ports for simultaneous App Stream instances. Up to 16 connections
can connect to one server; slow viewers are disconnected instead of accumulating
unbounded video queues. Viewers start at the next keyframe (approximately one
second at the configured capture rate).

### Idle mode

After a brief startup warmup to obtain H.264 parameters for RTSP negotiation,
App Stream **pauses window capture and H.264 encoding whenever no viewers are
playing**. The capture worker sleeps rather than spinning, and FFmpeg remains
loaded but blocked waiting for input. A lightweight window-validity check runs
once per second so closing the target is still reported. Resizing
while idle is handled automatically when capture next resumes.

An authenticated RTSP `PLAY` request wakes capture automatically. Merely opening
a connection, negotiating the stream, or failing authentication does not keep
capture running. Capture remains active while any viewer is playing and pauses
again after the last viewer tears down, disconnects, times out, or is dropped as
too slow. Reconnecting viewers wait for the next keyframe; buffered frames from
before the idle period are not sent as the new first frame.

Idle, resume, and window-resize transitions are silent. The tray's client log records each
successfully authenticated connection once, including the username and client IP
address (for example, `RTSP client connected: username="viewer", IP=127.0.0.1`).
No additional options are needed, and authentication and the localhost-only
listener remain available while idle.

## Capture behavior and limitations

- Finds visible, unowned, non-tool top-level windows belonging to the matching
  process when using `--process` or `--pid`. The largest eligible window is
  treated as the main window. If more than one matching PID has an eligible
  window, specify `--pid`. With neither option, the interactive selector captures
  the exact top-level window clicked, rather than choosing by size.
- Captures the window through Win32 `PrintWindow`, not a screenshot of the
  desktop. Other windows covering it are not deliberately captured. Window
  borders/title bar are included. This is video only: no audio or cursor overlay.
- Resizing is handled automatically, including while streaming or idle. Capture
  reallocates its window surface as needed and scales the complete window to the
  stream's initial even-sized resolution. Aspect ratio is preserved with black
  letterbox/pillarbox bars when needed. The encoder and RTSP sessions stay running;
  viewers do not need to reconnect. Restart only if you want a different output
  resolution.
- Once capture has started, minimizing or temporarily hiding the target (for
  example during a KVM/monitor switch) pauses capture without stopping App Stream
  or closing its RTSP sessions. It checks availability four times per second
  while viewers are playing, without capturing or encoding unavailable windows.
  Restoring/showing the window resumes capture automatically, including any
  changed dimensions. Viewers may freeze or show "no signal" during the pause;
  viewers that disconnect can reconnect normally. No pause/resume status messages
  are printed.
- The target must be restored when initially selected. Closing/destroying the
  captured window stops the stream and opens the log with an explicit error;
  the tray remains available to select a new target or exit.
- Some GPU-rendered, browser, game, protected-video, and elevated windows do not
  support `PrintWindow` and can return black, stale, or incomplete content even
  when Windows reports success. This backend cannot detect all such cases.
  Windows Graphics Capture/Desktop Duplication is not implemented.
- Do not run elevated unless necessary. Use the same desktop/session and
  compatible privilege level as the target. `PrintWindow` is synchronous, so an
  unresponsive target can stall capture; the tray's **Exit** still stops the server and
  terminates the encoder.
- The configured FPS is a target, not a guarantee; large/slow windows may encode
  more slowly. Encoding is software-based (`libx264`, ultrafast/zerolatency).

## Networking and authentication

The server binds **only `127.0.0.1`**, never all interfaces, and does not open
UDP ports. Use `127.0.0.1` rather than an IPv6-only `localhost` resolution.
RTP and RTCP are interleaved on the authenticated RTSP TCP connection. UDP RTSP
transport is intentionally unsupported and receives `461 Unsupported Transport`.

All RTSP methods require RFC-style Digest authentication with MD5, a random
per-connection nonce, `qop=auth`, and increasing nonce counts. Empty passwords
are rejected. The opt-in `--legacy-digest` mode instead advertises Digest without
`qop` for older camera clients; credentials, nonce, method, and request URI are
still checked, but legacy requests have no nonce-count replay protection.
The default mode never silently falls back to legacy authentication.
Digest is for compatibility with common RTSP clients, not modern
transport encryption: RTSP/video are **not encrypted**, and captured Digest
exchanges can be subject to offline password guessing. Use a strong password.
Localhost is not a security boundary against privileged local software.
Do not proxy or forward this port to an untrusted network.

The tray's **Exit** command shuts down the listener, disconnects viewers, and
terminates/reaps FFmpeg. Capture/encoder failures stop streaming and show the
diagnostic log, with the tray available to retry. Credential-store and other
fatal startup errors appear in an error dialog.

### Camera clients / Blue Iris troubleshooting

A viewer's generic **No signal / Check Port/User/Password** message can mean
authentication, an incorrect stream path, or an unsupported transport; it does
not necessarily mean that the password is incorrect.

For Blue Iris or another camera viewer, configure:

- Address: `127.0.0.1` (the viewer must be on this same computer).
- RTSP port: `8554`, or the value passed with `--port`.
- Stream/video path: `/app` (not just `/`).
- Username: the value passed with `--username`.
- Password: the password supplied to App Stream, **not** your Windows password
  or the Blue Iris administrator password unless deliberately set the same.
- Video: generic RTSP H.264, with RTP over RTSP **TCP/interleaved** transport.
  UDP and RTSP-over-HTTP tunneling are not supported.

Some older RTSP camera clients use legacy Digest authentication without
`qop=auth`. If the client log reports that mismatch, restart App Stream with:

```powershell
.\target\release\app-stream.exe --port 8554 --username viewer --legacy-digest
```

This remains authenticated and localhost-only, but legacy requests have weaker
replay protection. Use it only when your viewer requires it. App Stream logs
authentication rejection reasons and `404`/`461` negotiation failures without
printing passwords, authorization headers, or credential-bearing URLs. A single
initial `401` challenge is normal in Digest authentication.

To separate server issues from viewer configuration, try a known RTSP/TCP client:

```powershell
ffplay -rtsp_transport tcp 'rtsp://viewer:YOUR_PASSWORD@127.0.0.1:8554/app'
```

If this works but the camera viewer does not, check the viewer's transport,
path, and authentication settings. Do not paste a real credential-bearing URL
into shared logs or screenshots.

## Validation

### Icon assets

The supplied screenshot is preserved in [assets/icon-source.png](assets/icon-source.png).
The generator removes its gray backdrop and unmattes the antialiased blue edges;
[assets/icon-transparent.png](assets/icon-transparent.png) is the transparent master.
The generated
[Windows icon](assets/app-stream.ico) contains 16, 20, 24, 32, 40, 48, 64, 96,
128, and 256 pixel images. Matching PNGs are in [assets](assets); the executable,
application windows, and notification area use the embedded icon. Regenerate the
set on Windows with:

```powershell
.\tools\Generate-Icons.ps1
```

The application and password windows use normal Windows title bars with the app
icon, Windows Common Controls v6 visual styles, and the current system UI font
scaled for the monitor's DPI. App Stream's own windows are excluded from the
picker by their window class, rather than using icon-less tool-window frames.

### Tests

```powershell
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

An opt-in integration test uses FFmpeg's generated test pattern and an actual
FFmpeg RTSP client to check rejection of incorrect credentials, Digest
authentication in both default and legacy modes, and decoding received video.
It requires FFmpeg with `libx264`
on `PATH`:

```powershell
cargo test ffmpeg_round_trip -- --ignored --nocapture
```

Desktop integration tests create their own windows to verify BGRA capture,
automatic resize scaling, recovery from minimization/hiding, errors on destruction,
red hover outlines, exact-window
selection, click consumption, PID-only capture, and selector cleanup/cancellation.
GUI tests verify tray lifecycle, normal title-bar icons, activated Windows visual
styles, and DPI-scaled system fonts in both the password and client-log windows.
An additional FFmpeg/desktop integration test verifies that idle capture produces
zero frames and that two successive viewer connections wake capture and decode
video successfully across resizes, minimization, and temporary hiding, both while
idle and during playback:

```powershell
cargo test pipeline_idles_and_resumes_for_viewers -- --ignored --nocapture
```

The selector test briefly moves the pointer and restores it afterward. Run all
tests serially in an interactive Windows desktop session so the window fixtures
do not interfere with each other:

```powershell
cargo test -- --include-ignored --nocapture --test-threads=1
```
