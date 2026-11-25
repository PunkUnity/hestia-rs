//! A fake GameStream host for end-to-end tests of [`StreamSession`]: RTSP
//! over TCP, a minimal ENet server for the control stream and a video sender
//! that packetizes frames with FEC the way Sunshine/Hermes do.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::control;
use crate::rtsp::Message;

pub const PING_PAYLOAD: &[u8; 16] = b"0123456789abcdef";
pub const CONNECT_DATA: u32 = 0x1234_5678;

pub type ControlLog = Arc<Mutex<Vec<(u16, Vec<u8>)>>>;

pub struct MockHost {
    pub rtsp_port: u16,
    pub announce: Arc<Mutex<Option<String>>>,
    pub control_messages: ControlLog,
    stop: Arc<AtomicBool>,
}

impl Drop for MockHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

impl MockHost {
    /// Starts the host; it streams `frames` (Annex B) after the client's
    /// START B and a video ping, then terminates the session gracefully.
    pub fn start(key: [u8; 16], frames: Vec<Vec<u8>>, packet_size: usize) -> Self {
        Self::spawn(key, frames, packet_size, true, false)
    }

    /// Stops sending video after the given frames while audio and ENet continue.
    pub fn stall_video(key: [u8; 16], frames: Vec<Vec<u8>>, packet_size: usize) -> Self {
        Self::spawn(key, frames, packet_size, false, false)
    }

    pub fn reference_recovery(key: [u8; 16], frames: Vec<Vec<u8>>, packet_size: usize) -> Self {
        Self::spawn(key, frames, packet_size, true, true)
    }

    fn spawn(
        key: [u8; 16],
        frames: Vec<Vec<u8>>,
        packet_size: usize,
        terminate_after_video: bool,
        reference_recovery: bool,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let announce = Arc::new(Mutex::new(None));
        let control_messages = Arc::new(Mutex::new(Vec::new()));

        let video = UdpSocket::bind("127.0.0.1:0").unwrap();
        let audio = UdpSocket::bind("127.0.0.1:0").unwrap();
        let ctl = UdpSocket::bind("127.0.0.1:0").unwrap();
        let ports = (
            audio.local_addr().unwrap().port(),
            video.local_addr().unwrap().port(),
            ctl.local_addr().unwrap().port(),
        );
        let rtsp = TcpListener::bind("127.0.0.1:0").unwrap();
        let rtsp_port = rtsp.local_addr().unwrap().port();

        // RTSP
        {
            let announce = announce.clone();
            std::thread::spawn(move || {
                for stream in rtsp.incoming() {
                    let Ok(mut s) = stream else { break };
                    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // Read headers (+ body for ANNOUNCE).
                    loop {
                        let n = s.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Ok(m) = Message::parse(&buf) {
                            let want = m
                                .get("Content-length")
                                .and_then(|l| l.parse::<usize>().ok())
                                .unwrap_or(0);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") && m.body.len() >= want {
                                break;
                            }
                        }
                    }
                    let req = Message::parse(&buf).unwrap();
                    let (method, target) = req.request_line.clone().unwrap();
                    let seq = req.get("CSeq").unwrap_or("0").to_owned();
                    let mut resp = Message {
                        status: 200,
                        reason: "OK".into(),
                        ..Default::default()
                    }
                    .header("CSeq", seq);
                    match method.as_str() {
                        "DESCRIBE" => {
                            let mut body = b"a=x-ss-general.featureFlags:3\na=x-ss-general.encryptionSupported:5\na=x-ss-general.encryptionRequested:1\na=fmtp:97 surround-params=21101\n".to_vec();
                            if reference_recovery {
                                body.extend_from_slice(
                                    b"a=x-nv-video[0].refPicInvalidation:1\nsprop-parameter-sets=AAAAAU\n",
                                );
                            }
                            resp = resp.with_body("application/sdp", body);
                        }
                        "SETUP" => {
                            let port = if target.contains("audio") {
                                ports.0
                            } else if target.contains("video") {
                                ports.1
                            } else {
                                ports.2
                            };
                            resp = resp
                                .header("Session", "DEADBEEFCAFE;timeout = 90")
                                .header("Transport", format!("server_port={port}"));
                            resp = if target.contains("control") {
                                resp.header("X-SS-Connect-Data", CONNECT_DATA.to_string())
                            } else {
                                resp.header("X-SS-Ping-Payload", String::from_utf8_lossy(PING_PAYLOAD).into_owned())
                            };
                        }
                        "ANNOUNCE" => {
                            assert_eq!(req.get("Session"), Some("DEADBEEFCAFE"));
                            *announce.lock().unwrap() = Some(String::from_utf8_lossy(&req.body).into_owned());
                        }
                        _ => {}
                    }
                    let _ = s.write_all(&resp.serialize());
                    if method == "PLAY" {
                        break;
                    }
                }
            });
        }

        let (start_tx, start_rx): (Sender<()>, Receiver<()>) = mpsc::channel();
        let (frames_done_tx, frames_done_rx) = mpsc::channel::<()>();

        // Control (minimal ENet server)
        {
            let msgs = control_messages.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                ctl.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
                let mut client: Option<SocketAddr> = None;
                let mut buf = [0u8; 4096];
                let mut out_seq = 0u16;
                let mut started = false;
                let header = |sent_time: bool| -> Vec<u8> {
                    let mut h = ((1u16 << 12) | if sent_time { 0x8000 } else { 0 })
                        .to_be_bytes()
                        .to_vec();
                    if sent_time {
                        h.extend_from_slice(&0u16.to_be_bytes());
                    }
                    h
                };
                while !stop.load(Ordering::Relaxed) {
                    if frames_done_rx.try_recv().is_ok()
                        && let Some(c) = client
                    {
                        // Graceful termination, encrypted with the host-origin IV.
                        let mut plain_payload = Vec::new();
                        plain_payload.extend_from_slice(&control::TERMINATION_GRACEFUL.to_be_bytes());
                        let packet = seal_host(&key, 0, control::PT_TERMINATION, &plain_payload);
                        out_seq += 1;
                        let mut d = header(true);
                        d.extend_from_slice(&[6 | 0x80, 0, 0, 0]);
                        d[6..8].copy_from_slice(&out_seq.to_be_bytes());
                        d.extend_from_slice(&(packet.len() as u16).to_be_bytes());
                        d.extend_from_slice(&packet);
                        let _ = ctl.send_to(&d, c);
                    }
                    let Ok((len, from)) = ctl.recv_from(&mut buf) else {
                        continue;
                    };
                    client = Some(from);
                    let d = &buf[..len];
                    let flags = be16(d, 0);
                    let sent = if flags & 0x8000 != 0 { Some(be16(d, 2)) } else { None };
                    let mut pos = if sent.is_some() { 4 } else { 2 };
                    let mut acks = Vec::new();
                    while pos + 4 <= d.len() {
                        let cmd = d[pos] & 0x0f;
                        let ack_flag = d[pos] & 0x80 != 0;
                        let channel = d[pos + 1];
                        let seq = be16(d, pos + 2);
                        match cmd {
                            2 => {
                                // CONNECT → VERIFY_CONNECT
                                let c = &d[pos..pos + 48];
                                assert_eq!(u32::from_be_bytes([c[44], c[45], c[46], c[47]]), CONNECT_DATA);
                                let mut v = header(true);
                                v.extend_from_slice(&[3 | 0x80, 0xff, 0, 1]);
                                v.extend_from_slice(&0u16.to_be_bytes());
                                v.push(1); // client's incoming session
                                v.push(2); // client's outgoing session
                                for x in [1400u32, 65536, 0x30, 0, 0, 5000, 2, 2] {
                                    v.extend_from_slice(&x.to_be_bytes());
                                }
                                v.extend_from_slice(&c[40..44]);
                                ctl.send_to(&v, from).unwrap();
                                pos += 48;
                            }
                            1 => pos += 8,
                            5 => pos += 4,
                            4 => pos += 8,
                            6 => {
                                let n = be16(d, pos + 4) as usize;
                                let data = d[pos + 6..pos + 6 + n].to_vec();
                                if let Some((ptype, payload)) = control::open(&key, &data, b'C') {
                                    if ptype == control::PT_START_B && !started {
                                        started = true;
                                        let _ = start_tx.send(());
                                    }
                                    msgs.lock().unwrap().push((ptype, payload));
                                }
                                pos += 6 + n;
                            }
                            9 => {
                                let n = be16(d, pos + 6) as usize;
                                if let Some(message) = control::open(&key, &d[pos + 8..pos + 8 + n], b'C') {
                                    msgs.lock().unwrap().push(message);
                                }
                                pos += 8 + n;
                            }
                            _ => break,
                        }
                        if ack_flag
                            && cmd != 2
                            && let Some(st) = sent
                        {
                            let s = seq.to_be_bytes();
                            let t = st.to_be_bytes();
                            acks.extend_from_slice(&[1, channel, s[0], s[1], s[0], s[1], t[0], t[1]]);
                        }
                    }
                    if !acks.is_empty() {
                        let mut a = header(false);
                        a.extend_from_slice(&acks);
                        let _ = ctl.send_to(&a, from);
                    }
                }
            });
        }

        // Video
        {
            let stop = stop.clone();
            let control_messages = control_messages.clone();
            std::thread::spawn(move || {
                video.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
                let _ = start_rx.recv_timeout(Duration::from_secs(10));
                let mut buf = [0u8; 64];
                let deadline = Instant::now() + Duration::from_secs(10);
                let client = loop {
                    if stop.load(Ordering::Relaxed) || Instant::now() > deadline {
                        return;
                    }
                    if let Ok((n, from)) = video.recv_from(&mut buf)
                        && n == 20
                        && &buf[..16] == PING_PAYLOAD
                    {
                        break from;
                    }
                };
                let mut seq = 0u16;
                for (i, frame) in frames.iter().enumerate() {
                    let frame_type = match (i, reference_recovery) {
                        (0, _) => 2,
                        (3, true) => {
                            let deadline = Instant::now() + Duration::from_secs(1);
                            while !control_messages
                                .lock()
                                .unwrap()
                                .iter()
                                .any(|(ty, _)| *ty == control::PT_INVALIDATE_REF_FRAMES)
                            {
                                if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                                    return;
                                }
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            5
                        }
                        _ => 1,
                    };
                    let packets =
                        crate::video::packetize_frame_type(i as u32 + 1, frame, packet_size, 20, seq, frame_type);
                    seq = seq.wrapping_add(packets.len() as u16);
                    if reference_recovery && i == 1 {
                        continue;
                    }
                    for (n, p) in packets.iter().enumerate() {
                        // Lose one data packet per frame; FEC must cover it.
                        if n == 1 {
                            continue;
                        }
                        video.send_to(p, client).unwrap();
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                if terminate_after_video {
                    std::thread::sleep(Duration::from_millis(200));
                    let _ = frames_done_tx.send(());
                } else {
                    // Keep accepting pings while only the video output stops.
                    while !stop.load(Ordering::Relaxed) {
                        let _ = video.recv_from(&mut buf);
                    }
                }
            });
        }
        if !terminate_after_video {
            let stop = stop.clone();
            std::thread::spawn(move || send_audio_until_stopped(audio, &stop));
        }

        Self {
            rtsp_port,
            announce,
            control_messages,
            stop,
        }
    }
}

fn send_audio_until_stopped(audio: UdpSocket, stop: &AtomicBool) {
    audio.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    let mut buf = [0u8; 64];
    let peer = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if let Ok((n, from)) = audio.recv_from(&mut buf)
            && n == 20
            && &buf[..16] == PING_PAYLOAD
        {
            break from;
        }
    };
    // An RTP packet with a dummy payload. The test sink counts these packets
    // without decoding their audio.
    let mut packet = [0u8; 13];
    packet[0] = 0x80;
    packet[1] = 97;
    packet[12] = 1;
    let mut seq = 0u16;
    let mut timestamp = 0u32;
    while !stop.load(Ordering::Relaxed) {
        packet[2..4].copy_from_slice(&seq.to_be_bytes());
        packet[4..8].copy_from_slice(&timestamp.to_be_bytes());
        let _ = audio.send_to(&packet, peer);
        seq = seq.wrapping_add(1);
        timestamp = timestamp.wrapping_add(240);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Seals a control message the way the host does (origin 'H').
fn seal_host(key: &[u8; 16], seq: u32, ptype: u16, payload: &[u8]) -> Vec<u8> {
    let mut plain = Vec::new();
    plain.extend_from_slice(&ptype.to_le_bytes());
    plain.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    plain.extend_from_slice(payload);
    let mut iv = [0u8; 12];
    iv[..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = b'H';
    iv[11] = b'C';
    let (cipher, tag) = hestia_crypto::stream::gcm_encrypt(key, &iv, &plain);
    let mut out = Vec::new();
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&((4 + 16 + cipher.len()) as u16).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&tag);
    out.extend_from_slice(&cipher);
    out
}
