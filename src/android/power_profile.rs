//! A power profile (`core::power_profiles`) put in effect: the CPU boost it stands for goes in
//! the config, for the next start, and to the Linux programs that are running.
//!
//! The boost is a floor on how busy the scheduler takes a thread for, which threads and processes
//! pass on to those they start (`proot::process`): setting it on every thread of the running
//! programs changes them and what they start from now on. Each proot keeps the floor it started
//! with, which it raises while busy, until the next start.

use crate::android::proot::process::set_utilization_floor;
use crate::android::utils::application_context::{get_application_context, set_cpu_boost};
use crate::core::config::{with_value, PerformanceConfig, ARCH_FS_ROOT, CONFIG_FILE};
use crate::core::power_profiles::Profile;
use std::fs;
use std::io;
use std::path::Path;

/// The profile the config's CPU boost stands for.
pub fn current() -> Profile {
    Profile::from_cpu_boost(&get_application_context().local_config.performance.cpu_boost)
}

pub fn apply(profile: Profile) {
    let cpu_boost = profile.cpu_boost();
    if let Err(error) = save(cpu_boost) {
        log::error!("Power profile: {CONFIG_FILE} keeps the old CPU boost: {error}");
    }
    set_cpu_boost(cpu_boost);
    let floor = PerformanceConfig {
        cpu_boost: cpu_boost.into(),
        ..PerformanceConfig::default()
    }
    .utilization_floor();
    let (programs, refused) = boost_running(floor);
    log::info!(
        "Power profile: {}, CPU boost {cpu_boost} for {programs} running programs{}",
        profile.name(),
        if refused > 0 {
            format!(" ({refused} threads refused it)")
        } else {
            String::new()
        }
    );
}

/// `cpu_boost` in the config file, the rest of it as it was.
fn save(cpu_boost: &str) -> io::Result<()> {
    let path = Path::new(ARCH_FS_ROOT).join(CONFIG_FILE.trim_start_matches('/'));
    let content = fs::read_to_string(&path)?;
    let updated = with_value(
        &content,
        "performance",
        "cpu_boost",
        &format!("\"{cpu_boost}\""),
    );
    if updated == content {
        return Ok(());
    }
    let written = path.with_extension("toml.new");
    fs::write(&written, updated)?;
    fs::rename(&written, &path)
}

/// Whether the process is a Linux program: proot runs each through its loader, which is then the
/// process's executable, whatever its parent (daemons leave theirs).
fn is_linux_program(pid: &str) -> bool {
    fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| {
        exe.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("libproot_loader"))
    })
}

/// Put `floor` on every thread of the Linux programs (the app's processes are all there are in
/// `/proc`), not on proot itself. How many programs, and how many threads refused it.
fn boost_running(floor: u32) -> (usize, usize) {
    let (mut programs, mut refused) = (0, 0);
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = entry.file_name();
        let Some(pid) = pid.to_str().filter(|it| it.parse::<u32>().is_ok()) else {
            continue;
        };
        if !is_linux_program(pid) {
            continue;
        }
        programs += 1;
        let threads = fs::read_dir(format!("/proc/{pid}/task"));
        for thread in threads.into_iter().flatten().flatten() {
            let Some(thread) = thread.file_name().to_str().and_then(|it| it.parse().ok()) else {
                continue;
            };
            if !set_utilization_floor(thread, floor) {
                refused += 1;
            }
        }
    }
    (programs, refused)
}
