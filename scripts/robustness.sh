#!/usr/bin/env bash
# Robustness test on one Docker host: the situations a router meets in real
# life, each followed by a traffic check. Exits non-zero on the first failure.
#
#   1. fresh start, N connections
#   2. client restart (same N)
#   3. client N -> 1 -> N without touching the server (stale flows)
#   4. server restart
#   5. client gone longer than the peer timeout, then back
#   6. soak: TCP for SOAK seconds
#
# Usage: scripts/robustness.sh <image> [connections] [soak_seconds]
set -euo pipefail
image="${1:?image}"; conns="${2:-16}"; soak="${3:-120}"
cd "$(dirname "$0")"
p=tpb
peer_timeout=15   # short, so step 5 does not take minutes
export PEER_TIMEOUT=$peer_timeout BENCH_LOG=info UDP_BUFFER=${UDP_BUFFER:-212992}  # RouterOS-like cap
KEEP=1 ./bench.sh "$image" "$conns" 30 5 40 >/dev/null
trap 'docker rm -f $p-cli $p-srv $p-cli-wg $p-srv-wg >/dev/null 2>&1; docker network rm $p >/dev/null 2>&1' EXIT
srv_ip=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$p\").IPAddress}}" $p-srv-wg)

fail() { echo "FAIL: $*"; echo "--- server log"; docker logs $p-srv 2>&1 | tail -20; echo "--- client log"; docker logs $p-cli 2>&1 | tail -20; exit 1; }
client() { # (re)create the client with N connections
  docker rm -f $p-cli >/dev/null
  docker run -d --name $p-cli --cap-add NET_ADMIN --device /dev/net/tun --network container:$p-cli-wg \
    -e RUST_LOG=info -e PHANTUN_PEER_TIMEOUT=$peer_timeout -e PHANTUN_UDP_BUFFER=$UDP_BUFFER -e PHANTUN_RATE=${RATE:-} \
    -e PHANTUN_MODE=client -e PHANTUN_REMOTE=$srv_ip:4567 -e PHANTUN_CONNECTIONS=$1 "$image" >/dev/null
}
up() { # seconds until ping works through the tunnel
  for i in $(seq 1 40); do docker exec $p-cli-wg ping -c1 -W1 10.99.0.2 >/dev/null 2>&1 && { echo "$i"; return 0; }; done
  return 1
}
check() { # label: UDP both directions (loss) + TCP
  local label=$1 t
  t=$(up) || fail "$label: tunnel did not come up in 40 s"
  local u d tcp
  u=$(timeout 20 docker exec $p-cli-wg iperf3 -c 10.99.0.2 -u -b 30M -t 5 -l 1300 -J | docker exec -i $p-cli-wg jq -r '.end.sum.lost_percent') || fail "$label: UDP up"
  d=$(timeout 20 docker exec $p-cli-wg iperf3 -c 10.99.0.2 -u -b 30M -t 5 -l 1300 -R -J | docker exec -i $p-cli-wg jq -r '.end.sum.lost_percent') || fail "$label: UDP down"
  tcp=$(timeout 30 docker exec $p-cli-wg iperf3 -c 10.99.0.2 -t 8 -P 4 -J | docker exec -i $p-cli-wg jq -r '.end.sum_received.bits_per_second/1e6|floor') || fail "$label: TCP"
  printf "%-34s up after %2ss  udp loss up %5.2f%% down %5.2f%%  tcp %s Mbit\n" "$label" "$t" "$u" "$d" "$tcp"
  awk -v u="$u" -v d="$d" 'BEGIN{exit !(u<2 && d<2)}' || fail "$label: UDP loss too high"
}

check "1. fresh, $conns conns"
docker restart -t 2 $p-cli >/dev/null;           check "2. client restart"
client 1;                                         check "3a. client -> 1 conn"
client "$conns";                                  check "3b. client -> $conns conns again"
docker restart -t 2 $p-srv >/dev/null;           check "4. server restart"
docker stop -t 2 $p-cli >/dev/null; sleep $((peer_timeout * 2 + 5))
srvlog=$(docker logs $p-srv 2>&1); grep -q "Nothing from the peer" <<<"$srvlog" || fail "5. server kept the flow of a vanished client"
docker start $p-cli >/dev/null;                   check "5. client gone $((peer_timeout * 2 + 5)) s, back"
t=$(timeout $((soak + 30)) docker exec $p-cli-wg iperf3 -c 10.99.0.2 -t "$soak" -P 4 -J | docker exec -i $p-cli-wg jq -r '.end.sum_received.bits_per_second/1e6|floor') || fail "6. soak"
echo "6. soak ${soak}s                         tcp $t Mbit"
echo "--- drops"; docker logs $p-cli 2>&1 | grep "drops in container" | tail -1; docker logs $p-srv 2>&1 | grep "drops in container" | tail -1
echo "ALL PASSED"
