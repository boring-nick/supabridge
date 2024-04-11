use super::ChatPlatform;
use crate::{DbPool, IncomingMessage, OutgoingMessage};
use anyhow::Context;
use notify::{RecommendedWatcher, Watcher};
use serde::Deserialize;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};
use tokio::{net::TcpStream, select, sync::mpsc, task::JoinHandle};
use tracing::{debug, error, info};

pub struct Factorio {
    config: Config,
}

impl Factorio {
    async fn connect_rcon(&self) -> anyhow::Result<rcon::Connection<TcpStream>> {
        rcon::Connection::builder()
            .enable_factorio_quirks(true)
            .connect(&self.config.rcon_address, &self.config.rcon_password)
            .await
            .context("Could not connect to RCON")
    }
}

impl ChatPlatform for Factorio {
    const NAME: &'static str = "factorio";
    type Config = Config;

    async fn new(
        config: Self::Config,
        _global_config: &crate::Config,
        _channel_ids: Vec<String>,
        _db: &DbPool,
    ) -> anyhow::Result<Self> {
        Ok(Self { config })
    }

    async fn run(
        self,
        incoming_message_tx: mpsc::Sender<IncomingMessage>,
        mut outgoing_message_rx: mpsc::Receiver<OutgoingMessage>,
    ) -> anyhow::Result<()> {
        let mut rcon_client = self.connect_rcon().await?;

        let mut log_handle = start_log_watcher(
            self.config.bridge_output_log_path.clone(),
            incoming_message_tx,
        )?;

        loop {
            select! {
                Some(msg) = outgoing_message_rx.recv() => {
                    let cmd;
                    if msg.source_msg.contents.starts_with("/players ") || msg.source_msg.contents == "/players" {
                        cmd = String::from("/bridge-player-list");
                    } else {
                        let user_text = match msg.source_msg.user_name {
                            Some(name) => match msg.source_msg.user_color {
                                Some(color) => {
                                    format!("[color=#{color}]{name}:[/color] {}", msg.source_msg.contents)
                                }
                                None => format!("{name}: {}", msg.source_msg.contents)
                            }
                            None => msg.content.to_string()
                        };

                        cmd = format!("/puppet [{}] {user_text}", msg.source_platform_name);
                    }

                    if let Err(err) = rcon_client.cmd(&cmd).await {
                        error!("Could not send message to server: {err}");
                        info!("Attempting to reconect");

                        match self.connect_rcon().await {
                            Ok(new_client) => {
                                rcon_client = new_client;

                                if let Err(err) = rcon_client.cmd(&cmd).await {
                                    error!("Could not send message even after a reconnect: {err}");
                                }
                            }
                            Err(err) => error!("Could not reconnect: {err:#}"),
                        }
                    }
                },
                log_result = &mut log_handle => log_result.unwrap()?,
            }
        }
    }

    fn supports_zws() -> bool {
        false
    }
}

#[derive(Deserialize, Debug)]
pub struct Config {
    pub bridge_output_log_path: PathBuf,
    pub rcon_address: String,
    pub rcon_password: String,
}

fn start_log_watcher(
    log_path: PathBuf,
    mut incoming_tx: mpsc::Sender<IncomingMessage>,
) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
    let mut file = File::open(&log_path).context("Could not open log file")?;
    // Start reading from the end of the file
    file.seek(SeekFrom::End(0))?;

    let (tx, rx) = std::sync::mpsc::channel();

    let handle = tokio::task::spawn_blocking(move || {
        let mut watcher = RecommendedWatcher::new(tx, notify::Config::default())
            .context("Could not create file watcher")?;

        watcher
            .watch(&log_path, notify::RecursiveMode::NonRecursive)
            .context("Could not watch log file")?;
        debug!("Registered watcher for log file at {log_path:?}");

        for res in rx {
            match res {
                Ok(event) => {
                    if event.kind.is_modify() {
                        let mut new_contents = String::new();
                        match file.read_to_string(&mut new_contents) {
                            Ok(_) => {
                                process_log(&new_contents, &mut incoming_tx);
                            }
                            Err(err) => error!("Could not read new file contents: {err}"),
                        }
                    }
                }
                Err(err) => error!("Could not handle FS event: {err}"),
            }
        }
        info!("Event stream over");
        Ok(())
    });
    Ok(handle)
}

fn process_log(new_contents: &str, incoming_tx: &mut mpsc::Sender<IncomingMessage>) {
    for line in new_contents.lines() {
        debug!("Read new log line {line}");
        if let Some((event_type, contents)) = line.split_once(' ') {
            match event_type {
                "CHAT" => {
                    match contents.split_once(": ") {
                        Some((name, text)) => {
                            if name != "<server>" {
                                let msg = IncomingMessage {
                                    channel_id: None,
                                    user_id: Some(name.to_owned()),
                                    user_name: Some(name.to_owned()),
                                    contents: text.to_owned(),
                                    user_color: None,
                                };
                                incoming_tx.blocking_send(msg).unwrap();
                            }
                        }
                        None => error!("Could not process line '{line}', expected a split in chat message contents"),
                    }
                },
                "PLAYERLIST" => {
                    let txt = contents.split(';')
                            .map(|player| {
                                let (name, mut surface) = player.split_once(' ').unwrap();
                                // surface can be Phoebe, nauvis, Nauvis Orbit, ...

                                // for some reason nauvis is lowercase, this fixes that
                                if surface == "nauvis" {
                                    surface = "Nauvis";
                                }

                                format!("{name} is on {surface}")
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                    let msg = IncomingMessage {
                        channel_id: None,
                        user_id: None,
                        user_name: None,
                        contents: txt,
                        user_color: None,
                    };
                    incoming_tx.blocking_send(msg).unwrap();
                },
                _ => {
                    let msg = IncomingMessage {
                        user_id: None,
                        channel_id: None,
                        user_name: None,
                        contents: contents.to_owned(),
                        user_color: None,
                    };
                    incoming_tx.blocking_send(msg).unwrap();
                }
            }
        }
    }
}
