use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::sync::watch;

/// A publication barrier for local image availability. Only the reporter sends
/// heartbeats, so an older periodic inventory cannot overwrite a newer flush.
pub(super) struct HeartbeatBarrier {
    enabled: AtomicBool,
    requested: watch::Sender<u64>,
    completed: watch::Sender<u64>,
}

impl Default for HeartbeatBarrier {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            requested: watch::channel(0).0,
            completed: watch::channel(0).0,
        }
    }
}

impl HeartbeatBarrier {
    pub(super) fn enable(&self) -> watch::Receiver<u64> {
        self.enabled.store(true, Ordering::Release);
        self.requested.subscribe()
    }

    pub(super) fn generation(&self) -> u64 {
        *self.requested.borrow()
    }

    pub(super) fn complete(&self, generation: u64) {
        self.completed.send_replace(generation);
    }

    pub(super) async fn flush(&self) -> Result<()> {
        if !self.enabled.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut completed = self.completed.subscribe();
        let mut generation = 0;
        self.requested.send_modify(|value| {
            *value += 1;
            generation = *value;
        });
        tokio::time::timeout(
            Duration::from_secs(30),
            completed.wait_for(|value| *value >= generation),
        )
        .await
        .context("timed out publishing image availability to the scheduler")?
        .context("heartbeat reporter stopped")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn flush_requires_a_heartbeat_started_after_the_request() {
        let barrier = std::sync::Arc::new(HeartbeatBarrier::default());
        let mut requests = barrier.enable();
        let previous = barrier.generation();
        let mut task = tokio::spawn({
            let barrier = barrier.clone();
            async move { barrier.flush().await }
        });
        requests.changed().await.unwrap();
        barrier.complete(previous);
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut task)
            .await
            .is_err());
        barrier.complete(barrier.generation());
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn standalone_flush_needs_no_scheduler() {
        HeartbeatBarrier::default().flush().await.unwrap();
    }
}
