# Compose sandboxes

`POST /sandboxes-compose` runs an image-only Docker Compose project inside one
Firecracker sandbox. The node resolves each source image through `ImageResolver`.
Each service gets its own writable attached drive, including services using the
same image. Inside the VM, `containerd-plain-snapshotter` registers those mounted
root filesystems with Docker while preserving image configuration (entrypoint,
command, environment, user, working directory, healthcheck, and volumes).

## Enable the runtime

The initial runtime supports Linux x86-64/KVM. Build from the repository root and
publish to a registry accessible to the node:

```sh
docker build -f compose-image/Dockerfile -t REGISTRY/agentenv-compose:1 .
docker push REGISTRY/agentenv-compose:1
cd services
go build -o aenv-compose-plan ./compose/cmd
```

Install `aenv-compose-plan` on the node's PATH, or configure an absolute path:

```toml
[compose]
base_image = "REGISTRY/agentenv-compose:1"
planner_binary = "/usr/local/bin/aenv-compose-plan"
```

Environment overrides are `AENV_COMPOSE_BASE_IMAGE` and
`AENV_COMPOSE_PLANNER_BINARY`. The endpoint is disabled when no base image is set.
The server Docker image and release bundle include the planner. Extracting a
bundle alone does not add it to PATH; install it or set the absolute path above.

The dedicated image includes Docker 28.4.0, Compose 2.39.4, their containerd/runc,
and the plain snapshotter. `/init` supervises the runtime with tini. It uses an
explicit containerd socket and fails if Docker does not select `plain`. The
existing tools drive and envd are unchanged. The guest kernel must support
containers: cgroup v2, namespaces, veth, bridge, conntrack, iptables, and NAT.
Docker 28 also requires `CONFIG_IP_NF_RAW=y` (and `CONFIG_IP6_NF_RAW=y` for IPv6).
The current bundled 6.1.175 kernel lacks these options. Build a compatible kernel
as a non-root user and set its path on the Compose node:

```sh
bash compose-image/build-kernel.sh "$PWD/compose-kernel"
```

```toml
[kernel]
image_path = "/absolute/path/compose-kernel/vmlinux-compose-6.1.175"
```

The script pins the Firecracker guest configuration and Linux version, enabling
PCI support required for ACPI initialization in vanilla Linux, plus the two
raw-table options. It does not replace the host kernel or update other nodes.
Keep the resulting kernel consistent across nodes restoring the same
sandbox snapshots. Do not disable Docker's raw-table protections as a workaround.

## Create a sandbox

```sh
cat > compose.yaml <<'YAML'
services:
  web:
    image: nginx:1.27-alpine
    ports: ["8080:80"]
  redis:
    image: redis:7-alpine
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 1s
      timeout: 1s
      retries: 30
    volumes: ["data:/data"]
volumes:
  data: {}
YAML
jq -n --rawfile compose compose.yaml \
  '{compose:$compose,cpuCount:2,memoryMB:2048,timeout:600,startupTimeout:300}' \
  | curl --max-time 330 -fsS "$AENV_API_URL/sandboxes-compose" \
      -H "X-API-Key: $AENV_API_KEY" -H 'Content-Type: application/json' --data-binary @-
```

The response is the usual sandbox object and `x-agentenv-sandbox-id` header.
Access published TCP ports through the existing sandbox proxy; they are published
inside the VM. Port 49983 is reserved for envd. Services can reach one another by
Compose service name on their bridge network.

`composeEnv` supplies interpolation variables; `profiles` selects optional
services. The node does not read its process environment, `.env`, or local files.
Interpolation runs once: literal `$$` and dollars introduced by `composeEnv`
survive the guest's second Compose load. Image pulls happen on the node using
its configured registry credentials; the guest uses local aliases and
`--pull never --no-build`.

`startupTimeout` is 1–300 seconds (default 300), covering planning, image
resolution, VM boot, registration, and Compose readiness. Cleanup may extend
the response time. The gateway reserves at least 330 seconds for this route.
`timeout` is the sandbox TTL after readiness (default 300); `autoPause` defaults
to true. Compose creation enables secure envd access; use the returned token
when executing commands through envd.

The API publishes `Running` only after `docker compose up --wait` succeeds.
Services without healthchecks must be running; services with healthchecks must
be healthy. Dependencies use Compose's normal `depends_on` semantics. A failure
or deadline expiry destroys the partial sandbox through the existing lifecycle
cleanup. Guest runtime logs are under `/var/log/agentenv-compose`; normalized
Compose and source image configurations are under `/var/lib/agentenv-compose`.

## Supported scope

- One container per service, 1–24 selected services, maximum 1 MiB Compose source.
- Image references, commands, entrypoints, environment, healthchecks, dependencies,
  profiles, local named volumes, tmpfs, bridge networks, fixed published TCP ports.
- Existing sandbox pause/resume, capture, fork, and deletion. VM memory and disk
  snapshots preserve the running Docker/containerd/snapshotter processes; restore
  does not register images again or repeat Compose initialization.

Builds, replicas/scaling, host bind mounts, external files (`env_file`, `include`,
`extends`, `label_file`), secrets/configs, external networks/volumes, custom volume
drivers, host networking/devices, privileged mode, and UDP publishing are rejected.
Named volumes live in the sandbox root disk; they are not AgentENV managed volumes.

The plain snapshotter keeps active snapshot metadata in memory. Restarting
Docker/containerd/the snapshotter independently is unsupported. Recreating a
container reuses its service's writable rootfs rather than discarding its writes.
To reset the application, create a new sandbox. This initial integration depends
on the snapshotter introduced in upstream PR #318.

## Validate an installation

On an isolated test node with `aenv` installed, run:

```sh
AENV_API_URL=http://127.0.0.1:8001 AENV_API_KEY=... \
  python3 compose-image/test_e2e.py
```

The test checks same-image service isolation, health dependencies, DNS, published
ports, interpolation, pause/resume, fork, snapshot restore, and failure cleanup.
It deletes its sandboxes and snapshots and uses temporary CLI credentials.
