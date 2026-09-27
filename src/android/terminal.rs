//! The in-app terminal: a login shell in proot on a pty, relayed to xterm.js
//! (`assets/terminal.html`) in a WebView over a localhost WebSocket. It needs nothing from the
//! desktop, which makes it the way back in when the desktop is broken.
//!
//! Every app on the phone can connect to a localhost port, and this one hands out a shell running
//! as this app, so the server has a random token that only the page's URL carries; the page
//! offers it as its WebSocket subprotocol.

use crate::android::proot::{launch, process::ArchProcess};
use crate::android::utils::application_context::get_application_context;
use crate::android::session;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use websocket::sync::{Client, Server};
use websocket::OwnedMessage;
use winit::platform::android::activity::AndroidApp;

/// Port and token of the running server.
static SERVER: Mutex<Option<(u16, String)>> = Mutex::new(None);

/// The terminal page's URL, starting the server on first use.
pub fn url() -> io::Result<String> {
    let mut server = SERVER.lock().unwrap();
    if server.is_none() {
        *server = Some(start()?);
    }
    let (port, token) = server.as_ref().unwrap();
    Ok(format!(
        "file:///android_asset/terminal.html?port={port}&token={token}"
    ))
}

/// Show the terminal over the desktop.
pub fn open(android_app: &AndroidApp) {
    match url() {
        Ok(url) => session::open_page(android_app, &url),
        Err(error) => log::error!("Failed to start the terminal: {error}"),
    }
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn start() -> io::Result<(u16, String)> {
    let server = Server::bind("127.0.0.1:0")?;
    let port = server.local_addr()?.port();
    let token = random_token()?;

    let expected = token.clone();
    thread::spawn(move || {
        for request in server.filter_map(Result::ok) {
            if !request.protocols().iter().any(|it| *it == expected) {
                let _ = request.reject();
                continue;
            }
            match request.use_protocol(expected.clone()).accept() {
                Ok(client) => {
                    thread::spawn(move || {
                        if let Err(error) = serve(client) {
                            log::warn!("Terminal session failed: {error}");
                        }
                    });
                }
                Err((_, error)) => log::warn!("Failed to accept terminal client: {error}"),
            }
        }
    });
    Ok((port, token))
}

/// One page connection is one shell. Binary frames carry terminal bytes both ways; text frames
/// from the page are JSON control messages (`{"type": "resize", "cols": …, "rows": …}`).
fn serve(client: Client<TcpStream>) -> io::Result<()> {
    let (mut reader, writer) = client.split()?;
    let writer = Arc::new(Mutex::new(writer));
    let (master, mut child) = spawn_shell()?;

    let output_master = master.try_clone()?;
    let output_writer = writer.clone();
    thread::spawn(move || {
        let mut buffer = [0u8; 16384];
        // Reading fails with EIO once nothing holds the pty open any more: the shell is gone.
        while let Ok(n @ 1..) = (&output_master).read(&mut buffer) {
            let message = OwnedMessage::Binary(buffer[..n].to_vec());
            if output_writer.lock().unwrap().send_message(&message).is_err() {
                return;
            }
        }
        let _ = output_writer
            .lock()
            .unwrap()
            .send_message(&OwnedMessage::Close(None));
    });

    for message in reader.incoming_messages() {
        match message {
            Ok(OwnedMessage::Binary(bytes)) => (&master).write_all(&bytes)?,
            Ok(OwnedMessage::Text(text)) => control(&master, &text),
            Ok(OwnedMessage::Ping(data)) => {
                let _ = writer
                    .lock()
                    .unwrap()
                    .send_message(&OwnedMessage::Pong(data));
            }
            Ok(OwnedMessage::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }

    // The page went away: end the shell and whatever it started.
    launch::stop_proot(child.id());
    let _ = child.wait();
    Ok(())
}

fn control(master: &File, text: &str) {
    let Ok(message) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    if message["type"] == "resize" {
        let (Some(cols), Some(rows)) = (message["cols"].as_u64(), message["rows"].as_u64()) else {
            return;
        };
        set_size(master, cols as u16, rows as u16);
    }
}

fn set_size(master: &File, cols: u16, rows: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
}

/// A login shell for the configured user on a new pty, as the session leader with the pty as
/// its controlling terminal (so ^C and job control work).
fn spawn_shell() -> io::Result<(File, Child)> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { File::from_raw_fd(master) };
    let mut name = [0 as libc::c_char; 128];
    unsafe {
        if libc::grantpt(master.as_raw_fd()) != 0
            || libc::unlockpt(master.as_raw_fd()) != 0
            || libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let slave = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if slave < 0 {
        return Err(io::Error::last_os_error());
    }
    let slave = unsafe { File::from_raw_fd(slave) };
    set_size(&master, 80, 24);

    let user = get_application_context().local_config.user.username;
    let mut command = ArchProcess {
        command: "cd ~ 2>/dev/null; command -v bash >/dev/null && exec bash -l; exec sh -l".into(),
        user: Some(user),
        log: None,
    }
    .command();
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    Ok((master, child))
}
