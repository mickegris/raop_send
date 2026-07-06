//! raop_send — pipe raw PCM (s16le, 44100 Hz, stereo) from stdin to an
//! AirPlay-1 (RAOP) speaker.
//!
//!     ffmpeg -i song.flac -f s16le -ar 44100 -ac 2 - | raop_send --host 192.168.1.50
//!
//! One job, done carefully: unencrypted realtime RAOP with correct timing,
//! SYNC and retransmit so strict (Linkplay/WiiMu) receivers keep playing.

mod clock;
mod codec;
mod log;
mod mdns;
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
    /// Either a literal IP or an mDNS `_raop._tcp` instance name (e.g.
    /// "Office"), resolved in `run()`.
    host: String,
    /// `None` means "use the discovered port, or 5000 if not discovering".
    port: Option<u16>,
    volume: u8,
    codec: Codec,
    latency: u32,
    /// 0 = errors only, 1 = status (default), 2 = protocol trace.
    verbosity: u8,
}

fn usage() -> ! {
    eprintln!(
        "raop_send — send raw PCM from stdin to an AirPlay-1 speaker\n\n\
         USAGE:\n  \
           <pcm source> | raop_send --host <IP|name> [options]\n\n\
         OPTIONS:\n  \
           --host <IP|name>   speaker address, or its mDNS instance name (e.g. \"Office\") (required)\n  \
           --port <N>         RTSP port (default 5000, or the discovered port)\n  \
           --volume <0-100>   playback volume (default 50; 0 = mute)\n  \
           --codec <alac|pcm> audio codec (default alac; pcm is experimental)\n  \
           --latency <frames> buffer ahead of playout (default 88200 = 2.0 s)\n  \
           -q, --quiet        errors only\n  \
           -v, --verbose      protocol-level trace (RTSP verbs, mDNS steps)\n  \
           -h, --help         this help\n\n\
         INPUT: interleaved little-endian 16-bit stereo PCM at 44100 Hz.\n\n\
         EXAMPLE:\n  \
           ffmpeg -i in.flac -f s16le -ar 44100 -ac 2 - | raop_send --host 192.168.1.50\n  \
           ffmpeg -i in.flac -f s16le -ar 44100 -ac 2 - | raop_send --host Office\n"
    );
    exit(2);
}

fn parse_args() -> Args {
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut volume: u8 = 50;
    let mut codec = Codec::Alac;
    let mut latency: u32 = 88200;
    let mut verbosity: u8 = 1;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => host = Some(req(&mut it, "--host")),
            "--port" => port = Some(req(&mut it, "--port").parse().unwrap_or_else(|_| die("bad --port"))),
            "--volume" => volume = req(&mut it, "--volume").parse().unwrap_or_else(|_| die("bad --volume")),
            "--latency" => latency = req(&mut it, "--latency").parse().unwrap_or_else(|_| die("bad --latency")),
            "--codec" => {
                codec = match req(&mut it, "--codec").as_str() {
                    "alac" => Codec::Alac,
                    "pcm" => Codec::Pcm,
                    _ => die("--codec must be alac or pcm"),
                }
            }
            "-q" | "--quiet" => verbosity = 0,
            "-v" | "--verbose" => verbosity = 2,
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
        verbosity,
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
    log::set(args.verbosity);

    // `--host` is either a literal IP or an mDNS `_raop._tcp` instance name.
    let (host_ip, discovered_port): (IpAddr, Option<u16>) = match args.host.parse::<IpAddr>() {
        Ok(ip) => (ip, None),
        Err(_) => {
            vlog!(2, "raop_send: resolving \"{}\" via mDNS...", args.host);
            let found = mdns::resolve(&args.host, Duration::from_secs(3)).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("mDNS lookup for \"{}\" failed: {}", args.host, e),
                )
            })?;
            vlog!(
                1,
                "raop_send: resolved \"{}\" -> {}:{} ({})",
                args.host,
                found.addr,
                found.port,
                found.instance
            );
            if found.airplay2 {
                vlog!(
                    1,
                    "raop_send: note: \"{}\" also advertises AirPlay 2 (_airplay._tcp) — not yet supported, using AirPlay 1",
                    args.host
                );
            }
            (IpAddr::V4(found.addr), Some(found.port))
        }
    };
    let port = args.port.unwrap_or_else(|| discovered_port.unwrap_or(5000));

    let device = SocketAddr::new(host_ip, port);
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
    rtsp.announce(client_ip, host_ip, SAMPLE_RATE, args.codec)?;
    let dports = rtsp.setup(control_port, timing_port)?;
    vlog!(
        2,
        "raop_send: device ports server={} control={} timing={}",
        dports.server,
        dports.control,
        dports.timing
    );

    let seq_start: u16 = rand::random();
    let rtp_start: u32 = rand::random();
    let ssrc: u32 = rand::random();

    // --- data plane ---------------------------------------------------------
    let device_audio = SocketAddr::new(host_ip, dports.server);
    let device_control = SocketAddr::new(host_ip, dports.control);
    let device_timing = SocketAddr::new(host_ip, dports.timing);

    let clock = Clock::new();
    let shared = Arc::new(Shared {
        clock: clock.clone(),
        head: AtomicU32::new(rtp_start),
        latency: args.latency,
        history: Mutex::new(History::new(2048)), // ~16 s of packets, fixed memory
    });

    // Strict Linkplay/WiiMu receivers begin probing our advertised timing port
    // right after SETUP and withhold the RECORD response until the NTP timing
    // exchange completes. The timing (and retransmit) responders must therefore
    // be live *before* RECORD, or the handshake deadlocks. SYNC is started only
    // once the device is recording.
    stream::spawn_timing(timing_sock, device_timing, clock.clone());
    stream::spawn_retransmit(control_sock.try_clone()?, device_control, shared.clone());

    rtsp.record(seq_start, rtp_start)?;
    rtsp.set_volume(volume_db(args.volume))?;

    stream::spawn_sync(control_sock.try_clone()?, device_control, shared.clone());

    vlog!(
        1,
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
    vlog!(1, "raop_send: stream ended");
    result
}

fn main() {
    if let Err(e) = run() {
        eprintln!("raop_send: error: {}", e);
        exit(1);
    }
}
