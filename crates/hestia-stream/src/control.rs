//! The control stream: an ENet connection carrying (AES-GCM encrypted)
//! control messages in both directions — start packets, periodic pings, IDR
//! requests and input from us; termination, rumble and HDR state from the host.
//! Port of moonlight-common-c `ControlStream.c` (7.1.431+ encrypted path).

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use hestia_crypto::stream::{gcm_decrypt, gcm_encrypt};
use log::{debug, info, warn};

use crate::enet::{self, Delivery, EnetClient, Event};

/// Channels (one per input class keeps them from blocking each other).
pub const CHANNEL_GENERIC: u8 = 0x00;
pub const CHANNEL_URGENT: u8 = 0x01;
pub const CHANNEL_KEYBOARD: u8 = 0x02;
pub const CHANNEL_MOUSE: u8 = 0x03;
pub const CHANNEL_PEN: u8 = 0x04;
pub const CHANNEL_TOUCH: u8 = 0x05;
pub const CHANNEL_GAMEPAD_BASE: u8 = 0x10;
pub const CHANNEL_COUNT: usize = 0x30;

// Message types (Gen 7, encrypted control stream).
pub const PT_REQUEST_IDR: u16 = 0x0302; // also "START A"
pub const PT_START_B: u16 = 0x0307;
pub const PT_INVALIDATE_REF_FRAMES: u16 = 0x0301;
pub const PT_PERIODIC_PING: u16 = 0x0200;
pub const PT_INPUT: u16 = 0x0206;
pub const PT_RUMBLE: u16 = 0x010b;
pub const PT_TERMINATION: u16 = 0x0109;
pub const PT_HDR_MODE: u16 = 0x010e;
pub const PT_RUMBLE_TRIGGERS: u16 = 0x5500;
const PT_FRAME_FEC_STATUS: u16 = 0x5502;
const PT_ENCRYPTED: u16 = 0x0001;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_millis(100);
const TAG_LEN: usize = 16;

/// Termination code the host sends for a normal "app closed" shutdown.
pub const TERMINATION_GRACEFUL: u32 = 0x8003_0023;

#[derive(Debug, Clone, PartialEq)]
pub enum ControlEvent {
    /// The host ended the stream; `code` is its HRESULT-style reason.
    Terminated {
        code: u32,
    },
    Rumble {
        controller: u16,
        low: u16,
        high: u16,
    },
    TriggerRumble {
        controller: u16,
        left: u16,
        right: u16,
    },
    HdrMode {
        enabled: bool,
    },
    /// The ENet connection dropped without a termination message.
    ConnectionLost,
}

struct Inner {
    enet: EnetClient,
    key: [u8; 16],
    encrypted: bool,
    next_seq: u32,
    /// START A/B went out; pings wait for it (hosts expect START first).
    started: bool,
}

impl Inner {
    fn send(&mut self, ptype: u16, payload: &[u8], channel: u8, delivery: Delivery) -> io::Result<()> {
        let packet = if self.encrypted {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            seal(&self.key, seq, ptype, payload)
        } else {
            let mut p = Vec::with_capacity(2 + payload.len());
            p.extend_from_slice(&ptype.to_le_bytes());
            p.extend_from_slice(payload);
            p
        };
        self.enet.send(channel, &packet, delivery)
    }
}

fn control_iv(seq: u32, origin: u8) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = origin;
    iv[11] = b'C';
    iv
}

/// `[type=1 LE][length LE][seq LE][tag][AES-GCM(type LE, len LE, payload)]`
pub fn seal(key: &[u8; 16], seq: u32, ptype: u16, payload: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(4 + payload.len());
    plain.extend_from_slice(&ptype.to_le_bytes());
    plain.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    plain.extend_from_slice(payload);
    let (cipher, tag) = gcm_encrypt(key, &control_iv(seq, b'C'), &plain);
    let length = (4 + TAG_LEN + cipher.len()) as u16;
    let mut out = Vec::with_capacity(8 + TAG_LEN + cipher.len());
    out.extend_from_slice(&PT_ENCRYPTED.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&tag);
    out.extend_from_slice(&cipher);
    out
}

/// Decrypts a host message; returns `(type, payload)`.
pub fn open(key: &[u8; 16], data: &[u8], origin: u8) -> Option<(u16, Vec<u8>)> {
    if data.len() < 8 + TAG_LEN + 4 || u16::from_le_bytes([data[0], data[1]]) != PT_ENCRYPTED {
        return None;
    }
    let length = u16::from_le_bytes([data[2], data[3]]) as usize;
    if length < 4 + TAG_LEN + 4 || data.len() < 4 + length {
        return None;
    }
    let seq = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let tag = &data[8..8 + TAG_LEN];
    let cipher = &data[8 + TAG_LEN..4 + length];
    let plain = gcm_decrypt(key, &control_iv(seq, origin), cipher, tag)?;
    let ptype = u16::from_le_bytes([plain[0], plain[1]]);
    Some((ptype, plain[4..].to_vec()))
}

pub struct ControlStream {
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl ControlStream {
    /// Connects to the host's control port. `connect_data` is the
    /// `X-SS-Connect-Data` value from RTSP SETUP.
    pub fn connect(remote: SocketAddr, connect_data: u32, key: [u8; 16], encrypted: bool) -> io::Result<Self> {
        let enet = EnetClient::connect(remote, CHANNEL_COUNT, connect_data, CONNECT_TIMEOUT)?;
        info!("control stream connected to {remote}");
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                enet,
                key,
                encrypted,
                next_seq: 0,
                started: false,
            })),
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn send(&self, ptype: u16, payload: &[u8], channel: u8, delivery: Delivery) -> io::Result<()> {
        self.lock().send(ptype, payload, channel, delivery)
    }

    /// START A + START B; the host begins streaming after these. Sent under
    /// one lock, so no ping slips in between.
    pub fn start(&self) -> io::Result<()> {
        let mut inner = self.lock();
        inner.send(PT_REQUEST_IDR, &[0, 0], CHANNEL_GENERIC, Delivery::Reliable)?;
        inner.send(PT_START_B, &[0], CHANNEL_GENERIC, Delivery::Reliable)?;
        inner.started = true;
        Ok(())
    }

    pub fn request_idr(&self) -> io::Result<()> {
        debug!("requesting IDR frame");
        self.send(PT_REQUEST_IDR, &[0, 0], CHANNEL_URGENT, Delivery::Reliable)
    }

    pub(crate) fn invalidate_reference_frames(&self, first: u32, last: u32) -> io::Result<()> {
        // The wire format is two LE frame indices in 64-bit slots plus eight reserved bytes.
        let mut payload = [0; 24];
        payload[..4].copy_from_slice(&first.to_le_bytes());
        payload[8..12].copy_from_slice(&last.to_le_bytes());
        debug!("requesting reference frame invalidation for {first}–{last}");
        self.send(PT_INVALIDATE_REF_FRAMES, &payload, CHANNEL_URGENT, Delivery::Reliable)
    }

    /// Sends an already-encoded input packet (see [`crate::input`]).
    pub fn send_input(&self, packet: &[u8], channel: u8) -> io::Result<()> {
        self.send(PT_INPUT, packet, channel, Delivery::Reliable)
    }

    pub(crate) fn report_fec(&self, status: crate::video::FrameFecStatus) -> io::Result<()> {
        self.send(PT_FRAME_FEC_STATUS, &status.0, CHANNEL_GENERIC, Delivery::Unsequenced)
    }

    pub fn round_trip(&self) -> (u32, u32) {
        self.lock().enet.round_trip()
    }

    /// See [`EnetClient::measured_round_trip`].
    pub fn measured_round_trip(&self) -> Option<(u32, u32)> {
        self.lock().enet.measured_round_trip()
    }

    /// Starts the receive and ping threads. Host events go to `events`.
    pub fn run(&mut self, events: Sender<ControlEvent>) -> io::Result<()> {
        self.run_with_events(move |event| {
            let _ = events.send(event);
        })
    }

    pub(super) fn run_with_events(&mut self, emit: impl Fn(ControlEvent) + Send + 'static) -> io::Result<()> {
        let socket = self.lock().enet.socket_handle()?;

        // Receive / retransmit loop.
        {
            let inner = self.inner.clone();
            let stop = self.stop.clone();
            self.threads.push(
                std::thread::Builder::new()
                    .name("hestia-control-rx".into())
                    .spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            let (polled, deadline) = {
                                let mut guard = inner.lock().unwrap_or_else(|p| p.into_inner());
                                let polled = guard.enet.poll();
                                let deadline = guard.enet.next_deadline();
                                let key = guard.key;
                                let encrypted = guard.encrypted;
                                (polled.map(|e| (e, key, encrypted)), deadline)
                            };
                            match polled {
                                Ok((evs, key, encrypted)) => {
                                    for ev in evs {
                                        if !handle_event(ev, &key, encrypted, &emit) {
                                            stop.store(true, Ordering::Relaxed);
                                        }
                                    }
                                }
                                // A signal (e.g. Android freezing the app): try again.
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                                Err(e) => {
                                    warn!("control stream receive failed: {e}");
                                    emit(ControlEvent::ConnectionLost);
                                    break;
                                }
                            }
                            let _ = enet::wait_readable(&socket, deadline);
                        }
                    })?,
            );
        }

        // Periodic ping; the host drops the session without it.
        {
            let inner = self.inner.clone();
            let stop = self.stop.clone();
            self.threads.push(
                std::thread::Builder::new()
                    .name("hestia-control-ping".into())
                    .spawn(move || {
                        let mut payload = [0u8; 8];
                        payload[..4].copy_from_slice(&4u32.to_le_bytes());
                        while !stop.load(Ordering::Relaxed) {
                            let mut guard = inner.lock().unwrap_or_else(|p| p.into_inner());
                            if guard.started {
                                let result =
                                    guard.send(PT_PERIODIC_PING, &payload, CHANNEL_GENERIC, Delivery::Reliable);
                                if let Err(e) = result {
                                    debug!("control ping failed: {e}");
                                }
                            }
                            drop(guard);
                            std::thread::sleep(PING_INTERVAL);
                        }
                    })?,
            );
        }
        Ok(())
    }

    /// Disconnects gracefully and joins the threads.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        self.lock().enet.disconnect(Duration::from_millis(500));
    }
}

impl Drop for ControlStream {
    fn drop(&mut self) {
        if !self.stop.load(Ordering::Relaxed) || !self.threads.is_empty() {
            self.shutdown();
        }
    }
}

/// Returns false when the stream is over.
fn handle_event(ev: Event, key: &[u8; 16], encrypted: bool, emit: &impl Fn(ControlEvent)) -> bool {
    match ev {
        Event::Receive { data, .. } => {
            let message = if encrypted {
                open(key, &data, b'H')
            } else if data.len() >= 2 {
                Some((u16::from_le_bytes([data[0], data[1]]), data[2..].to_vec()))
            } else {
                None
            };
            let Some((ptype, payload)) = message else {
                debug!("dropping undecodable control message ({} bytes)", data.len());
                return true;
            };
            match parse_host_message(ptype, &payload) {
                Some(event) => {
                    let terminated = matches!(event, ControlEvent::Terminated { .. });
                    emit(event);
                    !terminated
                }
                None => true,
            }
        }
        Event::Disconnected { .. } | Event::TimedOut => {
            emit(ControlEvent::ConnectionLost);
            false
        }
        Event::Connected => true,
    }
}

fn le16(p: &[u8], at: usize) -> u16 {
    p.get(at..at + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

pub fn parse_host_message(ptype: u16, payload: &[u8]) -> Option<ControlEvent> {
    match ptype {
        PT_TERMINATION => {
            let code = if payload.len() >= 4 {
                u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
            } else {
                le16(payload, 0) as u32
            };
            Some(ControlEvent::Terminated { code })
        }
        PT_RUMBLE => Some(ControlEvent::Rumble {
            controller: le16(payload, 4),
            low: le16(payload, 6),
            high: le16(payload, 8),
        }),
        PT_RUMBLE_TRIGGERS => Some(ControlEvent::TriggerRumble {
            controller: le16(payload, 0),
            left: le16(payload, 2),
            right: le16(payload, 4),
        }),
        PT_HDR_MODE => Some(ControlEvent::HdrMode {
            enabled: payload.first().copied().unwrap_or(0) != 0,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_layout() {
        let key = [9u8; 16];
        let packet = seal(&key, 5, PT_START_B, &[0]);
        assert_eq!(&packet[0..2], &[1, 0]);
        // length = seq(4) + tag(16) + encrypted (4 header + 1 payload)
        assert_eq!(u16::from_le_bytes([packet[2], packet[3]]), 25);
        assert_eq!(&packet[4..8], &5u32.to_le_bytes());
        assert_eq!(packet.len(), 4 + 25);
        // We can open our own message when pretending to be the host.
        let (ptype, payload) = open(&key, &packet, b'C').unwrap();
        assert_eq!((ptype, payload), (PT_START_B, vec![0]));
        // ...but not with the host-origin IV.
        assert!(open(&key, &packet, b'H').is_none());
    }

    #[test]
    fn host_messages() {
        assert_eq!(
            parse_host_message(PT_TERMINATION, &0x8003_0023u32.to_be_bytes()),
            Some(ControlEvent::Terminated {
                code: TERMINATION_GRACEFUL
            })
        );
        let mut rumble = vec![0u8; 4];
        rumble.extend_from_slice(&[1, 0, 0x34, 0x12, 0xff, 0xff]);
        assert_eq!(
            parse_host_message(PT_RUMBLE, &rumble),
            Some(ControlEvent::Rumble {
                controller: 1,
                low: 0x1234,
                high: 0xffff
            })
        );
        assert_eq!(
            parse_host_message(PT_HDR_MODE, &[1]),
            Some(ControlEvent::HdrMode { enabled: true })
        );
        assert_eq!(parse_host_message(0x9999, &[]), None);
    }
}
