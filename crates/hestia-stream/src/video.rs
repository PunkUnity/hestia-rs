//! Video: UDP receive, (optional) decryption, FEC-block reassembly with
//! Reed–Solomon recovery and frame depacketization. Ported from
//! moonlight-common-c `VideoStream.c`, `RtpVideoQueue.c` and
//! `VideoDepacketizer.c`, simplified to "drop until the next IDR" on loss
//! (we don't use reference frame invalidation).
//!
//! Wire layout of one shard (`packet_size + 16` bytes):
//! `RTP(12) | reserved(4) | NV_VIDEO_PACKET(16) | payload`. The payload of the
//! first data shard starts with an 8-byte frame header. FEC covers whole
//! shards; after recovery only the payload bytes are trusted.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hestia_crypto::stream::gcm_decrypt;
use log::{debug, info, warn};

use crate::fec;
use crate::sink::{EncodedFrame, KeyframeRequester, SubmitResult, VideoCodec, VideoFormat, VideoSink};

pub const RTP_HEADER_LEN: usize = 12;
pub const MAX_RTP_HEADER_LEN: usize = 16;
pub const NV_HEADER_LEN: usize = 16;
pub const SHARD_HEADER_LEN: usize = MAX_RTP_HEADER_LEN + NV_HEADER_LEN;
const ENC_PREFIX_LEN: usize = 12 + 4 + 16;
const FLAG_EXTENSION: u8 = 0x10;
pub const FLAG_CONTAINS_PIC_DATA: u8 = 0x1;
pub const FLAG_EOF: u8 = 0x2;
pub const FLAG_SOF: u8 = 0x4;

const PING_INTERVAL: Duration = Duration::from_millis(500);
const FIRST_DATA_TIMEOUT: Duration = Duration::from_secs(10);
const VIDEO_STALL_PROBE: Duration = Duration::from_secs(2);
const VIDEO_STALL_TIMEOUT: Duration = Duration::from_secs(6);
const REORDER_WINDOW: Duration = Duration::from_millis(6);
const MAX_FRAME_LOOKAHEAD: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketInfo {
    pub seq: u16,
    pub timestamp: u32,
    pub frame: u32,
    pub flags: u8,
    pub fec_index: u16,
    pub data_shards: u16,
    pub fec_percent: u16,
    pub block: u8,
    pub last_block: u8,
}

impl PacketInfo {
    pub fn parity_shards(&self) -> u16 {
        ((self.data_shards as u32 * self.fec_percent as u32).div_ceil(100)) as u16
    }
}

pub fn parse_packet(p: &[u8]) -> Option<PacketInfo> {
    if p.len() < RTP_HEADER_LEN {
        return None;
    }
    let nv = if p[0] & FLAG_EXTENSION != 0 { 16 } else { 12 };
    if p.len() < nv + NV_HEADER_LEN {
        return None;
    }
    let le32 = |at: usize| u32::from_le_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]]);
    let fec_info = le32(nv + 12);
    let multi_blocks = p[nv + 11];
    Some(PacketInfo {
        seq: u16::from_be_bytes([p[2], p[3]]),
        timestamp: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
        frame: le32(nv + 4),
        flags: p[nv + 8],
        fec_index: ((fec_info & 0x003f_f000) >> 12) as u16,
        data_shards: ((fec_info & 0xffc0_0000) >> 22) as u16,
        fec_percent: ((fec_info & 0x0000_0ff0) >> 4) as u16,
        block: (multi_blocks >> 4) & 0x3,
        last_block: (multi_blocks >> 6) & 0x3,
    })
}

fn frame_before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// One reassembled access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledFrame {
    pub frame: u32,
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub host_latency_100us: u16,
    pub received_at: Instant,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AssemblerStats {
    pub frames: u64,
    pub recovered_packets: u64,
    pub lost_frames: u64,
    pub dropped_waiting_idr: u64,
    /// Packets that arrived for a frame already given up on (marked lost).
    pub late_packets: u64,
    /// Distinct lost frames that received at least one such late packet:
    /// frames declared lost although their data was still on the way.
    pub late_frames: u64,
    /// How late those packets were after the frame was given up on, in
    /// [`LATE_BUCKETS_MS`] buckets (the last one is everything later).
    pub late_buckets: [u64; LATE_BUCKETS_MS.len() + 1],
    pub late_max_us: u64,
}

/// Upper bounds of the lateness buckets in [`AssemblerStats::late_buckets`].
pub const LATE_BUCKETS_MS: [u64; 5] = [10, 25, 50, 100, 250];
/// How many recently lost frames are remembered for late-arrival accounting.
const LATE_TRACKED_FRAMES: usize = 256;

/// A frame given up on, remembered to count packets that still arrive.
struct ExpiredFrame {
    number: u32,
    at: Instant,
    late: bool,
}

/// Sunshine's SS_FRAME_FEC_STATUS, packed in network byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameFecStatus(pub [u8; 21]);

struct Block {
    info: PacketInfo,
    lowest_seq: u16,
    data_shards: usize,
    parity_shards: usize,
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
    received_data: usize,
}

impl Block {
    fn new(info: PacketInfo) -> Self {
        let data_shards = usize::from(info.data_shards);
        let parity_shards = usize::from(info.parity_shards());
        Self {
            info,
            lowest_seq: info.seq.wrapping_sub(info.fec_index),
            data_shards,
            parity_shards,
            shards: vec![None; data_shards + parity_shards],
            received: 0,
            received_data: 0,
        }
    }

    fn complete(&self) -> bool {
        self.received >= self.data_shards
    }

    fn status(&self) -> FrameFecStatus {
        let highest = self
            .shards
            .iter()
            .rposition(Option::is_some)
            .expect("block received a shard");
        let contiguous = self.shards.iter().take_while(|shard| shard.is_some()).count();
        let mut bytes = [0; 21];
        bytes[..4].copy_from_slice(&self.info.frame.to_be_bytes());
        let counts = [
            self.lowest_seq.wrapping_add(highest as u16),
            self.lowest_seq.wrapping_add(contiguous as u16),
            (highest + 1 - self.received) as u16,
            self.data_shards as u16,
            self.parity_shards as u16,
            self.received_data as u16,
            (self.received - self.received_data) as u16,
        ];
        for (field, count) in bytes[4..18].chunks_exact_mut(2).zip(counts) {
            field.copy_from_slice(&count.to_be_bytes());
        }
        bytes[18..].copy_from_slice(&[self.info.fec_percent as u8, self.info.block, self.info.last_block + 1]);
        FrameFecStatus(bytes)
    }

    fn add(
        &mut self,
        info: PacketInfo,
        packet: &[u8],
        shard_len: usize,
        report_fec: bool,
    ) -> Result<(usize, Option<FrameFecStatus>), fec::FecError> {
        let index = usize::from(info.seq.wrapping_sub(self.lowest_seq));
        if self.complete() || index >= self.shards.len() || self.shards[index].is_some() {
            return Ok((0, None));
        }
        let mut shard = Vec::with_capacity(shard_len);
        shard.extend_from_slice(packet);
        shard.resize(shard_len, 0);
        self.shards[index] = Some(shard);
        self.received += 1;
        if index < self.data_shards {
            self.received_data += 1;
        }
        if !self.complete() || self.received_data == self.data_shards {
            return Ok((0, None));
        }
        let status = report_fec.then(|| self.status());
        fec::reconstruct(&mut self.shards, self.data_shards, self.parity_shards, shard_len)?;
        Ok((self.data_shards - self.received_data, status))
    }
}

struct FrameState {
    number: u32,
    received_at: Instant,
    blocks: [Option<Block>; 4],
    last_block: usize,
    result: Option<Option<AssembledFrame>>,
}

/// Reassembles frames from shards. Pure logic, no I/O. Newer frames may finish
/// first but are emitted in sequence while the preceding frame can still arrive.
pub struct FrameAssembler {
    report_fec: bool,
    shard_len: usize,
    codec: VideoCodec,
    next_frame: Option<u32>,
    pending: Vec<FrameState>,
    waiting_for_idr: bool,
    /// Recently lost frames, oldest first (late-arrival accounting only).
    expired: std::collections::VecDeque<ExpiredFrame>,
    pub stats: AssemblerStats,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AssemblerOutput {
    FecStatus(FrameFecStatus),
    Frame(AssembledFrame),
    /// A frame was lost; the decoder needs an IDR to recover.
    NeedIdr,
}

impl FrameAssembler {
    /// `packet_size` is the value announced as `x-nv-video[0].packetSize`.
    pub fn new(packet_size: u32, codec: VideoCodec) -> Self {
        Self {
            shard_len: packet_size as usize + MAX_RTP_HEADER_LEN,
            codec,
            next_frame: None,
            pending: Vec::with_capacity(MAX_FRAME_LOOKAHEAD as usize + 2),
            report_fec: false,
            waiting_for_idr: true,
            expired: std::collections::VecDeque::with_capacity(LATE_TRACKED_FRAMES),
            stats: AssemblerStats::default(),
        }
    }

    pub fn shard_len(&self) -> usize {
        self.shard_len
    }

    /// Suppresses delta frames until the decoder has a fresh reference.
    pub fn wait_for_keyframe(&mut self) {
        self.waiting_for_idr = true;
    }

    pub fn add(&mut self, packet: &[u8]) -> Vec<AssemblerOutput> {
        self.add_at(packet, Instant::now())
    }

    fn add_at(&mut self, packet: &[u8], now: Instant) -> Vec<AssemblerOutput> {
        match self.packet_info(packet) {
            Some(info) => self.add_shard_at(packet, info, now),
            None => self.expire(now),
        }
    }

    fn packet_info(&self, packet: &[u8]) -> Option<PacketInfo> {
        let info = parse_packet(packet)?;
        let total_shards = usize::from(info.data_shards) + usize::from(info.parity_shards());
        (info.data_shards != 0
            && total_shards <= fec::MAX_SHARDS
            && usize::from(info.fec_index) < total_shards
            && info.block <= info.last_block
            && packet.len() <= self.shard_len)
            .then_some(info)
    }

    fn add_shard_at(&mut self, packet: &[u8], info: PacketInfo, now: Instant) -> Vec<AssemblerOutput> {
        let mut out = self.expire(now);
        let next = *self.next_frame.get_or_insert(info.frame);
        if frame_before(info.frame, next) {
            self.note_late(info.frame, now);
            return out;
        }
        let index = self
            .pending
            .binary_search_by_key(&info.frame.wrapping_sub(next), |f| f.number.wrapping_sub(next))
            .unwrap_or_else(|index| {
                self.pending.insert(
                    index,
                    FrameState {
                        number: info.frame,
                        received_at: now,
                        blocks: Default::default(),
                        last_block: usize::from(info.last_block),
                        result: None,
                    },
                );
                index
            });
        let frame = &mut self.pending[index];
        if frame.result.is_none() && frame.last_block == usize::from(info.last_block) {
            let block = frame.blocks[usize::from(info.block)].get_or_insert_with(|| Block::new(info));
            match block.add(info, packet, self.shard_len, self.report_fec) {
                Ok((recovered, status)) => {
                    if let Some(status) = status {
                        out.push(AssemblerOutput::FecStatus(status));
                    }
                    self.stats.recovered_packets += recovered as u64;
                    if frame.blocks[..=frame.last_block]
                        .iter()
                        .all(|b| b.as_ref().is_some_and(Block::complete))
                    {
                        let payloads: Vec<_> = frame
                            .blocks
                            .iter()
                            .flatten()
                            .flat_map(|block| block.shards[..block.data_shards].iter().flatten())
                            .map(|shard| &shard[SHARD_HEADER_LEN..])
                            .collect();
                        frame.result = Some(depacketize_at(frame.number, &payloads, self.codec, frame.received_at));
                        frame.blocks = Default::default();
                    }
                }
                Err(_) => frame.result = Some(None),
            }
        }

        self.drain(now, &mut out);
        out
    }

    fn expire(&mut self, now: Instant) -> Vec<AssemblerOutput> {
        let mut out = Vec::new();
        self.drain(now, &mut out);
        out
    }

    fn drain(&mut self, now: Instant, out: &mut Vec<AssemblerOutput>) {
        while let Some(first) = self.pending.first_mut() {
            let next = self.next_frame.expect("buffered frames have a sequence start");
            let number = first.number;
            if number == next
                && let Some(result) = first.result.take()
            {
                self.pending.remove(0);
                self.next_frame = Some(next.wrapping_add(1));
                match result {
                    Some(frame) if !self.waiting_for_idr || frame.keyframe => {
                        self.waiting_for_idr = false;
                        self.stats.frames += 1;
                        out.push(AssemblerOutput::Frame(frame));
                    }
                    Some(_) => self.stats.dropped_waiting_idr += 1,
                    None => self.mark_lost(1, out),
                }
                continue;
            }

            // Evidence of a newer frame starts the deadline. A slow sender
            // alone is never treated as packet loss.
            let future_index = if number == next { 1 } else { 0 };
            let Some(future) = self.pending.get(future_index..) else {
                break;
            };
            let Some(oldest_arrival) = future.iter().map(|frame| frame.received_at).min() else {
                break;
            };
            let newest = self.pending.last().expect("we have at least one future frame");
            if newest.number.wrapping_sub(next) <= MAX_FRAME_LOOKAHEAD
                && now.saturating_duration_since(oldest_arrival) < REORDER_WINDOW
            {
                break;
            }
            let skipped = if number == next {
                let expired = self.pending.remove(0);
                self.remember_expired(number, now);
                if self.report_fec {
                    for block in expired
                        .blocks
                        .iter()
                        .flatten()
                        .filter(|b| !b.complete() && b.received > 0)
                    {
                        out.push(AssemblerOutput::FecStatus(block.status()));
                    }
                }
                1
            } else {
                let skipped = number.wrapping_sub(next);
                // Frames with no packet yet: remember them too, a packet may
                // still arrive. Only the most recent ones fit.
                let first = skipped.saturating_sub(LATE_TRACKED_FRAMES as u32);
                for offset in first..skipped {
                    self.remember_expired(next.wrapping_add(offset), now);
                }
                skipped
            };
            self.next_frame = Some(next.wrapping_add(skipped));
            self.mark_lost(skipped, out);
        }
    }

    fn remember_expired(&mut self, number: u32, at: Instant) {
        if self.expired.len() == LATE_TRACKED_FRAMES {
            self.expired.pop_front();
        }
        self.expired.push_back(ExpiredFrame {
            number,
            at,
            late: false,
        });
    }

    /// A packet for a frame before the next expected one: count it when that
    /// frame was given up on (duplicates of delivered frames are ignored).
    fn note_late(&mut self, number: u32, now: Instant) {
        let Some(frame) = self.expired.iter_mut().rev().find(|f| f.number == number) else {
            return;
        };
        let late = now.saturating_duration_since(frame.at);
        self.stats.late_packets += 1;
        if !frame.late {
            frame.late = true;
            self.stats.late_frames += 1;
        }
        let ms = late.as_millis() as u64;
        let bucket = LATE_BUCKETS_MS
            .iter()
            .position(|&limit| ms < limit)
            .unwrap_or(LATE_BUCKETS_MS.len());
        self.stats.late_buckets[bucket] += 1;
        self.stats.late_max_us = self.stats.late_max_us.max(late.as_micros() as u64);
    }

    fn mark_lost(&mut self, count: u32, out: &mut Vec<AssemblerOutput>) {
        self.stats.lost_frames += u64::from(count);
        if self.codec == VideoCodec::PyroWave {
            return;
        }
        self.waiting_for_idr = true;
        if !matches!(out.last(), Some(AssemblerOutput::NeedIdr)) {
            out.push(AssemblerOutput::NeedIdr);
        }
    }
}

/// Joins shard payloads into an access unit and strips the frame header.
pub fn depacketize(frame: u32, payloads: &[Vec<u8>], codec: VideoCodec) -> Option<AssembledFrame> {
    depacketize_at(frame, payloads, codec, Instant::now())
}

fn depacketize_at(
    frame: u32,
    payloads: &[impl AsRef<[u8]>],
    codec: VideoCodec,
    received_at: Instant,
) -> Option<AssembledFrame> {
    let first = payloads.first()?.as_ref();
    if first.len() < 8 {
        return None;
    }
    let header_len = if first[0] == 0x01 { 8 } else { 44 };
    let frame_type = first[3];
    let latency = u16::from_le_bytes([first[1], first[2]]);
    let last_len = u16::from_le_bytes([first[4], first[5]]) as usize;
    let capacity = first.len();

    // Hermes pads the last RTP shard. Zeroes are valid codec bytes, so use
    // only the explicit last-payload length, including the single-shard case.
    if codec == VideoCodec::PyroWave {
        let (last, prefix) = payloads.split_last()?;
        let tail = last.as_ref().get(..last_len)?;
        if header_len != 8 || tail.is_empty() {
            return None;
        }
        let mut parts = prefix.iter().map(AsRef::as_ref).chain(std::iter::once(tail));
        let start = parts.next()?.get(header_len..)?;
        let mut data = Vec::with_capacity(start.len() + parts.clone().map(<[u8]>::len).sum::<usize>());
        data.extend_from_slice(start);
        for part in parts {
            data.extend_from_slice(part);
        }
        return (!data.is_empty()).then_some(AssembledFrame {
            frame,
            data,
            keyframe: true,
            host_latency_100us: latency,
            received_at,
        });
    }

    let total_len: usize = payloads.iter().map(|part| part.as_ref().len()).sum();
    if total_len < header_len {
        return None;
    }
    let mut data = Vec::with_capacity(total_len - header_len);
    data.extend_from_slice(first.get(header_len..)?);
    for part in &payloads[1..] {
        data.extend_from_slice(part.as_ref());
    }
    // Exact length: (packets - 1) full payloads + the last packet's length.
    if header_len == 8 && last_len > 0 && last_len <= capacity {
        let exact = (payloads.len() - 1) * capacity + last_len;
        if exact <= total_len && exact >= header_len {
            data.truncate(exact - header_len);
        }
    } else if codec != VideoCodec::Av1 {
        while data.last() == Some(&0) {
            data.pop();
        }
    }
    let keyframe = frame_type == 2 || is_keyframe_bitstream(&data, codec);
    Some(AssembledFrame {
        frame,
        data,
        keyframe,
        host_latency_100us: latency,
        received_at,
    })
}

/// Scans Annex B NAL headers for IDR/parameter sets.
pub fn is_keyframe_bitstream(data: &[u8], codec: VideoCodec) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        let start = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            Some(i + 3)
        } else {
            None
        };
        if let Some(n) = start {
            if n < data.len() {
                let header = data[n];
                match codec {
                    VideoCodec::H264 => {
                        let t = header & 0x1f;
                        if t == 5 || t == 7 {
                            return true;
                        }
                        if t == 1 {
                            return false;
                        }
                    }
                    VideoCodec::Hevc => {
                        let t = (header >> 1) & 0x3f;
                        if (16..=21).contains(&t) || t == 32 || t == 33 {
                            return true;
                        }
                        if t < 16 {
                            return false;
                        }
                    }
                    VideoCodec::Av1 | VideoCodec::PyroWave => return false,
                }
            }
            i = n;
        } else {
            i += 1;
        }
    }
    false
}

/// Configuration of the receive side.
#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub report_fec: bool,
    pub remote: SocketAddr,
    pub ping_payload: Option<[u8; 16]>,
    pub packet_size: u32,
    pub format: VideoFormat,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `Some(key)` when the host encrypts video.
    pub decrypt_key: Option<[u8; 16]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoFeedback {
    FecStatus(FrameFecStatus),
    NeedIdr { loss_detected_at: Option<Instant> },
    FirstFrame,
    NoVideo,
    VideoStalled,
    SocketFailed(String),
    SinkFailed(String),
}

#[derive(Debug, Default)]
pub struct VideoCounters {
    pub packets: AtomicU64,
    pub fec_reports_sent: AtomicU64,
    pub bytes: AtomicU64,
    pub frames: AtomicU64,
    pub recovered: AtomicU64,
    pub lost_frames: AtomicU64,
    pub dropped_waiting_idr: AtomicU64,
    /// See [`AssemblerStats::late_packets`] and the fields after it.
    pub late_packets: AtomicU64,
    pub late_frames: AtomicU64,
    pub late_buckets: [AtomicU64; LATE_BUCKETS_MS.len() + 1],
    pub late_max_us: AtomicU64,
    pub recovery: VideoRecoveryCounters,
    /// Sum of host-reported processing latency, and how many frames had it.
    pub host_latency_us: AtomicU64,
    pub host_latency_frames: AtomicU64,
}

#[derive(Debug, Default)]
pub struct VideoRecoveryCounters {
    pub loss_requests: AtomicU64,
    pub loss_sends: AtomicU64,
    pub loss_to_send_us: AtomicU64,
    pub max_loss_to_send_us: AtomicU64,
    pub decoder_requests: AtomicU64,
    pub silence_requests: AtomicU64,
    pub retries: AtomicU64,
    pub send_errors: AtomicU64,
    pub keyframes_received: AtomicU64,
    pub request_to_keyframe_us: AtomicU64,
    pub max_request_to_keyframe_us: AtomicU64,
    pub keyframes_already_arriving: AtomicU64,
    pub request_to_keyframe_packet_us: AtomicU64,
    pub max_request_to_keyframe_packet_us: AtomicU64,
    pub keyframe_packet_to_assembly_us: AtomicU64,
    pub max_keyframe_packet_to_assembly_us: AtomicU64,
}

impl VideoRecoveryCounters {
    pub fn requests(&self) -> u64 {
        [
            &self.loss_requests,
            &self.decoder_requests,
            &self.silence_requests,
            &self.retries,
        ]
        .into_iter()
        .map(|n| n.load(Ordering::Relaxed))
        .sum()
    }

    fn request(&self, reason: IdrReason, loss_detected_at: Option<Instant>, emit: &impl Fn(VideoFeedback)) {
        match reason {
            IdrReason::Loss => &self.loss_requests,
            IdrReason::Decoder => &self.decoder_requests,
            IdrReason::Silence => &self.silence_requests,
            IdrReason::Retry => &self.retries,
        }
        .fetch_add(1, Ordering::Relaxed);
        debug!("video keyframe requested ({reason:?})");
        emit(VideoFeedback::NeedIdr { loss_detected_at });
    }

    fn recovered(&self, requested: Instant, frame: &AssembledFrame, assembled: Instant) {
        let total_us = assembled.saturating_duration_since(requested).as_micros() as u64;
        let assembly_us = assembled.saturating_duration_since(frame.received_at).as_micros() as u64;
        self.request_to_keyframe_us.fetch_add(total_us, Ordering::Relaxed);
        self.max_request_to_keyframe_us.fetch_max(total_us, Ordering::Relaxed);
        self.keyframe_packet_to_assembly_us
            .fetch_add(assembly_us, Ordering::Relaxed);
        self.max_keyframe_packet_to_assembly_us
            .fetch_max(assembly_us, Ordering::Relaxed);
        if let Some(elapsed) = frame.received_at.checked_duration_since(requested) {
            let first_packet_us = elapsed.as_micros() as u64;
            self.request_to_keyframe_packet_us
                .fetch_add(first_packet_us, Ordering::Relaxed);
            self.max_request_to_keyframe_packet_us
                .fetch_max(first_packet_us, Ordering::Relaxed);
        } else {
            // The keyframe was already arriving when we asked for one.
            self.keyframes_already_arriving.fetch_add(1, Ordering::Relaxed);
        }
        self.keyframes_received.fetch_add(1, Ordering::Relaxed);
        debug!(
            "video recovered with keyframe {} after {:.2} ms (first packet to assembly {:.2} ms)",
            frame.frame,
            total_us as f64 / 1000.0,
            assembly_us as f64 / 1000.0,
        );
    }
}

pub struct VideoReceiver {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub counters: Arc<VideoCounters>,
}

pub fn bind_udp(remote: &SocketAddr) -> io::Result<UdpSocket> {
    let bind: SocketAddr = match remote.ip() {
        IpAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        IpAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    UdpSocket::bind(bind)
}

/// Receive buffer for the video socket. A 1080p keyframe arrives as a
/// burst of a few hundred packets; the default (~200 KB on Android) can't
/// hold one while the receive thread is busy, and the kernel drops what
/// doesn't fit, more than FEC can repair. Moonlight asks for ~3 MB too.
pub const VIDEO_RECV_BUFFER: usize = 4 << 20;

/// Asks the kernel for a `bytes` receive buffer (it may grant less, up to
/// `net.core.rmem_max`); returns what it actually reserved.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn set_recv_buffer(socket: &UdpSocket, bytes: usize) -> io::Result<usize> {
    use std::os::fd::AsRawFd;

    const SOL_SOCKET: i32 = 1;
    const SO_RCVBUF: i32 = 8;
    unsafe extern "C" {
        fn setsockopt(fd: i32, level: i32, name: i32, value: *const std::ffi::c_void, len: u32) -> i32;
        fn getsockopt(fd: i32, level: i32, name: i32, value: *mut std::ffi::c_void, len: *mut u32) -> i32;
    }
    let fd = socket.as_raw_fd();
    let wanted = i32::try_from(bytes).unwrap_or(i32::MAX);
    // SAFETY: plain setsockopt/getsockopt on a socket we own, with a
    // correctly sized i32 option value.
    unsafe {
        if setsockopt(fd, SOL_SOCKET, SO_RCVBUF, (&raw const wanted).cast(), 4) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut got: i32 = 0;
        let mut len: u32 = 4;
        if getsockopt(fd, SOL_SOCKET, SO_RCVBUF, (&raw mut got).cast(), &mut len) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(got.max(0) as usize)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn set_recv_buffer(_socket: &UdpSocket, _bytes: usize) -> io::Result<usize> {
    Ok(0)
}

/// `payload(16) | seq(be32)` or the legacy "PING".
pub fn ping_packet(payload: Option<&[u8; 16]>, seq: u32) -> Vec<u8> {
    match payload {
        Some(p) => {
            let mut v = p.to_vec();
            v.extend_from_slice(&seq.to_be_bytes());
            v
        }
        None => b"PING".to_vec(),
    }
}

impl VideoReceiver {
    pub fn start(cfg: VideoConfig, sink: Box<dyn VideoSink>, feedback: Sender<VideoFeedback>) -> io::Result<Self> {
        Self::start_with_events(cfg, sink, move |event| {
            let _ = feedback.send(event);
        })
    }

    pub(super) fn start_with_events(
        cfg: VideoConfig,
        mut sink: Box<dyn VideoSink>,
        emit: impl Fn(VideoFeedback) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let emit = Arc::new(emit);
        // The decoder's requests go through the receive loop, which keeps
        // one IDR request in flight (see `IdrRequests`).
        let decoder_wants_idr = Arc::new(AtomicBool::new(false));
        {
            let flag = decoder_wants_idr.clone();
            sink.attach_keyframe_requester(KeyframeRequester::new(move || {
                flag.store(true, Ordering::Relaxed);
            }));
        }
        let failed = emit.clone();
        sink.attach_failure_handler(Box::new(move |why| {
            failed(VideoFeedback::SinkFailed(why));
        }));
        sink.setup(cfg.format, cfg.width, cfg.height, cfg.fps)
            .map_err(|e| io::Error::other(format!("video decoder setup failed: {e}")))?;
        let socket = bind_udp(&cfg.remote)?;
        match set_recv_buffer(&socket, VIDEO_RECV_BUFFER) {
            // Linux reports twice the usable size (bookkeeping included).
            Ok(got) => info!("video receive buffer: {} KiB", got / 1024),
            Err(e) => warn!("could not enlarge the video receive buffer: {e}"),
        }
        socket.set_read_timeout(Some(REORDER_WINDOW))?;
        let mut receiver = Self {
            stop: Arc::new(AtomicBool::new(false)),
            counters: Arc::new(VideoCounters::default()),
            threads: Vec::new(),
        };

        {
            let socket = socket.try_clone()?;
            let stop = receiver.stop.clone();
            let remote = cfg.remote;
            let payload = cfg.ping_payload;
            receiver.threads.push(
                std::thread::Builder::new()
                    .name("hestia-video-ping".into())
                    .spawn(move || {
                        let mut seq = 0u32;
                        while !stop.load(Ordering::Relaxed) {
                            seq += 1;
                            let _ = socket.send_to(&ping_packet(payload.as_ref(), seq), remote);
                            std::thread::sleep(PING_INTERVAL);
                        }
                    })?,
            );
        }

        {
            let stop = receiver.stop.clone();
            let counters = receiver.counters.clone();
            receiver.threads.push(
                std::thread::Builder::new()
                    .name("hestia-video-rx".into())
                    .spawn(move || receive_loop(socket, cfg, sink, emit.as_ref(), stop, counters, decoder_wants_idr))?,
            );
        }
        Ok(receiver)
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for VideoReceiver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum VideoInactivity {
    WaitingForFirstPacket,
    Probe,
    Stalled,
}

struct VideoActivity {
    started: Instant,
    last_packet: Option<Instant>,
    probed: bool,
}

impl VideoActivity {
    fn new(started: Instant) -> Self {
        Self {
            started,
            last_packet: None,
            probed: false,
        }
    }

    fn received(&mut self, now: Instant) {
        if self.last_packet.replace(now).is_none() {
            info!("first video packet after {:?}", now.duration_since(self.started));
        }
        self.probed = false;
    }

    fn inactivity(&mut self, now: Instant) -> Option<VideoInactivity> {
        let Some(last) = self.last_packet else {
            return (now.duration_since(self.started) >= FIRST_DATA_TIMEOUT)
                .then_some(VideoInactivity::WaitingForFirstPacket);
        };
        let silent = now.duration_since(last);
        if silent >= VIDEO_STALL_TIMEOUT {
            return Some(VideoInactivity::Stalled);
        }
        if silent >= VIDEO_STALL_PROBE && !self.probed {
            self.probed = true;
            return Some(VideoInactivity::Probe);
        }
        None
    }
}

fn receive_loop(
    socket: UdpSocket,
    cfg: VideoConfig,
    mut sink: Box<dyn VideoSink>,
    emit: &impl Fn(VideoFeedback),
    stop: Arc<AtomicBool>,
    counters: Arc<VideoCounters>,
    decoder_wants_idr: Arc<AtomicBool>,
) {
    let mut assembler = FrameAssembler::new(cfg.packet_size, cfg.format.codec);
    assembler.report_fec = cfg.report_fec;
    let shard_len = assembler.shard_len();
    let mut buf = vec![0u8; shard_len + ENC_PREFIX_LEN + 64];
    let mut activity = VideoActivity::new(Instant::now());
    let mut first_frame = false;
    let mut idr = IdrRequests::default();
    let mut recovered_request = None;
    let mut recovered_loss = None;

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if decoder_wants_idr.swap(false, Ordering::Relaxed) && cfg.format.codec != VideoCodec::PyroWave {
            assembler.wait_for_keyframe();
            if let Some(reason) = idr.want(now, IdrReason::Decoder) {
                counters.recovery.request(reason, idr.loss_detected_at, emit);
            }
        }
        let packet = match socket.recv_from(&mut buf) {
            Ok((len, from)) if from.ip() == cfg.remote.ip() => {
                counters.packets.fetch_add(1, Ordering::Relaxed);
                counters.bytes.fetch_add(len as u64, Ordering::Relaxed);
                Some(&buf[..len])
            }
            Ok(_) => None,
            // ConnectionReset can be an ICMP port-unreachable. Interrupted
            // can be a signal while Android is sending the app to the background.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::Interrupted
                ) =>
            {
                None
            }
            Err(e) => {
                if !stop.load(Ordering::Relaxed) {
                    warn!("video socket error: {e}");
                    emit(VideoFeedback::SocketFailed(e.to_string()));
                }
                break;
            }
        };
        let now = Instant::now();
        let plain;
        let packet = match (packet, &cfg.decrypt_key) {
            (Some(packet), Some(key)) if packet.len() >= ENC_PREFIX_LEN + RTP_HEADER_LEN => {
                let iv: [u8; 12] = packet[..12].try_into().unwrap_or_default();
                plain = gcm_decrypt(key, &iv, &packet[ENC_PREFIX_LEN..], &packet[16..32]);
                plain.as_deref()
            }
            (packet, None) => packet,
            _ => None,
        };
        let outputs = match packet.and_then(|packet| assembler.packet_info(packet).map(|info| (packet, info))) {
            Some((packet, info)) => {
                activity.received(now);
                assembler.add_shard_at(packet, info, now)
            }
            None => assembler.expire(now),
        };
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match activity.inactivity(now) {
            Some(VideoInactivity::WaitingForFirstPacket) => {
                warn!("no video traffic from the host");
                emit(VideoFeedback::NoVideo);
                break;
            }
            Some(VideoInactivity::Probe) => {
                debug!("video paused; probing with a keyframe request");
                if let Some(reason) = idr.want(now, IdrReason::Silence) {
                    counters.recovery.request(reason, idr.loss_detected_at, emit);
                }
            }
            Some(VideoInactivity::Stalled) => {
                warn!("video stopped arriving from the host");
                emit(VideoFeedback::VideoStalled);
                break;
            }
            None => {}
        }
        for output in outputs {
            match output {
                AssemblerOutput::FecStatus(status) => emit(VideoFeedback::FecStatus(status)),
                AssemblerOutput::NeedIdr => {
                    if let Some(reason) = idr.want(Instant::now(), IdrReason::Loss) {
                        counters.recovery.request(reason, idr.loss_detected_at, emit);
                    }
                }
                AssemblerOutput::Frame(frame) => {
                    if !first_frame {
                        first_frame = true;
                        emit(VideoFeedback::FirstFrame);
                    }
                    counters.frames.fetch_add(1, Ordering::Relaxed);
                    if frame.keyframe {
                        let (request, loss) = idr.keyframe_arrived();
                        recovered_loss = loss.or(recovered_loss);
                        if let Some(request) = request {
                            counters.recovery.recovered(request, &frame, Instant::now());
                            recovered_request = Some(request);
                        }
                    }
                    if frame.host_latency_100us > 0 {
                        counters
                            .host_latency_us
                            .fetch_add(u64::from(frame.host_latency_100us) * 100, Ordering::Relaxed);
                        counters.host_latency_frames.fetch_add(1, Ordering::Relaxed);
                    }
                    let encoded = EncodedFrame {
                        frame_number: frame.frame,
                        keyframe: frame.keyframe,
                        data: frame.data,
                        received_at: frame.received_at,
                        idr_requested_at: recovered_request,
                        loss_detected_at: recovered_loss,
                        host_latency: Some(Duration::from_micros(frame.host_latency_100us as u64 * 100)),
                    };
                    if sink.submit(encoded) == SubmitResult::NeedKeyframe && cfg.format.codec != VideoCodec::PyroWave {
                        debug!("decoder asked for a keyframe");
                        assembler.wait_for_keyframe();
                        if let Some(reason) = idr.want(Instant::now(), IdrReason::Decoder) {
                            counters.recovery.request(reason, idr.loss_detected_at, emit);
                        }
                        break;
                    }
                }
            }
        }
        if let Some(reason) = idr.retry_due(Instant::now()) {
            counters.recovery.request(reason, idr.loss_detected_at, emit);
        }
        counters
            .recovered
            .store(assembler.stats.recovered_packets, Ordering::Relaxed);
        counters
            .lost_frames
            .store(assembler.stats.lost_frames, Ordering::Relaxed);
        counters
            .dropped_waiting_idr
            .store(assembler.stats.dropped_waiting_idr, Ordering::Relaxed);
        counters
            .late_packets
            .store(assembler.stats.late_packets, Ordering::Relaxed);
        counters
            .late_frames
            .store(assembler.stats.late_frames, Ordering::Relaxed);
        for (counter, value) in counters.late_buckets.iter().zip(assembler.stats.late_buckets) {
            counter.store(value, Ordering::Relaxed);
        }
        counters
            .late_max_us
            .store(assembler.stats.late_max_us, Ordering::Relaxed);
    }
    stop.store(true, Ordering::Relaxed);
    sink.cleanup();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdrReason {
    Loss,
    Decoder,
    Silence,
    Retry,
}

/// Keyframe requests: one in flight at a time. Losses while waiting don't
/// ask again (each IDR is large; asking per lost frame floods a lossy link
/// with keyframes and makes the loss worse); if none arrives within
/// [`IdrRequests::RETRY`], ask once more. When keyframes don't help (the
/// decoder fails again right after each one), requests space out, up to
/// [`IdrRequests::MAX_GAP`] apart, until the stream runs cleanly again.
#[derive(Debug, Default)]
struct IdrRequests {
    /// First assembly loss in this episode, including deferred requests.
    loss_detected_at: Option<Instant>,
    pending_since: Option<Instant>,
    first_request: Option<Instant>,
    last_sent: Option<Instant>,
    /// Least time between requests; grows while keyframes don't help.
    gap: Duration,
    /// The gap already grew for the last request.
    grew: bool,
    /// A request held back by the gap, sent once it has passed.
    deferred: Option<IdrReason>,
}

impl IdrRequests {
    const RETRY: Duration = Duration::from_millis(1000);
    /// Asking again this soon after a request means the keyframe didn't help.
    const STORM: Duration = Duration::from_millis(1500);
    const MIN_GAP: Duration = Duration::from_millis(250);
    const MAX_GAP: Duration = Duration::from_secs(4);

    /// A keyframe is needed: return the reason if a request should go out now.
    fn want(&mut self, now: Instant, reason: IdrReason) -> Option<IdrReason> {
        if reason == IdrReason::Loss {
            self.loss_detected_at.get_or_insert(now);
        }
        if self.pending_since.is_some() {
            return self.retry_due(now);
        }
        match self.last_sent.map(|at| now.duration_since(at)) {
            Some(since) if since < Self::STORM => {
                // A fresh transport loss after a usable IDR is a new episode.
                // Keep a spacing floor, preserving exponential backoff for decoder failures.
                if reason == IdrReason::Loss {
                    self.gap = Self::MIN_GAP;
                    self.deferred = Some(reason);
                    self.grew = true;
                } else if !self.grew {
                    self.gap = (self.gap * 2).clamp(Self::MIN_GAP, Self::MAX_GAP);
                    self.grew = true;
                }
                if since < self.gap {
                    self.deferred.get_or_insert(reason);
                    return None;
                }
            }
            _ => self.gap = Duration::ZERO,
        }
        Some(self.send(now, reason))
    }

    fn send(&mut self, now: Instant, reason: IdrReason) -> IdrReason {
        self.pending_since = Some(now);
        self.first_request.get_or_insert(now);
        self.last_sent = Some(now);
        self.grew = false;
        self.deferred = None;
        reason
    }

    /// A request is due: the pending one went unanswered for too long, or
    /// a held-back one's gap has passed.
    fn retry_due(&mut self, now: Instant) -> Option<IdrReason> {
        match (self.pending_since, self.last_sent) {
            (Some(at), _) if now.duration_since(at) >= Self::RETRY => Some(self.send(now, IdrReason::Retry)),
            (None, Some(last)) if now.duration_since(last) >= self.gap => {
                self.deferred.map(|reason| self.send(now, reason))
            }
            _ => None,
        }
    }

    fn keyframe_arrived(&mut self) -> (Option<Instant>, Option<Instant>) {
        self.pending_since = None;
        self.deferred = None;
        (self.first_request.take(), self.loss_detected_at.take())
    }
}

/// Test helper: packetizes a frame the way Sunshine/Hermes do.
#[cfg(test)]
pub(crate) fn packetize(
    frame: u32,
    data: &[u8],
    packet_size: usize,
    fec_percent: usize,
    seq_start: u16,
    idr: bool,
) -> Vec<Vec<u8>> {
    let blocksize = packet_size + MAX_RTP_HEADER_LEN;
    let capacity = blocksize - SHARD_HEADER_LEN;
    let mut header = vec![0x01, 0, 0, if idr { 2 } else { 1 }, 0, 0, 0, 0];
    let total = data.len() + header.len();
    let mut last = total % (packet_size - NV_HEADER_LEN);
    if last == 0 {
        last = packet_size - NV_HEADER_LEN;
    }
    header[4..6].copy_from_slice(&(last as u16).to_le_bytes());
    let mut payload = header;
    payload.extend_from_slice(data);
    let chunks: Vec<&[u8]> = payload.chunks(capacity).collect();
    let ds = chunks.len();
    let ps = (ds * fec_percent).div_ceil(100).max(1);
    let pct = (ps * 100).div_ceil(ds);
    let ps = (ds * pct).div_ceil(100);
    let mut shards: Vec<Vec<u8>> = chunks
        .iter()
        .enumerate()
        .map(|(x, c)| {
            let mut s = vec![0u8; blocksize];
            let nv = MAX_RTP_HEADER_LEN;
            s[nv + 4..nv + 8].copy_from_slice(&frame.to_le_bytes());
            let mut flags = FLAG_CONTAINS_PIC_DATA;
            if x == 0 {
                flags |= FLAG_SOF;
            }
            if x == ds - 1 {
                flags |= FLAG_EOF;
            }
            s[nv + 8] = flags;
            s[nv + 10] = 0x10;
            s[SHARD_HEADER_LEN..SHARD_HEADER_LEN + c.len()].copy_from_slice(c);
            s
        })
        .collect();
    shards.extend((0..ps).map(|_| vec![0u8; blocksize]));
    fec::encode(&mut shards, ds, ps).unwrap();
    for (x, s) in shards.iter_mut().enumerate() {
        let nv = MAX_RTP_HEADER_LEN;
        let fec_info = (x << 12 | ds << 22 | pct << 4) as u32;
        s[nv + 12..nv + 16].copy_from_slice(&fec_info.to_le_bytes());
        s[0] = 0x80 | FLAG_EXTENSION;
        s[2..4].copy_from_slice(&seq_start.wrapping_add(x as u16).to_be_bytes());
        s[nv + 4..nv + 8].copy_from_slice(&frame.to_le_bytes());
    }
    shards
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKET_SIZE: usize = 1392;

    fn h264_frame(len: usize, idr: bool) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, if idr { 0x65 } else { 0x41 }];
        v.extend((0..len).map(|i| (i * 7 + 3) as u8 | 1));
        v
    }

    #[test]
    fn video_inactivity_probes_then_times_out_and_rearms_on_packets() {
        let start = Instant::now();
        let mut watchdog = VideoActivity::new(start);
        assert_eq!(watchdog.inactivity(start + VIDEO_STALL_TIMEOUT), None);
        assert_eq!(
            watchdog.inactivity(start + FIRST_DATA_TIMEOUT),
            Some(VideoInactivity::WaitingForFirstPacket)
        );

        let first = start + FIRST_DATA_TIMEOUT;
        watchdog.received(first);
        assert_eq!(watchdog.inactivity(first + VIDEO_STALL_PROBE / 2), None);
        assert_eq!(
            watchdog.inactivity(first + VIDEO_STALL_PROBE),
            Some(VideoInactivity::Probe)
        );
        assert_eq!(watchdog.inactivity(first + VIDEO_STALL_PROBE), None);

        let recovered = first + VIDEO_STALL_PROBE + Duration::from_millis(50);
        watchdog.received(recovered);
        assert_eq!(
            watchdog.inactivity(recovered + VIDEO_STALL_PROBE),
            Some(VideoInactivity::Probe)
        );
        assert_eq!(
            watchdog.inactivity(recovered + VIDEO_STALL_TIMEOUT),
            Some(VideoInactivity::Stalled)
        );
    }

    #[test]
    fn reassembles_with_losses() {
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::H264);
        let data = h264_frame(10_000, true);
        let mut packets = packetize(1, &data, PACKET_SIZE, 20, 100, true);
        // Drop two data packets; 20% FEC on 8 shards gives 2 parity.
        packets.remove(3);
        packets.remove(0);
        packets.reverse(); // and deliver out of order
        let mut frames = Vec::new();
        for p in &packets {
            for o in asm.add(p) {
                if let AssemblerOutput::Frame(f) = o {
                    frames.push(f);
                }
            }
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, data);
        assert!(frames[0].keyframe);
        assert_eq!(asm.stats.recovered_packets, 2);
    }

    #[test]
    fn waits_for_idr_after_loss() {
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::H264);
        let mut seq = 0u16;
        let start = Instant::now();
        let mut feed = |asm: &mut FrameAssembler, frame: u32, idr: bool, drop: usize| {
            let packets = packetize(frame, &h264_frame(4000, idr), PACKET_SIZE, 20, seq, idr);
            seq = seq.wrapping_add(packets.len() as u16);
            let now = start + Duration::from_millis(u64::from(frame) * 16);
            let mut out = Vec::new();
            for p in packets.iter().skip(drop) {
                out.extend(asm.add_at(p, now));
            }
            out
        };
        assert!(matches!(feed(&mut asm, 1, true, 0)[..], [AssemblerOutput::Frame(_)]));
        assert!(feed(&mut asm, 2, false, 3).is_empty());
        // The newer frame is complete, but the missing frame gets a short grace period.
        assert!(feed(&mut asm, 3, false, 0).is_empty());
        let out = asm.expire(start + Duration::from_millis(48) + REORDER_WINDOW);
        assert_eq!(out, vec![AssemblerOutput::NeedIdr]);
        assert_eq!(asm.stats.lost_frames, 1);
        assert_eq!(asm.stats.dropped_waiting_idr, 1);
        let out = feed(&mut asm, 4, true, 0);
        assert!(matches!(&out[..], [AssemblerOutput::Frame(f)] if f.keyframe && f.frame == 4));
    }

    #[test]
    fn counts_packets_that_arrive_after_their_frame_was_lost() {
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::H264);
        let start = Instant::now();
        let ms = |n: u64| start + Duration::from_millis(n);
        let mut seq = 0u16;
        let mut frame = |number: u32, idr: bool| {
            let packets = packetize(number, &h264_frame(4000, idr), PACKET_SIZE, 20, seq, idr);
            seq = seq.wrapping_add(packets.len() as u16);
            packets
        };
        let (f1, f2, f3) = (frame(1, true), frame(2, false), frame(3, false));
        for p in &f1 {
            asm.add_at(p, ms(0));
        }
        // Frame 2 starts, frame 3 arrives whole; frame 2 is given up on.
        asm.add_at(&f2[0], ms(16));
        for p in &f3 {
            asm.add_at(p, ms(32));
        }
        asm.expire(ms(32) + REORDER_WINDOW);
        assert_eq!(asm.stats.lost_frames, 1);
        // The rest of frame 2 arrives 22 ms after it was given up on.
        for p in &f2[1..] {
            asm.add_at(p, ms(60));
        }
        assert_eq!(asm.stats.late_frames, 1);
        assert_eq!(asm.stats.late_packets, (f2.len() - 1) as u64);
        assert_eq!(asm.stats.late_buckets[1], (f2.len() - 1) as u64, "10-25 ms");
        assert_eq!(asm.stats.late_max_us, 22_000);
        // A duplicate of a delivered frame isn't a late arrival.
        asm.add_at(&f1[0], ms(70));
        assert_eq!(asm.stats.late_packets, (f2.len() - 1) as u64);
    }

    #[test]
    fn counts_frames_skipped_before_any_packet_arrived() {
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::H264);
        let start = Instant::now();
        let mut seq = 0u16;
        let mut packets = Vec::new();
        for number in 1..=6u32 {
            let p = packetize(number, &h264_frame(800, number == 1), PACKET_SIZE, 20, seq, number == 1);
            seq = seq.wrapping_add(p.len() as u16);
            packets.push(p);
        }
        for p in &packets[0] {
            asm.add_at(p, start);
        }
        // Frames 3-6 arrive before anything of frame 2: more than
        // MAX_FRAME_LOOKAHEAD ahead, so frame 2 is lost at once.
        for frame in &packets[2..] {
            for p in frame {
                asm.add_at(p, start + Duration::from_millis(1));
            }
        }
        assert_eq!(asm.stats.lost_frames, 1);
        for p in &packets[1] {
            asm.add_at(p, start + Duration::from_millis(8));
        }
        assert_eq!(asm.stats.late_frames, 1);
        assert_eq!(asm.stats.late_buckets[0], packets[1].len() as u64, "<10 ms");
    }

    /// A replay of a jittery link with **no packet loss at all**: 120 frames
    /// at 60 fps, every packet delayed by a deterministic 0-30 ms, delivered
    /// in arrival order. With the current 6 ms window, frames whose packets
    /// are all still coming are declared lost, and every one of them later
    /// shows up in the late-arrival counts.
    #[test]
    fn jittery_link_without_loss_replay_under_the_current_window() {
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::H264);
        let start = Instant::now();
        let mut rng = 0x2545_f491_u32;
        let mut jitter_us = || {
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            u64::from(rng >> 8) % 30_000
        };
        let mut seq = 0u16;
        let mut arrivals = Vec::new();
        for number in 1..=120u32 {
            let idr = number == 1;
            let packets = packetize(number, &h264_frame(6000, idr), PACKET_SIZE, 20, seq, idr);
            seq = seq.wrapping_add(packets.len() as u16);
            let sent_us = u64::from(number) * 16_667;
            for p in packets {
                arrivals.push((sent_us + jitter_us(), p));
            }
        }
        arrivals.sort_by_key(|(at, _)| *at);
        let mut delivered = 0;
        for (at, p) in &arrivals {
            for o in asm.add_at(p, start + Duration::from_micros(*at)) {
                delivered += usize::from(matches!(o, AssemblerOutput::Frame(_)));
            }
        }
        asm.expire(start + Duration::from_secs(5));
        let s = asm.stats;
        assert!(s.lost_frames > 0, "the 6 ms window loses frames on this replay: {s:?}");
        assert_eq!(
            s.late_frames, s.lost_frames,
            "every lost frame's packets still arrived: {s:?}"
        );
        assert!(s.late_packets >= s.late_frames);
        assert_eq!(delivered as u64, s.frames);
        assert_eq!(s.frames + s.dropped_waiting_idr + s.lost_frames, 120);
    }

    #[test]
    fn accepts_late_frame_before_its_deadline_across_sequence_wrap() {
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::H264);
        let start = Instant::now();
        let older = packetize(u32::MAX, &h264_frame(4000, true), PACKET_SIZE, 20, 100, true);
        let newer = packetize(0, &h264_frame(500, false), PACKET_SIZE, 20, 200, false);
        assert!(asm.add_at(&older[0], start).is_empty());
        for p in &newer {
            assert!(asm.add_at(p, start + Duration::from_millis(1)).is_empty());
        }
        let mut out = Vec::new();
        for p in &older[1..] {
            out.extend(asm.add_at(p, start + Duration::from_millis(2)));
        }
        assert!(matches!(
            &out[..],
            [AssemblerOutput::Frame(a), AssemblerOutput::Frame(b)]
                if a.frame == u32::MAX && a.received_at == start && b.frame == 0
        ));
        assert_eq!(asm.stats.lost_frames, 0);
    }

    #[test]
    fn pyrowave_preserves_binary_padding_and_resumes_after_loss() {
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::PyroWave);
        let start = Instant::now();
        let data: Vec<u8> = [0, 0, 1, 0, 0, 0, 0].into_iter().cycle().take(4200).collect();
        // Drop a shard and reconstruct through FEC. The picture is independent
        // even when the generic frame header does not mark an IDR.
        let packets = packetize(1, &data, PACKET_SIZE, 30, 10, false);
        let out: Vec<_> = packets.iter().skip(1).flat_map(|p| asm.add_at(p, start)).collect();
        assert!(matches!(&out[..], [AssemblerOutput::Frame(f)] if f.keyframe && f.data == data));
        let next = packetize(3, &[4, 0, 0], PACKET_SIZE, 20, 40, false);
        assert!(asm.add_at(&next[0], start).is_empty());
        let out = asm.expire(start + REORDER_WINDOW);
        assert!(matches!(&out[..], [AssemblerOutput::Frame(f)] if f.frame == 3 && f.data == [4,0,0]));
        assert_eq!((asm.stats.lost_frames, asm.stats.dropped_waiting_idr), (1, 0));
    }

    #[test]
    fn pyrowave_requires_an_exact_nonempty_payload() {
        for last in [0u16, 7, 8, 11] {
            let mut payload = vec![1, 0, 0, 2, 0, 0, 0, 0, 0, 0];
            payload[4..6].copy_from_slice(&last.to_le_bytes());
            assert!(depacketize(1, &[payload], VideoCodec::PyroWave).is_none());
        }
    }

    #[test]
    fn exact_length_without_padding() {
        let data = h264_frame(1300 - 5, false); // ends exactly at a packet boundary region
        let packets = packetize(7, &data, PACKET_SIZE, 20, 0, false);
        let payloads: Vec<Vec<u8>> = packets
            .iter()
            .take(packets.len() - 1)
            .map(|p| p[SHARD_HEADER_LEN..].to_vec())
            .collect();
        let frame = depacketize(7, &payloads, VideoCodec::H264).unwrap();
        assert_eq!(frame.data, data);
        assert!(!frame.keyframe);
    }

    #[test]
    fn one_keyframe_request_in_flight() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut idr = IdrRequests::default();
        assert_eq!(idr.want(t0, IdrReason::Loss), Some(IdrReason::Loss));
        assert_eq!(idr.want(ms(250), IdrReason::Decoder), None);
        assert_eq!(idr.retry_due(ms(900)), None);
        assert_eq!(idr.retry_due(ms(1000)), Some(IdrReason::Retry));
        assert_eq!(idr.retry_due(ms(1500)), None);
        assert_eq!(
            idr.keyframe_arrived(),
            (Some(t0), Some(t0)),
            "retries keep the first request time"
        );
        assert_eq!(idr.keyframe_arrived(), (None, None));
        assert_eq!(idr.retry_due(ms(5000)), None);
        assert_eq!(idr.want(ms(5001), IdrReason::Loss), Some(IdrReason::Loss));
    }

    #[test]
    fn burst_loss_replay_separates_keyframe_wait_from_assembly() {
        let start = Instant::now();
        let ms = |n| start + Duration::from_millis(n);
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::H264);
        let partial = packetize(9, &h264_frame(4000, false), PACKET_SIZE, 20, 0, false);
        let next = packetize(10, &h264_frame(500, false), PACKET_SIZE, 20, 8, false);
        assert!(asm.add_at(&partial[0], start).is_empty());
        assert!(asm.add_at(&next[0], start).is_empty());
        let requested = start + REORDER_WINDOW;
        assert!(matches!(&asm.expire(requested)[..], [AssemblerOutput::NeedIdr]));
        assert_eq!(asm.stats.dropped_waiting_idr, 1);

        let mut idr = IdrRequests::default();
        assert_eq!(idr.want(requested, IdrReason::Loss), Some(IdrReason::Loss));
        let recovery = VideoRecoveryCounters::default();
        let packets = packetize(11, &h264_frame(4000, true), PACKET_SIZE, 20, 16, true);
        assert!(asm.add_at(&packets[0], ms(400)).is_empty());
        let assembled = ms(430);
        let output: Vec<_> = packets[1..].iter().flat_map(|p| asm.add_at(p, assembled)).collect();
        let [AssemblerOutput::Frame(frame)] = &output[..] else {
            panic!("expected one assembled keyframe");
        };
        assert!(frame.keyframe);
        let (request, _) = idr.keyframe_arrived();
        recovery.recovered(request.unwrap(), frame, assembled);

        // A second request went out after its keyframe had started arriving.
        recovery.recovered(ms(420), frame, assembled);
        let count = |a: &AtomicU64| a.load(Ordering::Relaxed);
        assert_eq!(count(&recovery.keyframes_received), 2);
        assert_eq!(count(&recovery.keyframes_already_arriving), 1);
        assert_eq!(count(&recovery.request_to_keyframe_packet_us), 394_000);
        assert_eq!(count(&recovery.max_request_to_keyframe_packet_us), 394_000);
        assert_eq!(count(&recovery.keyframe_packet_to_assembly_us), 60_000);
        assert_eq!(count(&recovery.max_keyframe_packet_to_assembly_us), 30_000);
        assert_eq!(count(&recovery.max_request_to_keyframe_us), 424_000);
    }

    #[test]
    fn recovered_transport_losses_have_bounded_spacing() {
        let start = Instant::now();
        let ms = |n| start + Duration::from_millis(n);
        let mut idr = IdrRequests::default();
        assert_eq!(idr.want(start, IdrReason::Loss), Some(IdrReason::Loss));
        for previous in [0, 250, 500, 750, 1000] {
            let (request, loss) = idr.keyframe_arrived();
            assert_eq!(request, Some(ms(previous)));
            assert_eq!(loss, Some(ms(previous.saturating_sub(100))));
            assert_eq!(idr.want(ms(previous + 150), IdrReason::Loss), None);
            assert_eq!(idr.retry_due(ms(previous + 249)), None);
            assert_eq!(idr.retry_due(ms(previous + 250)), Some(IdrReason::Loss));
        }
        idr.keyframe_arrived();
        assert_eq!(idr.want(ms(1300), IdrReason::Loss), None);
        assert_eq!(idr.keyframe_arrived(), (None, Some(ms(1300))));
        assert_eq!(
            idr.retry_due(ms(1550)),
            None,
            "unsolicited keyframe cancelled deferred feedback"
        );
    }

    #[test]
    fn reports_partial_block_once_at_expiry_with_wrapping_sequences() {
        let mut asm = FrameAssembler::new(PACKET_SIZE as u32, VideoCodec::H264);
        asm.report_fec = true;
        let start = Instant::now();
        let partial = packetize(9, &h264_frame(4000, false), PACKET_SIZE, 20, 65535, false);
        for index in [0, 2, 2] {
            assert!(asm.add_at(&partial[index], start).is_empty());
        }
        let next = packetize(10, &h264_frame(500, true), PACKET_SIZE, 20, 4, true);
        assert!(asm.add_at(&next[0], start).is_empty());
        let out = asm.expire(start + REORDER_WINDOW);
        assert!(
            matches!(&out[..], [AssemblerOutput::FecStatus(FrameFecStatus(bytes)), AssemblerOutput::NeedIdr, AssemblerOutput::Frame(_)]
            if bytes == &[0,0,0,9, 0,1, 0,0, 0,1, 0,3, 0,2, 0,2, 0,0, 34,0,1])
        );
        assert!(asm.expire(start + REORDER_WINDOW * 2).is_empty());
    }

    #[test]
    fn keyframes_that_dont_help_space_requests_out() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut idr = IdrRequests::default();
        // The decoder fails 50 ms after every keyframe (40 ms after asking).
        let mut sent = vec![0u64];
        assert_eq!(idr.want(ms(0), IdrReason::Decoder), Some(IdrReason::Decoder));
        let mut t = 0;
        while t < 20_000 {
            t += 10;
            let last = *sent.last().unwrap();
            if t == last + 40 {
                idr.keyframe_arrived();
            }
            if t == last + 90 {
                assert!(
                    idr.want(ms(t), IdrReason::Decoder).is_none() || t - last >= 1500,
                    "held back at {t}"
                );
            }
            if let Some(reason) = idr.retry_due(ms(t)) {
                assert_eq!(reason, IdrReason::Decoder, "deferred requests keep their cause");
                sent.push(t);
            }
        }
        let gaps: Vec<u64> = sent.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps.windows(2).all(|w| w[1] >= w[0]), "{gaps:?}");
        assert!(*gaps.last().unwrap() >= 4000, "settles at the longest gap: {gaps:?}");
        assert!(sent.len() < 12, "{} requests in 20 s", sent.len());

        // Running cleanly for a while: the next loss asks right away.
        let later = 30_000;
        assert_eq!(idr.want(ms(later), IdrReason::Loss), Some(IdrReason::Loss));
    }

    #[test]
    fn waiting_for_a_keyframe_keeps_statistics() {
        let mut a = FrameAssembler::new(1024, VideoCodec::H264);
        a.stats.lost_frames = 7;
        a.wait_for_keyframe();
        assert_eq!(a.stats.lost_frames, 7);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn enlarges_the_receive_buffer() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let before = set_recv_buffer(&socket, 4096).unwrap();
        let after = set_recv_buffer(&socket, 1 << 20).unwrap();
        assert!(after > before, "{before} -> {after}");
    }

    #[test]
    fn keyframe_detection() {
        assert!(is_keyframe_bitstream(&[0, 0, 0, 1, 0x67, 1, 2], VideoCodec::H264));
        assert!(!is_keyframe_bitstream(&[0, 0, 1, 0x41, 1], VideoCodec::H264));
        assert!(is_keyframe_bitstream(&[0, 0, 0, 1, 0x40, 1], VideoCodec::Hevc)); // VPS
        assert_eq!(ping_packet(Some(b"0123456789abcdef"), 2).len(), 20);
        assert_eq!(ping_packet(None, 1), b"PING");
    }
}
