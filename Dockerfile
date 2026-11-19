# trafx-phantun: phantun (dndx/phantun v0.8.1, Apache-2.0) + udpmux + iptables.
# Prebuilt static musl phantun binaries in bin/<arch>/ (see bin/SHA256SUMS).
ARG BASE_IMAGE=alpine:3.20
ARG GO_IMAGE=golang:1.24

FROM --platform=$BUILDPLATFORM ${GO_IMAGE} AS udpmux
ARG TARGETARCH
ARG TARGETVARIANT
WORKDIR /src
COPY udpmux/ .
RUN GOARM=$(echo "${TARGETVARIANT}" | tr -d v) CGO_ENABLED=0 GOOS=linux GOARCH=${TARGETARCH} \
    go build -trimpath -buildvcs=false -ldflags="-s -w" -o /udpmux .

FROM ${BASE_IMAGE}
ARG TARGETARCH
ARG ALPINE_MIRROR=
RUN if [ -n "$ALPINE_MIRROR" ]; then sed -i "s#https\?://dl-cdn.alpinelinux.org/alpine#$ALPINE_MIRROR#" /etc/apk/repositories; fi \
 && apk add --no-cache iptables iptables-legacy
COPY bin/ /tmp/bin/
RUN a="${TARGETARCH}"; [ "$a" = arm ] && a="armv7"; \
    install -m 0755 /tmp/bin/$a/phantun_client /tmp/bin/$a/phantun_server /usr/local/bin/ && rm -rf /tmp/bin
COPY --from=udpmux /udpmux /usr/local/bin/udpmux
COPY entrypoint.sh /entrypoint.sh
RUN chmod 0755 /entrypoint.sh
LABEL org.opencontainers.image.title="trafx-phantun" \
      org.opencontainers.image.description="phantun fake-TCP tunnel (client/server, N connections) for MikroTik containers and Docker" \
      org.opencontainers.image.licenses="Apache-2.0"
ENTRYPOINT ["/entrypoint.sh"]
