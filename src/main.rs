mod builder;
mod config;
mod platforms;
mod router;

use anyhow::{anyhow, Context};
use axum::routing::get;
use builder::PlatformsBuilder;
use config::Config;
use futures::future::select_all;
use router::MessageRouter;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Pool, Sqlite,
};
use std::{convert::Infallible, fmt, fs, str::FromStr};
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};
use tracing::{debug, error, info};

const API_BODY_SIZE_LIMIT: usize = 64 * 1024;

type DbPool = Pool<Sqlite>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let raw_config = fs::read_to_string("config.toml").context("Could not read config file")?;
    let config: Config = toml::from_str(&raw_config).context("Could not parse config")?;

    tracing_subscriber::fmt()
        .with_env_filter(&config.general.log_level)
        .init();

    let db_opts = SqliteConnectOptions::from_str("sqlite://bridge.db")?.create_if_missing(true);

    let db_pool = SqlitePoolOptions::new()
        .max_connections(10)
        .connect_with(db_opts)
        .await
        .context("Could not open database")?;

    sqlx::migrate!().run(&db_pool).await?;
    info!("DB migrations finished");

    let message_router = MessageRouter::new(&config.bridge)?;

    let mut platforms = PlatformsBuilder::new(&config, &message_router, &db_pool);
    platforms.init_platform::<platforms::Twitch>().await?;
    platforms.init_platform::<platforms::Factorio>().await?;

    if platforms.platform_handles.is_empty() {
        return Err(anyhow!("No platforms configured"));
    }
    info!(
        "Platforms initialization complete, loaded {} platforms",
        platforms.platform_handles.len()
    );

    let mut handles = platforms.platform_handles;

    let mut incoming_message_rx = platforms.incoming_messages_rx;
    let message_senders = platforms.message_senders;
    let zws_support = platforms.zws_support;
    let platform_aliases = config.message.platform_aliases.clone();
    let channel_links = message_router.channel_links.clone();

    let send_handle = tokio::spawn(async move {
        while let Some((source_platform, incoming_msg)) = incoming_message_rx.recv().await {
            let identifier = ChannelIdentifier {
                platform: source_platform.to_owned(),
                value: incoming_msg.channel_id.clone(),
            };

            if let Some(target_channels) = channel_links.get(&identifier) {
                debug!("Mirroring message {incoming_msg:?} to channels {target_channels:?}");
                'target_channels: for target_channel in target_channels {
                    let platform = platform_aliases
                        .get(source_platform)
                        .map(|s| s.as_str())
                        .unwrap_or(source_platform);

                    let content = match incoming_msg.user_name.clone() {
                        Some(mut name) => {
                            let platform_supports_zws = *zws_support
                                .get(target_channel.channel.platform.as_str())
                                .unwrap();

                            if target_channel.insert_zws && name.len() > 1 && platform_supports_zws
                            {
                                let magic_char = char::from_u32(0x000E0000).unwrap();
                                name.insert(1, magic_char);
                            }

                            format!("[{platform}] {name}: {}", incoming_msg.contents)
                        }
                        None => format!("[{platform}] {}", incoming_msg.contents),
                    };

                    for exclude_filter in &target_channel.exclude_filters {
                        if exclude_filter.is_match(&content) {
                            debug!(
                                "Message '{content}' to {} filtered out by {exclude_filter}",
                                target_channel.channel
                            );
                            continue 'target_channels;
                        }
                    }

                    let outgoing_message = OutgoingMessage {
                        content,
                        target_channel_id: target_channel.channel.value.clone(),
                    };

                    match message_senders.get(target_channel.channel.platform.as_str()) {
                        Some(sender) => sender.send(outgoing_message).await.unwrap(),
                        None => error!(
                            "Could not get sender for platform {} (is it configured?)",
                            target_channel.channel.platform
                        ),
                    }
                }
            }
        }
        ("message_sender", Ok(()))
    });
    handles.push(send_handle);

    let web_app = axum::Router::new()
        .route("/", get("XD"))
        .nest("/platform", platforms.api_router)
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(API_BODY_SIZE_LIMIT))
        .layer(axum::Extension(db_pool));

    let listener = tokio::net::TcpListener::bind(&config.general.listen_address)
        .await
        .context("Could not set up web listener")?;

    info!("Web server listening on {}", config.general.listen_address);

    let web_handle = tokio::spawn(async move {
        axum::serve(listener, web_app)
            .await
            .expect("Web server error");
        ("web", Ok(()))
    });
    handles.push(web_handle);

    let (result, _, _) = select_all(handles).await;
    let (name, result) = result.unwrap();
    Err(anyhow!("Worker '{name}' exited unexpectedly: {result:?}"))
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
struct ChannelIdentifier {
    platform: String,
    value: Option<String>,
}

impl FromStr for ChannelIdentifier {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once(':') {
            Some((platform, channel)) => Ok(Self {
                platform: platform.to_owned(),
                value: Some(channel.to_owned()),
            }),
            None => Ok(Self {
                platform: s.to_owned(),
                value: None,
            }),
        }
    }
}

impl fmt::Display for ChannelIdentifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.platform)?;
        if let Some(value) = &self.value {
            write!(f, ":{value}")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct IncomingMessage {
    channel_id: Option<String>,
    user_id: Option<String>,
    user_name: Option<String>,
    contents: String,
}

#[derive(Debug)]
struct OutgoingMessage {
    target_channel_id: Option<String>,
    content: String,
}
