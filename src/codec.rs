//! Audio payload encoders for RAOP.
//!
//! Input is always interleaved little-endian 16-bit stereo PCM (`s16le`, two
//! channels) — i.e. 4 bytes per frame — which is what `ffmpeg -f s16le` and
//! friends emit. We turn one 352-frame block into either:
//!   * a raw "uncompressed" ALAC frame (codec `cn=1`), or
//!   * raw big-endian 16-bit PCM (codec `cn=0`).

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Alac,
    Pcm,
}

/// MSB-first bit writer appending into a reused `Vec<u8>`.
struct BitWriter<'a> {
    buf: &'a mut Vec<u8>,
    /// Bits already used in the final byte (0..8).
    used: u32,
}

impl<'a> BitWriter<'a> {
    fn new(buf: &'a mut Vec<u8>) -> Self {
        buf.clear();
        BitWriter { buf, used: 0 }
    }

    /// Write the low `bits` bits of `value`, most-significant bit first.
    fn write(&mut self, value: u32, bits: u32) {
        let mut remaining = bits;
        while remaining > 0 {
            if self.used == 0 {
                self.buf.push(0);
            }
            let free = 8 - self.used; // free bits in current byte
            let take = remaining.min(free);
            let shift = remaining - take; // value bits below this chunk
            let chunk = (value >> shift) & ((1u32 << take) - 1);
            *self.buf.last_mut().unwrap() |= (chunk as u8) << (free - take);
            self.used = (self.used + take) & 7;
            remaining -= take;
        }
    }
}

/// Encode one block of interleaved LE 16-bit stereo PCM into a raw ALAC frame.
///
/// Layout is the well-known "uncompressed ALAC" bitstream every RAOP sender
/// uses (mirrors owntone's `alac_encode_uncompressed`): a 23-bit header, then
/// each stereo sample written big-endian, then a 3-bit end tag.
pub fn encode_alac(out: &mut Vec<u8>, pcm: &[u8]) {
    let mut w = BitWriter::new(out);
    w.write(1, 3); // element tag: stereo channel pair
    w.write(0, 4);
    w.write(0, 8);
    w.write(0, 4);
    w.write(0, 1); // hassize = 0
    w.write(0, 2); // unused
    w.write(1, 1); // is-not-compressed = 1

    for s in pcm.chunks_exact(4) {
        // LE in: L = [s0 lo, s1 hi], R = [s2 lo, s3 hi]; write BE bytes.
        w.write(s[1] as u32, 8);
        w.write(s[0] as u32, 8);
        w.write(s[3] as u32, 8);
        w.write(s[2] as u32, 8);
    }

    w.write(7, 3); // end tag
}

/// Encode one block of interleaved LE 16-bit PCM into big-endian PCM (cn=0).
pub fn encode_pcm_be(out: &mut Vec<u8>, pcm: &[u8]) {
    out.clear();
    out.reserve(pcm.len());
    for s in pcm.chunks_exact(2) {
        out.push(s[1]);
        out.push(s[0]);
    }
}
