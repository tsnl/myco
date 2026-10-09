//! Native requests bind to a single process instance before opening a session.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use uuid::Uuid;

use super::{Error, Server};
use crate::service_protocol::{Identity, Output, Submit};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cursor {
    instance: Uuid,
    #[serde(default)]
    offset: usize,
}

fn validate(server: &Server, id: &str, instance: Uuid) -> Result<(), Error> {
    if instance != server.identity.instance {
        return Err(Error::Conflict("The service instance changed. The previous request's outcome is unknown; inspect the saved session before submitting new work. No request was replayed.".into()));
    }
    if Uuid::parse_str(id).is_err() || id.len() != 32 || id.to_ascii_lowercase() != id {
        return Err(Error::Invalid(
            "Service requests require the full 32-character session id.".into(),
        ));
    }
    Ok(())
}

pub(super) async fn identity(State(server): State<Arc<Server>>) -> Json<Identity> {
    Json(server.identity.clone())
}

pub(super) async fn submit(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Json(input): Json<Submit>,
) -> Result<StatusCode, Error> {
    validate(&server, &id, input.instance)?;
    let app = server.sessions.open(&id).await?;
    tokio::task::spawn_blocking(move || app.accept_service(input, id))
        .await
        .map_err(|error| Error::Internal(error.to_string()))??;
    Ok(StatusCode::ACCEPTED)
}

pub(super) async fn output(
    State(server): State<Arc<Server>>,
    Path((id, request)): Path<(String, Uuid)>,
    Query(cursor): Query<Cursor>,
) -> Result<Json<Output>, Error> {
    validate(&server, &id, cursor.instance)?;
    let app = server.sessions.open(&id).await?;
    app.service_output(server.identity.instance, request, cursor.offset)
        .map(Json)
}

pub(super) async fn cancel(
    State(server): State<Arc<Server>>,
    Path((id, request)): Path<(String, Uuid)>,
    Query(cursor): Query<Cursor>,
) -> Result<StatusCode, Error> {
    validate(&server, &id, cursor.instance)?;
    server.sessions.open(&id).await?.cancel_service(request)?;
    Ok(StatusCode::NO_CONTENT)
}
