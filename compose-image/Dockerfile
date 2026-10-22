# syntax=docker/dockerfile:1
# Guest base image for Compose sandboxes. Build from the repository root:
#   docker build -f compose-image/Dockerfile .
# The server runs /usr/bin/env, /usr/local/bin/docker and
# /usr/local/bin/plain-snapshotter through envd and requires the plain storage
# driver; keep these in sync with src/orchestrator/compose.rs.
FROM golang:1.25.0-bookworm AS snapshotter
WORKDIR /src
COPY containerd-plain-snapshotter/go.mod containerd-plain-snapshotter/go.sum ./
RUN go mod download
COPY containerd-plain-snapshotter/ ./
RUN CGO_ENABLED=0 go build -trimpath -o /plain-snapshotter .

FROM debian:bookworm-slim
ARG TARGETARCH
ARG DOCKER_VERSION=28.4.0
ARG DOCKER_SHA256=21516934188f06d0e5f232cbde8112701f6d82899016240bc7a5d619f6b0059c
ARG COMPOSE_VERSION=2.39.4
ARG COMPOSE_SHA256=7af95166a730b87e172d4fc9aefea8725d3c6c7327d59149267b452114ddb7d4
RUN test "$TARGETARCH" = amd64 || { echo 'Compose runtime requires linux/amd64' >&2; exit 1; }
# Docker's iptables rules are validated with the legacy backend on the AgentENV guest kernel.
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl iptables iproute2 procps \
    && rm -rf /var/lib/apt/lists/* \
    && update-alternatives --set iptables /usr/sbin/iptables-legacy \
    && update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy
RUN curl -fsSL "https://download.docker.com/linux/static/stable/x86_64/docker-${DOCKER_VERSION}.tgz" \
      -o /tmp/docker.tgz \
    && echo "${DOCKER_SHA256}  /tmp/docker.tgz" | sha256sum -c - \
    && tar -xzf /tmp/docker.tgz -C /usr/local/bin --strip-components=1 \
    && rm /tmp/docker.tgz
RUN mkdir -p /usr/local/lib/docker/cli-plugins \
    && curl -fsSL "https://github.com/docker/compose/releases/download/v${COMPOSE_VERSION}/docker-compose-linux-x86_64" \
      -o /usr/local/lib/docker/cli-plugins/docker-compose \
    && echo "${COMPOSE_SHA256}  /usr/local/lib/docker/cli-plugins/docker-compose" | sha256sum -c - \
    && chmod 755 /usr/local/lib/docker/cli-plugins/docker-compose
COPY --from=snapshotter /plain-snapshotter /usr/local/bin/plain-snapshotter
COPY compose-image/containerd.toml /etc/containerd/config.toml
# Sandbox ingress targets the guest address, so Docker's raw-table direct-access
# filter adds no isolation here and would require CONFIG_IP_NF_RAW in the guest kernel.
COPY compose-image/daemon.json /etc/docker/daemon.json
# AgentENV's tools drive mounts cgroup2, starts envd and then executes /init as PID 1.
COPY --chmod=755 compose-image/init /init
CMD ["/init"]
