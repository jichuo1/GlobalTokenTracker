//! Shell-side helpers for the update flow: UI state and launching the
//! (already SHA-256-verified) installer detached.

use globaltokentracker_core::update::Release;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// First automatic check waits for the startup scan to settle.
pub const FIRST_CHECK_SECS: u64 = 15;
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Debug)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading(Release),
    Failed(String),
}

/// Run the installer silently and have it relaunch the app afterwards.
pub fn spawn_installer(path: &Path) -> std::io::Result<()> {
    Command::new(path)
        .args(["--quiet", "--launch"])
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}
