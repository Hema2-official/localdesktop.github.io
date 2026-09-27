//! An OpenSSH server for the session user, configured by the `[ssh]` config section.
//!
//! sshd runs in the foreground in a proot of its own rather than in the desktop's: proot's
//! `--kill-on-exit` would take a daemonized sshd down with the shell that started it, and a
//! separate proot keeps SSH up when the desktop is broken.

use super::process::ArchProcess;
use super::setup::{SetupMessage, SetupOptions, StageOutput};
use crate::android::utils::application_context::get_application_context;
use crate::core::config::{LocalConfig, SshConfig, ARCH_FS_ROOT};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

const SSHD: &str = "usr/bin/sshd";
const LAUNCHER: &str = "usr/local/bin/localdesktop-sshd";
const CONFIG_DROP_IN: &str = "etc/ssh/sshd_config.d/10-localdesktop.conf";
const KEYS_TO_ADD: &str = "tmp/localdesktop-ssh-keys";

static SSHD_RUNNING: AtomicBool = AtomicBool::new(false);

/// Only run sshd when someone can log in.
fn wanted(ssh: &SshConfig, username: &str) -> bool {
    if !ssh.enabled {
        return false;
    }
    let home = if username == "root" {
        "root".to_string()
    } else {
        format!("home/{username}")
    };
    let authorized_keys = Path::new(ARCH_FS_ROOT).join(home).join(".ssh/authorized_keys");
    ssh.password_login
        || !ssh.keys().is_empty()
        || fs::metadata(authorized_keys).is_ok_and(|it| it.len() > 0)
}

/// Setup stage: install openssh the first time SSH is wanted, then write its config on every
/// launch. Host keys and authorized keys are handled by `start`, off the startup path.
pub fn setup_ssh(options: &SetupOptions) -> StageOutput {
    let local_config = get_application_context().local_config;
    if !wanted(&local_config.ssh, &local_config.user.username) {
        return None;
    }
    if Path::new(ARCH_FS_ROOT).join(SSHD).exists() {
        write_config(&local_config.ssh);
        return None;
    }

    let sender = options.mpsc_sender.clone();
    Some(thread::spawn(move || {
        sender
            .send(SetupMessage::Progress("Installing the SSH server...".to_string()))
            .unwrap_or(());
        let log_sender = sender.clone();
        ArchProcess {
            command: "rm -f /var/lib/pacman/db.lck; \
                      stdbuf -oL pacman -S --needed --noconfirm --noprogressbar openssh || \
                      stdbuf -oL pacman -Syu --needed --noconfirm --noprogressbar openssh"
                .into(),
            user: None,
            log: Some(Arc::new(move |it| {
                log_sender.send(SetupMessage::Progress(it)).unwrap_or(());
            })),
        }
        .run();
        if !Path::new(ARCH_FS_ROOT).join(SSHD).exists() {
            sender
                .send(SetupMessage::Error(
                    "Could not install the SSH server; the desktop starts without it.".to_string(),
                ))
                .unwrap_or(());
        }
    }))
}

fn write_config(ssh: &SshConfig) {
    let fs_root = Path::new(ARCH_FS_ROOT);

    let drop_in = fs_root.join(CONFIG_DROP_IN);
    let _ = fs::create_dir_all(drop_in.parent().unwrap());
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    fs::write(
        &drop_in,
        format!(
            "# Written by Local Desktop on every launch, from the [ssh] section of \
             /etc/localdesktop/localdesktop.toml.\n\
             Port {}\n\
             PubkeyAuthentication yes\n\
             PasswordAuthentication {}\n\
             KbdInteractiveAuthentication no\n\
             PermitRootLogin {}\n",
            ssh.port,
            yes_no(ssh.password_login),
            if ssh.password_login { "yes" } else { "prohibit-password" },
        ),
    )
    .expect("Failed to write the sshd config");

    let launcher = fs_root.join(LAUNCHER);
    fs::write(
        &launcher,
        r#"#!/bin/sh
# Runs sshd in the foreground; Local Desktop starts it in a proot of its own.
if [ "$(id -u)" = 0 ]; then
    exec /usr/bin/sshd -D -e "$@"
fi
# A non-root sshd can't use PAM or write /run/sshd.pid under proot.
exec /usr/bin/sshd -D -e -o UsePAM=no -o PidFile=none "$@"
"#,
    )
    .expect("Failed to write the sshd launcher");
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755))
        .expect("Failed to mark the sshd launcher executable");

    // Keys go through a file rather than the command line, so their comments need no quoting.
    let keys_file = fs_root.join(KEYS_TO_ADD);
    fs::write(&keys_file, ssh.keys().join("\n") + "\n").expect("Failed to write the SSH keys");
}

/// Host keys and the user's authorized keys, as root inside proot so ownership records come
/// out right: under proot sshd runs as the session user, which then has to own the host keys.
fn prepare(username: &str) -> bool {
    let output = ArchProcess {
        command: format!(
            r#"user='{username}'
ssh-keygen -A >/dev/null || exit 1
[ "$user" = root ] || chown "$user:" /etc/ssh/ssh_host_*_key
home=$(getent passwd "$user" | cut -d: -f6)
[ -n "$home" ] || exit 1
mkdir -p "$home/.ssh" && touch "$home/.ssh/authorized_keys" || exit 1
while IFS= read -r key; do
    [ -n "$key" ] || continue
    grep -qxF "$key" "$home/.ssh/authorized_keys" || printf '%s\n' "$key" >> "$home/.ssh/authorized_keys"
done < /{KEYS_TO_ADD}
rm -f /{KEYS_TO_ADD}
chmod 700 "$home/.ssh" && chmod 600 "$home/.ssh/authorized_keys"
chown -R "$user:" "$home/.ssh"
/usr/bin/sshd -t"#
        ),
        user: None,
        log: None,
    }
    .run();
    if !output.status.success() {
        log::warn!(
            "SSH setup failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output.status.success()
}

/// The user and port to log in with, when the SSH server runs.
pub fn login(local_config: &LocalConfig) -> Option<(String, u16)> {
    let username = &local_config.user.username;
    (wanted(&local_config.ssh, username) && Path::new(ARCH_FS_ROOT).join(SSHD).exists())
        .then(|| (username.clone(), local_config.ssh.port))
}

/// Start sshd next to the desktop, once per app process.
pub fn start(local_config: &LocalConfig) {
    let username = local_config.user.username.clone();
    if !wanted(&local_config.ssh, &username) || !Path::new(ARCH_FS_ROOT).join(SSHD).exists() {
        return;
    }
    if SSHD_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let port = local_config.ssh.port;
    thread::spawn(move || {
        if !prepare(&username) {
            SSHD_RUNNING.store(false, Ordering::Release);
            return;
        }
        log::info!("Starting sshd for {username} on port {port}");
        let output = ArchProcess {
            command: format!("exec /{LAUNCHER} 2>&1"),
            user: Some(username),
            log: Some(Arc::new(|it| log::info!("sshd: {it}"))),
        }
        .run();
        log::warn!("sshd exited: {}", output.status);
        SSHD_RUNNING.store(false, Ordering::Release);
    });
}
