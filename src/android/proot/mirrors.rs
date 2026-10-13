//! Puts the package mirrors that answer fastest from here first in pacman's mirrorlist, and lets
//! pacman download more files at once, once per install (`core::mirrors`). Done before setup's
//! first package install, and at the next start for installs made before.

use crate::core::config::ARCH_FS_ROOT;
use crate::core::mirrors::{self, Probe};
use reqwest::header::LAST_MODIFIED;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MIRRORLIST: &str = "etc/pacman.d/mirrorlist";
const PACMAN_CONF: &str = "etc/pacman.conf";
/// Written once the mirrors were ranked: how long each took (or `-`), fastest first.
const RANKED: &str = "var/lib/localdesktop/mirrors";
/// At most this many mirrors are tried.
const MAX_PROBED: usize = 40;
/// How long the mirrors get to answer, in all; the slower ones don't count.
const PROBE_TIME: Duration = Duration::from_secs(8);

static RANKING: Mutex<()> = Mutex::new(());

fn path(name: &str) -> PathBuf {
    Path::new(ARCH_FS_ROOT).join(name)
}

/// Whether the mirrors were ranked on this install.
pub fn ranked() -> bool {
    path(RANKED).exists()
}

/// Ranks the mirrors unless that was done. Another caller meanwhile waits for it. When no mirror
/// answers, nothing changes, and the next start tries again.
pub fn rank_once() {
    let _ranking = RANKING.lock().unwrap_or_else(|it| it.into_inner());
    if ranked() {
        return;
    }
    let Ok(list) = fs::read_to_string(path(MIRRORLIST)) else {
        return;
    };
    let servers: Vec<String> = mirrors::servers(mirrors::stock_part(&list))
        .into_iter()
        .take(MAX_PROBED)
        .collect();
    let started = Instant::now();
    let mut probes = probe(&servers);
    if probes.is_empty() {
        log::warn!(
            "Package mirrors: none of {} answered; trying again at the next start",
            servers.len()
        );
        return;
    }
    let fastest: Vec<&str> = mirrors::fastest(&probes, mirrors::KEEP)
        .iter()
        .map(|it| it.server.as_str())
        .collect();
    let ranked_list = mirrors::with_fastest_first(&list, &fastest);
    if let Err(error) = replace(&path(MIRRORLIST), &ranked_list) {
        log::warn!("Package mirrors: couldn't write the mirrorlist: {error}");
        return;
    }
    if let Ok(conf) = fs::read_to_string(path(PACMAN_CONF)) {
        let updated = mirrors::with_parallel_downloads(&conf);
        if updated != conf {
            if let Err(error) = replace(&path(PACMAN_CONF), &updated) {
                log::warn!("Package mirrors: couldn't write pacman.conf: {error}");
            }
        }
    }
    log::info!(
        "Package mirrors: {} of {} answered in {:.1} s; first now: {}",
        probes.len(),
        servers.len(),
        started.elapsed().as_secs_f64(),
        fastest
            .iter()
            .map(|it| host(it))
            .collect::<Vec<_>>()
            .join(", ")
    );

    probes.sort_by(|a, b| a.seconds.total_cmp(&b.seconds));
    let mut record: String = probes
        .iter()
        .map(|it| format!("{:.3} {}\n", it.seconds, it.server))
        .collect();
    for server in &servers {
        if !probes.iter().any(|it| &it.server == server) {
            record.push_str(&format!("- {server}\n"));
        }
    }
    let _ = fs::create_dir_all(path(RANKED).parent().unwrap_or(Path::new(ARCH_FS_ROOT)));
    if let Err(error) = fs::write(path(RANKED), record) {
        log::warn!("Package mirrors: couldn't note the ranking: {error}");
    }
}

/// Ranks the mirrors in the background, unless that was done.
pub fn rank_once_in_background() {
    if !ranked() {
        thread::spawn(rank_once);
    }
}

/// How each server answered that did within `PROBE_TIME`, all of them at once.
fn probe(servers: &[String]) -> Vec<Probe> {
    let Ok(client) = reqwest::blocking::Client::builder()
        .user_agent("Local Desktop")
        .timeout(PROBE_TIME)
        .build()
    else {
        return Vec::new();
    };
    let (sender, receiver) = mpsc::channel();
    for server in servers {
        let (client, server, sender) = (client.clone(), server.clone(), sender.clone());
        thread::spawn(move || {
            let _ = sender.send(probe_one(&client, server));
        });
    }
    drop(sender);
    let deadline = Instant::now() + PROBE_TIME;
    let mut probes = Vec::new();
    while let Ok(answer) = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        probes.extend(answer);
    }
    probes
}

/// Three fetches of a small file in a row, as pacman downloads: the first opens the connection,
/// which costs most from far away, the others reuse it. The mirrors answer all at once, so
/// fetches that come out slow by chance (sharing the line) count for every mirror alike.
fn probe_one(client: &reqwest::blocking::Client, server: String) -> Option<Probe> {
    let url = mirrors::probe_url(&server, std::env::consts::ARCH);
    let start = Instant::now();
    let mut modified = None;
    for _ in 0..3 {
        let response = client.get(&url).send().ok()?.error_for_status().ok()?;
        modified = response
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|it| it.to_str().ok())
            .and_then(mirrors::http_date);
        response.bytes().ok()?;
    }
    let seconds = start.elapsed().as_secs_f64();
    Some(Probe {
        server,
        seconds,
        modified,
    })
}

/// Replaces `target` as a whole, so pacman never reads half of it.
fn replace(target: &Path, content: &str) -> std::io::Result<()> {
    let new = target.with_extension("localdesktop-new");
    fs::write(&new, content)?;
    fs::rename(&new, target)
}

/// `de4.mirror.archlinuxarm.org` of `http://de4.mirror.archlinuxarm.org/$arch/$repo`.
fn host(server: &str) -> &str {
    let rest = server.split_once("://").map_or(server, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest)
}
