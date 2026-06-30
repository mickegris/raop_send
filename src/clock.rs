//! A monotonic clock that produces 64-bit NTP-format timestamps.
//!
//! RAOP's timing and sync packets carry NTP timestamps. We anchor an NTP value
//! (read from the system clock once at startup) to a monotonic `Instant`, then
//! derive every later timestamp from the monotonic clock. This gives the
//! receiver a smooth, never-jumping timeline even if `chrony`/`ntpd` steps the
//! wall clock mid-stream — important for keeping a strict receiver locked.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

#[derive(Clone)]
pub struct Clock {
    base_instant: Instant,
    /// NTP timestamp at `base_instant`, as 64-bit fixed point (secs << 32 | frac).
    base_ntp: u64,
}

impl Clock {
    pub fn new() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before 1970");
        let secs = now.as_secs() + NTP_UNIX_OFFSET;
        let frac = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
        Clock {
            base_instant: Instant::now(),
            base_ntp: (secs << 32) | frac,
        }
    }

    /// Current time as a 64-bit NTP fixed-point value.
    pub fn ntp64(&self) -> u64 {
        let e = self.base_instant.elapsed();
        let e_ntp = (e.as_secs() << 32) | (((e.subsec_nanos() as u64) << 32) / 1_000_000_000);
        self.base_ntp.wrapping_add(e_ntp)
    }

    /// Current time split into (seconds, fraction).
    pub fn ntp(&self) -> (u32, u32) {
        let v = self.ntp64();
        ((v >> 32) as u32, v as u32)
    }
}
