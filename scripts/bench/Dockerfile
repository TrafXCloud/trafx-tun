# Helper image for scripts/bench.sh: WireGuard + iperf3 + tc, run as a sidecar
# that shares the network namespace of a trafx-phantun container.
ARG BASE_IMAGE=alpine:3.20
FROM ${BASE_IMAGE}
ARG ALPINE_MIRROR=
RUN if [ -n "$ALPINE_MIRROR" ]; then sed -i "s#https\?://dl-cdn.alpinelinux.org/alpine#$ALPINE_MIRROR#" /etc/apk/repositories; fi \
 && apk add --no-cache wireguard-tools-wg iperf3 iproute2 iproute2-tc jq
