//! Compose preparation and the one-shot guest initialization contract.
use std::process::Stdio;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

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

/// Kept only in a fresh launch plan, never persisted or replayed on restore.
#[derive(Debug)]
pub struct ComposeBootstrap {
    pub plan: ComposePlan,
    pub deadline: Instant,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct InvalidCompose(pub String);

pub(crate) async fn prepare(binary: &str, request: serde_json::Value) -> Result<ComposePlan> {
    let mut child = tokio::process::Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start Compose planner (install aenv-compose-plan)")?;
    let mut stdin = child.stdin.take().context("planner stdin unavailable")?;
    let input = serde_json::to_vec(&request)?;
    // Drain output concurrently with writing input to avoid pipe backpressure.
    let write = async move {
        stdin.write_all(&input).await?;
        stdin.shutdown().await
    };
    let ((), output) = tokio::try_join!(write, child.wait_with_output())?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if output.status.code() == Some(2) {
            return Err(InvalidCompose(message).into());
        }
        anyhow::bail!("Compose planner failed: {message}");
    }
    serde_json::from_slice(&output.stdout).context("invalid Compose planner output")
}
