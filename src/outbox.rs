use anyhow::{anyhow, Result};
use axum::{
    extract::{Path, State},
    headers::{authorization::Bearer, Authorization},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json, TypedHeader,
};
use chrono::{TimeZone, Utc};
use rusqlite::ErrorCode;
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{SystemTime, SystemTimeError, UNIX_EPOCH},
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    actor::validate,
    object::create_object,
    protocol::{Created, JsonLD, ACTIVITIES},
    AppState,
};

#[derive(Error, Debug)]
pub enum OutboxError {
    #[error("internal outbox error: {0}")]
    Internal(#[from] anyhow::Error),
    #[error("outbox operation not authorized: {0}")]
    AuthFailed(anyhow::Error),
    #[error("outbox activity {0} already exists")]
    Duplicate(String),
    #[error("outbox activity id {0} is invalid")]
    Invalid(String),
}

impl IntoResponse for OutboxError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            OutboxError::Duplicate(id) => (
                StatusCode::BAD_REQUEST,
                format!("activity `{id}` already exists"),
            ),
            OutboxError::Internal(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
            OutboxError::Invalid(err) => (StatusCode::BAD_REQUEST, err),
            OutboxError::AuthFailed(err) => (StatusCode::UNAUTHORIZED, err.to_string()),
        };
        tracing::error!(error_message);
        let body = Json(json!({
            "error": error_message,
        }));

        (status, body).into_response()
    }
}

impl From<SystemTimeError> for OutboxError {
    fn from(value: SystemTimeError) -> Self {
        OutboxError::Internal(anyhow!(value))
    }
}
/*
impl From<ResourceError> for OutboxError {
    fn from(value: ResourceError) -> Self {
        OutboxError::Internal(value.into())
    }
}
 */

pub(crate) async fn post_outbox(
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    JsonLD(mut new_activity): JsonLD<Value>,
) -> Result<Created, OutboxError> {
    let token_username = validate(&state, auth.token())
        .map_err(OutboxError::AuthFailed)?
        .username;
    if token_username != username {
        return Err(OutboxError::AuthFailed(anyhow!(
            "can only post to own outbox"
        )));
    }
    let short_id = Uuid::new_v4();
    let activity_type = match new_activity.pointer("/type").and_then(|v| v.as_str()) {
        Some(activity) => activity.to_string(),
        None => {
            return Err(OutboxError::Invalid(String::from(
                "no `type` found in activity",
            )))
        }
    };
    if !ACTIVITIES.contains(&activity_type.as_str()) {
        let mut wrap_activity = json!({
            "object": new_activity,
            "type": "Create",
        });
        copy(
            &new_activity,
            &mut wrap_activity,
            &["to", "bto", "cc", "bcc"],
        );
        new_activity = wrap_activity;
    }
    add(
        &mut new_activity,
        "id",
        format!(
            "https://{}/actors/{username}/activities/{short_id}",
            state.base
        ),
    )?;
    let iat = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let activity_state =
        preprocess_activity(&state, &username, &mut new_activity, &activity_type, iat)?;
    /*

    */

    let conn = &state.conn()?;
    match conn.execute(
        "INSERT INTO Outbox VALUES (?1, ?2, ?3, ?4, ?5)",
        (&username, &short_id, &activity_type, iat, &new_activity),
    ) {
        Ok(_) => {}
        Err(rusqlite::Error::SqliteFailure(err, msg))
            if err.code == ErrorCode::ConstraintViolation
                && matches!(&msg,Some(msg) if msg.contains("UNIQUE") && msg.contains("Outbox")) =>
        {
            return Err(OutboxError::Duplicate(short_id.to_string()))
        }
        Err(err) => {
            return Err(OutboxError::Internal(anyhow!(err)));
        }
    }
    postprocess_activity(&state, &username, &mut new_activity, &activity_state, iat)?;

    Ok(Created::new(activity_state.location()))
}

fn preprocess_activity(
    state: &AppState,
    username: &str,
    activity: &mut Value,
    activity_type: &str,
    iat: u64,
) -> Result<ActivityState, OutboxError> {
    let ts = Utc.timestamp_opt(iat as i64, 0).unwrap().to_rfc3339();
    let actor = format!("https://{}/actors/{username}", state.base);
    add(activity, "published", ts.clone())?;
    add(activity, "actor", actor.clone())?;
    if activity_type == "Create" {
        let object_type = match activity.pointer("/object/type").and_then(|v| v.as_str()) {
            Some(activity) => activity.to_string(),
            None => {
                return Err(OutboxError::Invalid(String::from(
                    "no `/object/type` found in activity",
                )));
            }
        };

        let object_short_id = Uuid::new_v4();
        let object_id = format!(
            "https://{}/actors/{username}/objects/{}/{object_short_id}",
            state.base,
            object_type.to_lowercase()
        );
        match activity.pointer_mut("/object") {
            Some(obj) => {
                add(obj, "published", ts)?;
                add(obj, "id", object_id.clone())?;
                add(obj, "attributedTo", actor)?;
            }
            None => {
                return Err(OutboxError::Invalid(String::from(
                    "no `/object` JSON object found in activity",
                )))
            }
        };
        return Ok(ActivityState::Create {
            object_type,
            object_short_id,
            object_id,
        });
    }
    Ok(ActivityState::Other)
}

fn postprocess_activity(
    state: &AppState,
    username: &str,
    activity: &mut Value,
    activity_state: &ActivityState,
    iat: u64,
) -> Result<()> {
    match activity_state {
        ActivityState::Create {
            object_short_id,
            object_type,
            ..
        } if activity.pointer("/object").is_some() => {
            create_object(
                state,
                username,
                object_short_id,
                object_type,
                activity.pointer("/object").unwrap(),
                iat,
            )?;
        }
        _ => {}
    }
    Ok(())
}

pub enum ActivityState {
    Other,
    Create {
        object_type: String,
        object_short_id: Uuid,
        object_id: String,
    },
}

impl ActivityState {
    fn location(self) -> Option<String> {
        match self {
            ActivityState::Create { object_id, .. } => Some(object_id),
            ActivityState::Other => None,
        }
    }
}

fn copy(from: &Value, to: &mut Value, fields: &[&str]) {
    for field in fields {
        if let Some(value) = from.get(field) {
            to.as_object_mut()
                .unwrap()
                .insert(field.to_string(), value.clone());
        }
    }
}

fn add<T>(to: &mut Value, key: &str, value: T) -> Result<(), OutboxError>
where
    T: Into<Value>,
{
    match to.as_object_mut() {
        Some(map) => map.insert(key.to_string(), value.into()),
        None => {
            return Err(OutboxError::Invalid(String::from(
                "activity not a JSON object",
            )))
        }
    };
    Ok(())
}