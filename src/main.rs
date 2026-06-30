//! raop_send — pipe raw PCM (s16le, 44100 Hz, stereo) from stdin to an
//! AirPlay-1 (RAOP) speaker.
//!
//!     ffmpeg -i song.flac -f s16le -ar 44100 -ac 2 - | raop_send --host 10.0.1.155
//!
//! One job, done carefully: unencrypted realtime RAOP with correct timing,
//! SYNC and retransmit so strict (Linkplay/WiiMu) receivers keep playing.

mod clock;
mod codec;
mod rtsp;
mod stream;

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::process::exit;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use clock::Clock;
use codec::Codec;
use rtsp::Rtsp;
use stream::{History, Shared, SAMPLE_RATE};

struct Args {
    host: IpAddr,
    port: u16,
    volume: u8,
    codec: Codec,
    latency: u32,
}

fn usage() -> ! {
    eprintln!(
        "raop_send — send raw PCM from stdin to an AirPlay-1 speaker\n\n\
         USAGE:\n  \
           <pcm source> | raop_send --host <IP> [options]\n\n\
         OPTIONS:\n  \
           --host <IP>        speaker address (required)\n  \
           --port <N>         RTSP port (default 5000)\n  \
           --volume <0-100>   playback volume (default 50; 0 = mute)\n  \
           --codec <alac|pcm> audio codec (default alac; pcm is experimental)\n  \
           --latency <frames> buffer ahead of playout (default 88200 = 2.0 s)\n  \
           -h, --help         this help\n\n\
         INPUT: interleaved little-endian 16-bit stereo PCM at 44100 Hz.\n\n\
         EXAMPLE:\n  \
           ffmpeg -i in.flac -f s16le -ar 44100 -ac 2 - | raop_send --host 10.0.1.155\n"
    );
    exit(2);
}

fn parse_args() -> Args {
    let mut host: Option<IpAddr> = None;
    let mut port: u16 = 5000;
    let mut volume: u8 = 50;
    let mut codec = Codec::Alac;
    let mut latency: u32 = 88200;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => host = Some(req(&mut it, "--host").parse().unwrap_or_else(|_| die("bad --host IP"))),
            "--port" => port = req(&mut it, "--port").parse().unwrap_or_else(|_| die("bad --port")),
            "--volume" => volume = req(&mut it, "--volume").parse().unwrap_or_else(|_| die("bad --volume")),
            "--latency" => latency = req(&mut it, "--latency").parse().unwrap_or_else(|_| die("bad --latency")),
            "--codec" => {
                codec = match req(&mut it, "--codec").as_str() {
                    "alac" => Codec::Alac,
                    "pcm" => Codec::Pcm,
                    _ => die("--codec must be alac or pcm"),
                }
            }
            "-h" | "--help" => usage(),
            other => die(&format!("unknown argument: {}", other)),
        }
    }

    Args {
        host: host.unwrap_or_else(|| {
            eprintln!("error: --host is required\n");
            usage()
        }),
        port,
        volume: volume.min(100),
        codec,
        latency,
    }
}

fn req(it: &mut impl Iterator<Item = String>, flag: &str) -> String {
    it.next().unwrap_or_else(|| die(&format!("{} needs a value", flag)))
}

fn die(msg: &str) -> ! {
    eprintln!("error: {}", msg);
    exit(2);
}

/// Local IP address the kernel would use to reach `target` (for the SDP).
fn local_ip_for(target: SocketAddr) -> io::Result<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0")?;
    s.connect(target)?;
    Ok(s.local_addr()?.ip())
}

fn volume_db(vol: u8) -> f32 {
    if vol == 0 {
        -144.0 // AirPlay mute
    } else {
        -30.0 + (vol as f32 / 100.0) * 30.0 // -30 dB .. 0 dB
    }
}

fn run() -> io::Result<()> {
    let args = parse_args();
    let device = SocketAddr::new(args.host, args.port);
    let client_ip = local_ip_for(device)?;

    // Bind our control + timing sockets first so we can advertise their ports
    // in SETUP. The device sends retransmit/timing requests to these.
    let control_sock = UdpSocket::bind("0.0.0.0:0")?;
    let timing_sock = UdpSocket::bind("0.0.0.0:0")?;
    let control_port = control_sock.local_addr()?.port();
    let timing_port = timing_sock.local_addr()?.port();

    // --- RTSP handshake -----------------------------------------------------
    let mut rtsp = Rtsp::connect(device, client_ip)?;
    rtsp.options()?;
    rtsp.announce(client_ip, args.host, SAMPLE_RATE, args.codec)?;
    let dports = rtsp.setup(control_port, timing_port)?;

    let seq_start: u16 = rand::random();
    let rtp_start: u32 = rand::random();
    let ssrc: u32 = rand::random();

    rtsp.record(seq_start, rtp_start)?;
    rtsp.set_volume(volume_db(args.volume))?;

    eprintln!(
        "raop_send: streaming to {} [{}], codec={}, vol={} ({:.1} dB), latency={} frames",
        args.host,
        device,
        match args.codec {
            Codec::Alac => "alac",
            Codec::Pcm => "pcm",
        },
        args.volume,
        volume_db(args.volume),
        args.latency,
    );
    eprintln!(
        "raop_send: device ports server={} control={} timing={}",
        dports.server, dports.control, dports.timing
    );

    // --- data plane ---------------------------------------------------------
    let device_audio = SocketAddr::new(args.host, dports.server);
    let device_control = SocketAddr::new(args.host, dports.control);
    let device_timing = SocketAddr::new(args.host, dports.timing);

    let clock = Clock::new();
    let shared = Arc::new(Shared {
        clock: clock.clone(),
        head: AtomicU32::new(rtp_start),
        latency: args.latency,
        history: Mutex::new(History::new(2048)), // ~16 s of packets, fixed memory
    });

    stream::spawn_timing(timing_sock, device_timing, clock.clone());
    stream::spawn_retransmit(control_sock.try_clone()?, device_control, shared.clone());
    stream::spawn_sync(control_sock.try_clone()?, device_control, shared.clone());

    // Keep the idle RTSP TCP connection alive while audio flows.
    let rtsp = Arc::new(Mutex::new(rtsp));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let rtsp = rtsp.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_secs(2));
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let _ = rtsp.lock().unwrap().keepalive();
            }
        });
    }

    let audio_sock = UdpSocket::bind("0.0.0.0:0")?;
    let stdin = io::stdin();
    let result = stream::run_audio(
        audio_sock,
        device_audio,
        stdin.lock(),
        args.codec,
        seq_start,
        rtp_start,
        ssrc,
        shared.clone(),
    );

    stop.store(true, Ordering::Relaxed);
    let _ = rtsp.lock().unwrap().teardown();
    eprintln!("raop_send: stream ended");
    result
}

fn main() {
    if let Err(e) = run() {
        eprintln!("raop_send: error: {}", e);
        exit(1);
    }
}
