# Build from the repository root: docker build -f compose-image/Dockerfile .
FROM golang:1.25.0-bookworm AS snapshotter
WORKDIR /src
COPY containerd-plain-snapshotter/go.mod containerd-plain-snapshotter/go.sum ./
RUN go mod download
COPY containerd-plain-snapshotter/ ./
RUN CGO_ENABLED=0 go build -trimpath -o /plain-snapshotter .

FROM debian:bookworm-slim
ARG DOCKER_VERSION=28.4.0
ARG COMPOSE_VERSION=2.39.4
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl python3 iptables iproute2 util-linux procps \
    && rm -rf /var/lib/apt/lists/* \
    && update-alternatives --set iptables /usr/sbin/iptables-legacy \
    && update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy
RUN curl -fsSL "https://download.docker.com/linux/static/stable/x86_64/docker-${DOCKER_VERSION}.tgz" \
      | tar -xz -C /usr/local/bin --strip-components=1 \
    && mkdir -p /usr/local/lib/docker/cli-plugins \
    && curl -fsSL "https://github.com/docker/compose/releases/download/v${COMPOSE_VERSION}/docker-compose-linux-x86_64" \
      -o /usr/local/lib/docker/cli-plugins/docker-compose \
    && chmod 755 /usr/local/lib/docker/cli-plugins/docker-compose
COPY --from=snapshotter /plain-snapshotter /usr/local/bin/plain-snapshotter
COPY compose-image/containerd.toml /etc/containerd/config.toml
COPY compose-image/daemon.json /etc/docker/daemon.json
COPY compose-image/init /init
COPY compose-image/init.py /usr/local/bin/aenv-compose-init
COPY compose-image/start.py /usr/local/bin/aenv-compose-start
RUN chmod 755 /init /usr/local/bin/aenv-compose-init /usr/local/bin/aenv-compose-start
CMD ["/init"]
