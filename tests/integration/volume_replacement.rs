use std::path::{Path, PathBuf};
use std::time::Duration;

use agentenv::sandbox::{
    publish_captured, FirecrackerSandbox, SandboxBackend, SandboxExecutor, SandboxForkSpec,
    SandboxLaunchConfig, SnapshotMountBusy,
};
use agentenv::snapshot::{
    SnapshotId, SnapshotManager, SnapshotPublishMetadata, SnapshotPublishSource, SnapshotVolume,
};
use agentenv::types::{ExtraDrive, SandboxId, SandboxResources};
use agentenv::volume::{VolumeManager, VolumeMode, VolumeRecord};
use anyhow::{ensure, Context, Result};

use crate::common;

fn drive(volume: &VolumeRecord, path: &str, persistent: bool) -> ExtraDrive {
    let config = volume.backing_image_config.clone().expect("volume backing");
    ExtraDrive::Overlaybd {
        drive_id: volume.id.clone(),
        image_config_path: config.clone(),
        read_only: volume.mode == VolumeMode::ReadOnly,
        mount_path: PathBuf::from(path),
        virtual_size: Some(volume.size_mb * 1024 * 1024),
        sub_path: None,
        snapshot_output_dir: persistent.then(|| config.parent().unwrap().to_path_buf()),
        volume: persistent,
    }
}

async fn guest(sandbox: &FirecrackerSandbox, script: &str) -> Result<()> {
    let script = format!("set -eu\n{script}");
    let output = sandbox
        .executor()?
        .run_command_with_opts(
            "/agentenv/bin/busybox",
            &["sh", "-c", &script],
            &agentenv::sandbox::ProcessOpts::default()
                .with_cwd("/")
                .with_timeout(Duration::from_secs(30)),
        )
        .await?;
    ensure!(
        output.exit_code == 0,
        "guest command failed: {script}\n{}\n{}",
        output.stdout,
        output.stderr
    );
    Ok(())
}

async fn capture(
    sandbox: &mut FirecrackerSandbox,
    volumes: &VolumeManager,
    snapshots: &SnapshotManager,
    source: &[(String, String)],
) -> Result<agentenv::snapshot::RunnableSnapshot> {
    let captured = SandboxBackend::snapshot(sandbox).await?;
    let ids = source.iter().map(|(_, id)| id.clone()).collect::<Vec<_>>();
    volumes.publish_backings("source", &ids).await?;
    let mut volume_snapshots = Vec::new();
    for (path, id) in source {
        let volume = volumes.get(id).await?;
        volume_snapshots.push(SnapshotVolume {
            mount_path: path.clone(),
            mode: volume.mode,
            size_mb: volume.size_mb,
            layers: volume.backing_layers,
        });
    }
    let config = agentenv::cfg::ConfigManager::global_config();
    let record = publish_captured(
        snapshots,
        SnapshotPublishMetadata {
            id: SnapshotId::generate(),
            alias: None,
            source: SnapshotPublishSource::Sandbox {
                source_sandbox_id: "source".into(),
                source_sandbox_metadata: None,
            },
            context: agentenv::snapshot::CommandContext::default().with_user(Some("root".into())),
            startup: None,
            resources: SandboxResources {
                cpu_count: 1,
                memory_mib: 256,
                disk_size_mib: 0,
            },
            runtime_versions: agentenv::snapshot::SnapshotRuntimeVersions {
                kernel_version: "test".into(),
                firecracker_version: "test".into(),
                envd_version: "test".into(),
                tools_drive_version: config.resolved_tools_version().into(),
            },
            virtualization_mode: config.virtualization_mode,
            image_configs: Default::default(),
            volume_snapshots,
            custom_extension_params: None,
        },
        captured,
    )
    .await?;
    snapshots.resolve_runnable(record).await
}

#[tokio::test]
async fn snapshot_volume_replacement_releases_old_filesystems_and_rejects_busy_mounts() -> Result<()>
{
    common::setup().await;
    let root = tempfile::tempdir()?;
    let (_, snapshots, repository) = common::snapshot_test_parts(root.path());
    let volumes =
        VolumeManager::open_with_repository(root.path().join("volumes"), repository).await?;
    let mut config = common::default_sandbox_config()?;
    config.vcpu_count = 1;
    config.mem_size_mib = 256;
    config.common.default_user = Some("root".into());
    let mut source_mounts = Vec::new();
    for (name, path) in [("old-data", "/data"), ("old-models", "/models")] {
        let volume = volumes
            .create(name.into(), VolumeMode::Exclusive, None, None, 16)
            .await?;
        volumes.reserve(&volume.id, "source").await?;
        config.common.extra_drives.push(drive(&volume, path, true));
        source_mounts.push((path.to_owned(), volume.id));
    }
    let mut source = FirecrackerSandbox::new(config)?;
    if let Err(error) = source.start().await {
        let _ = source.stop().await;
        return Err(error);
    }
    let result: Result<()> = async {
        guest(&source, "echo old-data > /data/marker; echo old-models > /models/marker").await?;
        let snapshot = capture(&mut source, &volumes, &snapshots, &source_mounts).await?;
        let staged = source_mounts.iter().map(|(path, id)| async {
            Ok::<_, anyhow::Error>(drive(&volumes.get(id).await?, path, false))
        });
        let staged = futures::future::try_join_all(staged).await?;
        let staged_configs = staged.iter().map(|drive| match drive {
            ExtraDrive::Overlaybd { image_config_path, .. } => std::fs::read(image_config_path),
        }).collect::<std::io::Result<Vec<_>>>()?;
        let restored_drives = source_mounts.iter().map(|(path, id)| async {
            Ok::<_, anyhow::Error>(drive(&volumes.get(id).await?, path, true))
        });
        let mut restored = FirecrackerSandbox::from_snapshot(&snapshot, &SandboxLaunchConfig {
            extra_drives: futures::future::try_join_all(restored_drives).await?,
            extra_drives_in_snapshot: true, ..Default::default()
        })?;
        let restore_result: Result<()> = async {
            restored.start().await?;
            guest(&restored, "test \"$(cat /data/marker)\" = old-data; test \"$(cat /models/marker)\" = old-models").await
        }.await;
        let restore_stop = restored.stop().await;
        restore_result?;
        restore_stop?;
        let seed = volumes.create("replacement-seed".into(), VolumeMode::Exclusive, None, None, 16).await?;
        let mut seed_config = common::default_sandbox_config()?;
        seed_config.common.default_user = Some("root".into());
        seed_config.common.extra_drives.push(drive(&seed, "/data", true));
        let mut seed_sandbox = FirecrackerSandbox::new(seed_config)?;
        let seed_result: Result<()> = async {
            seed_sandbox.start().await?;
            guest(&seed_sandbox, "echo replacement > /data/marker").await?;
            SandboxBackend::snapshot_volumes(&mut seed_sandbox).await?;
            Ok(())
        }.await;
        let seed_stop = seed_sandbox.stop().await;
        seed_result?;
        seed_stop?;
        for (case, paths) in [vec![], vec!["/data"], vec!["/other"], vec!["/a", "/b", "/c"]].into_iter().enumerate() {
            let mut new_drives = Vec::new();
            for (index, path) in paths.iter().enumerate() {
                let mode = if case == 2 { VolumeMode::ReadOnly } else { VolumeMode::Exclusive };
                let volume = volumes.create(format!("new-{case}-{index}"), mode, None, seed.backing_image_config.clone(), 16).await?;
                new_drives.push(drive(&volume, path, true));
            }
            let launch = SandboxLaunchConfig { extra_drives: new_drives, replaced_snapshot_drives: staged.clone(), ..Default::default() };
            let mut replacement = FirecrackerSandbox::from_snapshot(&snapshot, &launch)?;
            let check: Result<()> = async {
                replacement.start().await.context(format!("replacement case {case}"))?;
                for path in &paths {
                    guest(&replacement, &format!("test \"$(cat {path}/marker)\" = replacement")).await?;
                    if case != 2 {
                        guest(&replacement, &format!("echo new > {path}/marker; test \"$(cat {path}/marker)\" = new")).await?;
                    }
                }
                for old in ["/data", "/models"] {
                    if !paths.contains(&old) {
                        guest(&replacement, &format!("! /agentenv/bin/busybox awk '$5 == \"{old}\" {{ found=1 }} END {{ exit !found }}' /proc/self/mountinfo")).await?;
                    }
                }
                // Every unused old slot must have lost its superblock entirely.
                for index in paths.len()..2 {
                    let device = char::from(b'c' + index as u8);
                    guest(&replacement, &format!("test ! -e /sys/fs/ext4/vd{device}")).await?;
                }
                guest(&replacement, "! /agentenv/bin/busybox dmesg | /agentenv/bin/busybox grep -Ei 'EXT4-fs (error|warning)|I/O error|Buffer I/O'").await?;
                replacement.pause().await?;
                replacement.resume().await?;
                if let Some(path) = paths.first() {
                    guest(&replacement, &format!("test -f {path}/marker")).await?;
                }
                let captured = SandboxBackend::snapshot(&mut replacement).await?;
                drop(captured);
                if case == 2 {
                    guest(&replacement, "! (echo forbidden > /other/forbidden)").await?;
                    let ro_drive = &launch.extra_drives[0];
                    let ro_volume = volumes.get(ro_drive.drive_id()).await?;
                    let ro_snapshot = capture(&mut replacement, &volumes, &snapshots,
                        &[("/other".into(), ro_volume.id.clone())]).await?;
                    let rw_volume = volumes.create("ro-to-rw".into(), VolumeMode::Exclusive, None, seed.backing_image_config.clone(), 16).await?;
                    let mut rw = FirecrackerSandbox::from_snapshot(&ro_snapshot, &SandboxLaunchConfig {
                        extra_drives: vec![drive(&rw_volume, "/other", true)],
                        replaced_snapshot_drives: vec![drive(&ro_volume, "/other", false)],
                        ..Default::default()
                    })?;
                    let rw_result: Result<()> = async {
                        rw.start().await?;
                        guest(&rw, "test \"$(cat /other/marker)\" = replacement; echo writable > /other/marker; test \"$(cat /other/marker)\" = writable").await
                    }.await;
                    let rw_stop = rw.stop().await;
                    rw_result?;
                    rw_stop?;
                }
                let children = SandboxBackend::fork(&mut replacement, &[SandboxForkSpec {
                    sandbox_id: SandboxId::new(), envd_access_token: None,
                    extra_drives: Vec::new(), replace_drive_ids: Vec::new(),
                }]).await?;
                for child in children {
                    let mut child = child?;
                    child.stop().await?;
                }
                Ok(())
            }.await;
            let stop = replacement.stop().await;
            check?;
            stop?;
            guest(&source, "test \"$(cat /data/marker)\" = old-data; test \"$(cat /models/marker)\" = old-models").await?;
            for (drive, expected) in staged.iter().zip(&staged_configs) {
                let ExtraDrive::Overlaybd { image_config_path, .. } = drive;
                ensure!(std::fs::read(image_config_path)? == *expected, "staged source config changed");
            }
        }
        // A bind alias allows umount of the original path, but retains the
        // superblock. A captured cwd makes the non-lazy umount itself fail.
        for (setup, cleanup, detail) in [
            ("mkdir -p /alias; /agentenv/bin/busybox mount --bind /data /alias",
             "/agentenv/bin/busybox umount /alias", "ext4 superblock is still active"),
            ("(cd /data; exec /agentenv/bin/busybox sleep 600) </dev/null >/dev/null 2>&1 & echo $! > /tmp/volume-holder",
             "kill $(cat /tmp/volume-holder)", "non-lazy umount failed"),
        ] {
            guest(&source, setup).await?;
            let busy = capture(&mut source, &volumes, &snapshots, &source_mounts).await?;
            let launch = SandboxLaunchConfig { replaced_snapshot_drives: staged.clone(), ..Default::default() };
            let mut replacement = FirecrackerSandbox::from_snapshot(&busy, &launch)?;
            let error = replacement.start().await.err().context("busy snapshot must reject replacement")?;
            let stop = replacement.stop().await;
            let busy = error.downcast_ref::<SnapshotMountBusy>().context("typed busy error")?;
            ensure!(busy.mount_path == Path::new("/data"), "{busy}");
            ensure!(busy.detail.contains(detail), "{busy}");
            stop?;
            guest(&source, cleanup).await?;
        }
        Ok(())
    }.await;
    let stop = source.stop().await;
    result?;
    stop?;
    Ok(())
}
