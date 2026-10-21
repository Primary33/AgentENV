//! Guest-side volume filesystem operations run through envd.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::sandbox::envd::EnvdInstance;
use crate::sandbox::process::Executor;
use crate::types::ExtraDrive;

#[derive(Debug, thiserror::Error)]
#[error("snapshot mount {mount_path} cannot be released: {detail}")]
pub struct SnapshotMountBusy {
    pub mount_path: PathBuf,
    pub detail: String,
}

pub(super) fn guest_device_name(index: usize) -> String {
    format!("vd{}", char::from(b'c' + index as u8))
}

pub(super) async fn release_snapshot_mounts(
    envd: EnvdInstance,
    mut mounts: Vec<(usize, PathBuf)>,
) -> Result<()> {
    if mounts.is_empty() {
        return Ok(());
    }
    mounts.sort_by_key(|(_, path)| std::cmp::Reverse(path.components().count()));
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(release_snapshot_mounts_inner(envd, mounts))
    })
    .await
    .context("snapshot mount release task failed")?
}

async fn release_snapshot_mounts_inner(
    envd: EnvdInstance,
    mounts: Vec<(usize, PathBuf)>,
) -> Result<()> {
    let entries = mounts
        .iter()
        .map(|(index, path)| format!("{}:{}", guest_device_name(*index), path.display()))
        .collect::<Vec<_>>();
    let script = r#"
        set -eu
        fail() {
            printf '%s\n' "$entry"
            printf '%s\n' "$1" >&2
            exit 1
        }
        for entry in "$@"; do
            /agentenv/bin/busybox umount "${entry#*:}" || fail 'non-lazy umount failed'
        done
        for entry in "$@"; do
            dev=${entry%%:*}
            majmin=$(/agentenv/bin/busybox cat "/sys/block/$dev/dev") || fail 'cannot read device number'
            test ! -e "/sys/fs/ext4/$dev" || fail 'ext4 superblock is still active'
            for info in /proc/[0-9]*/mountinfo; do
                # Check the device column, including other mount namespaces.
                # Do not accept unreadable mountinfo as proof of release. Only
                # disappearing processes can be skipped during this scan.
                status=0
                /agentenv/bin/busybox awk -v dev="$majmin" \
                    '$3 == dev { found=1 } END { exit (found ? 42 : 0) }' "$info" || status=$?
                case "$status" in
                    0) ;;
                    42) fail "device is still mounted in $info" ;;
                    *) test ! -e "$info" || fail "cannot inspect $info" ;;
                esac
            done
        done
    "#;
    let mut args = vec!["sh", "-c", script, "release-snapshot-mounts"];
    args.extend(entries.iter().map(String::as_str));
    let timeout = std::time::Duration::from_secs(30);
    let output = tokio::time::timeout(
        timeout,
        Executor::new(envd).with_root_user().run_command_with_opts(
            "/agentenv/bin/busybox",
            &args,
            &crate::sandbox::ProcessOpts::default()
                .with_cwd("/")
                .with_timeout(timeout),
        ),
    )
    .await;
    let (entry, detail) = match output {
        Ok(Ok(output)) if output.exit_code == 0 => return Ok(()),
        Ok(Ok(output)) => (
            Some(output.stdout.trim().to_owned()),
            output.stderr.trim().to_owned(),
        ),
        Ok(Err(error)) => (None, format!("guest release command failed: {error:#}")),
        Err(_) => (
            None,
            "guest release command timed out after 30 seconds".to_owned(),
        ),
    };
    let index = entry
        .as_ref()
        .and_then(|entry| entries.iter().position(|value| value == entry))
        .unwrap_or(0);
    Err(SnapshotMountBusy {
        mount_path: mounts[index].1.clone(),
        detail,
    }
    .into())
}

pub(super) async fn mount_initial_guest_drives(
    envd: EnvdInstance,
    drives: Vec<(usize, ExtraDrive)>,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(mount_initial_guest_drives_inner(envd, drives))
    })
    .await
    .context("initial volume mount task failed")??;
    Ok(())
}

async fn mount_initial_guest_drives_inner(
    envd: EnvdInstance,
    drives: Vec<(usize, ExtraDrive)>,
) -> Result<()> {
    for (index, drive) in &drives {
        let device = format!("/dev/{}", guest_device_name(*index));
        let target = drive.mount_path().to_string_lossy().into_owned();
        create_guest_directory(&envd, &target).await?;
        let mount_options = if drive.read_only() {
            // Volume snapshots are crash-consistent: even after sync, ext4
            // retains the needs-recovery flag until a clean unmount. A
            // read-only OverlayBD device cannot replay that journal, so
            // suppress replay while also enforcing a read-only guest mount.
            &["-o", "ro,noload"][..]
        } else {
            &[][..]
        };

        if let Some(sub_path) = drive.sub_path() {
            let stage = format!("/run/agentenv-drive-{}", drive.drive_id());
            create_guest_directory(&envd, &stage).await?;
            mount_guest_path_with_options(envd.clone(), mount_options, &device, &stage).await?;
            let source = format!("{stage}/{}", sub_path.display());
            mount_guest_path_with_options(envd.clone(), &["--bind"], &source, &target).await?;
            let _ = run_guest_command(
                envd.clone(),
                "/agentenv/bin/busybox".to_owned(),
                vec!["umount".to_owned(), "-l".to_owned(), stage.clone()],
            )
            .await;
        } else {
            mount_guest_path_with_options(envd.clone(), mount_options, &device, &target).await?;
        }
    }
    Ok(())
}

async fn create_guest_directory(envd: &EnvdInstance, path: &str) -> Result<()> {
    Executor::new(envd.clone())
        .with_root_user()
        .create_dir_all(path)
        .await
}

async fn run_guest_command(
    envd: EnvdInstance,
    command: String,
    args: Vec<String>,
) -> Result<crate::sandbox::process::ProcessOutput> {
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    Executor::new(envd)
        .with_root_user()
        .run_command_with_opts(
            &command,
            &args,
            &crate::sandbox::ProcessOpts::default().with_cwd("/"),
        )
        .await
}

pub(super) async fn sync_writable_volume_filesystems(envd: EnvdInstance) -> Result<()> {
    let output = tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(run_guest_command(
            envd,
            "/agentenv/bin/busybox".to_owned(),
            vec!["sync".to_owned()],
        ))
    })
    .await
    .context("volume filesystem flush task failed")??;
    anyhow::ensure!(
        output.exit_code == 0,
        "failed to flush writable volume filesystems before pause (exit code {}): {}",
        output.exit_code,
        output.stderr.trim()
    );
    Ok(())
}

pub(super) async fn set_volume_freeze(
    envd: EnvdInstance,
    mount: &Path,
    frozen_device: Option<&str>,
) -> Result<crate::sandbox::process::ProcessOutput> {
    let mount = mount
        .to_str()
        .context("volume mount path is not valid UTF-8")?;
    // Keep one opened filesystem across validation and the ioctl, so an
    // unmount cannot redirect fsfreeze to the root filesystem hosting envd.
    let script = r#"
        set -eu
        exec 3< "$1"
        device=$(/agentenv/bin/busybox stat -Lc %d /proc/self/fd/3)
        if [ "$2" = --freeze ]; then
            test "$device" != "$(/agentenv/bin/busybox stat -c %d /)"
        else
            test "$device" = "$3"
        fi
        /agentenv/bin/busybox fsfreeze "$2" /proc/self/fd/3
        printf '%s\n' "$device"
    "#;
    let operation = if frozen_device.is_some() {
        "--unfreeze"
    } else {
        "--freeze"
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        Executor::new(envd)
            .with_root_user()
            .run_command_with_opts(
                "/agentenv/bin/busybox",
                &[
                    "sh",
                    "-c",
                    script,
                    "freeze-volume",
                    mount,
                    operation,
                    frozen_device.unwrap_or_default(),
                ],
                &crate::sandbox::ProcessOpts::default().with_cwd("/"),
            )
            .await
    })
    .await
    .context("volume filesystem freeze/thaw timed out")?
}

async fn mount_guest_path_with_options(
    envd: EnvdInstance,
    options: &[&str],
    source: &str,
    target: &str,
) -> Result<()> {
    let mut args = vec!["mount".to_owned(), "-n".to_owned()];
    args.extend(options.iter().map(|option| (*option).to_owned()));
    args.extend([source.to_owned(), target.to_owned()]);
    let output = run_guest_command(
        envd.clone(),
        "/agentenv/bin/busybox".to_owned(),
        args.clone(),
    )
    .await?;
    if output.exit_code == 0 {
        return Ok(());
    }

    // A stale snapshot mount must not be accepted just because its target
    // matches. Remove it and retry with the requested source/options.
    let _ = run_guest_command(
        envd.clone(),
        "/agentenv/bin/busybox".to_owned(),
        vec!["umount".to_owned(), "-l".to_owned(), target.to_owned()],
    )
    .await;
    let retry = run_guest_command(envd, "/agentenv/bin/busybox".to_owned(), args).await?;
    if retry.exit_code == 0 {
        return Ok(());
    }
    anyhow::bail!(
        "failed to mount {} at {} during snapshot launch (exit code {}): {}",
        source,
        target,
        retry.exit_code,
        retry.stderr.trim()
    )
}
