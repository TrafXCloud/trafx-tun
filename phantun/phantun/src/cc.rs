//! TrafX: congestion control for the pacer, modelled on BBR. EXPERIMENTAL:
//! only with PHANTUN_RATE=auto. Steady state works; after a peer restart it
//! can drop traffic for a few seconds (scripts/robustness.sh step 4).
//!
//! Fake TCP has no congestion control: whatever the tunnel is given goes out
//! at once, and the path drops what it cannot carry. A fixed pacing rate
//! helps only when it sits just below what the path carries, which changes
//! during the day. This controller finds that rate.
//!
//! Every `INTERVAL` each side sends a PROBE on its first connection with a
//! timestamp and how much data it has sent so far. The other side answers
//! with an ACK that echoes those and adds how much data it has received. From
//! two ACKs in a row the sender gets, for that interval: the delivery rate,
//! the loss and the round trip time. No clocks need to agree: only the
//! sender's own timestamp comes back.
//!
//! As in BBR, the path estimate is the best delivery rate seen recently
//! (a max filter over `BW_WINDOW`), and the pacing rate is `GAIN` above it.
//! Unlike BBR, loss does not lower the estimate: the Iran paths drop a few
//! tenths of a percent at any rate, and a loss-driven controller would
//! follow TCP inside the tunnel down in a spiral. When the path gets
//! weaker, the window forgets the old best rate within `BW_WINDOW`.
//! Pacing is on only while the path showed loss in the last `LOSS_MEMORY`:
//! a path with room to spare needs no protection. Without recent traffic
//! (idle, new flow) there is no pacing either. When the
//! pacer itself holds traffic back (it queues or drops) while the path
//! shows no loss, the estimate came from lighter traffic: the ceiling then
//! grows by `GROW` per ACK until the path starts to lose.
//!
//! A peer without this extension forwards our PROBE to WireGuard (dropped
//! there) and never answers; after `FEEDBACK_TIMEOUT` pacing is turned off.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const CTL_MAGIC: &[u8; 8] = b"TRFXCTL1";
const T_PROBE: u8 = 1;
const T_ACK: u8 = 2;
pub const PROBE_LEN: usize = 9 + 3 * 8;
pub const ACK_LEN: usize = 9 + 5 * 8;
pub const INTERVAL: Duration = Duration::from_millis(50);
pub const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(1);
/// Ceiling above the best recent delivery rate: room for TCP inside the
/// tunnel to grow, while line-rate bursts are smoothed out.
const GAIN: f64 = 1.25;
/// Delivery rate measured over this many ACKs (200 ms).
const SPAN: usize = 4;
/// The best delivery rate of this window is the path estimate.
const BW_WINDOW: Duration = Duration::from_secs(10);
/// Samples below this (keepalives, idle) say nothing about the path.
const MIN_SAMPLE_BW: f64 = 1e6;
/// Samples in the window before pacing starts (1 s of traffic).
const MIN_SAMPLES: usize = 20;
/// Floor of the ceiling: pacing below this smooths nothing that matters,
/// and a low ceiling is what made a sudden load wait.
const MIN_RATE: f64 = 10e6;
const MAX_RATE: f64 = 10e9;
/// Ceiling growth per ACK while the pacer is the bottleneck.
const GROW: f64 = 2.0;
/// Loss below which the pacer, not the path, is taken as the bottleneck;
/// at or above it the path is full.
const GROW_LOSS: f64 = 0.01;
/// Pacing stays on this long after the path was last seen full.
const LOSS_MEMORY: Duration = Duration::from_secs(30);
const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);

/// Microseconds since this process started.
pub fn now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctl {
    Probe { t: u64, sent_bytes: u64, sent_pkts: u64 },
    Ack { t: u64, sent_bytes: u64, sent_pkts: u64, recv_bytes: u64, recv_pkts: u64 },
}

impl Ctl {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(ACK_LEN);
        b.extend_from_slice(CTL_MAGIC);
        match *self {
            Ctl::Probe { t, sent_bytes, sent_pkts } => {
                b.push(T_PROBE);
                for v in [t, sent_bytes, sent_pkts] {
                    b.extend_from_slice(&v.to_be_bytes());
                }
            }
            Ctl::Ack { t, sent_bytes, sent_pkts, recv_bytes, recv_pkts } => {
                b.push(T_ACK);
                for v in [t, sent_bytes, sent_pkts, recv_bytes, recv_pkts] {
                    b.extend_from_slice(&v.to_be_bytes());
                }
            }
        }
        b
    }

    /// Cheap check first: WireGuard packets start with type 1..4 and zeros.
    #[inline]
    pub fn is_ctl(b: &[u8]) -> bool {
        (b.len() == PROBE_LEN || b.len() == ACK_LEN) && b[0] == b'T' && &b[..8] == CTL_MAGIC
    }

    pub fn parse(b: &[u8]) -> Option<Ctl> {
        if !Ctl::is_ctl(b) {
            return None;
        }
        let u = |i: usize| u64::from_be_bytes(b[9 + i * 8..17 + i * 8].try_into().unwrap());
        match (b[8], b.len()) {
            (T_PROBE, PROBE_LEN) => Some(Ctl::Probe { t: u(0), sent_bytes: u(1), sent_pkts: u(2) }),
            (T_ACK, ACK_LEN) => Some(Ctl::Ack {
                t: u(0),
                sent_bytes: u(1),
                sent_pkts: u(2),
                recv_bytes: u(3),
                recv_pkts: u(4),
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AckSample {
    t: u64,
    sent_bytes: u64,
    sent_pkts: u64,
    recv_bytes: u64,
    recv_pkts: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Status {
    pub rate: f64,
    pub btl_bw: f64,
    pub min_rtt_ms: f64,
    pub loss: f64,
}

pub struct Controller {
    /// the last few ACKs: delivery is measured over `SPAN` of them
    acks: VecDeque<AckSample>,
    /// (when, delivery rate bits/s), only samples above `MIN_SAMPLE_BW`
    bw: VecDeque<(Instant, f64)>,
    rtt: VecDeque<(Instant, f64)>,
    last_feedback: Option<Instant>,
    /// last time the path was seen full (loss in two intervals in a row)
    last_loss: Option<Instant>,
    prev_lossy: bool,
    rate: f64,
    loss: f64,
}

impl Default for Controller {
    fn default() -> Self {
        Self::new()
    }
}

impl Controller {
    pub fn new() -> Controller {
        Controller {
            acks: VecDeque::new(),
            bw: VecDeque::new(),
            rtt: VecDeque::new(),
            last_feedback: None,
            last_loss: None,
            prev_lossy: false,
            rate: 0.0,
            loss: 0.0,
        }
    }

    fn min_rtt(&self) -> f64 {
        self.rtt.iter().map(|&(_, r)| r).fold(f64::INFINITY, f64::min)
    }

    fn btl_bw(&self) -> f64 {
        self.bw.iter().map(|&(_, b)| b).fold(0.0, f64::max)
    }

    fn reset(&mut self) {
        *self = Controller::new();
    }

    /// Feeds an ACK; returns the new pacing rate in bits/s (0 = unlimited).
    #[allow(clippy::too_many_arguments)]
    pub fn on_ack(&mut self, now: Instant, now_us: u64, t: u64, sent_bytes: u64, sent_pkts: u64, recv_bytes: u64, recv_pkts: u64, pressure: bool) -> f64 {
        self.last_feedback = Some(now);
        let rtt = now_us.saturating_sub(t) as f64 / 1e6;
        self.rtt.push_back((now, rtt));
        while self.rtt.front().is_some_and(|&(w, _)| now.duration_since(w) > MIN_RTT_WINDOW) {
            self.rtt.pop_front();
        }
        let cur = AckSample { t, sent_bytes, sent_pkts, recv_bytes, recv_pkts };
        if let Some(last) = self.acks.back() {
            if cur.t <= last.t || cur.recv_bytes < last.recv_bytes || cur.sent_bytes < last.sent_bytes {
                // reordered ACK or a peer restart: start over from this one
                self.acks.clear();
            }
        }
        self.acks.push_back(cur);
        if self.acks.len() > SPAN + 1 {
            self.acks.pop_front();
        }
        if self.acks.len() < SPAN + 1 {
            return self.rate;
        }
        let prev = self.acks[0];
        let dt = (cur.t - prev.t) as f64 / 1e6;
        let bw = (cur.recv_bytes - prev.recv_bytes) as f64 * 8.0 / dt;
        let d_sent = cur.sent_pkts - prev.sent_pkts;
        let d_recv = cur.recv_pkts.saturating_sub(prev.recv_pkts);
        let loss = if d_sent >= 20 { (1.0 - d_recv as f64 / d_sent as f64).clamp(0.0, 1.0) } else { 0.0 };
        self.loss = 0.9 * self.loss + 0.1 * loss;

        // loss counts only when two intervals in a row show it: a single one
        // is often counting skew (data still in flight on other connections)
        let lossy = loss >= GROW_LOSS && self.prev_lossy;
        self.prev_lossy = loss >= GROW_LOSS;
        // the path estimate comes only from busy intervals (we held traffic
        // back, or the path lost some): light traffic says nothing about it
        if bw >= MIN_SAMPLE_BW && (pressure || lossy || bw > self.btl_bw()) {
            self.bw.push_back((now, bw));
        }
        while self.bw.front().is_some_and(|&(w, _)| now.duration_since(w) > BW_WINDOW) {
            self.bw.pop_front();
        }
        if lossy {
            self.last_loss = Some(now);
        }
        // pace only when the path recently showed it is full (loss) and we
        // know what it carries; otherwise there is nothing to protect and
        // holding traffic back would only cost speed
        let path_full = self.last_loss.is_some_and(|t| now.duration_since(t) <= LOSS_MEMORY);
        if self.bw.len() < MIN_SAMPLES || !path_full {
            self.rate = 0.0;
            return self.rate;
        }
        let base = (self.btl_bw() * GAIN).max(MIN_RATE);
        self.rate = if pressure && loss < GROW_LOSS {
            // we are what holds traffic back and the path loses nothing:
            // the estimate came from lighter traffic, grow fast
            (self.rate.max(base) * GROW).min(MAX_RATE)
        } else {
            base
        };
        self.rate
    }

    /// Called every INTERVAL; Some(0) when feedback stopped (pacing off).
    pub fn on_tick(&mut self, now: Instant) -> Option<f64> {
        match self.last_feedback {
            Some(t) if now.duration_since(t) > FEEDBACK_TIMEOUT && self.rate > 0.0 => {
                self.reset();
                Some(0.0)
            }
            _ => None,
        }
    }

    pub fn has_feedback(&self) -> bool {
        self.last_feedback.is_some()
    }

    pub fn status(&self) -> Status {
        Status {
            rate: self.rate,
            btl_bw: self.btl_bw(),
            min_rtt_ms: if self.min_rtt().is_finite() { self.min_rtt() * 1e3 } else { 0.0 },
            loss: self.loss,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctl_roundtrip() {
        let p = Ctl::Probe { t: 1, sent_bytes: 2, sent_pkts: 3 };
        assert_eq!(Ctl::parse(&p.encode()), Some(p));
        let a = Ctl::Ack { t: 1, sent_bytes: 2, sent_pkts: 3, recv_bytes: 4, recv_pkts: 5 };
        assert_eq!(Ctl::parse(&a.encode()), Some(a));
        let mut wg = vec![0u8; PROBE_LEN];
        wg[0] = 4;
        assert!(!Ctl::is_ctl(&wg));
    }

    /// Path of `cap` bits/s, sender offering `offer` bits/s for `secs`.
    fn simulate(cap: f64, offer: f64, secs: f64) -> Controller {
        let mut c = Controller::new();
        let t0 = Instant::now();
        let (mut sent_b, mut recv_b, mut sent_p, mut recv_p) = (0u64, 0u64, 0u64, 0u64);
        let rtt_us = 80_000u64;
        let steps = (secs / INTERVAL.as_secs_f64()) as u64;
        for i in 1..=steps {
            let rate = if c.rate > 0.0 { c.rate.min(offer) } else { offer };
            let bytes = (rate / 8.0 * INTERVAL.as_secs_f64()) as u64;
            let delivered = bytes.min((cap / 8.0 * INTERVAL.as_secs_f64()) as u64);
            sent_b += bytes;
            recv_b += delivered;
            sent_p += bytes / 1400;
            recv_p += delivered / 1400;
            let t = i * INTERVAL.as_micros() as u64;
            let now = t0 + Duration::from_micros(t + rtt_us);
            c.on_ack(now, t + rtt_us, t, sent_b, sent_p, recv_b, recv_p, offer > rate);
        }
        c
    }

    #[test]
    fn converges_to_path_capacity() {
        let c = simulate(100e6, 1e9, 20.0);
        let s = c.status();
        assert!(s.btl_bw > 90e6 && s.btl_bw < 110e6, "btl {}", s.btl_bw);
        assert!(s.rate > 70e6 && s.rate < 130e6, "rate {}", s.rate);
    }

    #[test]
    fn app_limited_does_not_collapse() {
        // sender offers only 20 of 100: the rate stays above what is offered
        let c = simulate(100e6, 20e6, 10.0);
        let r = c.status().rate;
        assert!(r == 0.0 || r >= 20e6, "rate {}", r);
    }

    #[test]
    fn grows_from_a_low_estimate() {
        // light traffic, then 50 Mbit offered on a 100 Mbit path while the
        // path was lately full: within 1 s the ceiling is out of the way
        let mut c = simulate(100e6, 3e6, 2.0);
        c.last_loss = Some(Instant::now());
        let t0 = Instant::now();
        let (mut sb, mut rb, mut sp, mut rp) = (10_000_000u64, 10_000_000u64, 7000u64, 7000u64);
        let mut t = 10_000_000u64;
        for _ in 0..20 {
            t += 50_000;
            let r = c.status().rate;
            let rate = if r > 0.0 { r.min(50e6) } else { 50e6 };
            let bytes = (rate / 8.0 * 0.05) as u64;
            sb += bytes; rb += bytes; sp += bytes / 1400; rp += bytes / 1400;
            c.on_ack(t0 + Duration::from_micros(t), t + 80_000, t, sb, sp, rb, rp, r > 0.0 && r < 50e6);
        }
        let r = c.status().rate;
        assert!(r == 0.0 || r >= 50e6, "rate after 1 s: {}", r);
    }

    #[test]
    fn heavy_loss_does_not_spiral() {
        // path drops 10% at any rate: the estimate must stay near what gets through
        let mut c = Controller::new();
        let t0 = Instant::now();
        let (mut sb, mut rb, mut sp, mut rp) = (0u64, 0u64, 0u64, 0u64);
        for i in 1..=400u64 {
            let rate = if c.rate > 0.0 { c.rate.min(1e9) } else { 1e9 };
            let bytes = (rate.min(60e6) / 8.0 * 0.05) as u64;
            sb += bytes;
            sp += bytes / 1400;
            rb += bytes * 9 / 10;
            rp += bytes / 1400 * 9 / 10;
            let t = i * 50_000;
            c.on_ack(t0 + Duration::from_micros(t + 80_000), t + 80_000, t, sb, sp, rb, rp, false);
        }
        assert!(c.status().btl_bw > 30e6, "btl {}", c.status().btl_bw);
    }

    #[test]
    fn feedback_timeout_turns_off() {
        let mut c = simulate(50e6, 1e9, 5.0);
        assert!(c.status().rate > 0.0);
        assert_eq!(c.on_tick(Instant::now() + Duration::from_secs(30)), Some(0.0));
        assert_eq!(c.status().rate, 0.0);
    }
}
