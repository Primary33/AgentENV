#!/usr/bin/python3
"""Supervise the Compose runtime; tini reaps children and tools supervise envd.

Do not restart the snapshotter: its active snapshot metadata is in memory.
VM snapshots preserve these processes; service restarts are unsupported.
"""
import os
from pathlib import Path
import signal
import subprocess
import time


def main():
    os.umask(0o077)
    os.environ["PATH"] = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    Path("/var/log/agentenv-compose").mkdir(parents=True, exist_ok=True)
    Path("/sys/fs/cgroup").mkdir(parents=True, exist_ok=True)
    if not os.path.ismount("/sys/fs/cgroup"):
        subprocess.run(["mount", "-t", "cgroup2", "none", "/sys/fs/cgroup"], check=True)
    children = []

    def stop(*_):
        for child in reversed(children):
            child.terminate()
        raise SystemExit(1)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    for name, command, socket in [
        ("snapshotter", ["plain-snapshotter"], "/run/containerd-plain-snapshotter/snapshotter.sock"),
        ("containerd", ["containerd", "--config", "/etc/containerd/config.toml"], "/run/containerd/containerd.sock"),
        ("docker", ["dockerd", "--config-file", "/etc/docker/daemon.json"], "/var/run/docker.sock"),
    ]:
        with open(f"/var/log/agentenv-compose/{name}.log", "ab", buffering=0) as log:
            children.append(subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT))
        deadline = time.monotonic() + 60
        while not Path(socket).exists():
            if any(child.poll() is not None for child in children) or time.monotonic() > deadline:
                stop()
            time.sleep(0.1)
    while True:
        if any(child.poll() is not None for child in children):
            stop()
        time.sleep(1)


if __name__ == "__main__":
    main()
