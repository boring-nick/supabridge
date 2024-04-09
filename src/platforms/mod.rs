mod factorio;
mod twitch;

pub use factorio::Factorio;
pub use twitch::Twitch;

use crate::{config::Config, DbPool, IncomingMessage, OutgoingMessage};
use axum::Router;
use futures::Future;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;

pub trait ChatPlatform: 'static + Sized + Send {
    const NAME: &'static str;
    type Config: DeserializeOwned;

    async fn new(
        config: Self::Config,
        bridge_config: &Config,
        channel_ids: Vec<String>,
        db: &DbPool,
    ) -> anyhow::Result<Self>;

    fn api_routes(&mut self) -> Router {
        Router::new()
    }

    fn run(
        self,
        incoming_message_tx: mpsc::Sender<IncomingMessage>,
        outgoing_message_rx: mpsc::Receiver<OutgoingMessage>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    fn supports_zws() -> bool {
        true
    }
}
