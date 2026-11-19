use clap::{crate_version, Arg, ArgAction, Command};
use fake_tcp::packet::MAX_PACKET_LEN;
use fake_tcp::{Socket, Stack};
use log::{debug, error, info};
use phantun::mux::{self, Flow};
use phantun::utils::{assign_ipv6_address, new_udp_reuseport};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tokio::time;
use tokio_tun::TunBuilder;


/// Time a new connection has to send its first data packet.
const FIRST_PACKET_TIMEOUT: time::Duration = time::Duration::from_secs(10);
/// After a first packet that is not a HELLO, how long to wait for a HELLO
/// (sent second when the client uses a handshake packet).
const SECOND_PACKET_TIMEOUT: time::Duration = time::Duration::from_millis(1500);

#[tokio::main]
async fn main() -> io::Result<()> {
    pretty_env_logger::init();

    let matches = Command::new("Phantun Server")
        .version(crate_version!())
        .author("Datong Sun (github.com/dndx)")
        .arg(
            Arg::new("local")
                .short('l')
                .long("local")
                .required(true)
                .value_name("PORT")
                .help("Sets the port where Phantun Server listens for incoming Phantun Client TCP connections")
        )
        .arg(
            Arg::new("remote")
                .short('r')
                .long("remote")
                .required(true)
                .value_name("IP or HOST NAME:PORT")
                .help("Sets the address or host name and port where Phantun Server forwards UDP packets to, IPv6 address need to be specified as: \"[IPv6]:PORT\"")
        )
        .arg(
            Arg::new("tun")
                .long("tun")
                .required(false)
                .value_name("tunX")
                .help("Sets the Tun interface name, if absent, pick the next available name")
                .default_value("")
        )
        .arg(
            Arg::new("tun_local")
                .long("tun-local")
                .required(false)
                .value_name("IP")
                .help("Sets the Tun interface local address (O/S's end)")
                .default_value("192.168.201.1")
        )
        .arg(
            Arg::new("tun_peer")
                .long("tun-peer")
                .required(false)
                .value_name("IP")
                .help("Sets the Tun interface destination (peer) address (Phantun Server's end). \
                       You will need to setup DNAT rules to this address in order for Phantun Server \
                       to accept TCP traffic from Phantun Client")
                .default_value("192.168.201.2")
        )
        .arg(
            Arg::new("ipv4_only")
                .long("ipv4-only")
                .short('4')
                .required(false)
                .help("Do not assign IPv6 addresses to Tun interface")
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["tun_local6", "tun_peer6"]),
        )
        .arg(
            Arg::new("tun_local6")
                .long("tun-local6")
                .required(false)
                .value_name("IP")
                .help("Sets the Tun interface IPv6 local address (O/S's end)")
                .default_value("fcc9::1")
        )
        .arg(
            Arg::new("tun_peer6")
                .long("tun-peer6")
                .required(false)
                .value_name("IP")
                .help("Sets the Tun interface IPv6 destination (peer) address (Phantun Client's end). \
                       You will need to setup SNAT/MASQUERADE rules on your Internet facing interface \
                       in order for Phantun Client to connect to Phantun Server")
                .default_value("fcc9::2")
        )
        .arg(
            Arg::new("handshake_packet")
                .long("handshake-packet")
                .required(false)
                .value_name("PATH")
                .help("Specify a file, which, after TCP handshake, its content will be sent as the \
                      first data packet to the client.\n\
                      Note: ensure this file's size does not exceed the MTU of the outgoing interface. \
                      The content is always sent out in a single packet and will not be further segmented")
        )
        .get_matches();

    let local_port: u16 = matches
        .get_one::<String>("local")
        .unwrap()
        .parse()
        .expect("bad local port");

    let remote_addr = tokio::net::lookup_host(matches.get_one::<String>("remote").unwrap())
        .await
        .expect("bad remote address or host")
        .next()
        .expect("unable to resolve remote host name");

    info!("Remote address is: {}", remote_addr);

    let tun_local: Ipv4Addr = matches
        .get_one::<String>("tun_local")
        .unwrap()
        .parse()
        .expect("bad local address for Tun interface");
    let tun_peer: Ipv4Addr = matches
        .get_one::<String>("tun_peer")
        .unwrap()
        .parse()
        .expect("bad peer address for Tun interface");

    let (tun_local6, tun_peer6) = if matches.get_flag("ipv4_only") {
        (None, None)
    } else {
        (
            matches
                .get_one::<String>("tun_local6")
                .map(|v| v.parse().expect("bad local address for Tun interface")),
            matches
                .get_one::<String>("tun_peer6")
                .map(|v| v.parse().expect("bad peer address for Tun interface")),
        )
    };

    let tun_name = matches.get_one::<String>("tun").unwrap();
    let handshake_packet: Option<Vec<u8>> = matches
        .get_one::<String>("handshake_packet")
        .map(fs::read)
        .transpose()?;

    let num_cpus = num_cpus::get();
    info!("{} cores available", num_cpus);

    let tun = TunBuilder::new()
        .name(tun_name) // if name is empty, then it is set by kernel.
        .up() // or set it up manually using `sudo ip link set <tun-name> up`.
        .address(tun_local)
        .destination(tun_peer)
        .queues(num_cpus)
        .build()
        .unwrap();

    if let (Some(tun_local6), Some(tun_peer6)) = (tun_local6, tun_peer6) {
        assign_ipv6_address(tun[0].name(), tun_local6, tun_peer6);
    }

    info!("Created TUN device {}", tun[0].name());

    let mut stack = Stack::new(tun, tun_local, tun_local6);
    stack.listen(local_port);
    info!("Listening on {}", local_port);

    let flows = Arc::new(Mutex::new(HashMap::<u64, Arc<Flow>>::new()));
    let ctx = Arc::new(Ctx { remote_addr, num_cpus, flows });

    let main_loop = tokio::spawn(async move {
        loop {
            let sock = Arc::new(stack.accept().await);
            info!("New connection: {}", sock);
            if let Some(ref p) = handshake_packet {
                if sock.send(p).await.is_none() {
                    error!("Failed to send handshake packet to remote, closing connection.");
                    continue;
                }

                debug!("Sent handshake packet to: {}", sock);
            }
            // Reading the first packets must not block accepting others.
            tokio::spawn(handle_connection(sock, ctx.clone()));
        }
    });

    tokio::join!(main_loop).0.unwrap()
}

struct Ctx {
    remote_addr: SocketAddr,
    num_cpus: usize,
    flows: Arc<Mutex<HashMap<u64, Arc<Flow>>>>,
}

/// Reads the first data packet(s) of a new connection to find its HELLO.
/// A TrafX client sends HELLO first, or second after a handshake packet.
/// Anything else is an upstream client: a flow of its own, and the packets
/// read so far are forwarded.
async fn handle_connection(sock: Arc<Socket>, ctx: Arc<Ctx>) {
    let mut pending: Vec<Vec<u8>> = Vec::new();
    let mut hello = None;
    let mut buf = [0u8; MAX_PACKET_LEN];
    for wait in [FIRST_PACKET_TIMEOUT, SECOND_PACKET_TIMEOUT] {
        let size = loop {
            match time::timeout(wait, sock.recv(&mut buf)).await {
                Ok(Some(0)) => continue,
                Ok(Some(size)) => break Some(size),
                Ok(None) => return,
                Err(_) => break None,
            }
        };
        let Some(size) = size else { break };
        if let Some(h) = mux::Hello::parse(&buf[..size]) {
            hello = Some(h);
            // a handshake packet before HELLO is not for the UDP target
            pending.clear();
            break;
        }
        pending.push(buf[..size].to_vec());
    }
    if hello.is_none() && pending.is_empty() {
        info!("No data from {} in time, closing connection", sock);
        return;
    }

    let flow = match hello {
        Some(h) => {
            let mut flows = ctx.flows.lock().unwrap();
            match flows.get(&h.session) {
                Some(f) if !f.quit.is_cancelled() => {
                    let now = f.add(sock.clone());
                    info!("Connection {}/{} joined flow {:016x} ({} now)", h.index + 1, h.total, h.session, now);
                    let f = f.clone();
                    drop(flows);
                    spawn_readers(&ctx, &f, sock, h.index as usize);
                    return;
                }
                _ => {
                    let f = Flow::new();
                    f.add(sock.clone());
                    flows.insert(h.session, f.clone());
                    info!("Connection {}/{} started flow {:016x}", h.index + 1, h.total, h.session);
                    (f, Some(h.session))
                }
            }
        }
        None => {
            let f = Flow::new();
            f.add(sock.clone());
            (f, None)
        }
    };
    let (flow, session) = flow;

    // the UDP side of a new flow: one ephemeral port, one socket per core
    let local_addr = match UdpSocket::bind(if ctx.remote_addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).await
        .and_then(|s| s.local_addr())
    {
        Ok(a) => a,
        Err(e) => {
            error!("Unable to bind UDP socket: {}", e);
            flow.quit.cancel();
            remove_flow(&ctx, session);
            return;
        }
    };
    let mut socks = Vec::with_capacity(ctx.num_cpus);
    for _ in 0..ctx.num_cpus {
        let s = new_udp_reuseport(local_addr);
        if let Err(e) = s.connect(ctx.remote_addr).await {
            error!("Unable to connect UDP socket to {}: {}", ctx.remote_addr, e);
            flow.quit.cancel();
            remove_flow(&ctx, session);
            return;
        }
        socks.push(Arc::new(s));
    }
    for p in &pending {
        if let Err(e) = socks[0].send(p).await {
            debug!("Unable to send UDP packet: {}", e);
        }
    }
    for s in &socks {
        mux::spawn_udp_reader(s.clone(), flow.clone());
    }
    *flow.udp.write().unwrap() = socks;
    spawn_readers(&ctx, &flow, sock, 0);

    tokio::spawn(flow.clone().peer_watch(mux::peer_timeout()));
    flow.quit.cancelled().await;
    remove_flow(&ctx, session);
    debug!("flow closed");
}

fn remove_flow(ctx: &Ctx, session: Option<u64>) {
    if let Some(s) = session {
        ctx.flows.lock().unwrap().remove(&s);
    }
}

fn spawn_readers(ctx: &Ctx, flow: &Arc<Flow>, sock: Arc<Socket>, index: usize) {
    let readers = mux::readers_per_conn(ctx.num_cpus, flow.len());
    for r in 0..readers {
        let flow = flow.clone();
        let sock = sock.clone();
        tokio::spawn(async move {
            // a joining connection may arrive before the flow has its UDP sockets
            let udp = loop {
                match flow.udp_socket(index * readers + r) {
                    Some(s) => break s,
                    None => tokio::select! {
                        _ = time::sleep(time::Duration::from_millis(20)) => {}
                        _ = flow.quit.cancelled() => return,
                    },
                }
            };
            mux::tcp_to_udp(sock.clone(), udp, flow.clone()).await;
            // a broken connection leaves the flow; the flow ends with the last one
            if flow.remove(&sock) == 0 {
                flow.quit.cancel();
            }
        });
    }
}
