//! The realtime data plane: audio RTP out, plus the three things a strict
//! Linkplay receiver needs to keep playing instead of muting after ~1 second:
//!   1. a NTP-style **timing** responder,
//!   2. periodic **SYNC** packets (RTP timestamp -> clock anchor), and
//!   3. a **retransmit** responder backed by a bounded packet history.

use std::io::{self, Read};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::clock::Clock;
use crate::codec::{encode_alac, encode_pcm_be, Codec};

pub const SAMPLE_RATE: u32 = 44100;
pub const FRAMES_PER_PACKET: u32 = 352;

/// Bounded, fixed-memory ring of recently sent audio packets, indexed by
/// sequence number. Used to answer retransmit requests without unbounded
/// buffering. `cap` must be a power of two.
pub struct History {
    slots: Vec<Option<(u16, Vec<u8>)>>,
    mask: usize,
}

impl History {
    pub fn new(cap_pow2: usize) -> Self {
        assert!(cap_pow2.is_power_of_two());
        History {
            slots: vec![None; cap_pow2],
            mask: cap_pow2 - 1,
        }
    }

    pub fn put(&mut self, seq: u16, pkt: Vec<u8>) {
        let i = (seq as usize) & self.mask;
        self.slots[i] = Some((seq, pkt));
    }

    pub fn get(&self, seq: u16) -> Option<&[u8]> {
        let i = (seq as usize) & self.mask;
        match &self.slots[i] {
            Some((s, p)) if *s == seq => Some(p.as_slice()),
            _ => None,
        }
    }
}

/// Shared state read by the sync/control threads while audio is sent.
pub struct Shared {
    pub clock: Clock,
    /// RTP timestamp the sender has produced up to (advertised in SYNC).
    pub head: AtomicU32,
    pub latency: u32,
    pub history: Mutex<History>,
}

/// Answer the device's NTP timing requests (type 0x52) with a timing reply
/// (type 0x53). The reply echoes the request's transmit time and stamps our
/// receive + transmit times so the receiver can discipline its clock.
pub fn spawn_timing(sock: UdpSocket, device_timing: SocketAddr, clock: Clock) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 64];
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, _)) if n >= 32 => {
                    let mut r = [0u8; 32];
                    r[0] = 0x80;
                    r[1] = 0xD3; // timing response, marker bit set
                    r[3] = 0x07;
                    // reftime = request's transmit time (its bytes 24..32)
                    r[8..16].copy_from_slice(&buf[24..32]);
                    let (rs, rf) = clock.ntp();
                    r[16..20].copy_from_slice(&rs.to_be_bytes());
                    r[20..24].copy_from_slice(&rf.to_be_bytes());
                    let (ss, sf) = clock.ntp();
                    r[24..28].copy_from_slice(&ss.to_be_bytes());
                    r[28..32].copy_from_slice(&sf.to_be_bytes());
                    let _ = sock.send_to(&r, device_timing);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    })
}

/// Emit a SYNC packet (type 0x54) once per second mapping the current RTP
/// head timestamp to NTP time. The first sync sets the extension bit (0x90).
pub fn spawn_sync(
    sock: UdpSocket,
    device_control: SocketAddr,
    shared: Arc<Shared>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut first = true;
        loop {
            let ts = shared.head.load(Ordering::Relaxed);
            let now_without_latency = ts.wrapping_sub(shared.latency);
            let (s, f) = shared.clock.ntp();

            let mut p = [0u8; 20];
            p[0] = if first { 0x90 } else { 0x80 };
            p[1] = 0xD4;
            p[3] = 0x07;
            p[4..8].copy_from_slice(&now_without_latency.to_be_bytes());
            p[8..12].copy_from_slice(&s.to_be_bytes());
            p[12..16].copy_from_slice(&f.to_be_bytes());
            p[16..20].copy_from_slice(&ts.to_be_bytes());
            let _ = sock.send_to(&p, device_control);

            first = false;
            thread::sleep(Duration::from_secs(1));
        }
    })
}

/// Answer retransmit requests (type 0x55) by re-sending the requested audio
/// packets, each wrapped in a 0x56 ("response") container.
pub fn spawn_retransmit(
    sock: UdpSocket,
    device_control: SocketAddr,
    shared: Arc<Shared>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; 64];
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, _)) if n >= 8 => {
                    if buf[1] & 0x7f != 0x55 {
                        continue;
                    }
                    let first = u16::from_be_bytes([buf[4], buf[5]]);
                    let count = u16::from_be_bytes([buf[6], buf[7]]);
                    let hist = shared.history.lock().unwrap();
                    for i in 0..count {
                        let seq = first.wrapping_add(i);
                        if let Some(orig) = hist.get(seq) {
                            let mut out = Vec::with_capacity(4 + orig.len());
                            out.extend_from_slice(&[0x80, 0xD6]);
                            out.extend_from_slice(&seq.to_be_bytes());
                            out.extend_from_slice(orig);
                            let _ = sock.send_to(&out, device_control);
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    })
}

/// Read PCM from `input`, packetize, and send RTP audio paced to a wall clock.
///
/// Pacing keeps us a fixed `latency` worth of audio *ahead* of real-time
/// playout: the first ~`latency` seconds are sent as a burst to prime the
/// receiver's buffer, after which we settle into one packet every 352/44100 s.
/// Because the schedule is anchored to a monotonic clock, drift cannot
/// accumulate, and a blocked socket back-pressures the stdin reader instead of
/// growing memory.
pub fn run_audio(
    audio_sock: UdpSocket,
    device_audio: SocketAddr,
    mut input: impl Read,
    codec: Codec,
    seq_start: u16,
    rtp_start: u32,
    ssrc: u32,
    shared: Arc<Shared>,
) -> io::Result<()> {
    let in_bytes = (FRAMES_PER_PACKET as usize) * 4; // 2ch * 2 bytes
    let mut pcm = vec![0u8; in_bytes];
    let mut payload: Vec<u8> = Vec::with_capacity(1500);

    let frame_ns: u128 = (FRAMES_PER_PACKET as u128) * 1_000_000_000 / (SAMPLE_RATE as u128);
    let lead_ns: u128 = (shared.latency as u128) * 1_000_000_000 / (SAMPLE_RATE as u128);
    let start = Instant::now();

    let mut n: u64 = 0;
    loop {
        // Fill one frame; pad the final short read with silence.
        let mut filled = 0;
        let mut eof = false;
        while filled < in_bytes {
            match input.read(&mut pcm[filled..]) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(k) => filled += k,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if filled == 0 {
            break;
        }
        if filled < in_bytes {
            for b in &mut pcm[filled..] {
                *b = 0;
            }
        }

        match codec {
            Codec::Alac => encode_alac(&mut payload, &pcm),
            Codec::Pcm => encode_pcm_be(&mut payload, &pcm),
        }

        let seq = seq_start.wrapping_add(n as u16);
        let ts = rtp_start.wrapping_add((n as u32).wrapping_mul(FRAMES_PER_PACKET));

        let mut pkt = Vec::with_capacity(12 + payload.len());
        pkt.push(0x80);
        pkt.push(if n == 0 { 0xE0 } else { 0x60 }); // PT 96; marker on first
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&ts.to_be_bytes());
        pkt.extend_from_slice(&ssrc.to_be_bytes());
        pkt.extend_from_slice(&payload);

        // Pace: don't send packet n before start + n*frame - lead.
        let send_offset_ns = (n as u128 * frame_ns).saturating_sub(lead_ns);
        let send_at = start + Duration::from_nanos(send_offset_ns as u64);
        let now = Instant::now();
        if send_at > now {
            thread::sleep(send_at - now);
        }

        audio_sock.send_to(&pkt, device_audio)?;
        shared
            .head
            .store(ts.wrapping_add(FRAMES_PER_PACKET), Ordering::Relaxed);
        shared.history.lock().unwrap().put(seq, pkt);

        n += 1;
        if eof {
            break;
        }
    }
    Ok(())
}
