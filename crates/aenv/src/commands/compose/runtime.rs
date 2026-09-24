//! Guest-only Compose startup and daemon supervision.
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout, timeout_at, Instant};

use serde::{Deserialize, Serialize};

/// Maximum complete startup frame, including image configurations and newline.
pub const MAX_PLAN_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
pub struct ComposeService {
    pub name: String,
    pub image: String,
    #[serde(rename = "localImage")]
    pub local_image: String,
    #[serde(rename = "driveID")]
    pub drive_id: String,
    #[serde(rename = "mountPath")]
    pub mount_path: String,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ComposePlan {
    pub compose: serde_json::Value,
    pub services: Vec<ComposeService>,
}

const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const WORKDIR: &str = "/var/lib/agentenv-compose";

async fn run_command(command: &mut Command, check: bool) -> Result<Output> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let mut output = command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("run {program}"))?;
    output
        .stderr
        .drain(..output.stderr.len().saturating_sub(8192));
    if check && !output.status.success() {
        bail!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

async fn until_signal<F, T>(future: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::select! {
        result = future => result,
        _ = term.recv() => bail!("received SIGTERM"),
        _ = interrupt.recv() => bail!("received SIGINT"),
    }
}

fn mountpoints() -> Result<HashSet<PathBuf>> {
    Ok(fs::read_to_string("/proc/self/mountinfo")?
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        // Only fixed paths without mountinfo escapes are queried by this runtime.
        .map(PathBuf::from)
        .collect())
}

fn environment(document: &Value) -> BTreeMap<String, String> {
    let mut environment: BTreeMap<_, _> = [
        ("PATH", PATH),
        ("HOME", "/root"),
        ("DOCKER_HOST", "unix:///var/run/docker.sock"),
        ("COMPOSE_ANSI", "never"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    if let Some(services) = document.get("services").and_then(Value::as_object) {
        for service in services.values() {
            if let Some(variables) = service.get("environment").and_then(Value::as_object) {
                for (key, value) in variables {
                    if value.is_null() {
                        environment.remove(key);
                    }
                }
            }
        }
    }
    environment
}

fn validate(
    plan: &ComposePlan,
    mounts: &HashSet<PathBuf>,
    device: impl Fn(&Path) -> Result<u64>,
) -> Result<()> {
    anyhow::ensure!(
        plan.compose.get("services").is_some_and(Value::is_object),
        "Compose services must be an object"
    );
    let root_device = device(Path::new("/"))?;
    let mut devices = HashSet::new();
    for service in &plan.services {
        let mount = &service.mount_path;
        let path = Path::new(mount);
        anyhow::ensure!(
            mounts.contains(path),
            "service drive is not mounted: {mount}"
        );
        let dev = device(path)?;
        anyhow::ensure!(
            dev != root_device && devices.insert(dev),
            "service drive is not isolated: {mount}"
        );
        anyhow::ensure!(service.config.is_object(), "source image config is missing");
    }
    Ok(())
}

fn device(path: &Path) -> Result<u64> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "service drive is not a directory mount: {}",
        path.display()
    );
    Ok(metadata.dev())
}

fn command(program: &str, environment: &BTreeMap<String, String>) -> Command {
    // Resolve independently of the sanitized environment: a null service PATH
    // must stay absent during Compose's second interpolation pass.
    let mut command = Command::new(format!("/usr/local/bin/{program}"));
    command.current_dir(WORKDIR).env_clear().envs(environment);
    command
}

fn write_private_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    Ok(())
}

async fn read_plan(reader: impl AsyncRead + Unpin) -> Result<ComposePlan> {
    let mut reader = BufReader::new(reader.take((MAX_PLAN_BYTES + 1) as u64));
    let mut input = Vec::new();
    // A newline completes the frame; envd's stdin stream does not need EOF.
    reader.read_until(b'\n', &mut input).await?;
    anyhow::ensure!(input.len() <= MAX_PLAN_BYTES, "Compose plan exceeds 4 MiB");
    serde_json::from_slice(&input).context("invalid Compose startup plan")
}

async fn launch(plan: ComposePlan, deadline: Instant) -> Result<()> {
    validate(&plan, &mountpoints()?, device)?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(WORKDIR)?;
    let env = environment(&Value::Null);
    loop {
        let output = run_command(
            command("docker", &env).args(["info", "--format", "{{json .}}"]),
            false,
        )
        .await?;
        if output.status.success() {
            let info: Value =
                serde_json::from_slice(&output.stdout).context("decode Docker info")?;
            anyhow::ensure!(
                info.get("Driver").and_then(Value::as_str) == Some("plain"),
                "Docker must use the plain snapshotter"
            );
            break;
        }
        anyhow::ensure!(
            deadline.saturating_duration_since(Instant::now()) > Duration::from_millis(200),
            "Docker did not become ready: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        sleep(Duration::from_millis(200)).await;
    }
    // Registration updates shared configuration and must remain serial.
    for service in &plan.services {
        let metadata = Path::new(WORKDIR).join(format!("{}.json", service.drive_id));
        write_private_json(&metadata, &service.config)?;
        run_command(
            command("plain-snapshotter", &env)
                .args(["register", "--image-metadata"])
                .arg(metadata)
                .args([&service.local_image, &service.mount_path]),
            true,
        )
        .await?;
    }
    let compose_file = Path::new(WORKDIR).join("compose.json");
    write_private_json(&compose_file, &plan.compose)?;
    let wait_seconds = deadline
        .saturating_duration_since(Instant::now())
        .as_secs_f64()
        .ceil()
        .max(1.0)
        .to_string();
    run_command(
        command("docker", &environment(&plan.compose))
            .args([
                "compose",
                "--project-name",
                "aenv",
                "--env-file",
                "/dev/null",
                "--file",
            ])
            .arg(compose_file)
            .args([
                "up",
                "--detach",
                "--no-build",
                "--pull",
                "never",
                "--wait",
                "--wait-timeout",
                &wait_seconds,
            ]),
        true,
    )
    .await?;
    Ok(())
}

/// Entry point of aenv compose guest-start.
pub async fn start(reader: impl AsyncRead + Unpin, budget: Duration) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(budget)
        .context("invalid startup timeout")?;
    until_signal(async {
        timeout_at(deadline, async {
            launch(read_plan(reader).await?, deadline).await
        })
        .await
        .context("Compose startup deadline exceeded")?
    })
    .await
}

async fn supervise_daemons(children: &mut Vec<(&str, Child)>) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create("/var/log/agentenv-compose")?;
    fs::create_dir_all("/sys/fs/cgroup")?;
    if !mountpoints()?.contains(Path::new("/sys/fs/cgroup")) {
        timeout(
            Duration::from_secs(60),
            run_command(
                Command::new("/usr/bin/mount").args(["-t", "cgroup2", "none", "/sys/fs/cgroup"]),
                true,
            ),
        )
        .await
        .context("mount cgroup2 timed out")??;
    }
    for (name, program, args, socket) in [
        (
            "snapshotter",
            "plain-snapshotter",
            vec![],
            "/run/containerd-plain-snapshotter/snapshotter.sock",
        ),
        (
            "containerd",
            "containerd",
            vec!["--config", "/etc/containerd/config.toml"],
            "/run/containerd/containerd.sock",
        ),
        (
            "docker",
            "dockerd",
            vec!["--config-file", "/etc/docker/daemon.json"],
            "/var/run/docker.sock",
        ),
    ] {
        let log = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(format!("/var/log/agentenv-compose/{name}.log"))?;
        let mut command = Command::new(format!("/usr/local/bin/{program}"));
        command
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .args(args)
            .env("PATH", PATH)
            .stdout(log.try_clone()?)
            .stderr(log);
        children.push((
            name,
            command.spawn().with_context(|| format!("start {name}"))?,
        ));
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            check_children(children)?;
            if fs::symlink_metadata(socket).is_ok_and(|m| m.file_type().is_socket()) {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "{name} did not create {socket} within 60s"
            );
            sleep(Duration::from_millis(100)).await;
        }
    }
    loop {
        check_children(children)?;
        sleep(Duration::from_secs(1)).await;
    }
}

fn check_children(children: &mut [(&str, Child)]) -> Result<()> {
    for (name, process) in children {
        if let Some(status) = process
            .try_wait()
            .with_context(|| format!("monitor {name}"))?
        {
            bail!("{name} exited unexpectedly: {status}");
        }
    }
    Ok(())
}

/// Entry point of aenv compose guest-init. Tini remains PID 1 and reaps orphans.
/// Never restart runtime services: the snapshotter's active metadata is in
/// memory and survives VM snapshot restore.
pub async fn supervise() -> Result<()> {
    let mut children = Vec::new();
    until_signal(supervise_daemons(&mut children)).await
}

pub(super) fn run(budget: Option<&str>) -> Result<()> {
    let budget = budget
        .map(|value| {
            let seconds: f64 = value.parse().context("invalid startup timeout")?;
            let duration =
                Duration::try_from_secs_f64(seconds).context("invalid startup timeout")?;
            anyhow::ensure!(!duration.is_zero(), "startup timeout must be positive");
            Ok::<_, anyhow::Error>(duration)
        })
        .transpose()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        match budget {
            Some(budget) => start(tokio::io::stdin(), budget).await,
            None => supervise().await,
        }
    });
    // Blocking stdin cannot be cancelled; process exit must not wait for EOF.
    runtime.shutdown_background();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    fn plan() -> ComposePlan {
        ComposePlan {
            compose: json!({"services": {"web": {}}}),
            services: (0..2)
                .map(|i| ComposeService {
                    name: format!("web{i}"),
                    image: "busybox:1.37".into(),
                    drive_id: format!("compose_{i}"),
                    mount_path: format!("/mnt/compose_{i}"),
                    local_image: format!("aenv-compose/service-{i}:local"),
                    config: json!({}),
                })
                .collect(),
        }
    }

    #[test]
    fn rejects_unmounted_or_shared_drives() {
        let mut plan = plan();
        let mounts = plan
            .services
            .iter()
            .map(|s| PathBuf::from(&s.mount_path))
            .collect();
        assert!(validate(&plan, &HashSet::new(), |_| Ok(1))
            .unwrap_err()
            .to_string()
            .contains("not mounted"));
        assert!(validate(&plan, &mounts, |_| Ok(1))
            .unwrap_err()
            .to_string()
            .contains("not isolated"));
        let device = |path: &Path| Ok(if path == Path::new("/") { 1 } else { 2 });
        assert!(validate(&plan, &mounts, device)
            .unwrap_err()
            .to_string()
            .contains("not isolated"));
        plan.services[0].mount_path = "/elsewhere".into();
        assert!(validate(&plan, &mounts, device)
            .unwrap_err()
            .to_string()
            .contains("not mounted"));
    }

    #[test]
    fn validates_isolated_drives_and_image_config() {
        let mut plan = plan();
        let mounts = plan
            .services
            .iter()
            .map(|s| PathBuf::from(&s.mount_path))
            .collect();
        let device = |path: &Path| {
            Ok(match path.to_str().unwrap() {
                "/" => 1,
                "/mnt/compose_0" => 2,
                _ => 3,
            })
        };
        validate(&plan, &mounts, device).unwrap();
        plan.services[0].config = Value::Null;
        assert!(validate(&plan, &mounts, device)
            .unwrap_err()
            .to_string()
            .contains("image config"));
    }

    #[test]
    fn unresolved_variables_stay_unset() {
        let env = environment(
            &json!({"services": {"web": {"environment": {"HOME": null, "PATH": null}}}}),
        );
        assert!(!env.contains_key("HOME"));
        assert!(!env.contains_key("PATH"));
        assert_eq!(env.len(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn framed_input_does_not_require_eof() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let mut frame = serde_json::to_vec(&plan()).unwrap();
        frame.push(b'\n');
        writer.write_all(&frame).await.unwrap();
        assert_eq!(
            timeout_at(Instant::now() + Duration::from_secs(1), read_plan(reader))
                .await
                .unwrap()
                .unwrap()
                .services
                .len(),
            2
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_oversized_input_and_times_out_missing_frame() {
        assert!(read_plan(&vec![b' '; MAX_PLAN_BYTES + 1][..])
            .await
            .unwrap_err()
            .to_string()
            .contains("exceeds 4 MiB"));
        let (_writer, reader) = tokio::io::duplex(128);
        assert!(start(reader, Duration::from_millis(10))
            .await
            .unwrap_err()
            .to_string()
            .contains("deadline exceeded"));
    }

    #[test]
    fn metadata_is_private_and_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        write_private_json(&path, &json!({"Env": ["VALUE=private"]})).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_private_json(&path, &json!({})).is_err());
    }
}
