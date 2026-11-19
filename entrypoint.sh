#!/bin/sh
# trafx-phantun: runs phantun (https://github.com/dndx/phantun) client or server
# with the NAT rule it needs, inside its own network namespace (MikroTik
# RouterOS container or Docker). Config is env only.
#
#   PHANTUN_MODE      client | server                      (required)
#   client:
#     PHANTUN_REMOTE  server host:port (TCP)               (required)
#     PHANTUN_LISTEN  UDP listen ip:port, WireGuard endpoint [0.0.0.0:51830]
#   server:
#     PHANTUN_PORT    TCP port clients connect to          [4567]
#     PHANTUN_FORWARD UDP target ip:port (WireGuard)       (required)
#   PHANTUN_TUN       tun name                             [phantun0]
#   PHANTUN_TUN_LOCAL / PHANTUN_TUN_PEER  tun addresses    [client 192.168.200.1/.2, server 192.168.201.1/.2]
#   PHANTUN_OUT_IF    outward interface                    [interface of the default route]
#   PHANTUN_HANDSHAKE_PACKET  file sent as first packet after the handshake
#   PHANTUN_CONNECTIONS  client: fake-TCP connections; >1 puts udpmux in front [1]
#                        (the server always merges with udpmux, any number works)
#   PHANTUN_BUFFER    udpmux socket buffer bytes           [4194304]
#   RUST_LOG          [info]
set -eu
MUX_PORT=127.0.0.1:51900

log() { echo "trafx-phantun: $*"; }
die() { log "ERROR: $*"; exit 2; }

MODE="${PHANTUN_MODE:-}"
TUN="${PHANTUN_TUN:-phantun0}"
OUT_IF="${PHANTUN_OUT_IF:-$(ip -4 route show default 2>/dev/null | awk '{for(i=1;i<NF;i++) if($i=="dev"){print $(i+1); exit}}')}"
[ -n "$OUT_IF" ] || die "no default route; set PHANTUN_OUT_IF"
export RUST_LOG="${RUST_LOG:-info}"

case "$MODE" in
  client)
    REMOTE="${PHANTUN_REMOTE:-}"; [ -n "$REMOTE" ] || die "PHANTUN_REMOTE is required (server host:port)"
    LISTEN="${PHANTUN_LISTEN:-0.0.0.0:51830}"
    TUN_LOCAL="${PHANTUN_TUN_LOCAL:-192.168.200.1}"; TUN_PEER="${PHANTUN_TUN_PEER:-192.168.200.2}"
    CONNS="${PHANTUN_CONNECTIONS:-1}"
    case "$CONNS" in ''|*[!0-9]*) die "PHANTUN_CONNECTIONS must be a number" ;; esac
    [ "$CONNS" -ge 1 ] && [ "$CONNS" -le 256 ] || die "PHANTUN_CONNECTIONS must be 1..256"
    MUX=
    if [ "$CONNS" -gt 1 ]; then
      MUX="/usr/local/bin/udpmux -mode client -listen $LISTEN -upstream $MUX_PORT -n $CONNS -buffer ${PHANTUN_BUFFER:-4194304}"
      LISTEN="$MUX_PORT"
    fi
    set -- /usr/local/bin/phantun_client --local "$LISTEN" --remote "$REMOTE" -4 ;;
  server)
    PORT="${PHANTUN_PORT:-4567}"
    FORWARD="${PHANTUN_FORWARD:-}"; [ -n "$FORWARD" ] || die "PHANTUN_FORWARD is required (WireGuard ip:port)"
    TUN_LOCAL="${PHANTUN_TUN_LOCAL:-192.168.201.1}"; TUN_PEER="${PHANTUN_TUN_PEER:-192.168.201.2}"
    # udpmux merges the client's N connections into one WireGuard endpoint.
    # PHANTUN_CONNECTIONS=1 on the server skips it (no extra hop) when every
    # client uses a single connection.
    if [ "${PHANTUN_CONNECTIONS:-}" = 1 ]; then
      MUX=
      set -- /usr/local/bin/phantun_server --local "$PORT" --remote "$FORWARD" -4
    else
      MUX="/usr/local/bin/udpmux -mode server -listen $MUX_PORT -upstream $FORWARD -buffer ${PHANTUN_BUFFER:-4194304}"
      set -- /usr/local/bin/phantun_server --local "$PORT" --remote "$MUX_PORT" -4
    fi ;;
  *) die "PHANTUN_MODE must be client or server" ;;
esac
set -- "$@" --tun "$TUN" --tun-local "$TUN_LOCAL" --tun-peer "$TUN_PEER"
[ -n "${PHANTUN_HANDSHAKE_PACKET:-}" ] && set -- "$@" --handshake-packet "$PHANTUN_HANDSHAKE_PACKET"

[ -c /dev/net/tun ] || { mkdir -p /dev/net && mknod /dev/net/tun c 10 200 2>/dev/null; } || true
[ -c /dev/net/tun ] || die "/dev/net/tun missing"

{ echo 1 > /proc/sys/net/ipv4/ip_forward; } 2>/dev/null || true
[ "$(cat /proc/sys/net/ipv4/ip_forward 2>/dev/null)" = 1 ] || log "WARNING: ip_forward is off (on a Linux host: sysctl -w net.ipv4.ip_forward=1)"

# Phantun's fake TCP does not negotiate window scaling, so strict conntrack
# window tracking (which the NAT rule below needs) marks most in-flight packets
# INVALID once bytes-in-flight > 64 KB: those skip NAT and are lost. At 85 ms
# RTT that caps a tunnel at ~14 Mbit/s instead of ~210. Liberal tracking fixes
# it; it is per network namespace, so it only affects this container.
{ echo 1 > /proc/sys/net/netfilter/nf_conntrack_tcp_be_liberal; } 2>/dev/null || true
LIBERAL=$(cat /proc/sys/net/netfilter/nf_conntrack_tcp_be_liberal 2>/dev/null || echo "?")
log "conntrack tcp_be_liberal: $LIBERAL"
[ "$LIBERAL" = 1 ] || log "WARNING: tcp_be_liberal is not 1, so speed is capped on high-latency links (Docker: --sysctl net.netfilter.nf_conntrack_tcp_be_liberal=1)"

# nft iptables where available, legacy otherwise (older kernels / RouterOS).
IPT=iptables
if ! iptables -t nat -S >/dev/null 2>&1 && iptables-legacy -t nat -S >/dev/null 2>&1; then IPT=iptables-legacy; fi
TAG="-m comment --comment trafx-phantun"

nat_clean() {
  for chain in PREROUTING POSTROUTING; do
    $IPT -t nat -S "$chain" 2>/dev/null | grep -- '--comment trafx-phantun' | sed 's/^-A /-D /' \
      | while read -r rule; do eval "$IPT -t nat $rule" 2>/dev/null || true; done
  done
}
nat_clean
if [ "$MODE" = client ]; then
  eval "$IPT -t nat -A POSTROUTING -s $TUN_PEER/32 -o $OUT_IF $TAG -j MASQUERADE"
else
  eval "$IPT -t nat -A PREROUTING -i $OUT_IF -p tcp --dport $PORT $TAG -j DNAT --to-destination $TUN_PEER"
fi
log "mode=$MODE out_if=$OUT_IF tun=$TUN $TUN_LOCAL<->$TUN_PEER ($IPT)"

PID=
MPID=
stop() {
  for p in $PID $MPID; do kill "$p" 2>/dev/null; done
  for p in $PID $MPID; do wait "$p" 2>/dev/null; done
  nat_clean; exit 0
}
trap stop TERM INT
if [ -n "$MUX" ]; then
  log "exec: $MUX"
  $MUX &
  MPID=$!
fi
log "exec: $*"
"$@" &
PID=$!
set +e
# Exit (and let the restart policy restart us) when either process dies.
while kill -0 "$PID" 2>/dev/null && { [ -z "$MPID" ] || kill -0 "$MPID" 2>/dev/null; }; do
  sleep 2
done
for p in $PID $MPID; do kill "$p" 2>/dev/null; done
wait "$PID" 2>/dev/null
nat_clean
log "phantun or udpmux exited"
exit 1
