use crate::{DbPool, IncomingMessage};
use axum::{
    extract::{Query, State},
    http::{self, StatusCode},
    response::Redirect,
    Extension,
};
use http_body_util::BodyExt;
use serde::Deserialize;
use std::{fmt, sync::Arc};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use twitch_api::eventsub;
use twitch_oauth2::{CsrfToken, Scope, UserTokenBuilder};
use url::Url;

pub async fn eventsub_callback(
    State(platform): State<Arc<super::Twitch>>,
    Extension(message_tx): Extension<mpsc::Sender<IncomingMessage>>,
    request: http::Request<axum::body::Body>,
) -> Result<String, (StatusCode, String)> {
    let (parts, body) = request.into_parts();
    let body = body
        .collect()
        .await
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?
        .to_bytes();
    let request = http::Request::from_parts(parts, body);

    let valid =
        eventsub::Event::verify_payload(&request, platform.config.eventsub_secret.as_bytes());
    if !valid {
        return Err((StatusCode::FORBIDDEN, "Invalid signature".to_owned()));
    }

    match eventsub::Event::parse_http(&request) {
        Ok(event) => match event {
            eventsub::Event::ChannelChatMessageV1(payload) => match payload.message {
                eventsub::Message::Notification(notification) => {
                    if let Err(err) = platform.handle_message(notification, message_tx).await {
                        error!("Could not handle message: {err}");
                    }
                    Ok(String::new())
                }
                eventsub::Message::VerificationRequest(verification) => Ok(verification.challenge),
                other => {
                    warn!("Got unexpected message {other:?}, skipping",);
                    Ok(String::new())
                }
            },
            other => {
                warn!(
                    "Got unexpected EventSub notification {:?}, skipping",
                    other.subscription()
                );
                Ok(String::new())
            }
        },
        Err(err) => {
            warn!("Got invalid EventSub event: {err}");
            Err((StatusCode::BAD_REQUEST, "Invalid payload".to_owned()))
        }
    }
}

#[derive(Deserialize)]
pub struct AuthenticateParams {
    pub mode: AuthenticationMode,
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum AuthenticationMode {
    Channel,
    User,
}

impl fmt::Display for AuthenticationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self, f)
    }
}

pub async fn auth(
    Query(params): Query<AuthenticateParams>,
    State(platform): State<Arc<super::Twitch>>,
) -> Redirect {
    let redirect_url = Url::parse(&format!(
        "{}/platform/twitch/auth/redirect",
        platform.base_url
    ))
    .unwrap();

    let scopes = match params.mode {
        AuthenticationMode::Channel => vec![Scope::ChannelBot],
        AuthenticationMode::User => vec![Scope::UserBot, Scope::UserReadChat, Scope::UserWriteChat],
    };

    let mut builder = UserTokenBuilder::new(
        platform.config.client_id.clone(),
        platform.config.client_secret.clone(),
        redirect_url,
    )
    .set_scopes(scopes);

    let (url, csrf_token) = builder.generate_url();

    platform
        .csrf_tokens
        .lock()
        .unwrap()
        .insert(csrf_token, builder);

    Redirect::to(url.as_str())
}

#[derive(Deserialize)]
pub struct AuthRedirectParams {
    pub state: String,
    pub error_description: Option<String>,
    pub code: Option<String>,
    pub scope: Option<String>,
}

pub async fn auth_redirect(
    Query(params): Query<AuthRedirectParams>,
    Extension(db): Extension<DbPool>,
    State(platform): State<Arc<super::Twitch>>,
) -> (StatusCode, String) {
    if let Some(err) = params.error_description {
        return (StatusCode::UNPROCESSABLE_ENTITY, err);
    }

    let given_token = CsrfToken::new(params.state);

    let Some(builder) = platform.csrf_tokens.lock().unwrap().remove(&given_token) else {
        return (
            StatusCode::UNAUTHORIZED,
            "Invalid state provided".to_owned(),
        );
    };
    let (Some(code), Some(scopes)) = (params.code, params.scope) else {
        return (
            StatusCode::BAD_REQUEST,
            "missing code or scopes param".to_owned(),
        );
    };

    match builder
        .get_user_token(&platform.helix, given_token.as_str(), &code)
        .await
    {
        Ok(user_token) => {
            let refresh_token = user_token.refresh_token.expect("Missing refresh token");
            let refresh_token_str = refresh_token.as_str();
            let access_token = user_token.access_token.as_str();
            let user_id = user_token.user_id.as_str();

            sqlx::query!(
                "
                INSERT INTO twitch_login(user_id, access_token, refresh_token, scopes) 
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(user_id) DO UPDATE
                SET access_token = ?2, refresh_token = ?3, scopes = ?4",
                user_id,
                access_token,
                refresh_token_str,
                scopes,
            )
            .execute(&db)
            .await
            .expect("DB error");
            info!("Saved auth for user '{}'", user_token.login);

            tokio::spawn(async move {
                if let Err(err) = platform.setup_eventsub().await {
                    error!("Could not reconfigure EventSub: {err:#}");
                }
            });
            (StatusCode::OK, "Authentication succesful".to_owned())
        }
        Err(err) => {
            warn!("Could not trade token: {err}");
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Could not trade token".to_owned(),
            )
        }
    }
}
