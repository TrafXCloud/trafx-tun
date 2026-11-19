//! TrafX extension: several fake-TCP connections carry one UDP flow.
//!
//! The client opens N fake-TCP connections for one UDP peer and sends a HELLO
//! as the first packet (after the optional handshake packet) on each. The
//! server groups the connections that share a session id behind one UDP
//! socket, so the UDP target (WireGuard) sees one stable endpoint.
//!
//! Traffic is spread in bursts: up to `BURST` datagrams in a row go to the
//! same connection before moving on to the next one.
//!
//! The client sends a HELLO also with N = 1, so the server handles every
//! TrafX client the same way. A connection without HELLO is an upstream
//! phantun client and becomes a flow of its own.
//!
//! A flow closes when nothing arrived from the other phantun for
//! `PHANTUN_PEER_TIMEOUT` seconds (default 60). WireGuard keepalives (25 s)
//! keep a live tunnel above that; a peer that restarted or vanished leaves a
//! stale flow behind, and this removes it, so the next packet builds a fresh
//! set of connections on both sides.

use bytes::Bytes;
use crate::cc::{self, Controller, Ctl};
use crate::pacer::{self, Pacer};
use fake_tcp::Socket;
use fake_tcp::packet::MAX_PACKET_LEN;
use log::{debug, error, info, warn};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use tokio_util::sync::CancellationToken;

pub const HELLO_MAGIC: &[u8; 8] = b"TRFXMUX3";
pub const HELLO_LEN: usize = 18;
/// Datagrams sent on one connection before moving to the next one.
pub const BURST: usize = 32;
pub const MAX_CONNECTIONS: usize = 64;
/// Largest fake-TCP packet that leaves without fragmentation on a 1500 MTU.
const MAX_WIRE: usize = 1500;
const IP_TCP_HEADERS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub session: u64,
    pub index: u8,
    pub total: u8,
}

impl Hello {
    pub fn encode(&self) -> [u8; HELLO_LEN] {
        let mut b = [0u8; HELLO_LEN];
        b[..8].copy_from_slice(HELLO_MAGIC);
        b[8..16].copy_from_slice(&self.session.to_be_bytes());
        b[16] = self.index;
        b[17] = self.total;
        b
    }

    pub fn parse(b: &[u8]) -> Option<Hello> {
        if b.len() != HELLO_LEN || &b[..8] != HELLO_MAGIC {
            return None;
        }
        Some(Hello {
            session: u64::from_be_bytes(b[8..16].try_into().unwrap()),
            index: b[16],
            total: b[17],
        })
    }
}

pub fn random_session() -> u64 {
    RandomState::new().build_hasher().finish()
}

pub fn peer_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("PHANTUN_PEER_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v: &u64| v > 0)
            .unwrap_or(60),
    )
}

/// Batches queued per connection writer before the UDP reader waits.
const WRITER_QUEUE: usize = 256;

struct Conn {
    sock: Arc<Socket>,
    tx: mpsc::Sender<Vec<Bytes>>,
}

/// Shared state of one UDP flow carried by a set of fake-TCP connections.
///
/// Each connection has its own writer task. Writing to the Tun device runs
/// the kernel's whole forwarding path (routing, NAT, conntrack) inside the
/// write call, so with one writer per connection that work spreads over the
/// cores instead of running on the single task that reads the UDP socket.
pub struct Flow {
    conns: RwLock<Vec<Conn>>,
    /// UDP sockets towards the peer (all bound to the same address).
    pub udp: RwLock<Vec<Arc<UdpSocket>>>,
    pub quit: CancellationToken,
    /// something arrived from the other phantun since the last check
    peer_seen: AtomicBool,
    oversize_warned: AtomicBool,
    mode: pacer::Mode,
    pacer: Arc<Pacer>,
    cc: Mutex<Controller>,
    counters: Arc<Counters>,
}

/// Data datagrams (not control) sent to and received from the other phantun.
#[derive(Default)]
pub struct Counters {
    sent_bytes: AtomicU64,
    sent_pkts: AtomicU64,
    recv_bytes: AtomicU64,
    recv_pkts: AtomicU64,
}

impl Flow {
    pub fn new() -> Arc<Flow> {
        let mode = pacer::mode();
        let pacer = Arc::new(match mode {
            pacer::Mode::Fixed(r) => Pacer::new(r),
            _ => Pacer::new(0.0),
        });
        let flow = Arc::new(Flow {
            conns: RwLock::new(Vec::new()),
            udp: RwLock::new(Vec::new()),
            quit: CancellationToken::new(),
            peer_seen: AtomicBool::new(true),
            oversize_warned: AtomicBool::new(false),
            mode,
            pacer,
            cc: Mutex::new(Controller::new()),
            counters: Arc::new(Counters::default()),
        });
        match mode {
            pacer::Mode::Auto => info!("pacing: auto (congestion control, experimental)"),
            pacer::Mode::Off => info!("pacing: off"),
            pacer::Mode::Fixed(r) => info!("pacing: fixed {:.1} Mbit/s", r / 1e6),
        }
        tokio::spawn(flow.clone().cc_task());
        flow
    }

    /// Adds a connection and starts its writer; returns the new count.
    pub fn add(&self, sock: Arc<Socket>) -> usize {
        let (tx, rx) = mpsc::channel(WRITER_QUEUE);
        tokio::spawn(writer(sock.clone(), rx, self.quit.clone(), self.pacer.clone(), self.counters.clone()));
        let mut c = self.conns.write().unwrap();
        c.push(Conn { sock, tx });
        c.len()
    }

    /// Removes a dead connection (its writer stops); returns the number still alive.
    pub fn remove(&self, sock: &Arc<Socket>) -> usize {
        let mut c = self.conns.write().unwrap();
        c.retain(|s| !Arc::ptr_eq(&s.sock, sock));
        c.len()
    }

    pub fn first(&self) -> Option<Arc<Socket>> {
        self.conns.read().unwrap().first().map(|c| c.sock.clone())
    }

    /// Sends one datagram on the first connection (packets that reach the
    /// listening socket instead of a flow socket).
    pub async fn send_first(&self, data: &[u8]) -> Option<()> {
        self.first()?.send(data).await
    }

    fn sender(&self, i: usize) -> Option<mpsc::Sender<Vec<Bytes>>> {
        let c = self.conns.read().unwrap();
        if c.is_empty() { None } else { Some(c[i % c.len()].tx.clone()) }
    }

    /// Picks UDP socket `i` (mod count); None until the sockets exist.
    pub fn udp_socket(&self, i: usize) -> Option<Arc<UdpSocket>> {
        let u = self.udp.read().unwrap();
        if u.is_empty() { None } else { Some(u[i % u.len()].clone()) }
    }

    #[inline]
    fn peer_touch(&self) {
        // Plain store: cheaper than a Notify per packet.
        self.peer_seen.store(true, Ordering::Relaxed);
    }

    pub fn len(&self) -> usize {
        self.conns.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Closes the flow when the other phantun sent nothing for `timeout`.
    pub async fn peer_watch(self: Arc<Self>, timeout: Duration) {
        loop {
            tokio::select! {
                _ = time::sleep(timeout) => {
                    if !self.peer_seen.swap(false, Ordering::Relaxed) {
                        info!("Nothing from the peer in the last {:?}, closing flow", timeout);
                        self.quit.cancel();
                        return;
                    }
                }
                _ = self.quit.cancelled() => return,
            }
        }
    }

    /// Sends PROBEs (auto mode), turns pacing off without feedback, logs.
    async fn cc_task(self: Arc<Self>) {
        let auto = self.mode == pacer::Mode::Auto;
        let mut tick = time::interval(cc::INTERVAL);
        tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        let mut n = 0u64;
        let mut last_drop = 0;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = self.quit.cancelled() => return,
            }
            n += 1;
            if auto {
                if let Some(sock) = self.first() {
                    let probe = Ctl::Probe {
                        t: cc::now_us(),
                        sent_bytes: self.counters.sent_bytes.load(Ordering::Relaxed),
                        sent_pkts: self.counters.sent_pkts.load(Ordering::Relaxed),
                    };
                    let _ = sock.send(&probe.encode()).await;
                }
                let off = self.cc.lock().unwrap().on_tick(std::time::Instant::now());
                if let Some(r) = off {
                    self.pacer.set_rate(r);
                    info!("pacing: no feedback from the peer for {:?}, off until it answers", cc::FEEDBACK_TIMEOUT);
                }
            }
            // status every 30 s
            if n % 600 == 0 {
                let d = self.pacer.dropped();
                let st = self.cc.lock().unwrap().status();
                let fb = self.cc.lock().unwrap().has_feedback();
                if auto && fb {
                    info!(
                        "pacing: rate {}, path {:.1} Mbit/s, rtt {:.0} ms, loss {:.2}%, dropped {}",
                        if st.rate > 0.0 { format!("{:.1} Mbit/s", st.rate / 1e6) } else { "unlimited".into() },
                        st.btl_bw / 1e6, st.min_rtt_ms, st.loss * 100.0, d - last_drop
                    );
                } else if d != last_drop {
                    info!("pacing: dropped {} in the last 30s", d - last_drop);
                }
                last_drop = d;
            }
        }
    }

    /// Handles a control datagram from the other phantun on `sock`.
    async fn on_ctl(&self, sock: &Socket, ctl: Ctl) {
        match ctl {
            Ctl::Probe { t, sent_bytes, sent_pkts } => {
                let ack = Ctl::Ack {
                    t,
                    sent_bytes,
                    sent_pkts,
                    recv_bytes: self.counters.recv_bytes.load(Ordering::Relaxed),
                    recv_pkts: self.counters.recv_pkts.load(Ordering::Relaxed),
                };
                let _ = sock.send(&ack.encode()).await;
            }
            Ctl::Ack { t, sent_bytes, sent_pkts, recv_bytes, recv_pkts } => {
                if self.mode == pacer::Mode::Auto {
                    let r = self.cc.lock().unwrap().on_ack(
                        std::time::Instant::now(), cc::now_us(), t, sent_bytes, sent_pkts, recv_bytes, recv_pkts,
                        self.pacer.pressure(),
                    );
                    self.pacer.set_rate(r);
                }
            }
        }
    }
}

async fn writer(
    sock: Arc<Socket>,
    mut rx: mpsc::Receiver<Vec<Bytes>>,
    quit: CancellationToken,
    pacer: Arc<Pacer>,
    counters: Arc<Counters>,
) {
    loop {
        let batch = tokio::select! {
            b = rx.recv() => match b { Some(b) => b, None => return },
            _ = quit.cancelled() => return,
        };
        for p in batch {
            while let Some(wait) = pacer.take(p.len() + pacer::WIRE_OVERHEAD) {
                tokio::select! {
                    _ = time::sleep(wait) => {}
                    _ = quit.cancelled() => return,
                }
            }
            let ok = sock.send(&p).await.is_some();
            pacer.release(p.len());
            if !ok {
                error!("Unable to write to the Tun device, closing flow");
                quit.cancel();
                return;
            }
            counters.sent_bytes.fetch_add(p.len() as u64, Ordering::Relaxed);
            counters.sent_pkts.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Datagrams read per recvmmsg call.
const RECV_BATCH: usize = 64;

/// UDP -> fake TCP on a dedicated OS thread.
///
/// The kernel may cap the socket receive buffer (RouterOS containers refuse
/// SO_RCVBUFFORCE: ~416 KB, about 180 WireGuard packets). A burst then only
/// survives if the socket is drained at once, so a plain thread blocks in
/// poll() and drains with recvmmsg(), 64 datagrams per call, without waiting
/// for the async scheduler. Datagrams go to the connection writers,
/// `BURST` in a row per connection.
pub fn spawn_udp_reader(udp: Arc<UdpSocket>, flow: Arc<Flow>) {
    std::thread::Builder::new()
        .name("udp-rx".into())
        .spawn(move || udp_reader(udp, flow))
        .expect("unable to start UDP reader thread");
}

fn udp_reader(udp: Arc<UdpSocket>, flow: Arc<Flow>) {
    use std::os::fd::AsRawFd;
    let fd = udp.as_raw_fd();
    let slot = MAX_PACKET_LEN;
    let mut bufs = vec![0u8; RECV_BATCH * slot];
    let mut iovs: Vec<libc::iovec> = (0..RECV_BATCH)
        .map(|k| libc::iovec {
            iov_base: unsafe { bufs.as_mut_ptr().add(k * slot) } as *mut libc::c_void,
            iov_len: MAX_PACKET_LEN,
        })
        .collect();
    let mut msgs: Vec<libc::mmsghdr> = (0..RECV_BATCH)
        .map(|k| {
            let mut m: libc::mmsghdr = unsafe { std::mem::zeroed() };
            m.msg_hdr.msg_iov = unsafe { iovs.as_mut_ptr().add(k) };
            m.msg_hdr.msg_iovlen = 1;
            m
        })
        .collect();

    let mut cur = 0usize;
    let mut sent = 0usize;
    let mut batch: Vec<Bytes> = Vec::with_capacity(BURST);
    let dispatch = |batch: &mut Vec<Bytes>, cur: &mut usize, sent: &mut usize| {
        if batch.is_empty() {
            return;
        }
        let n = batch.len();
        let out = std::mem::replace(batch, Vec::with_capacity(BURST));
        if let Some(tx) = flow.sender(*cur) {
            let pc = &flow.pacer;
            let bytes: usize = out.iter().map(|p| p.len()).sum();
            if !pc.admit(bytes) {
                // paced: the queue in front of the pacer is full (50 ms)
                pc.count_drop(n);
            } else if pc.limited() {
                if tx.try_send(out).is_err() {
                    pc.release(bytes);
                    pc.count_drop(n);
                }
            } else if tx.blocking_send(out).is_err() {
                // waits only when that writer is far behind (WRITER_QUEUE batches)
                pc.release(bytes);
            }
        }
        *sent += n;
        if *sent >= BURST {
            *sent = 0;
            *cur = cur.wrapping_add(1);
        }
    };

    loop {
        if flow.quit.is_cancelled() {
            return;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // the timeout only bounds how long a finished flow keeps this thread
        let r = unsafe { libc::poll(&mut pfd, 1, 200) };
        if r <= 0 {
            continue;
        }
        loop {
            for m in msgs.iter_mut() {
                m.msg_len = 0;
            }
            let n = unsafe {
                libc::recvmmsg(fd, msgs.as_mut_ptr(), RECV_BATCH as _, libc::MSG_DONTWAIT as _, std::ptr::null_mut())
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::WouldBlock => break,
                    io::ErrorKind::Interrupted => continue,
                    // e.g. ECONNREFUSED after an ICMP error: keep going like upstream does
                    _ => {
                        debug!("UDP recv error: {}", e);
                        break;
                    }
                }
            }
            let n = n as usize;
            for k in 0..n {
                let len = msgs[k].msg_len as usize;
                let start = k * slot;
                if len + IP_TCP_HEADERS > MAX_WIRE && !flow.oversize_warned.swap(true, Ordering::Relaxed) {
                    warn!(
                        "UDP datagram of {} bytes does not fit a 1500 MTU as fake TCP: \
                         set the WireGuard MTU to 1428 or lower",
                        len
                    );
                }
                batch.push(Bytes::copy_from_slice(&bufs[start..start + len]));
                if sent + batch.len() >= BURST {
                    dispatch(&mut batch, &mut cur, &mut sent);
                }
            }
            if n < RECV_BATCH {
                break; // drained
            }
        }
        // socket empty: hand over what we have
        dispatch(&mut batch, &mut cur, &mut sent);
    }
}

/// Fake TCP -> UDP for one connection. Returns when the connection is broken
/// (the caller decides what that means for the flow) or the flow quits.
pub async fn tcp_to_udp(sock: Arc<Socket>, udp: Arc<UdpSocket>, flow: Arc<Flow>) {
    let mut buf = [0u8; MAX_PACKET_LEN];
    loop {
        tokio::select! {
            res = sock.recv(&mut buf) => match res {
                Some(0) => flow.peer_touch(), // bare ACK: still the peer
                Some(size) => {
                    // HELLO is sent more than once; repeats are not data
                    if size == HELLO_LEN && Hello::parse(&buf[..size]).is_some() {
                        continue;
                    }
                    flow.peer_touch();
                    if Ctl::is_ctl(&buf[..size]) {
                        if let Some(ctl) = Ctl::parse(&buf[..size]) {
                            flow.on_ctl(&sock, ctl).await;
                        }
                        continue;
                    }
                    flow.counters.recv_bytes.fetch_add(size as u64, Ordering::Relaxed);
                    flow.counters.recv_pkts.fetch_add(1, Ordering::Relaxed);
                    // non-blocking: a full socket buffer drops, like the network would
                    if let Err(e) = udp.try_send(&buf[..size]) {
                        if e.kind() != io::ErrorKind::WouldBlock {
                            debug!("Unable to send UDP packet: {}", e);
                        }
                    }
                }
                None => return,
            },
            _ = flow.quit.cancelled() => return,
        }
    }
}

/// How many reader tasks to run per connection: the upstream runs one per
/// core for a single connection; with N connections, N readers already exist.
pub fn readers_per_conn(cpus: usize, conns: usize) -> usize {
    (cpus / conns.max(1)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrip() {
        let h = Hello { session: 0x0102030405060708, index: 3, total: 4 };
        assert_eq!(Hello::parse(&h.encode()), Some(h));
        assert_eq!(Hello::parse(b"TRFXMUX3"), None);
        let mut wg = [0u8; HELLO_LEN];
        wg[0] = 1;
        assert_eq!(Hello::parse(&wg), None);
    }

    #[test]
    fn readers() {
        assert_eq!(readers_per_conn(2, 1), 2);
        assert_eq!(readers_per_conn(2, 4), 1);
        assert_eq!(readers_per_conn(8, 4), 2);
    }
}
