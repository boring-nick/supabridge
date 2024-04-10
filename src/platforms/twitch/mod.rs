mod web;

use super::ChatPlatform;
use crate::{DbPool, IncomingMessage, OutgoingMessage};
use anyhow::Context;
use axum::routing::{get, post};
use futures::StreamExt;
use reqwest::StatusCode;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use twitch_api::{
    eventsub::{self, channel::ChannelChatMessageV1Payload, EventSubscription, Status},
    helix::{self, ClientRequestError, HelixRequestPostError},
    twitch_oauth2::AppAccessToken,
    types::MsgId,
};
use twitch_oauth2::{CsrfToken, Scope, UserTokenBuilder};

type HelixClient = twitch_api::HelixClient<'static, reqwest::Client>;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub username: String,
    pub client_id: String,
    pub eventsub_secret: String,
    pub client_secret: String,
}

#[derive(Clone)]
pub struct Twitch {
    helix: HelixClient,
    bot_user: helix::users::User,
    app_token: AppAccessToken,
    base_url: String,
    config: Config,
    csrf_tokens: Arc<Mutex<HashMap<CsrfToken, UserTokenBuilder>>>,
    channel_ids: Vec<String>,
    recently_sent_messages: Arc<tokio::sync::Mutex<HashSet<MsgId>>>,
}

impl ChatPlatform for Twitch {
    const NAME: &'static str = "twitch";
    type Config = Config;

    async fn new(
        config: Self::Config,
        global_config: &crate::Config,
        channel_ids: Vec<String>,
        _db: &DbPool,
    ) -> anyhow::Result<Self> {
        let helix = HelixClient::new();

        let app_token = twitch_oauth2::AppAccessToken::get_app_access_token(
            helix.get_client(),
            config.client_id.clone().into(),
            config.client_secret.clone().into(),
            vec![Scope::UserBot],
        )
        .await?;

        let bot_user = helix
            .get_user_from_login(&config.username, &app_token)
            .await?
            .context("The bot's user does not exist")?;

        Ok(Self {
            app_token,
            helix,
            bot_user,
            config,
            base_url: global_config.general.base_url.clone(),
            csrf_tokens: Arc::default(),
            channel_ids,
            recently_sent_messages: Arc::default(),
        })
    }

    async fn run(
        self,
        _message_tx: mpsc::Sender<IncomingMessage>,
        mut outgoing_message_rx: mpsc::Receiver<OutgoingMessage>,
    ) -> anyhow::Result<()> {
        self.setup_eventsub()
            .await
            .context("Could not set up EventSub")?;

        while let Some(outgoing_msg) = outgoing_message_rx.recv().await {
            if let Err(err) = self.send_msg(outgoing_msg).await {
                error!("Could not send message: {err:#}");
            }
        }
        Ok(())
    }

    fn api_routes(&mut self) -> axum::Router {
        axum::Router::new()
            .route("/eventsub", post(web::eventsub_callback))
            .route("/auth", get(web::auth))
            .route("/auth/redirect", get(web::auth_redirect))
            .with_state(Arc::new(self.clone()))
    }
}

impl Twitch {
    async fn handle_message(
        &self,
        msg: ChannelChatMessageV1Payload,
        message_tx: mpsc::Sender<IncomingMessage>,
    ) -> anyhow::Result<()> {
        // The message was sent by the bridge itself
        if self
            .recently_sent_messages
            .lock()
            .await
            .remove(&msg.message_id)
        {
            return Ok(());
        }

        let color = msg.color.as_str().trim_start_matches('#').to_owned();
        let user_color = if color.is_empty() { None } else { Some(color) };

        message_tx
            .send(IncomingMessage {
                channel_id: Some(msg.broadcaster_user_id.to_string()),
                user_id: Some(msg.chatter_user_id.to_string()),
                user_name: Some(msg.chatter_user_name.to_string()),
                contents: msg.message.text,
                user_color,
            })
            .await?;

        Ok(())
    }

    async fn send_msg(&self, outgoing_msg: OutgoingMessage) -> anyhow::Result<()> {
        let mut recently_sent = self.recently_sent_messages.lock().await;

        let channel_id = outgoing_msg
            .target_channel_id
            .context("Cannot send without a channel")?;

        let sender_id = outgoing_msg
            .sender_user_id
            .as_deref()
            .unwrap_or(self.bot_user.id.as_str());

        let req = helix::chat::SendChatMessageRequest::new();
        let body =
            helix::chat::SendChatMessageBody::new(channel_id, sender_id, outgoing_msg.content);
        match self
            .helix
            .req_post(req.clone(), body.clone(), &self.app_token)
            .await
        {
            Ok(response) => {
                if !response.data.is_sent {
                    error!("Message did not get sent: {:?}", response.data.drop_reason);
                }
                if let Some(msg_id) = response.data.message_id {
                    recently_sent.insert(msg_id);
                }
            }
            Err(err) => match err {
                ClientRequestError::HelixRequestPostError(HelixRequestPostError::Error {
                    status: StatusCode::TOO_MANY_REQUESTS,
                    ..
                }) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    self.helix.req_post(req, body, &self.app_token).await?;
                }
                other => return Err(other.into()),
            },
        }
        Ok(())
    }

    async fn setup_eventsub(&self) -> anyhow::Result<()> {
        info!("Updating EventSub subscriptions");
        let mut channel_ids = self.channel_ids.clone();
        let callback_url = format!("{}/platform/twitch/eventsub", self.base_url);

        let mut current_subs = self.helix.get_eventsub_subscriptions(
            Status::Enabled,
            Some(eventsub::channel::ChannelChatMessageV1::EVENT_TYPE),
            None,
            &self.app_token,
        );

        while let Some(current_sub) = current_subs.next().await.transpose()? {
            for sub in current_sub.subscriptions {
                if sub
                    .transport
                    .try_into_webhook()
                    .is_ok_and(|webhook| webhook.callback == callback_url)
                {
                    let condition: eventsub::channel::ChannelChatMessageV1 =
                        serde_json::from_value(sub.condition)
                            .context("Invalid Twitch EventSub response")?;

                    if let Some(pos) = channel_ids
                        .iter()
                        .position(|id| id == condition.broadcaster_user_id.as_str())
                    {
                        debug!(
                            "Channel {} already has an active EventSub subscription",
                            condition.broadcaster_user_id
                        );
                        channel_ids.remove(pos);
                    }
                }
            }
        }

        let transport =
            eventsub::Transport::webhook(callback_url, self.config.eventsub_secret.clone());
        for channel_id in channel_ids {
            match self
                .helix
                .create_eventsub_subscription(
                    eventsub::channel::ChannelChatMessageV1::new(
                        channel_id.clone(),
                        self.bot_user.id.clone(),
                    ),
                    transport.clone(),
                    &self.app_token,
                )
                .await
            {
                Ok(response) => {
                    info!(
                        "Established subscription to channel {}",
                        response.condition.broadcaster_user_id
                    );
                }
                Err(err) => {
                    error!("Could not establish subscription to channel {channel_id}: {err}",);
                }
            }
        }

        Ok(())
    }
}
