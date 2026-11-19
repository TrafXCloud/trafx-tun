//! TrafX: pacing of outgoing fake TCP in user space.
//!
//! Without pacing, fake TCP leaves in bursts at line rate. Shallow queues on
//! the way and the capped UDP buffer of the receiving container (RouterOS
//! refuses SO_RCVBUFFORCE) drop parts of those bursts, and TCP inside the
//! tunnel keeps halving its speed. RouterOS containers also offer no qdisc
//! (tc fails for every kind), so pacing has to happen here.
//!
//! One pacer per flow: a token bucket holding `BURST_TIME` worth of bytes and
//! a queue in front of it holding at most `QUEUE_TIME`. What does not fit is
//! dropped here, once, instead of somewhere on the path: TCP inside the
//! tunnel gets a clean signal at our rate and the delay stays bounded.
//! A rate of zero means no pacing.
//!
//! The rate comes from `PHANTUN_RATE`:
//!   unset / `off`   no pacing
//!   `200mbit` ...   fixed
//!   `auto`          set by the congestion controller (cc.rs), EXPERIMENTAL

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const BURST_TIME: f64 = 0.002; // 2 ms
const QUEUE_TIME: f64 = 0.050; // 50 ms
const MIN_BURST: f64 = 16.0 * 1500.0;
const MIN_QUEUE: usize = 64 * 1500;
/// IP + TCP header bytes added to every payload on the wire.
pub const WIRE_OVERHEAD: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Off,
    Auto,
    /// bits per second
    Fixed(f64),
}

pub fn mode() -> Mode {
    let v = std::env::var("PHANTUN_RATE").unwrap_or_default();
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("off") {
        Mode::Off
    } else if v.eq_ignore_ascii_case("auto") {
        Mode::Auto
    } else {
        match parse_rate(v) {
            Some(r) => Mode::Fixed(r),
            None => {
                log::warn!("PHANTUN_RATE={:?} not understood (off, e.g. 200mbit, or auto), pacing off", v);
                Mode::Off
            }
        }
    }
}

/// Parses `200mbit`, `200m`, `200M`, `1.5gbit`, `800kbit`, `200000000`
/// (bits per second).
pub fn parse_rate(s: &str) -> Option<f64> {
    let s = s.trim().to_ascii_lowercase();
    let s = s.strip_suffix("bit").or_else(|| s.strip_suffix("bps")).unwrap_or(&s);
    let (num, mult) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1e3),
        'm' => (&s[..s.len() - 1], 1e6),
        'g' => (&s[..s.len() - 1], 1e9),
        _ => (s, 1.0),
    };
    let v: f64 = num.parse().ok()?;
    (v > 0.0).then_some(v * mult)
}

struct Bucket {
    /// bytes per second, 0 = unlimited
    rate: f64,
    tokens: f64,
    last: Instant,
}

pub struct Pacer {
    st: Mutex<Bucket>,
    /// bytes accepted but not sent yet
    queued: AtomicUsize,
    dropped: AtomicU64,
    pressure_mark: AtomicU64,
}

impl Default for Pacer {
    fn default() -> Self {
        Self::new(0.0)
    }
}

impl Pacer {
    pub fn new(bits_per_sec: f64) -> Pacer {
        let rate = bits_per_sec / 8.0;
        Pacer {
            st: Mutex::new(Bucket { rate, tokens: (rate * BURST_TIME).max(MIN_BURST), last: Instant::now() }),
            queued: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            pressure_mark: AtomicU64::new(0),
        }
    }

    /// Sets the rate in bits per second (0 = unlimited).
    pub fn set_rate(&self, bits_per_sec: f64) {
        self.st.lock().unwrap().rate = (bits_per_sec / 8.0).max(0.0);
    }

    /// Current rate in bits per second (0 = unlimited).
    pub fn rate(&self) -> f64 {
        self.st.lock().unwrap().rate * 8.0
    }

    pub fn limited(&self) -> bool {
        self.st.lock().unwrap().rate > 0.0
    }

    /// Takes tokens for `bytes`, or says how long to wait before trying again.
    pub fn take(&self, bytes: usize) -> Option<Duration> {
        let mut st = self.st.lock().unwrap();
        if st.rate <= 0.0 {
            return None;
        }
        let now = Instant::now();
        let burst = (st.rate * BURST_TIME).max(MIN_BURST);
        let tokens = (st.tokens + now.duration_since(st.last).as_secs_f64() * st.rate).min(burst);
        st.last = now;
        let need = bytes as f64;
        if tokens >= need {
            st.tokens = tokens - need;
            None
        } else {
            st.tokens = tokens;
            Some(Duration::from_secs_f64((need - tokens) / st.rate))
        }
    }

    fn limit(&self) -> usize {
        let rate = self.st.lock().unwrap().rate;
        if rate <= 0.0 { usize::MAX } else { ((rate * QUEUE_TIME) as usize).max(MIN_QUEUE) }
    }

    /// Reserves queue room for `bytes`; false means drop them.
    pub fn admit(&self, bytes: usize) -> bool {
        let limit = self.limit();
        let q = self.queued.fetch_add(bytes, Ordering::Relaxed);
        if q.saturating_add(bytes) > limit {
            self.queued.fetch_sub(bytes, Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    pub fn release(&self, bytes: usize) {
        self.queued.fetch_sub(bytes, Ordering::Relaxed);
    }

    pub fn count_drop(&self, packets: usize) {
        self.dropped.fetch_add(packets as u64, Ordering::Relaxed);
    }

    /// True when the pacer held traffic back since the last call: it dropped
    /// or its queue is over a quarter full.
    pub fn pressure(&self) -> bool {
        let d = self.dropped.load(Ordering::Relaxed);
        let was = self.pressure_mark.swap(d, Ordering::Relaxed);
        d != was || self.queued.load(Ordering::Relaxed) > self.limit() / 4
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates() {
        assert_eq!(parse_rate("200mbit"), Some(200e6));
        assert_eq!(parse_rate("200M"), Some(200e6));
        assert_eq!(parse_rate("1.5gbit"), Some(1.5e9));
        assert_eq!(parse_rate("800kbit"), Some(800e3));
        assert_eq!(parse_rate("200000000"), Some(200e6));
        assert_eq!(parse_rate("fast"), None);
        assert_eq!(parse_rate("0"), None);
    }

    #[test]
    fn bucket_paces() {
        let p = Pacer::new(8e6); // 1 MB/s, burst 24 KB (minimum)
        let mut sent = 0;
        while p.take(1500).is_none() {
            sent += 1500;
        }
        assert!(sent >= 22_500 && sent <= 24_000, "burst {}", sent);
        let w = p.take(1500).unwrap();
        assert!(w <= Duration::from_millis(2), "wait {:?}", w);
    }

    #[test]
    fn unlimited() {
        let p = Pacer::new(0.0);
        for _ in 0..10_000 {
            assert!(p.take(1500).is_none());
        }
        assert!(p.admit(1 << 30));
        p.set_rate(8e6);
        assert!(p.limited());
    }

    #[test]
    fn queue_limit() {
        let p = Pacer::new(8e6);
        let limit = p.limit();
        assert!(p.admit(limit));
        assert!(!p.admit(1));
        p.release(limit);
        assert!(p.admit(1));
    }
}
