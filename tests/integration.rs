mod common;

#[path = "integration/envd_supervisor.rs"]
mod envd_supervisor;

#[path = "integration/fc.rs"]
mod fc;

#[path = "integration/process.rs"]
mod process;

#[path = "integration/snapshot.rs"]
mod snapshot;

#[path = "integration/snapshot_attached_drive.rs"]
mod snapshot_attached_drive;

#[path = "integration/volume_replacement.rs"]
mod volume_replacement;

#[path = "integration/posix_startup_pack.rs"]
mod posix_startup_pack;

#[path = "integration/ublk.rs"]
mod ublk;
