//! RTSP as GameStream hosts speak it, plus the AES-GCM framing of encrypted
//! RTSP (`rtspenc://`, negotiated with `corever=1` at launch), and the
//! handshake itself: OPTIONS → DESCRIBE → SETUP ×3 → ANNOUNCE → PLAY
//! (port of moonlight-common-c `RtspConnection.c`, 7.1.431+ path).
//!
//! Every request goes over its own TCP connection; the host closes the
//! connection after responding.

use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use hestia_crypto::stream::{gcm_decrypt, gcm_encrypt};
use log::{debug, info, warn};

use crate::sdp::{self, AnnounceParams, HostSdp};
use crate::sink::{AudioFormat, VideoFormat};

pub const DEFAULT_RTSP_PORT: u16 = 48010;
const ENCRYPTED_BIT: u32 = 0x8000_0000;
const ENC_HEADER_LEN: usize = 4 + 4 + 16;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE: usize = 1024 * 1024;
const RTSP_CLIENT_VERSION: u32 = 14;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtspError {
    Malformed(&'static str),
    Crypto,
    NoCommonVideoFormat,
    Io(String),
    Status { request: &'static str, code: u16 },
}

impl fmt::Display for RtspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed RTSP message: {what}"),
            Self::Crypto => f.write_str("RTSP decryption failed"),
            Self::NoCommonVideoFormat => f.write_str("no video format supported by both the host and the decoder"),
            Self::Io(e) => write!(f, "RTSP connection failed: {e}"),
            Self::Status { request, code } => write!(f, "RTSP {request} failed with status {code}"),
        }
    }
}

impl std::error::Error for RtspError {}

impl From<std::io::Error> for RtspError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Where to connect, parsed from the `sessionUrl0` of `/launch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUrl {
    pub host: String,
    pub port: u16,
    pub encrypted: bool,
    pub raw: String,
}

impl SessionUrl {
    /// Parses `rtsp://host:port` / `rtspenc://host:port`. An empty URL (very
    /// old hosts) falls back to the given host and the default port.
    pub fn parse(url: &str, fallback_host: &str) -> Result<Self, RtspError> {
        let url = url.trim();
        if url.is_empty() {
            return Ok(Self {
                host: fallback_host.to_owned(),
                port: DEFAULT_RTSP_PORT,
                encrypted: false,
                raw: String::new(),
            });
        }
        let (encrypted, rest) = if let Some(rest) = url.strip_prefix("rtspenc://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("rtsp://") {
            (false, rest)
        } else {
            return Err(RtspError::Malformed("unknown session URL scheme"));
        };
        let rest = rest.split('/').next().unwrap_or_default();
        let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
            let (h, after) = v6.split_once(']').ok_or(RtspError::Malformed("bad IPv6 literal"))?;
            (h, after.strip_prefix(':'))
        } else if let Some((h, p)) = rest.rsplit_once(':') {
            (h, Some(p))
        } else {
            (rest, None)
        };
        let port = match port {
            Some(p) => p.parse().map_err(|_| RtspError::Malformed("bad port"))?,
            None => DEFAULT_RTSP_PORT,
        };
        let host = if host.is_empty() || host == "0.0.0.0" || host == "::" {
            fallback_host
        } else {
            host
        };
        Ok(Self {
            host: host.to_owned(),
            port,
            encrypted,
            raw: url.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Message {
    /// `Some((method, target))` for requests, `None` for responses.
    pub request_line: Option<(String, String)>,
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Message {
    pub fn request(method: &str, target: &str) -> Self {
        Self {
            request_line: Some((method.to_owned(), target.to_owned())),
            ..Default::default()
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    pub fn with_body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers.push(("Content-type".into(), content_type.to_owned()));
        self.headers.push(("Content-length".into(), body.len().to_string()));
        self.body = body;
        self
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = String::new();
        match &self.request_line {
            Some((method, target)) => out.push_str(&format!("{method} {target} RTSP/1.0\r\n")),
            None => out.push_str(&format!("RTSP/1.0 {} {}\r\n", self.status, self.reason)),
        }
        for (k, v) in &self.headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }

    pub fn parse(data: &[u8]) -> Result<Self, RtspError> {
        let split = data
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| (i, i + 4))
            .or_else(|| data.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2)))
            .unwrap_or((data.len(), data.len()));
        let head = std::str::from_utf8(&data[..split.0]).map_err(|_| RtspError::Malformed("non UTF-8 header"))?;
        let mut lines = head.lines();
        let first = lines.next().ok_or(RtspError::Malformed("empty message"))?;
        let mut msg = Message::default();
        if let Some(rest) = first.strip_prefix("RTSP/1.0 ") {
            let (code, reason) = rest.split_once(' ').unwrap_or((rest, ""));
            msg.status = code
                .trim()
                .parse()
                .map_err(|_| RtspError::Malformed("bad status code"))?;
            msg.reason = reason.trim().to_owned();
        } else {
            let mut parts = first.split_whitespace();
            let method = parts.next().ok_or(RtspError::Malformed("missing method"))?;
            let target = parts.next().ok_or(RtspError::Malformed("missing target"))?;
            msg.request_line = Some((method.to_owned(), target.to_owned()));
        }
        msg.headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
        let body = &data[split.1..];
        let len = msg
            .get("Content-length")
            .and_then(|l| l.parse::<usize>().ok())
            .unwrap_or(body.len())
            .min(body.len());
        msg.body = body[..len].to_vec();
        Ok(msg)
    }
}

/// AES-128-GCM sealing of RTSP messages. Client→host frames use IV bytes
/// `seq(le32) .. 'C' 'R'`, host→client frames `'H' 'R'`.
pub struct RtspCipher {
    key: [u8; 16],
    next_seq: u32,
}

impl RtspCipher {
    /// `key` is the `rikey` sent with `/launch`.
    pub fn new(key: &[u8; 16]) -> Self {
        Self { key: *key, next_seq: 0 }
    }

    fn iv(seq: u32, origin: u8) -> [u8; 12] {
        let mut iv = [0u8; 12];
        iv[..4].copy_from_slice(&seq.to_le_bytes());
        iv[10] = origin;
        iv[11] = b'R';
        iv
    }

    /// Encrypts a serialized client message into a framed packet.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, RtspError> {
        self.next_seq = self.next_seq.wrapping_add(1);
        self.seal_with(plaintext, self.next_seq, b'C')
    }

    fn seal_with(&self, plaintext: &[u8], seq: u32, origin: u8) -> Result<Vec<u8>, RtspError> {
        let len = u32::try_from(plaintext.len())
            .ok()
            .filter(|l| l & ENCRYPTED_BIT == 0)
            .ok_or(RtspError::Malformed("message too large"))?;
        let (ciphertext, tag) = gcm_encrypt(&self.key, &Self::iv(seq, origin), plaintext);
        let mut out = Vec::with_capacity(ENC_HEADER_LEN + ciphertext.len());
        out.extend_from_slice(&(ENCRYPTED_BIT | len).to_be_bytes());
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&tag);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypts a host frame. Returns `Ok(None)` if more bytes are needed.
    pub fn unseal(&self, frame: &[u8]) -> Result<Option<Vec<u8>>, RtspError> {
        self.unseal_with(frame, b'H')
    }

    fn unseal_with(&self, frame: &[u8], origin: u8) -> Result<Option<Vec<u8>>, RtspError> {
        if frame.len() < ENC_HEADER_LEN {
            return Ok(None);
        }
        let type_and_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
        if type_and_len & ENCRYPTED_BIT == 0 {
            return Err(RtspError::Malformed("expected an encrypted frame"));
        }
        let len = (type_and_len & !ENCRYPTED_BIT) as usize;
        if frame.len() < ENC_HEADER_LEN + len {
            return Ok(None);
        }
        let seq = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]);
        let plaintext = gcm_decrypt(
            &self.key,
            &Self::iv(seq, origin),
            &frame[ENC_HEADER_LEN..ENC_HEADER_LEN + len],
            &frame[8..24],
        )
        .ok_or(RtspError::Crypto)?;
        Ok(Some(plaintext))
    }
}

/// Inputs to the handshake.
#[derive(Debug, Clone)]
pub struct HandshakeConfig {
    pub session_url: SessionUrl,
    pub server_codec_modes: u32,
    pub remote_input_key: [u8; 16],
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub packet_size: u32,
    pub formats: Vec<VideoFormat>,
    pub audio_channels: u8,
    pub audio_channel_mask: u32,
    pub streaming_remotely: bool,
}

/// What the handshake negotiated; everything the UDP/ENet streams need.
#[derive(Debug, Clone)]
pub struct Negotiated {
    pub remote: SocketAddr,
    pub audio_port: u16,
    pub video_port: u16,
    pub control_port: u16,
    pub audio_ping_payload: Option<[u8; 16]>,
    pub video_ping_payload: Option<[u8; 16]>,
    pub control_connect_data: u32,
    pub video_format: VideoFormat,
    pub audio_format: AudioFormat,
    pub audio_packet_duration_ms: u32,
    pub encryption_enabled: u32,
    pub audio_encrypted: bool,
    pub packet_size: u32,
    pub host_feature_flags: u32,
    pub supports_fec_status: bool,
    pub reference_frame_invalidation: bool,
}

impl Negotiated {
    /// Preserve the interface scope of the resolved RTSP peer for UDP streams.
    pub(crate) fn endpoint(&self, port: u16) -> SocketAddr {
        let mut endpoint = self.remote;
        endpoint.set_port(port);
        endpoint
    }
}

struct RtspClient {
    remote: SocketAddr,
    host_header: String,
    cipher: Option<RtspCipher>,
    seq: u32,
    session: Option<String>,
}

impl RtspClient {
    fn transact(
        &mut self,
        method: &'static str,
        target: &str,
        extra: Vec<(&str, String)>,
        body: Option<Vec<u8>>,
    ) -> Result<Message, RtspError> {
        let mut req = Message::request(method, target)
            .header("CSeq", self.seq.to_string())
            .header("X-GS-ClientVersion", RTSP_CLIENT_VERSION.to_string())
            .header("Host", self.host_header.clone());
        self.seq += 1;
        if let Some(session) = &self.session {
            req = req.header("Session", session.clone());
        }
        for (k, v) in extra {
            req = req.header(k, v);
        }
        if let Some(body) = body {
            req = req.with_body("application/sdp", body);
        }
        let plaintext = req.serialize();
        let wire = match &mut self.cipher {
            Some(c) => c.seal(&plaintext)?,
            None => plaintext,
        };

        // The host may not be listening yet right after /launch.
        let started = Instant::now();
        let mut stream = loop {
            match TcpStream::connect_timeout(&self.remote, CONNECT_TIMEOUT) {
                Ok(s) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused && started.elapsed() < CONNECT_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => return Err(e.into()),
            }
        };
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(RECEIVE_TIMEOUT))?;
        stream.write_all(&wire)?;

        let mut buf = Vec::new();
        let mut chunk = [0u8; 16384];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > MAX_RESPONSE {
                        return Err(RtspError::Malformed("response too large"));
                    }
                    // Encrypted frames carry their length; stop once complete.
                    if self.cipher.is_some() && buf.len() >= 4 {
                        let len = (u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) & !ENCRYPTED_BIT) as usize;
                        if buf.len() >= ENC_HEADER_LEN + len {
                            break;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset && !buf.is_empty() => break,
                Err(e) => return Err(e.into()),
            }
        }
        let plaintext = match &self.cipher {
            Some(c) => c
                .unseal(&buf)?
                .ok_or(RtspError::Malformed("truncated encrypted response"))?,
            None => buf,
        };
        let response = Message::parse(&plaintext)?;
        debug!("RTSP {method} {target} -> {}", response.status);
        if response.status != 200 {
            return Err(RtspError::Status {
                request: method,
                code: response.status,
            });
        }
        if method == "PLAY"
            && let Ok(local) = stream.local_addr()
        {
            info!("RTSP PLAY completed from {local} to {}", self.remote);
        }
        Ok(response)
    }
}

fn server_port(msg: &Message) -> Option<u16> {
    msg.get("Transport")?.split([';', ',']).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("server_port") {
            return None;
        }
        let first = value.split('-').next()?.trim();
        first.parse::<u16>().ok().filter(|port| *port != 0)
    })
}

fn ping_payload(msg: &Message) -> Option<[u8; 16]> {
    let p = msg.get("X-SS-Ping-Payload")?.as_bytes();
    p.try_into().ok()
}

/// Runs the full handshake.
pub fn handshake(cfg: &HandshakeConfig) -> Result<Negotiated, RtspError> {
    handshake_with_reference_recovery(cfg, |_| false)
}

pub(crate) fn handshake_with_reference_recovery(
    cfg: &HandshakeConfig,
    decoder_supports_rfi: impl FnOnce(VideoFormat) -> bool,
) -> Result<Negotiated, RtspError> {
    let url = &cfg.session_url;
    let remote = (url.host.as_str(), url.port)
        .to_socket_addrs()?
        .next()
        .ok_or(RtspError::Malformed("RTSP host did not resolve"))?;
    let host_for_url = match remote {
        SocketAddr::V6(a) => format!("[{}]", a.ip()),
        SocketAddr::V4(a) => a.ip().to_string(),
    };
    let target = if url.raw.is_empty() {
        format!("rtsp://{host_for_url}:{}", url.port)
    } else {
        url.raw.clone()
    };
    let mut client = RtspClient {
        remote,
        host_header: host_for_url.clone(),
        cipher: url.encrypted.then(|| RtspCipher::new(&cfg.remote_input_key)),
        seq: 1,
        session: None,
    };
    info!(
        "RTSP handshake with {remote} ({})",
        if url.encrypted { "encrypted" } else { "plain" }
    );

    client.transact("OPTIONS", &target, vec![], None)?;

    let describe = client.transact(
        "DESCRIBE",
        &target,
        vec![
            ("Accept", "application/sdp".into()),
            ("If-Modified-Since", "Thu, 01 Jan 1970 00:00:00 GMT".into()),
        ],
        None,
    )?;
    let host_sdp = HostSdp::new(String::from_utf8_lossy(&describe.body).into_owned());
    let video_format = host_sdp
        .negotiate_video(&cfg.formats, cfg.server_codec_modes)
        .ok_or(RtspError::NoCommonVideoFormat)?;
    let (normal_audio, high_audio) = host_sdp.opus_configs(cfg.audio_channels);
    let high_quality_audio = cfg.bitrate_kbps >= 15_000 && cfg.audio_channels > 2 && high_audio.is_some();
    let audio_format = if high_quality_audio { high_audio } else { normal_audio }
        .ok_or(RtspError::Malformed("host offered no usable audio layout"))?;

    // Encryption: control V2 whenever supported; video/audio only when the
    // host insists (we don't opt in on our own on a LAN).
    let supported = host_sdp.encryption_supported();
    let requested = host_sdp.encryption_requested();
    let mut encryption = 0;
    if supported & sdp::SS_ENC_CONTROL_V2 != 0 {
        encryption |= sdp::SS_ENC_CONTROL_V2;
    }
    if requested & sdp::SS_ENC_VIDEO != 0 && supported & sdp::SS_ENC_VIDEO != 0 {
        encryption |= sdp::SS_ENC_VIDEO;
    }
    if requested & sdp::SS_ENC_AUDIO != 0 {
        encryption |= sdp::SS_ENC_AUDIO;
    }

    let setup = |client: &mut RtspClient, stream: &str| {
        client.transact(
            "SETUP",
            stream,
            vec![
                ("Transport", "unicast;X-GS-ClientPort=50000-50001".into()),
                ("If-Modified-Since", "Thu, 01 Jan 1970 00:00:00 GMT".into()),
            ],
            None,
        )
    };

    let audio = setup(&mut client, "streamid=audio/0/0")?;
    let session = audio
        .get("Session")
        .and_then(|s| s.split(';').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(RtspError::Malformed("SETUP reply without Session"))?
        .to_owned();
    client.session = Some(session);
    let video = setup(&mut client, "streamid=video/0/0")?;
    let control = setup(&mut client, "streamid=control/13/0")?;

    let packet_size = cfg.packet_size - cfg.packet_size % 16;
    let sunshine = host_sdp.raw.lines().any(|line| line.trim().starts_with("a=x-ss-"));
    let negotiated = Negotiated {
        remote,
        audio_port: server_port(&audio).unwrap_or(48000),
        video_port: server_port(&video).unwrap_or(47998),
        control_port: server_port(&control).unwrap_or_else(|| {
            warn!("RTSP SETUP returned no usable control server_port; using UDP 47999");
            47999
        }),
        audio_ping_payload: ping_payload(&audio),
        video_ping_payload: ping_payload(&video),
        control_connect_data: control
            .get("X-SS-Connect-Data")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0),
        video_format,
        audio_format,
        audio_packet_duration_ms: 5,
        encryption_enabled: encryption,
        audio_encrypted: encryption & sdp::SS_ENC_AUDIO != 0,
        packet_size,
        host_feature_flags: host_sdp.feature_flags(),
        // Sunshine-family protocol extension, independent of host input feature bits.
        supports_fec_status: sunshine || video.get("X-SS-Ping-Payload").is_some(),
        // Hermes and Sunshine mark the recovery frame in the video header.
        reference_frame_invalidation: sunshine && host_sdp.supports_rfi() && decoder_supports_rfi(video_format),
    };

    let announce = sdp::build_announce_with_reference_recovery(
        &AnnounceParams {
            width: cfg.width,
            height: cfg.height & !1,
            fps: cfg.fps,
            packet_size,
            bitrate_kbps: cfg.bitrate_kbps,
            format: video_format,
            audio_channels: cfg.audio_channels,
            audio_channel_mask: cfg.audio_channel_mask,
            high_quality_audio,
            audio_packet_duration_ms: negotiated.audio_packet_duration_ms,
            encryption_enabled: encryption,
            audio_encryption: negotiated.audio_encrypted,
            streaming_remotely: cfg.streaming_remotely,
            ipv6: remote.is_ipv6(),
            host_address: remote.ip().to_string(),
            video_port: negotiated.video_port,
            refresh_rate_x100: cfg.fps * 100,
            encoder_csc_mode: if video_format.codec == crate::sink::VideoCodec::PyroWave {
                2
            } else {
                0
            },
        },
        negotiated.reference_frame_invalidation,
    );
    client.transact("ANNOUNCE", "streamid=control/13/0", vec![], Some(announce.into_bytes()))?;
    client.transact("PLAY", "/", vec![], None)?;

    info!(
        "RTSP done: video {:?} on {}, audio {} ch on {}, control on {}, encryption {:#x}",
        negotiated.video_format.codec,
        negotiated.video_port,
        negotiated.audio_format.channels,
        negotiated.audio_port,
        negotiated.control_port,
        negotiated.encryption_enabled
    );
    Ok(negotiated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_urls() {
        let u = SessionUrl::parse("rtspenc://192.168.0.10:48010", "x").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.encrypted), ("192.168.0.10", 48010, true));
        let u = SessionUrl::parse("rtsp://[fe80::2]:48011", "x").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.encrypted), ("fe80::2", 48011, false));
        let u = SessionUrl::parse("", "pc").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("pc", DEFAULT_RTSP_PORT));
        // An unspecified address stands for the host we asked.
        let u = SessionUrl::parse("rtsp://[::]:48010", "fd00::20").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("fd00::20", 48010));
        assert!(SessionUrl::parse("http://x", "x").is_err());
    }

    #[test]
    fn message_roundtrip() {
        let req = Message::request("ANNOUNCE", "streamid=control/13/0")
            .header("CSeq", "6")
            .header("X-GS-ClientVersion", "14")
            .with_body("application/sdp", b"v=0\r\n".to_vec());
        let parsed = Message::parse(&req.serialize()).unwrap();
        assert_eq!(parsed, req);

        let resp = Message::parse(
            b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nSession: DEADBEEFCAFE;timeout = 90\r\nTransport: server_port=47998\r\nX-SS-Ping-Payload: 0123456789abcdef\r\n\r\n",
        )
        .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.get("session"), Some("DEADBEEFCAFE;timeout = 90"));
        assert_eq!(server_port(&resp), Some(47998));
        assert_eq!(ping_payload(&resp), Some(*b"0123456789abcdef"));
        assert!(resp.request_line.is_none());
    }

    #[test]
    fn server_port_accepts_ranges_and_ignores_unrelated_parameters() {
        let transport = |value| Message::default().header("Transport", value);
        assert_eq!(
            server_port(&transport("RTP/AVP/UDP;unicast; Server_Port = 54007-54008;mode=play")),
            Some(54007)
        );
        assert_eq!(server_port(&transport("x-server_port=54007;server_port=0")), None);
    }

    #[test]
    fn encrypted_framing() {
        let key = [3u8; 16];
        let mut client = RtspCipher::new(&key);
        let sealed = client.seal(b"OPTIONS rtsp://x RTSP/1.0\r\n\r\n").unwrap();
        assert_eq!(sealed[0] & 0x80, 0x80);
        assert_eq!(&sealed[4..8], &1u32.to_be_bytes());

        // A host frame (origin 'H') decrypts; our own 'C' frame must not.
        let host = RtspCipher::new(&key);
        let from_host = host.seal_with(b"RTSP/1.0 200 OK\r\n\r\n", 7, b'H').unwrap();
        assert_eq!(client.unseal(&from_host).unwrap().unwrap(), b"RTSP/1.0 200 OK\r\n\r\n");
        assert_eq!(client.unseal(&sealed), Err(RtspError::Crypto));
        assert_eq!(client.unseal(&from_host[..10]).unwrap(), None);
    }
}
