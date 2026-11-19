# trafx-phantun

A small container that carries WireGuard over fake TCP where UDP is throttled. It runs a
fork of [phantun](https://github.com/dndx/phantun) v0.8.1 (MIT OR Apache-2.0, source in
[`phantun/`](phantun/)) as a client or a server, on MikroTik RouterOS containers and on
Docker. The fork spreads one WireGuard flow over several fake-TCP connections by itself.

```
VPN users ─► WireGuard (RouterOS / Linux kernel) ─UDP─► [trafx-phantun client]
   ══ N fake-TCP connections to SERVER:4567 ══►
[trafx-phantun server] ─UDP─► WireGuard ─► Internet
```

Phantun does not encrypt. WireGuard does. A fake-TCP packet is 40 bytes of IP + TCP header
around one WireGuard datagram, so the WireGuard MTU must be **1428** (or lower) on a
1500-byte path.

**Version 0.4.0.** Both sides must run 0.4.x. 0.3.x (udpmux) and 0.4.x do not work
together when the client uses more than one connection.

## What the fork adds to phantun

All TrafX code is in [`phantun/phantun/src/mux.rs`](phantun/phantun/src/mux.rs),
[`pacer.rs`](phantun/phantun/src/pacer.rs) and [`cc.rs`](phantun/phantun/src/cc.rs);
[`phantun/UPSTREAM`](phantun/UPSTREAM) records the upstream commit.

- **N connections per flow** (`--connections N`). The client opens N fake-TCP connections
  for one WireGuard peer and sends a HELLO with a session id on each, also for N = 1. The
  server groups connections by session id behind one UDP socket, so WireGuard sees one
  stable endpoint. Datagrams go out in bursts of 32 per connection, then the next one:
  sending each datagram on a different connection reorders them and slows TCP inside the
  tunnel. A connection without HELLO is an upstream phantun client and works as before.
- **Fast UDP reader.** RouterOS containers refuse `SO_RCVBUFFORCE`, so the UDP receive
  buffer stays at ~416 KB (about 180 WireGuard packets). A dedicated OS thread per socket
  drains it with `recvmmsg` (64 per call), and one writer task per connection writes to
  the tun device, so that work spreads over the cores.
- **Stale flows close by themselves.** A flow closes when nothing arrived from the other
  side for `PHANTUN_PEER_TIMEOUT` seconds (default 60; WireGuard keepalives every 25 s keep
  a live tunnel above it). A client that restarts, reboots or changes its connection count
  never leaves the server in a broken state.
- **Pacing** (`PHANTUN_RATE`, off by default). Without it, fake TCP leaves in bursts at line
  rate; shallow queues on the path and the small receive buffer drop parts of those
  bursts, and TCP inside the tunnel starts fast, then keeps slowing down. A token bucket
  just below what the path carries turns those drops into a little queueing delay.
  RouterOS containers offer no qdisc at all (`tc` fails for every kind, even `pfifo`), so
  phantun paces in user space. The queue in front of the pacer holds at most 50 ms; what
  does not fit is dropped there.
- **Automatic pacing rate** (`PHANTUN_RATE=auto`), **experimental**. The two sides exchange
  a small PROBE/ACK every 50 ms (sent and received counters, round trip time) and the
  ceiling follows the best delivery rate of the last 10 s (BBR-style max filter). It works
  in steady state, but after a peer restart it can drop traffic for a few seconds, so it
  is not for production yet.
- **Drop counters in the log.** Every 60 s, when they grew: `tun_dropped` (kernel could not
  queue to phantun) and `udp_rcvbuf_errors` (receive buffer overflow). Loss counted there is
  inside the container, not on the network.

## Build

```
scripts/build-phantun.sh amd64        # Rust in Docker -> bin/amd64/phantun_{client,server}
scripts/image.sh 0.4.0 amd64          # -> dist/trafx-phantun-amd64.tar
```

`build-phantun.sh` also takes `arm64` and `armv7`; those build under QEMU (slow) and have
not been run on a real ARM router yet. The tar is a classic docker-archive, so both RouterOS
(`/container add file=`) and `docker load` can load it. Docker Hub is closed from Iran, so
`BASE_IMAGE` and `RUST_IMAGE` default to the Arvan mirror; override them in the environment.
`OUT_VERSION=x` puts the version into the tar name, `NO_BUILD=1` packs an existing image.

## Environment

| Variable | Mode | Default | Description |
|---|---|---|---|
| `PHANTUN_MODE` | both | — | `client` or `server` |
| `PHANTUN_REMOTE` | client | — | server `host:port` (TCP) |
| `PHANTUN_LISTEN` | client | `0.0.0.0:51830` | UDP address that WireGuard uses as its endpoint |
| `PHANTUN_CONNECTIONS` | client | `1` | fake-TCP connections per WireGuard flow, 1..64. **16** gave the best results on the test routers. The server needs no setting |
| `PHANTUN_PORT` | server | `4567` | TCP port that clients connect to |
| `PHANTUN_FORWARD` | server | — | UDP `ip:port` of the server WireGuard |
| `PHANTUN_RATE` | both | off | pacing of what this side sends: unset/`off`, a fixed rate such as `150mbit` (also `150M`, `1.5gbit`), or `auto` (experimental). Set it on the side that sends the direction you want to steady |
| `PHANTUN_PEER_TIMEOUT` | both | `60` | seconds without a packet from the other side before a flow closes |
| `PHANTUN_UDP_BUFFER` | both | `4194304` | UDP socket buffers in bytes (RouterOS caps the receive side at ~416 KB anyway) |
| `PHANTUN_TXQUEUELEN` | both | `10000` | tun queue length (RouterOS does not allow it; stays 500) |
| `PHANTUN_TUN` | both | `phantun0` | name of the tun interface |
| `PHANTUN_TUN_LOCAL` / `PHANTUN_TUN_PEER` | both | client `192.168.200.1/.2`, server `192.168.201.1/.2` | tun addresses |
| `PHANTUN_OUT_IF` | both | interface of the default route | outward interface |
| `PHANTUN_HANDSHAKE_PACKET` | both | — | file sent as the first packet after the handshake |
| `RUST_LOG` | both | `info` | phantun log level |

What the entrypoint does:

- It adds `MASQUERADE` (client) or `DNAT` (server), tagged with the comment
  `trafx-phantun`, and removes it again on stop. It uses nft iptables and falls back to
  legacy iptables.
- It sets `net.netfilter.nf_conntrack_tcp_be_liberal=1` in the container's own network
  namespace. Phantun does not negotiate TCP window scaling, so strict conntrack marks
  in-flight packets INVALID; those skip NAT and are lost, which caps a tunnel at a few
  Mbit/s on high-latency links. In Docker `/proc/sys` is read-only: pass
  `--sysctl net.netfilter.nf_conntrack_tcp_be_liberal=1`. The log line
  `conntrack tcp_be_liberal: 1` confirms it.
- It tries to raise `net.core.rmem_max` and the tun queue; on RouterOS neither is allowed,
  and the log says so (`rmem_max: ?`, `txqueuelen 500`, `SO_RCVBUFFORCE refused`).
- If phantun exits, the container exits, and the restart policy brings it back.

Useful log lines: `N fake TCP connection(s) per UDP flow` (client), `Connection i/N joined
flow …` (server), `pacing: …`, `drops in container (total): …`, `Nothing from the peer in
the last 60s, closing flow`.

## MikroTik

All objects are commented `trafx-phantun`. Both sides use the same container network:

```
/interface bridge add name=br-trafx-phantun comment=trafx-phantun
/ip address add address=172.31.252.1/24 interface=br-trafx-phantun comment=trafx-phantun
/interface veth add name=veth-trafx-phantun address=172.31.252.2/24 gateway=172.31.252.1 comment=trafx-phantun
/interface bridge port add bridge=br-trafx-phantun interface=veth-trafx-phantun comment=trafx-phantun
/ip firewall nat add chain=srcnat action=masquerade src-address=172.31.252.0/24 comment=trafx-phantun
/interface wireguard add name=wg-phantun mtu=1428 listen-port=13240 comment=trafx-phantun
```

**Server side** (the side that is reached on TCP 4567):

```
/ip address add address=10.250.0.2/30 interface=wg-phantun comment=trafx-phantun
/interface wireguard peers add interface=wg-phantun public-key="CLIENT_WG_PUBKEY" allowed-address=10.250.0.1/32 comment=trafx-phantun
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_MODE value=server
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_FORWARD value=172.31.252.1:13240
/ip firewall nat add chain=dstnat action=dst-nat protocol=tcp dst-address=PUBLIC_IP dst-port=4567 to-addresses=172.31.252.2 to-ports=4567 comment=trafx-phantun
```

The server-side WireGuard peer needs no endpoint: it learns it from the first packet.

**Client side** (the side that dials):

```
/ip address add address=10.250.0.1/30 interface=wg-phantun comment=trafx-phantun
/interface wireguard peers add interface=wg-phantun public-key="SERVER_WG_PUBKEY" endpoint-address=172.31.252.2 endpoint-port=51830 allowed-address=0.0.0.0/0 persistent-keepalive=25s comment=trafx-phantun
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_MODE value=client
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_REMOTE value=SERVER_PUBLIC_IP:4567
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_CONNECTIONS value=16
```

Keep `persistent-keepalive=25s`: it is what keeps an idle flow under the 60 s peer timeout.
The client opens its connections only when the first WireGuard packet arrives, so 2–25 s
between container start and `Connection … established` is normal.

**Both sides**, to install the container:

```
/container add file=trafx-phantun-amd64.tar interface=veth-trafx-phantun envlists=ENV_TRAFX_PHANTUN root-dir=trafx-phantun-root name=trafx-phantun start-on-boot=yes restart-policy=on-failure logging=yes
# wait until the container is "stopped" (extracted), then:
/container start trafx-phantun
/log print where message~"trafx-phantun|flow|pacing"
```

Optional pacing, on the side that sends the direction to steady (example: Iran → abroad,
set on the Iran router; choose ~60% of a plain `/tool bandwidth-test` at the same hour):

```
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_RATE value=150mbit
```

Environment changes take effect after `/container stop`, waiting for `stopped`, and
`/container start`.

Which side is client or server does not matter for WireGuard. In our tests **reverse**
(the Iran router as phantun server listening on 4567, the foreign router dialing in) gave
about 10% more with 0.3.1.

To send users through the tunnel, run this on the side where the users are. The WireGuard
peer there needs `allowed-address=0.0.0.0/0`:

```
/ip firewall nat add chain=srcnat action=masquerade out-interface=wg-phantun comment=trafx-phantun
/ip firewall mangle add chain=forward protocol=tcp tcp-flags=syn out-interface=wg-phantun action=change-mss new-mss=clamp-to-pmtu comment=trafx-phantun
/routing table add name=to-phantun fib
/ip route add dst-address=0.0.0.0/0 gateway=wg-phantun routing-table=to-phantun
/routing rule add src-address=USER_POOL action=lookup table=to-phantun
```

Never route the container subnet (`172.31.252.0/24`) into `to-phantun`. If you do, the
tunnel loops into itself.

To update, run `/container stop`, wait for `stopped`, run `/container remove`, and check
that `/container print` no longer shows it. Then upload the new tar, run `add`, wait for
`stopped`, and run `start`. If you see `root-dir overlap`, the old container is not gone
yet. Update both sides.

## Linux (Docker)

```
docker run -d --name trafx-phantun --restart unless-stopped \
  --cap-add NET_ADMIN --device /dev/net/tun -p 4567:4567 \
  --sysctl net.netfilter.nf_conntrack_tcp_be_liberal=1 \
  -e PHANTUN_MODE=server -e PHANTUN_FORWARD=172.17.0.1:51830 trafx-phantun:0.4.0-amd64
```

Give the container its own network namespace (not `--network host`), so that the liberal
conntrack setting stays inside the container.

## Tests

- `scripts/bench.sh <image> [connections] [rate_mbit] [seconds] [delay_ms]`: WireGuard over
  trafx-phantun between two containers with netem delay; prints Mbit/s, loss and CPU ms per
  MB of each side (from the cgroup), which stays meaningful on a busy host. Knobs: `JITTER`,
  `LOSS`, `NETEM_RATE`, `NETEM_LIMIT` (counts delayed packets too: keep it above the
  bandwidth-delay product), `UDP_BUFFER` (212992 = RouterOS-like), `RATE`, `KEEP=1` (leave
  the containers running).
- `scripts/robustness.sh <image> [connections] [soak_s]`: fresh start, client restart,
  client N → 1 → N, server restart, client gone longer than the peer timeout, soak. Each step
  checks UDP loss both ways and TCP. 0.4.0 passes all steps (also with `RATE=200mbit`).

## Results (2026-11-18/19)

CHR (Iran) ↔ MikroTik (abroad), 82 ms RTT, RouterOS btest from the Iran router, 20 TCP
streams, reverse mode. btest runs on the same routers, so the numbers are lower than the
real forwarding capacity. Daytime paths change a lot within minutes: compare only tests
run back to back, and always measure the plain path (`/tool bandwidth-test` to the other
router's public IP) at the same time.

| Version / setup | TX Mbit/s | RX Mbit/s | CPU (Iran / abroad) |
|---|---|---|---|
| 0.1.0 (strict conntrack in container) | 3.4 | 22 | 3 / 5 % |
| 0.1.1 (liberal conntrack), 1 connection | 70 | 53 | 20 / 32 % |
| 0.3.1, udpmux 4 connections, reverse (night) | 134 | 135 | 56–64 / 87 % |
| 0.4.0, 16 connections, no pacing (day) | 95–108 | 60–94 | 40–65 / 40–60 % |
| 0.4.0, 16 connections, `PHANTUN_RATE=150mbit` on the Iran side (day) | **120, steady** | 51 | 62 / 70 % |
| plain path at that moment (day) | 277 | 49 | — |
| UDP btest through the tunnel, 250M each way at once | 242 | 230 | 86 / 84 % |

- The tunnel itself carries far more than TCP gets through it (UDP: 240 + 230 at once).
  What limits TCP is loss: fake TCP has no congestion control and no retransmission, so
  every drop on the path or in the receive buffer reaches TCP inside the tunnel.
- With a fixed pacing rate below the path capacity, TX stayed flat for 60 s instead of
  starting fast and sinking. In the abroad → Iran direction the tunnel matched the plain
  path, which was the limit at that hour.
- Router conntrack (`liberal-tcp-tracking=no`) was not a problem, nor MTU.

## Limits

- Phantun is TCP-shaped but is not TLS, so strict DPI can tell it apart: it never
  retransmits, and its payload does not look like TLS.
- Everything runs in user space. On weak routers CPU sets the speed.
- RouterOS containers: no qdisc, no `SO_RCVBUFFORCE`, no tun queue length. The fork works
  around the first two; the third stays at 500.
- `PHANTUN_RATE=auto` is experimental (see above). Fixed pacing needs a rate below the
  path capacity, which changes during the day.
- The ARM images have not been run on a real ARM router.
