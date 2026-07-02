# raop_send

Pipe raw PCM from stdin to an **AirPlay 1 (RAOP)** speaker. One job, done carefully.

Built because `raop_play`/libraop played ~1 second then went silent on
Linkplay/WiiMu-based AirPlay-2 speakers (e.g. Audio Pro C10). That symptom is
the signature of broken realtime **timing / SYNC / retransmit** — not a codec or
encryption problem. This sender focuses on getting those three right:

- a precise **NTP timing responder**,
- periodic **SYNC** packets anchoring the RTP clock, and
- a **retransmit responder** backed by a bounded packet ring.

It uses **unencrypted** RAOP (`et=0`) and defaults to **ALAC** (`cn=1`).

## Build

Requires a Rust toolchain (`rustup`/`cargo`). One small dependency.

```sh
cargo build --release
# binary: ./target/release/raop_send
```

## Use

Input must be **interleaved little-endian 16-bit stereo PCM at 44100 Hz**.

```sh
# From a file via ffmpeg:
ffmpeg -i song.flac -f s16le -ar 44100 -ac 2 - | ./target/release/raop_send --host 10.0.1.155

# Or by its mDNS/Bonjour instance name instead of an IP:
ffmpeg -i song.flac -f s16le -ar 44100 -ac 2 - | ./target/release/raop_send --host Kontoret

# From MPD (configure a fifo/pipe output emitting s16le 44100:16:2), e.g.:
cat /path/to/mpd.fifo | ./target/release/raop_send --host 10.0.1.155
```

### Options

| Flag | Default | Meaning |
|------|---------|---------|
| `--host <IP\|name>` | (required) | Speaker address, or its `_raop._tcp` mDNS instance name (e.g. `Kontoret`) |
| `--port <N>` | `5000`, or the discovered port | RTSP port |
| `--volume <0-100>` | `50` | 0 = mute, 100 = 0 dB |
| `--codec <alac\|pcm>` | `alac` | Audio codec. `pcm` is experimental (L16 announce; ALAC is the interoperable path) |
| `--latency <frames>` | `88200` | Buffer ahead of playout (2.0 s) |
| `-q`, `--quiet` | | Errors only |
| `-v`, `--verbose` | | Protocol-level trace (RTSP verbs, mDNS steps, timing-sync confirmation) |

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

Verified: full-track playback on an Audio Pro C10 (Linkplay/WiiMu).

- Encryption (`et=1` RSA / FairPlay) — not needed for this speaker.
- AirPlay 2 (buffered audio, HomeKit pairing).
- Sample rates other than 44100 / non-stereo.
