# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

`raop_send` does exactly one thing: read raw PCM from **stdin** and stream it to an
**AirPlay 1 (RAOP)** speaker over the network. It exists to replace `raop_play`/libraop,
which played ~1 second then went silent on the user's Linkplay/WiiMu-based Audio Pro C
speakers (tested on a C10).

The protocol implementation is generic RAOP (NTP timing/SYNC/retransmit, standard RTSP
verbs), so it should work against other AirPlay 1 receivers too — treat a bug report from
different hardware as legitimate, not as scope creep, as long as the fix is still "make
RAOP work correctly" and not a new feature.

Scope is deliberately narrow. Do not add features outside "PCM in → one AirPlay-1
speaker." No GUI, no playlist logic, no transcoding, no LMS/bridge layer.

## The core problem this code is built around

"~1 second then silence" is the signature of broken **realtime timing/sync**, not a codec
or encryption issue. The speaker accepts unencrypted RAOP and plays its initial buffer,
then mutes because it never gets a valid ongoing RTP-timestamp → clock mapping. The whole
design pours effort into the three things a strict receiver needs:

1. an NTP **timing** responder (`spawn_timing`),
2. periodic **SYNC** packets anchoring the RTP clock (`spawn_sync`), and
3. a **retransmit** responder backed by a bounded ring (`spawn_retransmit`).

The most likely place a device-specific failure hides is the **SYNC latency-anchor math**
in `stream.rs` (`now_without_latency = head - latency`). That is the first knob to tune
against hardware, along with `--latency`.

## Confirmed: RECORD blocks on the timing responder

On the target speaker, RECORD is **not answered until the device's NTP timing probe (to
the `timing_port` from SETUP) gets a reply.** Spawning `spawn_timing`/`spawn_retransmit`
after `rtsp.record()` deadlocks: the device sends its RECORD response as soon as we ack a
timing probe, so `record()` hangs until a 10s TCP read timeout (`os error 11`, EAGAIN) if
nothing is listening on that port yet. Fixed in `main.rs` by binding the UDP responders
before calling `record()`; `spawn_sync` still starts after, once actually recording. Do
not reorder this — verified against hardware (full-track playback on a C10) after the fix.

## Confirmed facts about the target speaker (mDNS `_raop._tcp` TXT)

`et=0,4` → unencrypted is allowed (we use `et=0`, no crypto). `cn=0,1` → both PCM and
ALAC supported. `tp=UDP`, `vs=211.1`, port 5000, `am=WiiMu-A28:AudioPro_C10` (Linkplay).
These rule out encryption as the cause; do not add RSA/FairPlay to chase this bug.

## Architecture

```
src/main.rs    CLI parsing, RTSP handshake orchestration, keepalive, thread wiring
src/mdns.rs    one-shot mDNS-SD resolver: --host <name> -> _raop._tcp IP:port
src/rtsp.rs    RTSP/TCP client: OPTIONS/ANNOUNCE/SETUP/RECORD/SET_PARAMETER/TEARDOWN
src/stream.rs  data plane: paced RTP audio + timing/sync/retransmit threads + History ring
src/codec.rs   ALAC (uncompressed) and big-endian PCM encoders
src/clock.rs   monotonic NTP clock (won't jump if the system clock is stepped)
src/log.rs     global verbosity level (-q/-v) + the `vlog!` macro
```

Diagnostic detail (RTSP verb tracing, mDNS steps, the timing-probe confirmation) lives
behind `vlog!(2, ...)` (`-v`/`--verbose`), not as scaffolding that gets deleted after each
debugging session — keep new protocol-level traces gated the same way instead of bare
`eprintln!`.

Reference sources for the wire formats (keep parity if changing them): packet layouts
follow pyatv `raop/packets.py`; the ALAC frame mirrors owntone `alac_encode_uncompressed`.

## Non-negotiable invariants

- **Bounded memory.** The packet history is a fixed-size power-of-two ring; audio is paced
  to a monotonic clock so a slow speaker back-pressures the stdin reader. Never introduce
  an unbounded queue or buffer — stability/no-OOM is a hard project requirement.
- **Input format is fixed:** interleaved little-endian 16-bit stereo PCM at 44100 Hz.
- Keep the dependency footprint tiny (currently just `rand`) so the build stays trivial.

## Build / run / verify

```sh
cargo build --release          # the entire build
./target/release/raop_send --host 192.168.1.50   # reads PCM from stdin

# realistic test:
ffmpeg -i song.flac -f s16le -ar 44100 -ac 2 - | ./target/release/raop_send --host 192.168.1.50
```

There are no automated tests; this is verified against real hardware on the user's LAN
(Claude cannot reach the speaker). When a change is meant to fix playback, the verification
is a hardware run by the user. The decisive diagnostic when audio cuts: **does the sender
keep emitting RTP after sound stops?** (yes → receiver-side sync rejection, tune the anchor;
no → a stdin/pacing bug). `tcpdump -n host <speaker-ip>` captures this.

## Conventions

- Comments explain *why* (esp. protocol quirks), not *what*. Match the existing density.
- Prefer `std` (threads, `UdpSocket`, `TcpStream`) over pulling async runtimes or crates.
