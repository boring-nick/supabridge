use crate::{
    platforms::ChatPlatform, router::MessageRouter, DbPool, IncomingMessage, OutgoingMessage,
};
use anyhow::Context;
use std::collections::HashMap;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, info};

type PlatformHandle = JoinHandle<(&'static str, anyhow::Result<()>)>;

pub struct PlatformsBuilder<'a> {
    message_router: &'a MessageRouter,
    global_config: &'a crate::Config,
    db: &'a DbPool,

    pub message_senders: HashMap<&'static str, mpsc::Sender<OutgoingMessage>>,
    pub api_router: axum::Router,
    pub incoming_messages_tx: mpsc::Sender<(&'static str, IncomingMessage)>,
    pub incoming_messages_rx: mpsc::Receiver<(&'static str, IncomingMessage)>,
    pub platform_handles: Vec<PlatformHandle>,
    pub zws_support: HashMap<&'static str, bool>,
}

impl<'a> PlatformsBuilder<'a> {
    pub fn new(
        global_config: &'a crate::Config,
        message_router: &'a MessageRouter,
        db: &'a DbPool,
    ) -> Self {
        let (incoming_messages_tx, incoming_messages_rx) = mpsc::channel(1000);

        Self {
            message_router,
            global_config,
            db,
            message_senders: HashMap::new(),
            api_router: axum::Router::new(),
            incoming_messages_tx,
            incoming_messages_rx,
            platform_handles: Vec::new(),
            zws_support: HashMap::new(),
        }
    }

    pub async fn init_platform<T: ChatPlatform>(&mut self) -> anyhow::Result<()> {
        self.zws_support.insert(T::NAME, T::supports_zws());

        match self.global_config.platforms.get(T::NAME) {
            Some(raw_config) => {
                let channels = self
                    .message_router
                    .channel_links
                    .keys()
                    .filter(|channel| channel.platform == T::NAME)
                    .filter_map(|channel| channel.value.clone())
                    .collect::<Vec<String>>();

                info!("Initializing platform {}...", T::NAME);
                let platform_config: T::Config = raw_config
                    .clone()
                    .try_into()
                    .with_context(|| format!("Could not parse config for platform {}", T::NAME))?;

                let (platform_incoming_tx, mut platform_incoming_rx) = mpsc::channel(100);

                let mut platform = T::new(platform_config, self.global_config, channels, self.db)
                    .await
                    .with_context(|| format!("Could initialize platform {}", T::NAME))?;

                let platform_router = platform
                    .api_routes()
                    .layer(axum::Extension(platform_incoming_tx.clone()));

                let original_api_router = std::mem::take(&mut self.api_router);
                self.api_router =
                    original_api_router.nest(&format!("/{}", T::NAME), platform_router);

                // This forwards the messages from the platform to a global sender, adding platform info to the message
                let incoming_messages_tx = self.incoming_messages_tx.clone();
                tokio::spawn(async move {
                    while let Some(message) = platform_incoming_rx.recv().await {
                        incoming_messages_tx.send((T::NAME, message)).await.unwrap();
                    }
                });

                let (platform_outgoing_tx, platform_outgoing_rx) = mpsc::channel(100);
                self.message_senders.insert(T::NAME, platform_outgoing_tx);

                let handle = tokio::spawn(async {
                    let handle = platform
                        .run(platform_incoming_tx, platform_outgoing_rx)
                        .await;
                    (T::NAME, handle)
                });
                self.platform_handles.push(handle);

                Ok(())
            }
            None => {
                debug!("Platform {} is not configured, skipping", T::NAME);
                Ok(())
            }
        }
    }
}
