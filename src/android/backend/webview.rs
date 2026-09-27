use crate::android::proot::setup::SetupMessage;
use serde_json::json;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use websocket::sync::{Server, Writer};
use websocket::OwnedMessage;

pub enum ErrorVariant {
    None,
    Unsupported,
}

pub struct WebviewBackend {
    pub socket_port: u16,
    pub progress: Arc<Mutex<u16>>, // 0-100
    pub error: ErrorVariant,
}

fn desktop_question(progress: u16) -> OwnedMessage {
    OwnedMessage::Text(
        json!({
            "progress": progress,
            "message": "Choose a desktop",
            "choose": "desktop",
        })
        .to_string(),
    )
}

impl WebviewBackend {
    /// Start accepting connections and listening for messages. The page's answer to
    /// `SetupMessage::ChooseDesktop` goes to `desktop_choice`.
    pub fn build(
        receiver: Receiver<SetupMessage>,
        progress: Arc<Mutex<u16>>,
        desktop_choice: Sender<String>,
    ) -> Self {
        let socket = Server::bind("127.0.0.1:0").expect("Failed to bind socket");
        let socket_port = socket.local_addr().unwrap().port();

        let active_client: Arc<Mutex<Option<Writer<TcpStream>>>> = Arc::new(Mutex::new(None));
        // The question stays open until the page answers, so a page that connects (or reloads)
        // later still gets it.
        let desktop_question_open = Arc::new(AtomicBool::new(false));

        let active_client_clone = active_client.clone();
        let progress_clone = progress.clone();
        let question_open = desktop_question_open.clone();
        thread::spawn(move || {
            for message in receiver {
                let progress = *progress_clone.lock().unwrap();
                let message = match message {
                    SetupMessage::Progress(msg) => OwnedMessage::Text(
                        json!({
                            "progress": progress,
                            "message": msg,
                        })
                        .to_string(),
                    ),
                    SetupMessage::Error(msg) => {
                        log::info!("Setup error [{}%]: {}", progress, msg);
                        OwnedMessage::Text(
                            json!({
                                "progress": progress,
                                "message": msg,
                                "isError": true
                            })
                            .to_string(),
                        )
                    }
                    SetupMessage::ChooseDesktop => {
                        question_open.store(true, Ordering::Release);
                        desktop_question(progress)
                    }
                };

                let mut active_client = active_client_clone.lock().unwrap();

                if let Some(writer) = active_client.as_mut() {
                    if writer.send_message(&message).is_err() {
                        log::info!("Setup progress client disconnected");
                        *active_client = None;
                    }
                }
            }
        });

        let active_client_clone = active_client.clone();
        let progress_clone = progress.clone();
        thread::spawn(move || {
            for request in socket.filter_map(Result::ok) {
                if !request.protocols().contains(&"rust-websocket".to_string()) {
                    if let Err(error) = request.reject() {
                        log::warn!("Failed to reject setup progress client: {error:?}");
                    }
                    continue;
                }

                let client = match request.use_protocol("rust-websocket").accept() {
                    Ok(client) => client,
                    Err(error) => {
                        log::warn!("Failed to accept setup progress client: {error:?}");
                        continue;
                    }
                };
                match client.peer_addr() {
                    Ok(ip) => log::info!("Setup progress connection from {}", ip),
                    Err(error) => {
                        log::warn!("Failed to read setup progress client address: {error}")
                    }
                }
                let (mut reader, mut writer) = match client.split() {
                    Ok(halves) => halves,
                    Err(error) => {
                        log::warn!("Failed to split setup progress client: {error}");
                        continue;
                    }
                };

                let progress = *progress_clone.lock().unwrap();
                let message = OwnedMessage::Text(
                    json!({
                        "progress": progress,
                        "message": "Connected to installer",
                    })
                    .to_string(),
                );
                if writer.send_message(&message).is_err() {
                    log::info!("Setup progress client disconnected during initial update");
                    continue;
                }

                let question_open = desktop_question_open.clone();
                let desktop_choice = desktop_choice.clone();
                thread::spawn(move || {
                    for message in reader.incoming_messages() {
                        match message {
                            Ok(OwnedMessage::Text(text)) => {
                                let value: serde_json::Value =
                                    serde_json::from_str(&text).unwrap_or_default();
                                if let Some(desktop) = value["desktop"].as_str() {
                                    if question_open.swap(false, Ordering::AcqRel) {
                                        let _ = desktop_choice.send(desktop.to_string());
                                    }
                                }
                            }
                            Ok(OwnedMessage::Close(_)) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                });

                // Under the lock, so a question asked meanwhile reaches this client either way.
                let mut active_client = active_client_clone.lock().unwrap();
                if desktop_question_open.load(Ordering::Acquire)
                    && writer.send_message(&desktop_question(progress)).is_err()
                {
                    log::info!("Setup progress client disconnected during initial update");
                    continue;
                }
                if active_client.replace(writer).is_some() {
                    log::info!("Replaced stale setup progress client");
                }
            }
        });

        Self {
            socket_port,
            progress,
            error: ErrorVariant::None,
        }
    }
}
