#!/usr/bin/python3
"""Register prepared drives, then start one Compose project under a deadline."""
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

WORKDIR = Path("/var/lib/agentenv-compose")
ENV = {
    "PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "HOME": "/root",
    "DOCKER_HOST": "unix:///var/run/docker.sock",
    "COMPOSE_DISABLE_ENV_FILE": "1",
    "COMPOSE_ANSI": "never",
}


def remaining(deadline):
    seconds = deadline - time.monotonic()
    if seconds <= 0:
        raise TimeoutError("Compose startup deadline exceeded")
    return seconds


def run(args, deadline, check=True, env=None):
    args = [shutil.which(args[0], path=ENV["PATH"]) or args[0], *args[1:]]
    result = subprocess.run(args, cwd=WORKDIR, env=ENV if env is None else env, timeout=remaining(deadline),
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if check and result.returncode:
        raise RuntimeError(f"{args[0]} failed: {result.stderr[-8192:]}")
    return result


def compose_environment(document):
    # Null environment entries mean "unset" when absent from composeEnv.
    # Do not let a second Compose load inherit HOME/PATH from the runtime.
    env = ENV.copy()
    for service in document["services"].values():
        for key, value in service.get("environment", {}).items():
            if value is None:
                env.pop(key, None)
    return env


def validate_drives(services):
    devices = set()
    for index, service in enumerate(services):
        mount = f"/mnt/compose_{index}"
        if (service["driveID"] != f"compose_{index}" or service["mountPath"] != mount
                or service["localImage"] != f"aenv-compose/service-{index}:local"):
            raise ValueError("invalid service drive mapping")
        path = Path(mount)
        if path.is_symlink() or not os.path.ismount(path):
            raise ValueError(f"service drive is not mounted: {mount}")
        device = path.stat().st_dev
        if device == Path("/").stat().st_dev or device in devices:
            raise ValueError(f"service drive is not isolated: {mount}")
        devices.add(device)
        if not isinstance(service["config"], dict):
            raise ValueError("source image config is missing")


def start(plan, deadline):
    services = plan["services"]
    if not 1 <= len(services) <= 24:
        raise ValueError("expected 1..24 services")
    validate_drives(services)
    last_error = "Docker did not become ready"
    while True:
        info = run(["docker", "info", "--format", "{{json .}}"], deadline, check=False)
        if info.returncode == 0:
            info = json.loads(info.stdout)
            if info.get("Driver") != "plain":
                raise RuntimeError("Docker must use the plain snapshotter")
            break
        last_error = info.stderr[-2048:]
        if remaining(deadline) < 0.2:
            raise TimeoutError(last_error)
        time.sleep(0.2)
    # register updates a shared config file; calls MUST remain serial.
    for service in services:
        metadata = WORKDIR / (service["driveID"] + ".json")
        metadata.write_text(json.dumps(service["config"]))
        run(["plain-snapshotter", "register", "--image-metadata", str(metadata),
             service["localImage"], service["mountPath"]], deadline)
    compose_file = WORKDIR / "compose.json"
    compose_file.write_text(json.dumps(plan["compose"]))
    run(["docker", "compose", "--project-name", "aenv", "--env-file", "/dev/null",
         "--file", str(compose_file), "up", "--detach", "--no-build", "--pull", "never",
         "--wait", "--wait-timeout", str(max(1, math.ceil(remaining(deadline))))], deadline,
        env=compose_environment(plan["compose"]))


if __name__ == "__main__":
    try:
        os.umask(0o077)
        WORKDIR.mkdir(parents=True, exist_ok=True)
        deadline = time.monotonic() + float(sys.argv[1])
        # A single newline-framed JSON document: envd stdin does not need EOF.
        start(json.loads(sys.stdin.readline(4 * 1024 * 1024)), deadline)
    except Exception as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
