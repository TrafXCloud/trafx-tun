#!/usr/bin/env bash
# Local CPU-cost benchmark: WireGuard over trafx-phantun, two containers on one
# Docker host with netem delay between them.
#
# It reports CPU used by each trafx-phantun container (phantun, from
# its cgroup) per MB carried. CPU per MB stays meaningful on a busy or small
# host, where raw Mbit/s numbers do not.
#
# Usage: scripts/bench.sh <image> [connections] [rate_mbit] [seconds] [delay_ms]
#   e.g. scripts/bench.sh trafx-phantun:0.3.1-amd64 4 100 20 40
# delay_ms is added on each side (RTT = 2 x delay_ms).
set -euo pipefail
image="${1:?image}"; conns="${2:-1}"; rate="${3:-100}"; secs="${4:-20}"; delay="${5:-40}"
bench_img=trafx-phantun-bench:local
p=tpb
cd "$(dirname "$0")"

docker image inspect "$bench_img" >/dev/null 2>&1 || docker build -q \
  --build-arg BASE_IMAGE="${BASE_IMAGE:-docker.arvancloud.ir/library/alpine:3.20}" \
  ${ALPINE_MIRROR:+--build-arg ALPINE_MIRROR="$ALPINE_MIRROR"} \
  -t "$bench_img" bench >/dev/null

cleanup() { docker rm -f $p-cli $p-srv $p-cli-wg $p-srv-wg >/dev/null 2>&1 || true; docker network rm $p >/dev/null 2>&1 || true; }
[ -n "${KEEP:-}" ] || trap cleanup EXIT
cleanup
docker network create $p >/dev/null

WG_MTU=${WG_MTU:-1428}
# The WireGuard sidecars own the network namespaces; phantun joins them, so a
# phantun container can be restarted like on a router (WireGuard stays up).
envs=(-e RUST_LOG=${BENCH_LOG:-warn} -e PHANTUN_PEER_TIMEOUT=${PEER_TIMEOUT:-60} -e PHANTUN_RATE=${RATE:-} -e PHANTUN_UDP_BUFFER=${UDP_BUFFER:-4194304})
ks=$(docker run --rm "$bench_img" wg genkey); kc=$(docker run --rm "$bench_img" wg genkey)
ps=$(echo "$ks" | docker run --rm -i "$bench_img" wg pubkey); pc=$(echo "$kc" | docker run --rm -i "$bench_img" wg pubkey)
wgup() { # name addr key peer_pub peer_ip [endpoint]
  echo "tc qdisc add dev eth0 root netem delay ${delay}ms ${JITTER:+${JITTER}ms} ${LOSS:+loss ${LOSS}%} ${NETEM_RATE:+rate $NETEM_RATE} limit ${NETEM_LIMIT:-100000}
ip link add wg0 type wireguard
echo $3 > /k; wg set wg0 private-key /k listen-port 51820 peer $4 allowed-ips $5/32 ${6:+endpoint $6 persistent-keepalive 5}
ip addr add $2/24 dev wg0; ip link set wg0 mtu ${WG_MTU} up"
}
ns=(--cap-add NET_ADMIN --sysctl net.netfilter.nf_conntrack_tcp_be_liberal=1 --network $p)
docker run -d --name $p-srv-wg "${ns[@]}" "$bench_img" \
  sh -c "$(wgup srv 10.99.0.2 "$ks" "$pc" 10.99.0.1); exec iperf3 -s" >/dev/null
srv_ip=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$p\").IPAddress}}" $p-srv-wg)
docker run -d --name $p-cli-wg "${ns[@]}" "$bench_img" \
  sh -c "$(wgup cli 10.99.0.1 "$kc" "$ps" 10.99.0.2 127.0.0.1:51830); exec sleep infinity" >/dev/null
pt=(--cap-add NET_ADMIN --device /dev/net/tun)
docker run -d --name $p-srv "${pt[@]}" --network container:$p-srv-wg "${envs[@]}" \
  -e PHANTUN_MODE=server -e PHANTUN_FORWARD=127.0.0.1:51820 "$image" >/dev/null
docker run -d --name $p-cli "${pt[@]}" --network container:$p-cli-wg "${envs[@]}" \
  -e PHANTUN_MODE=client -e PHANTUN_REMOTE=$srv_ip:4567 -e PHANTUN_CONNECTIONS=$conns "$image" >/dev/null

for i in $(seq 1 30); do docker exec $p-cli-wg ping -c1 -W1 10.99.0.2 >/dev/null 2>&1 && break; sleep 1; done
docker exec $p-cli-wg ping -c1 -W2 10.99.0.2 >/dev/null || { docker logs $p-cli; docker logs $p-srv; echo "tunnel did not come up" >&2; exit 1; }
timeout 25 docker exec $p-cli-wg iperf3 -c 10.99.0.2 -t 3 >/dev/null 2>&1 || true   # warm-up

cg() { echo "/sys/fs/cgroup/system.slice/docker-$(docker inspect -f '{{.Id}}' "$1").scope/cpu.stat"; }
cgc=$(cg $p-cli); cgs=$(cg $p-srv)
usec() { awk -v k="$2" '$1==k{print $2}' "$1"; }

# ms/MB = CPU milliseconds per MB received; sys% = share of that CPU spent in the kernel
[ -n "${KEEP:-}" ] && { echo "KEEP: containers left running ($p-*)"; exit 0; }
printf "%-22s %8s %7s %10s %10s %6s\n" "test (conns=$conns)" "Mbit/s" "loss%" "cli ms/MB" "srv ms/MB" "sys%"
run() { # label iperf3-args...
  local label=$1; shift
  local c0 s0 k0 c1 s1 k1 j
  c0=$(usec "$cgc" usage_usec); s0=$(usec "$cgs" usage_usec)
  k0=$(( $(usec "$cgc" system_usec) + $(usec "$cgs" system_usec) ))
  j=$(timeout $((secs+20)) docker exec $p-cli-wg iperf3 -c 10.99.0.2 -t "$secs" -J "$@") || { echo "$label: iperf3 hung or failed (tunnel stalled?)"; return; }
  c1=$(usec "$cgc" usage_usec); s1=$(usec "$cgs" usage_usec)
  k1=$(( $(usec "$cgc" system_usec) + $(usec "$cgs" system_usec) ))
  echo "$j" | docker exec -i $p-cli-wg jq -r --arg l "$label" --argjson c $((c1-c0)) --argjson s $((s1-s0)) --argjson k $((k1-k0)) '
    (.end.sum_received // .end.sum) as $r
    | ($r.bytes / 1e6) as $mb
    | [$l, ($r.bits_per_second/1e6|floor), ((.end.sum.lost_percent // 0)*10|floor/10),
       ($c/1000/$mb*100|floor/100), ($s/1000/$mb*100|floor/100), ($k*100/($c+$s)|floor)] | @tsv' |
    awk -F'\t' '{printf "%-22s %8s %7s %10s %10s %6s\n",$1,$2,$3,$4,$5,$6}'
}
run "udp ${rate}M up"   -u -b "${rate}M" -l 1300
run "udp ${rate}M down" -u -b "${rate}M" -l 1300 -R
run "tcp up (4 streams)"   -P 4
run "tcp down (4 streams)" -P 4 -R
for c in cli srv; do echo "$c UdpRcvbufErrors: $(docker exec $p-$c-wg nstat -az UdpRcvbufErrors | awk '/UdpRcvbufErrors/{print $2}')"; done
[ -n "${BENCH_LOG:-}" ] && { echo "--- server log"; docker logs $p-srv 2>&1 | grep -v -i "sent\|established" | tail -15; }
