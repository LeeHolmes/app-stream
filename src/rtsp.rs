use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, tcp::OwnedWriteHalf},
    sync::{Semaphore, broadcast, mpsc, watch},
    task::JoinSet,
    time::{Instant, timeout},
};

use crate::media::{Media, rtp_packets};

const REALM: &str = "App Stream";
const PUBLIC: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, GET_PARAMETER, TEARDOWN";

pub struct Credentials {
    pub username: String,
    pub password: String,
    pub legacy_digest: bool,
}

pub async fn serve(
    listener: TcpListener,
    media: Media,
    credentials: Credentials,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let credentials = Arc::new(credentials);
    let slots = Arc::new(Semaphore::new(16));
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                let (socket, _) = accepted.context("Accepting RTSP client")?;
                match slots.clone().try_acquire_owned() {
                    Ok(permit) => {
                        let media = media.clone();
                        let credentials = credentials.clone();
                        clients.spawn(async move {
                            let _permit = permit;
                            if let Err(error) = client(socket, media, credentials).await {
                                eprintln!("RTSP client disconnected: {error:#}");
                            }
                        });
                    }
                    Err(_) => eprintln!("RTSP connection rejected: 16-client limit reached"),
                }
            }
            Some(result) = clients.join_next(), if !clients.is_empty() => {
                result.context("RTSP client task panicked")?;
            }
        }
    }
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    Ok(())
}

#[derive(Debug)]
struct Request {
    method: String,
    uri: String,
    headers: HashMap<String, String>,
}

async fn read_request(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Result<Option<Request>> {
    // Consume receiver RTCP frames interleaved with RTSP requests.
    loop {
        let prefix = reader.fill_buf().await?;
        if prefix.is_empty() {
            return Ok(None);
        }
        if prefix[0] != b'$' {
            break;
        }
        let mut header = [0; 4];
        reader.read_exact(&mut header).await?;
        let length = u16::from_be_bytes([header[2], header[3]]) as usize;
        let mut packet = vec![0; length];
        reader.read_exact(&mut packet).await?;
    }
    let mut bytes = Vec::new();
    // read_until alone has no size bound, so copy only the permitted prefix.
    loop {
        let available = reader.fill_buf().await?;
        ensure!(!available.is_empty(), "Incomplete RTSP headers");
        let remaining = 16_384usize.saturating_sub(bytes.len());
        ensure!(remaining > 0, "RTSP headers exceed 16 KiB");
        let take = available
            .iter()
            .position(|&b| b == b'\n')
            .map_or(available.len(), |i| i + 1)
            .min(remaining);
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&bytes).context("RTSP headers must be UTF-8/ASCII")?;
    let mut lines = text.split("\r\n");
    let mut request_line = lines
        .next()
        .context("Missing request line")?
        .split_whitespace();
    let method = request_line.next().context("Missing method")?.to_owned();
    let uri = request_line.next().context("Missing URI")?.to_owned();
    ensure!(
        request_line.next() == Some("RTSP/1.0") && request_line.next().is_none(),
        "Invalid RTSP version/request"
    );
    let mut headers = HashMap::new();
    for line in lines.filter(|s| !s.is_empty()) {
        let (key, value) = line.split_once(':').context("Malformed RTSP header")?;
        ensure!(
            headers
                .insert(key.to_ascii_lowercase(), value.trim().to_owned())
                .is_none(),
            "Duplicate RTSP header"
        );
    }
    let cseq = headers.get("cseq").context("Missing CSeq")?;
    ensure!(
        !cseq.is_empty() && cseq.bytes().all(|b| b.is_ascii_digit()),
        "Invalid CSeq"
    );
    let length = headers
        .get("content-length")
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(0);
    ensure!(length <= 65_536, "RTSP body exceeds 64 KiB");
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(Request {
        method,
        uri,
        headers,
    }))
}

async fn respond(
    writer: &mut OwnedWriteHalf,
    request: &Request,
    status: &str,
    headers: &str,
    body: &str,
) -> Result<()> {
    match status {
        "461 Unsupported Transport" => eprintln!(
            "RTSP request rejected: 461 Unsupported Transport. Configure the viewer for RTP/RTSP over TCP (interleaved), not UDP or HTTP tunneling."
        ),
        "404 Not Found" => eprintln!(
            "RTSP request rejected: 404 Not Found. Use rtsp://127.0.0.1:PORT/app (track: /app/trackID=0)."
        ),
        "200 OK" | "401 Unauthorized" => {}
        _ => eprintln!("RTSP request rejected: {status}"),
    }
    let cseq = &request.headers["cseq"];
    let message = format!(
        "RTSP/1.0 {status}\r\nCSeq: {cseq}\r\nServer: App-Stream\r\n{headers}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    timeout(Duration::from_secs(5), writer.write_all(message.as_bytes()))
        .await
        .context("RTSP client stopped reading responses")??;
    Ok(())
}

struct Authentication {
    nonce: String,
    highest_nc: u32,
}

impl Authentication {
    fn new() -> Self {
        Self {
            nonce: format!("{:032x}", rand::random::<u128>()),
            highest_nc: 0,
        }
    }

    fn challenge(&self, credentials: &Credentials) -> String {
        let qop = if credentials.legacy_digest {
            ""
        } else {
            ", qop=\"auth\""
        };
        format!(
            "WWW-Authenticate: Digest realm=\"{REALM}\", nonce=\"{}\", algorithm=MD5{qop}\r\n",
            self.nonce
        )
    }

    fn verify(&mut self, request: &Request, credentials: &Credentials) -> Result<(), &'static str> {
        let Some(header) = request.headers.get("authorization") else {
            return Err("credentials required");
        };
        let Some(fields) = digest_fields(header) else {
            return Err("expected Digest authentication, not Basic or a malformed header");
        };
        let get = |key: &str| fields.get(key).map(String::as_str);
        if get("username") != Some(credentials.username.as_str())
            || get("realm") != Some(REALM)
            || get("nonce") != Some(self.nonce.as_str())
            || get("uri") != Some(request.uri.as_str())
            || get("algorithm").is_some_and(|a| !a.eq_ignore_ascii_case("MD5"))
        {
            return Err("username, realm, nonce, request URI, or Digest algorithm does not match");
        }
        let response = get("response")
            .filter(|value| value.len() == 32)
            .ok_or("missing or invalid Digest response")?;
        let ha1 = hash(&format!(
            "{}:{REALM}:{}",
            credentials.username, credentials.password
        ));
        let ha2 = hash(&format!("{}:{}", request.method, request.uri));
        let (expected, counter) = match get("qop") {
            Some("auth") => {
                let nc = get("nc").ok_or("missing Digest nonce count")?;
                let cnonce = get("cnonce")
                    .filter(|value| !value.is_empty())
                    .ok_or("missing Digest client nonce")?;
                let counter =
                    u32::from_str_radix(nc, 16).map_err(|_| "invalid Digest nonce count")?;
                if nc.len() != 8 || counter <= self.highest_nc {
                    return Err("Digest nonce count was replayed or is invalid");
                }
                (
                    hash(&format!("{ha1}:{}:{nc}:{cnonce}:auth:{ha2}", self.nonce)),
                    Some(counter),
                )
            }
            None if credentials.legacy_digest && get("nc").is_none() && get("cnonce").is_none() => {
                (hash(&format!("{ha1}:{}:{ha2}", self.nonce)), None)
            }
            None => {
                return Err(
                    "viewer uses legacy Digest without qop; restart App Stream with --legacy-digest if this client requires it",
                );
            }
            _ => return Err("unsupported Digest qop; expected auth"),
        };
        let difference = expected
            .bytes()
            .zip(response.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b.to_ascii_lowercase()));
        if difference != 0 {
            return Err("incorrect password or Digest response");
        }
        if let Some(counter) = counter {
            self.highest_nc = counter;
        }
        Ok(())
    }
}

fn hash(value: &str) -> String {
    format!("{:x}", md5::compute(value))
}

fn digest_fields(header: &str) -> Option<HashMap<String, String>> {
    let (scheme, mut rest) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Digest") {
        return None;
    }
    let mut fields = HashMap::new();
    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        let (key, value) = rest.split_once('=')?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim_start();
        let (parsed, remaining) = if let Some(quoted) = value.strip_prefix('"') {
            let mut parsed = String::new();
            let mut escaped = false;
            let mut end = None;
            for (index, ch) in quoted.char_indices() {
                if escaped {
                    parsed.push(ch);
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    end = Some(index);
                    break;
                } else {
                    parsed.push(ch);
                }
            }
            (parsed, &quoted[end? + 1..])
        } else {
            let end = value.find(',').unwrap_or(value.len());
            (value[..end].trim().to_owned(), &value[end..])
        };
        if key.is_empty() || fields.insert(key, parsed).is_some() {
            return None;
        }
        let remaining = remaining.trim_start();
        rest = if remaining.is_empty() {
            ""
        } else {
            remaining.strip_prefix(',')?
        };
    }
    Some(fields)
}

fn resource(uri: &str, port: u16) -> Option<bool> {
    let path = if uri.starts_with('/') {
        uri
    } else {
        let rest = uri.strip_prefix("rtsp://")?;
        let (authority, path) = rest.split_once('/')?;
        if authority != format!("127.0.0.1:{port}") && authority != format!("localhost:{port}") {
            return None;
        }
        return match path {
            "app" | "app/" => Some(false),
            "app/trackID=0" => Some(true),
            _ => None,
        };
    };
    match path {
        "/app" | "/app/" => Some(false),
        "/app/trackID=0" => Some(true),
        _ => None,
    }
}

fn transport(value: &str) -> Option<(u8, u8)> {
    let mut parts = value.split(';').map(str::trim);
    if !parts.next()?.eq_ignore_ascii_case("RTP/AVP/TCP") {
        return None;
    }
    let mut channels = None;
    for part in parts {
        if let Some(value) = part.strip_prefix("interleaved=") {
            let (rtp, rtcp) = value.split_once('-')?;
            let pair = (rtp.parse().ok()?, rtcp.parse().ok()?);
            if pair.0 == pair.1 || channels.replace(pair).is_some() {
                return None;
            }
        } else if let Some((key, value)) = part.split_once('=') {
            if !key.eq_ignore_ascii_case("mode")
                || !value.trim_matches('"').eq_ignore_ascii_case("PLAY")
            {
                return None;
            }
        } else if !part.eq_ignore_ascii_case("unicast") {
            return None;
        }
    }
    Some(channels.unwrap_or((0, 1)))
}

async fn client(socket: TcpStream, mut media: Media, credentials: Arc<Credentials>) -> Result<()> {
    socket.set_nodelay(true)?;
    let port = socket.local_addr()?.port();
    let peer_ip = socket.peer_addr()?.ip();
    let (read, mut writer) = socket.into_split();
    let mut reader = BufReader::new(read);
    let (requests_tx, mut requests) = mpsc::channel(4);
    // A separate reader preserves partially read requests when a video frame wins select!.
    let _reader_task = AbortOnDrop(tokio::spawn(async move {
        loop {
            let result = match timeout(Duration::from_secs(90), read_request(&mut reader)).await {
                Ok(Ok(Some(request))) => Ok(request),
                Ok(Ok(None)) => break,
                Ok(Err(error)) => Err(error),
                Err(error) => Err(anyhow::Error::new(error).context("RTSP keepalive timed out")),
            };
            let failed = result.is_err();
            if requests_tx.send(result).await.is_err() || failed {
                break;
            }
        }
    }));
    let mut auth = Authentication::new();
    let mut connection_logged = false;
    let session = format!("{:016x}", rand::random::<u64>());
    let ssrc = rand::random::<u32>();
    let mut sequence = rand::random::<u16>();
    let mut channels = None;
    let mut playing = false;
    let mut playing_guard = None;
    let mut awaiting_keyframe = true;
    let mut frames = media.frames.subscribe();
    let mut packet_count = 0u32;
    let mut octet_count = 0u32;
    let mut last_report = Instant::now();
    loop {
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else { return Ok(()) };
                let request = request?;
                if let Err(reason) = auth.verify(&request, &credentials) {
                    if request.headers.contains_key("authorization") {
                        eprintln!("RTSP authentication rejected: {reason}");
                    }
                    let challenge = auth.challenge(&credentials);
                    respond(&mut writer, &request, "401 Unauthorized", &challenge, "").await?;
                    continue;
                }
                if !connection_logged {
                    eprintln!("RTSP client connected: username={:?}, IP={peer_ip}", credentials.username);
                    connection_logged = true;
                }
                let track = resource(&request.uri, port);
                if request.uri != "*" && track.is_none() {
                    respond(&mut writer, &request, "404 Not Found", "", "").await?;
                    continue;
                }
                if request.method != "OPTIONS" && request.uri == "*" {
                    respond(&mut writer, &request, "404 Not Found", "", "").await?;
                    continue;
                }
                if matches!(request.method.as_str(), "PLAY" | "GET_PARAMETER" | "TEARDOWN") {
                    let valid_session = request.headers.get("session")
                        .and_then(|s| s.split(';').next()) == Some(session.as_str());
                    if channels.is_none() || !valid_session {
                        respond(&mut writer, &request, "454 Session Not Found", "", "").await?;
                        continue;
                    }
                }
                match request.method.as_str() {
                    "OPTIONS" => respond(&mut writer, &request, "200 OK", &format!("Public: {PUBLIC}\r\n"), "").await?,
                    "DESCRIBE" if track == Some(false) => {
                        let parameters = timeout(Duration::from_secs(10), async {
                            loop {
                                if let Some(parameters) = media.parameters.borrow().clone() {
                                    break Ok::<_, anyhow::Error>(parameters);
                                }
                                media.parameters.changed().await.context("Encoder stopped before producing video")?;
                            }
                        }).await.context("Encoder did not produce SPS/PPS within 10 seconds")??;
                        ensure!(parameters.sps.len() >= 4, "Invalid encoder SPS");
                        let sps = STANDARD.encode(&parameters.sps);
                        let pps = STANDARD.encode(&parameters.pps);
                        let profile = parameters.sps[1..4].iter().map(|b| format!("{b:02x}")).collect::<String>();
                        let body = format!(
                            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=App Stream\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\na=control:*\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 packetization-mode=1;profile-level-id={profile};sprop-parameter-sets={sps},{pps}\r\na=control:trackID=0\r\n"
                        );
                        respond(&mut writer, &request, "200 OK", &format!("Content-Type: application/sdp\r\nContent-Base: rtsp://127.0.0.1:{port}/app/\r\n"), &body).await?;
                    }
                    "SETUP" if track == Some(true) && channels.is_none() => {
                        let pair = request.headers.get("transport").and_then(|s| transport(s));
                        if let Some(pair) = pair {
                            channels = Some(pair);
                            respond(&mut writer, &request, "200 OK", &format!("Session: {session};timeout=90\r\nTransport: RTP/AVP/TCP;unicast;interleaved={}-{};ssrc={ssrc:08X}\r\n", pair.0, pair.1), "").await?;
                        } else {
                            respond(&mut writer, &request, "461 Unsupported Transport", "", "").await?;
                        }
                    }
                    "PLAY" if track == Some(false) => {
                        frames = media.frames.subscribe();
                        awaiting_keyframe = true;
                        playing = true;
                        respond(&mut writer, &request, "200 OK", &format!("Session: {session}\r\nRange: npt=0.000-\r\n"), "").await?;
                        playing_guard.get_or_insert_with(|| media.activity.playing());
                    }
                    "GET_PARAMETER" => respond(&mut writer, &request, "200 OK", &format!("Session: {session}\r\n"), "").await?,
                    "TEARDOWN" => {
                        respond(&mut writer, &request, "200 OK", &format!("Session: {session}\r\n"), "").await?;
                        return Ok(());
                    }
                    "DESCRIBE" | "SETUP" | "PLAY" => respond(&mut writer, &request, "455 Method Not Valid in This State", "", "").await?,
                    _ => respond(&mut writer, &request, "405 Method Not Allowed", &format!("Allow: {PUBLIC}\r\n"), "").await?,
                }
            }
            frame = frames.recv(), if playing => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(broadcast::error::RecvError::Lagged(count)) => bail!("Client too slow: missed {count} video frames"),
                    Err(broadcast::error::RecvError::Closed) => bail!("Encoder stopped"),
                };
                if awaiting_keyframe && !frame.keyframe { continue; }
                awaiting_keyframe = false;
                let (rtp, rtcp) = channels.context("Missing RTP channels")?;
                let packets = rtp_packets(&frame, &mut sequence, ssrc);
                timeout(Duration::from_secs(5), async {
                    for packet in packets {
                        packet_count = packet_count.wrapping_add(1);
                        octet_count = octet_count.wrapping_add((packet.len() - 12) as u32);
                        interleaved(&mut writer, rtp, &packet).await?;
                    }
                    if packet_count != 0 && (last_report.elapsed() >= Duration::from_secs(5) || frame.keyframe) {
                        let report = sender_report(ssrc, frame.timestamp, packet_count, octet_count);
                        interleaved(&mut writer, rtcp, &report).await?;
                        last_report = Instant::now();
                    }
                    Ok::<_, anyhow::Error>(())
                }).await.context("RTSP client stopped reading video")??;
            }
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn interleaved(writer: &mut OwnedWriteHalf, channel: u8, packet: &[u8]) -> Result<()> {
    let length = u16::try_from(packet.len())?;
    writer
        .write_all(&[b'$', channel, (length >> 8) as u8, length as u8])
        .await?;
    writer.write_all(packet).await?;
    Ok(())
}

fn sender_report(ssrc: u32, timestamp: u32, packets: u32, octets: u32) -> Vec<u8> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System clock predates 1970");
    let ntp_seconds = (now.as_secs() + 2_208_988_800) as u32;
    let ntp_fraction = ((u64::from(now.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    let mut report = vec![0x80, 200, 0, 6];
    for value in [ssrc, ntp_seconds, ntp_fraction, timestamp, packets, octets] {
        report.extend_from_slice(&value.to_be_bytes());
    }
    // Compound RTCP requires an SDES CNAME alongside the sender report.
    report.extend_from_slice(&[0x81, 202, 0, 5]);
    report.extend_from_slice(&ssrc.to_be_bytes());
    report.extend_from_slice(&[1, 10]);
    report.extend_from_slice(b"app-stream");
    report.extend_from_slice(&[0, 0, 0, 0]);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authorized(auth: &Authentication, credentials: &Credentials, nc: &str) -> Request {
        let uri = "rtsp://127.0.0.1:8554/app";
        let ha1 = hash(&format!(
            "{}:{REALM}:{}",
            credentials.username, credentials.password
        ));
        let ha2 = hash(&format!("DESCRIBE:{uri}"));
        let response = hash(&format!("{ha1}:{}:{nc}:client:auth:{ha2}", auth.nonce));
        let header = format!(
            "Digest username=\"{}\", realm=\"{REALM}\", nonce=\"{}\", uri=\"{uri}\", qop=auth, nc={nc}, cnonce=\"client\", response=\"{response}\"",
            credentials.username, auth.nonce
        );
        Request {
            method: "DESCRIBE".into(),
            uri: uri.into(),
            headers: HashMap::from([("authorization".into(), header)]),
        }
    }

    #[test]
    fn digest_verifies_credentials_uri_and_replay_counter() {
        let credentials = Credentials {
            username: "viewer".into(),
            password: "secret".into(),
            legacy_digest: false,
        };
        let mut auth = Authentication::new();
        let request = authorized(&auth, &credentials, "00000001");
        assert!(auth.verify(&request, &credentials).is_ok());
        assert!(auth.verify(&request, &credentials).is_err());
        let mut request = authorized(&auth, &credentials, "00000002");
        request.uri.push_str("/different");
        assert!(auth.verify(&request, &credentials).is_err());
        let request = authorized(&auth, &credentials, "00000003");
        assert!(
            auth.verify(
                &request,
                &Credentials {
                    username: "viewer".into(),
                    password: "wrong".into(),
                    legacy_digest: false,
                }
            )
            .is_err()
        );
        assert!(auth.verify(&request, &credentials).is_ok());
    }

    fn legacy_authorized(auth: &Authentication, credentials: &Credentials) -> Request {
        let uri = "rtsp://127.0.0.1:8554/app";
        let ha1 = hash(&format!(
            "{}:{REALM}:{}",
            credentials.username, credentials.password
        ));
        let ha2 = hash(&format!("DESCRIBE:{uri}"));
        let response = hash(&format!("{ha1}:{}:{ha2}", auth.nonce));
        Request {
            method: "DESCRIBE".into(),
            uri: uri.into(),
            headers: HashMap::from([(
                "authorization".into(),
                format!(
                    "Digest username=\"{}\", realm=\"{REALM}\", nonce=\"{}\", uri=\"{uri}\", response=\"{response}\"",
                    credentials.username, auth.nonce
                ),
            )]),
        }
    }

    #[test]
    fn legacy_digest_is_opt_in_and_still_checks_password_nonce_method_and_uri() {
        let mut credentials = Credentials {
            username: "viewer".into(),
            password: "secret".into(),
            legacy_digest: false,
        };
        let mut auth = Authentication::new();
        let request = legacy_authorized(&auth, &credentials);
        assert!(auth.challenge(&credentials).contains("qop=\"auth\""));
        assert!(
            auth.verify(&request, &credentials)
                .unwrap_err()
                .contains("--legacy-digest")
        );
        credentials.legacy_digest = true;
        assert!(!auth.challenge(&credentials).contains("qop"));
        assert!(auth.verify(&request, &credentials).is_ok());
        credentials.password = "wrong".into();
        assert!(auth.verify(&request, &credentials).is_err());
        credentials.password = "secret".into();
        assert!(
            Authentication::new()
                .verify(&request, &credentials)
                .is_err()
        );
        let mut different_method = legacy_authorized(&auth, &credentials);
        different_method.method = "PLAY".into();
        assert!(auth.verify(&different_method, &credentials).is_err());
        let mut different_uri = legacy_authorized(&auth, &credentials);
        different_uri.uri.push_str("/other");
        assert!(auth.verify(&different_uri, &credentials).is_err());
        let modern = authorized(&auth, &credentials, "00000001");
        assert!(auth.verify(&modern, &credentials).is_ok());
        assert!(
            auth.verify(&modern, &credentials).is_err(),
            "qop replay checks remain enforced in compatibility mode"
        );
    }

    #[test]
    fn digest_parser_handles_quoted_commas_and_rejects_duplicates() {
        let fields = digest_fields("Digest username=\"a,b\", qop=auth").unwrap();
        assert_eq!(fields["username"], "a,b");
        assert!(digest_fields("Digest nonce=\"a\", nonce=\"b\"").is_none());
        assert!(digest_fields("Basic abc").is_none());
    }

    #[test]
    fn only_tcp_unicast_transport_and_local_resource_are_accepted() {
        assert_eq!(
            transport("RTP/AVP/TCP;unicast;interleaved=2-3"),
            Some((2, 3))
        );
        assert_eq!(transport("RTP/AVP;unicast;client_port=5000-5001"), None);
        assert_eq!(transport("RTP/AVP/TCP;interleaved=1-1"), None);
        assert_eq!(
            transport("RTP/AVP/TCP;unicast;interleaved=0-1;mode=\"PLAY\""),
            Some((0, 1))
        );
        assert_eq!(transport("RTP/AVP/TCP;unicast;mode=play"), Some((0, 1)));
        assert_eq!(transport("RTP/AVP/TCP;unicast;mode=RECORD"), None);
        assert_eq!(
            resource("rtsp://127.0.0.1:8554/app/trackID=0", 8554),
            Some(true)
        );
        assert_eq!(resource("rtsp://example.com:8554/app", 8554), None);
        assert_eq!(resource("/private", 8554), None);
    }

    #[test]
    fn rtcp_compound_lengths_are_correct() {
        let report = sender_report(42, 90_000, 4, 500);
        assert_eq!(report.len(), 52);
        assert_eq!(report[1], 200);
        assert_eq!(report[29], 202);
        assert_eq!(
            (u16::from_be_bytes([report[30], report[31]]) as usize + 1) * 4,
            report.len() - 28
        );
    }

    #[tokio::test]
    async fn unauthenticated_describe_is_challenged_over_tcp() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (media, _parameters) = crate::media::channel();
        let (shutdown, rx) = watch::channel(false);
        let server = tokio::spawn(serve(
            listener,
            media,
            Credentials {
                username: "user".into(),
                password: "secret".into(),
                legacy_digest: false,
            },
            rx,
        ));
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(
                format!("DESCRIBE rtsp://127.0.0.1:{port}/app RTSP/1.0\r\nCSeq: 1\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = [0; 2048];
        let count = timeout(Duration::from_secs(2), stream.read(&mut response))
            .await
            .unwrap()
            .unwrap();
        let response = std::str::from_utf8(&response[..count]).unwrap();
        assert!(response.starts_with("RTSP/1.0 401 Unauthorized\r\n"));
        assert!(response.contains("WWW-Authenticate: Digest"));
        assert!(response.contains("qop=\"auth\""));
        shutdown.send(true).unwrap();
        server.await.unwrap().unwrap();
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    }

    async fn test_request(
        stream: &mut BufReader<TcpStream>,
        method: &str,
        uri: &str,
        headers: &str,
        auth: Option<(&str, u32)>,
    ) -> (String, HashMap<String, String>) {
        let authorization = if let Some((nonce, nc)) = auth {
            let ha1 = hash(&format!("viewer:{REALM}:secret"));
            let ha2 = hash(&format!("{method}:{uri}"));
            let response = hash(&format!("{ha1}:{nonce}:{nc:08x}:client:auth:{ha2}"));
            format!(
                "Authorization: Digest username=\"viewer\", realm=\"{REALM}\", nonce=\"{nonce}\", uri=\"{uri}\", qop=auth, nc={nc:08x}, cnonce=\"client\", response=\"{response}\"\r\n"
            )
        } else {
            String::new()
        };
        stream
            .get_mut()
            .write_all(
                format!("{method} {uri} RTSP/1.0\r\nCSeq: 1\r\n{authorization}{headers}\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        timeout(Duration::from_secs(2), async {
            let mut status = String::new();
            stream.read_line(&mut status).await.unwrap();
            let mut response_headers = HashMap::new();
            loop {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                assert!(!line.is_empty(), "Unexpected EOF");
                let (name, value) = line.split_once(':').unwrap();
                response_headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
            }
            let mut body = vec![0; response_headers["content-length"].parse().unwrap()];
            stream.read_exact(&mut body).await.unwrap();
            (status, response_headers)
        })
        .await
        .unwrap()
    }

    async fn test_play(
        port: u16,
        activity: &crate::media::Activity,
    ) -> (BufReader<TcpStream>, String, String) {
        let before_play = activity.viewer_count();
        let mut stream = BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        let uri = format!("rtsp://127.0.0.1:{port}/app");
        let (status, headers) = test_request(&mut stream, "DESCRIBE", &uri, "", None).await;
        assert!(status.contains("401"));
        let nonce = digest_fields(&headers["www-authenticate"]).unwrap()["nonce"].clone();
        let (status, _) = test_request(&mut stream, "DESCRIBE", &uri, "", Some((&nonce, 1))).await;
        assert!(status.contains("200"));
        assert_eq!(
            activity.viewer_count(),
            before_play,
            "DESCRIBE must not wake capture"
        );
        let (status, headers) = test_request(
            &mut stream,
            "SETUP",
            &format!("{uri}/trackID=0"),
            "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n",
            Some((&nonce, 2)),
        )
        .await;
        assert!(status.contains("200"));
        assert_eq!(
            activity.viewer_count(),
            before_play,
            "SETUP must not wake capture"
        );
        let session = headers["session"].split(';').next().unwrap().to_owned();
        let (status, _) = test_request(
            &mut stream,
            "PLAY",
            &uri,
            &format!("Session: {session}\r\n"),
            Some((&nonce, 3)),
        )
        .await;
        assert!(status.contains("200"));
        (stream, nonce, session)
    }

    #[tokio::test]
    async fn playing_demand_tracks_multiple_clients_teardown_disconnect_and_shutdown() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (media, parameters) = crate::media::channel();
        parameters.send_replace(Some(crate::media::Parameters {
            sps: vec![0x67, 0x42, 0, 0x1f],
            pps: vec![0x68, 0xce],
        }));
        let activity = media.activity.clone();
        let (shutdown, rx) = watch::channel(false);
        let server = tokio::spawn(serve(
            listener,
            media,
            Credentials {
                username: "viewer".into(),
                password: "secret".into(),
                legacy_digest: false,
            },
            rx,
        ));
        assert!(!activity.wait(Duration::ZERO));
        let (mut first, nonce, session) = test_play(port, &activity).await;
        let (second, _, _) = test_play(port, &activity).await;
        let uri = format!("rtsp://127.0.0.1:{port}/app");
        let (status, _) = test_request(
            &mut first,
            "PLAY",
            &uri,
            &format!("Session: {session}\r\n"),
            Some((&nonce, 4)),
        )
        .await;
        assert!(status.contains("200"));
        assert_eq!(activity.viewer_count(), 2);
        let (status, _) = test_request(
            &mut first,
            "TEARDOWN",
            &uri,
            &format!("Session: {session}\r\n"),
            Some((&nonce, 5)),
        )
        .await;
        assert!(status.contains("200"));
        drop(first);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            activity.wait(Duration::ZERO),
            "Remaining viewer must keep capture running"
        );
        drop(second);
        timeout(Duration::from_secs(2), async {
            while activity.wait(Duration::ZERO) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let (_third, _, _) = test_play(port, &activity).await;
        assert!(activity.wait(Duration::ZERO));
        shutdown.send(true).unwrap();
        server.await.unwrap().unwrap();
        assert!(
            !activity.wait(Duration::ZERO),
            "Server shutdown must release all viewers"
        );
    }

    #[tokio::test]
    #[ignore = "requires FFmpeg with libx264 on PATH"]
    async fn ffmpeg_round_trip() {
        for legacy_digest in [false, true] {
            ffmpeg_round_trip_mode(legacy_digest).await;
        }
    }

    async fn ffmpeg_round_trip_mode(legacy_digest: bool) {
        use std::process::Stdio;
        use tokio::process::Command;

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (media, parameters) = crate::media::channel();
        let (shutdown, rx) = watch::channel(false);
        let mut encoder = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-re",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=10",
                "-an",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "10",
                "-bf",
                "0",
                "-x264-params",
                "aud=1:repeat-headers=1",
                "-f",
                "h264",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Start FFmpeg encoder");
        let output = encoder.stdout.take().unwrap();
        struct EncoderGuard(std::process::Child);
        impl Drop for EncoderGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _encoder = EncoderGuard(encoder);
        let reader_media = media.clone();
        let _reader = std::thread::spawn(move || {
            crate::media::read_h264(output, 10, reader_media, parameters)
        });
        let _server = AbortOnDrop(tokio::spawn(async move {
            serve(
                listener,
                media,
                Credentials {
                    username: "viewer".into(),
                    password: "test-password".into(),
                    legacy_digest,
                },
                rx,
            )
            .await
            .unwrap();
        }));

        let bad = timeout(
            Duration::from_secs(10),
            Command::new("ffmpeg")
                .kill_on_drop(true)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-rtsp_transport",
                    "tcp",
                    "-i",
                    &format!("rtsp://viewer:wrong@127.0.0.1:{port}/app"),
                    "-frames:v",
                    "1",
                    "-f",
                    "null",
                    "-",
                ])
                .output(),
        )
        .await
        .expect("Wrong-password client timed out")
        .unwrap();
        assert!(
            !bad.status.success(),
            "Wrong password must not receive video"
        );
        assert!(
            String::from_utf8_lossy(&bad.stderr).contains("401"),
            "{}",
            String::from_utf8_lossy(&bad.stderr)
        );

        let decoded = timeout(
            Duration::from_secs(20),
            Command::new("ffmpeg")
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
                    "5",
                    "-an",
                    "-f",
                    "framemd5",
                    "pipe:1",
                ])
                .output(),
        )
        .await
        .expect("Authenticated client timed out")
        .unwrap();
        assert!(
            decoded.status.success(),
            "{}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        let text = String::from_utf8(decoded.stdout).unwrap();
        assert!(text.contains("#dimensions 0: 320x240"), "{text}");
        assert_eq!(
            text.lines()
                .filter(|line| !line.starts_with('#') && !line.is_empty())
                .count(),
            5,
            "{text}"
        );
        shutdown.send(true).unwrap();
    }
}
