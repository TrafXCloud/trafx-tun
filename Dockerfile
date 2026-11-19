# trafx-phantun: phantun (fork of dndx/phantun v0.8.1, MIT OR Apache-2.0, source in phantun/) + iptables.
# Static musl phantun binaries in bin/<arch>/, built by scripts/build-phantun.sh.
ARG BASE_IMAGE=alpine:3.20

FROM ${BASE_IMAGE}
ARG TARGETARCH
ARG ALPINE_MIRROR=
RUN if [ -n "$ALPINE_MIRROR" ]; then sed -i "s#https\?://dl-cdn.alpinelinux.org/alpine#$ALPINE_MIRROR#" /etc/apk/repositories; fi \
 && apk add --no-cache iptables iptables-legacy
COPY bin/ /tmp/bin/
RUN a="${TARGETARCH}"; [ "$a" = arm ] && a="armv7"; \
    install -m 0755 /tmp/bin/$a/phantun_client /tmp/bin/$a/phantun_server /usr/local/bin/ && rm -rf /tmp/bin
COPY entrypoint.sh /entrypoint.sh
RUN chmod 0755 /entrypoint.sh
LABEL org.opencontainers.image.title="trafx-phantun" \
      org.opencontainers.image.description="phantun fake-TCP tunnel (client/server, N connections) for MikroTik containers and Docker" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
ENTRYPOINT ["/entrypoint.sh"]
