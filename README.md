# trafx-phantun

A small container that carries WireGuard over fake TCP where UDP is throttled. It runs
[phantun](https://github.com/dndx/phantun) v0.8.1 (Apache-2.0) as a client or a server,
together with **udpmux**, which spreads one WireGuard flow over several fake-TCP
connections. It works on MikroTik RouterOS containers and on Docker.

```
VPN users ─► WireGuard (RouterOS / Linux kernel) ─UDP─► [trafx-phantun client: udpmux ─► phantun]
   ══ N fake-TCP connections to SERVER:4567 ══►
[trafx-phantun server: phantun ─► udpmux] ─UDP─► WireGuard ─► Internet
```

Phantun does not encrypt. WireGuard does. Phantun adds 12 bytes to each packet, so the
WireGuard MTU should be **1428** on a 1500-byte path.

## Build

```
scripts/fetch-phantun.sh       # downloads phantun into bin/<arch>/ and checks bin/SHA256SUMS
scripts/image.sh 0.3.1 amd64   # also arm64, armv7 -> dist/trafx-phantun-<arch>.tar
```

The tar is a classic docker-archive, so both RouterOS (`/container add file=`) and
`docker load` can load it. udpmux is built from `udpmux/` inside Docker, with its Go
modules vendored, so the build needs no module proxy. Docker Hub is closed from Iran, so
`BASE_IMAGE` and `GO_IMAGE` default to the Arvan mirror. You can override both in the
environment.

## Environment

| Variable | Mode | Default | Description |
|---|---|---|---|
| `PHANTUN_MODE` | both | — | `client` or `server` |
| `PHANTUN_REMOTE` | client | — | server `host:port` (TCP) |
| `PHANTUN_LISTEN` | client | `0.0.0.0:51830` | UDP address that WireGuard uses as its endpoint |
| `PHANTUN_CONNECTIONS` | client | `1` | number of fake-TCP connections. If it is more than 1, udpmux spreads WireGuard packets over them. **4** worked best on a 2-core CHR |
| `PHANTUN_CONNECTIONS` | server | unset | `1` turns udpmux off. Use it only when every client uses 1 connection. Unset (the default) or any other value keeps udpmux on, and it accepts any number of connections |
| `PHANTUN_PORT` | server | `4567` | TCP port that clients connect to |
| `PHANTUN_FORWARD` | server | — | UDP `ip:port` of the server WireGuard |
| `PHANTUN_BUFFER` | both | `4194304` | udpmux socket buffers in bytes (forced with SO_RCVBUFFORCE/SO_SNDBUFFORCE) |
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
  in-flight packets INVALID. Those packets skip NAT and are lost, which caps a tunnel at a
  few Mbit/s on high-latency links. In Docker, `/proc/sys` is read-only, so pass
  `--sysctl net.netfilter.nf_conntrack_tcp_be_liberal=1`. The log line
  `conntrack tcp_be_liberal: 1` confirms that the setting is on.
- If phantun or udpmux exits, the container exits, so the restart policy brings it back.

## udpmux

- **client**: reads WireGuard packets in batches (recvmmsg). It sends each batch whole to
  one of N UDP sockets, and the next batch to the next socket. Phantun opens one fake-TCP
  connection per socket. Packets keep their order: splitting a batch packet by packet
  reorders it and collapses the TCP inside WireGuard.
- **server**: merges the connections into one UDP flow towards WireGuard. Replies go
  round robin over the connections seen in the last 60 s.
- **keepalive**: the client sends a 1-byte keepalive on each connection every 10 s, so idle
  connections stay open. The server drops it.

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

**Client side** (the side that dials):

```
/ip address add address=10.250.0.1/30 interface=wg-phantun comment=trafx-phantun
/interface wireguard peers add interface=wg-phantun public-key="SERVER_WG_PUBKEY" endpoint-address=172.31.252.2 endpoint-port=51830 allowed-address=0.0.0.0/0 persistent-keepalive=25s comment=trafx-phantun
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_MODE value=client
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_REMOTE value=SERVER_PUBLIC_IP:4567
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_CONNECTIONS value=4
```

**Both sides**, to install the container:

```
/container add file=trafx-phantun-amd64.tar interface=veth-trafx-phantun envlists=ENV_TRAFX_PHANTUN root-dir=trafx-phantun-root name=trafx-phantun start-on-boot=yes restart-policy=on-failure logging=yes
# wait until the container is "stopped" (extracted), then:
/container start trafx-phantun
/log print where message~"liberal|batch|flows"
```

Where WireGuard sits does not depend on which side is the client or the server. In our
test, **reverse** gave about 10% more: the Iran router was the phantun server, listening
on 4567, and the foreign router dialed in. With reverse, the dialing (client) side does
the udpmux spreading, so it needs more CPU.

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
yet.

## Linux (Docker)

```
docker run -d --name trafx-phantun --restart unless-stopped \
  --cap-add NET_ADMIN --device /dev/net/tun -p 4567:4567 \
  --sysctl net.netfilter.nf_conntrack_tcp_be_liberal=1 \
  -e PHANTUN_MODE=server -e PHANTUN_FORWARD=172.17.0.1:51830 trafx-phantun:0.3.1-amd64
```

Give the container its own network namespace (not `--network host`), so that the liberal
conntrack setting stays inside the container.

## Results (2026-11-18/19)

These numbers are CHR to MikroTik, 82 ms RTT, measured with RouterOS btest (20 TCP
streams). btest runs on the same routers, so the numbers are lower than the real
forwarding capacity.

| Version / setup | TX Mbit/s | RX Mbit/s | CPU (client / server router) |
|---|---|---|---|
| Plain TCP, no tunnel (1 stream / 20) | 280 / 400 | — | — |
| 0.1.0 (strict conntrack in container) | 3.4 | 22 | 3 / 5 % |
| 0.1.1 (liberal conntrack), 1 connection | 70 (steady 60 s) | 53 | 20 / 32 % |
| 0.2.0, udpmux 8 connections | 75 | 81 | 68 / 70 % |
| 0.3.1, batched udpmux, 4 connections | 118 | 128 | 53–74 / 63 % |
| **0.3.1, 4 connections, reverse** | **134** | **135** | 56–64 / 87 % |

Router conntrack (`liberal-tcp-tracking=no`) was not a problem: in 30 s of traffic, it
counted 0 INVALID packets on one router and 1 on the other. MTU was not the cause either:
1280 gave the same result. On a 2-core CHR, the limit is CPU, because the tunnel, btest
and RouterOS all share it.

## Limits

- Phantun is TCP-shaped but is not TLS, so strict DPI can tell it apart: it never
  retransmits, and its payload does not look like TLS. On the links we tested, nothing
  throttled it.
- Everything runs in userspace. On weak routers, CPU sets the speed, and more
  connections use more CPU.
- The ARM images have only been started under qemu, not on a real ARM router.
