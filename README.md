# raop_send

Pipe raw PCM from stdin to an **AirPlay 1 (RAOP)** speaker. One job, done carefully.

Developed against **Audio Pro C-series speakers** (Linkplay/WiiMu-based hardware, e.g. the
C10), because `raop_play`/libraop played ~1 second of audio then went silent on them. That
symptom is the signature of broken realtime **timing / SYNC / retransmit** — not a codec or
encryption problem — so this sender focuses on getting those three things right:

- a precise **NTP timing responder**,
- periodic **SYNC** packets anchoring the RTP clock, and
- a **retransmit responder** backed by a bounded packet ring.

It implements the standard RAOP protocol, so it should work with other AirPlay 1 receivers
too — particularly other strict, Linkplay/WiiMu-based devices — but only the Audio Pro
C-series has actually been verified against real hardware so far. It uses **unencrypted**
RAOP (`et=0`) and defaults to **ALAC** (`cn=1`).

## A note on how this was built

This project was largely **vibe coded**: built through iterative pair-programming sessions
with Claude Code (an AI coding assistant), with real hardware runs — not a test suite, there
isn't one — as the actual verification step for every fix. Read the code with that in mind.

## Build

Requires a Rust toolchain (`rustup`/`cargo`, stable, 2021 edition). No system
dependencies — no OpenSSL, no C libraries, no avahi headers — just Rust's `std` plus one
small crate (`rand`, for session/sequence randomization).

```sh
cargo build --release
# binary: ./target/release/raop_send
```

## Use

Input must be **interleaved little-endian 16-bit stereo PCM at 44100 Hz**.

```sh
# From a file via ffmpeg:
ffmpeg -i song.flac -f s16le -ar 44100 -ac 2 - | ./target/release/raop_send --host 192.168.1.50

# Or by its mDNS/Bonjour instance name instead of an IP:
ffmpeg -i song.flac -f s16le -ar 44100 -ac 2 - | ./target/release/raop_send --host Office

# From MPD (configure a fifo/pipe output emitting s16le 44100:16:2), e.g.:
cat /path/to/mpd.fifo | ./target/release/raop_send --host 192.168.1.50
```

### Options

| Flag | Default | Meaning |
|------|---------|---------|
| `--host <IP\|name>` | (required) | Speaker address, or its `_raop._tcp` mDNS instance name (e.g. `Office`) |
| `--port <N>` | `5000`, or the discovered port | RTSP port |
| `--volume <0-100>` | `50` | 0 = mute, 100 = 0 dB (see [Volume](#volume) below) |
| `--codec <alac\|pcm>` | `alac` | Audio codec. `pcm` is experimental (L16 announce; ALAC is the interoperable path) |
| `--latency <frames>` | `88200` | Buffer ahead of playout (2.0 s) |
| `-q`, `--quiet` | | Errors only |
| `-v`, `--verbose` | | Protocol-level trace (RTSP verbs, mDNS steps, timing-sync confirmation) |

### Volume

If you're piping from something with its own volume control (MPD's software mixer, for
example), there are two independent, stacking volume stages to be aware of:

1. **Upstream mixer** (e.g. MPD's `mixer_type "software"`) scales the actual PCM sample
   values before they ever reach `raop_send`'s stdin — this is whatever your player exposes
   as a live, adjustable volume.
2. **`raop_send --volume`** is a separate value sent once, at startup, via RTSP
   `SET_PARAMETER` — the *speaker itself* applies this gain to the decoded audio. It can't be
   changed while `raop_send` is running; a new value only takes effect on the next run.

Two common setups:

- **Pass-through:** if upstream already has a volume control you like, set `--volume 100`
  (0 dB, unity) here and let upstream be the only knob.
- **Safety ceiling:** if you want a hard cap that no upstream client (a phone app, a web UI)
  can ever exceed, bake a conservative `--volume` into the invocation (e.g. `--volume 40`) —
  the speaker will never get louder than that, regardless of what upstream's volume is set
  to, since this value can only change by editing the command and restarting the process.

## How it works

```
--host <name> ─► mDNS PTR/SRV/A query (_raop._tcp.local) ─► resolved IP:port
stdin PCM ─► 352-frame blocks ─► ALAC/PCM ─► RTP audio (UDP) ──► device:server_port
                                                  │  paced to wall clock, latency-ahead
RTSP/TCP (OPTIONS/ANNOUNCE/SETUP/RECORD/SET_PARAMETER/TEARDOWN, + keepalive)
SYNC (UDP, ~1/s) ───────────────────────────────────────────► device:control_port
retransmit replies (UDP) ◄── requests ───────────────────────  device:control_port
timing replies (UDP) ◄── NTP requests ───────────────────────  our timing_port
```

Name resolution (`src/mdns.rs`) is a minimal one-shot mDNS-SD client: it sends
a unicast-response ("QU") PTR query so it can listen on an ephemeral UDP port
instead of fighting `avahi-daemon` for port 5353, then matches the reply's PTR
name (stripping any `mac@` prefix) and reads the bundled SRV/A records.

Stability/OOM by construction: a **fixed-size** packet ring and audio **paced to
a monotonic clock**, so a slow speaker back-pressures the stdin reader rather
than growing memory.

## Status / not yet implemented

Verified on hardware: Audio Pro C10 (Linkplay/WiiMu), including mDNS name-based discovery.

- Encryption (`et=1` RSA / FairPlay) — not implemented; not needed for the tested hardware.
- AirPlay 2 (buffered audio, HomeKit pairing).
- Sample rates other than 44100 / non-stereo.
