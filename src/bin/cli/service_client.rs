//! Each operation is bound to the handshake instance; transport retries retain identity.

use std::time::Duration;

use reqwest::{Client, Response};
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::service_protocol::{Identity, Output, Submit};

pub(super) struct Service {
    http: Client,
    base: String,
    session: String,
    pub(super) identity: Identity,
}

impl Service {
    pub(super) async fn connect(base: &str, session: &str) -> Result<Self, String> {
        let (base, profile) = endpoint(base)?;
        if Uuid::parse_str(session).is_err()
            || session.len() != 32
            || session.to_ascii_lowercase() != session
        {
            return Err("--server requires --resume with the full 32-character session id.".into());
        }
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| e.to_string())?;
        let identity: Identity = decode(
            http.get(format!("{base}/api/service"))
                .send()
                .await
                .map_err(|e| {
                    format!("Cannot reach service: {e}. No local fallback was started.")
                })?,
        )
        .await?;
        identity.validate()?;
        if identity.profile != profile {
            return Err(
                "The service reported an unexpected profile. No local fallback was started.".into(),
            );
        }
        Ok(Self {
            http,
            base,
            session: session.into(),
            identity,
        })
    }

    pub(super) async fn image_limit(&self) -> Result<u64, String> {
        let snapshot: serde_json::Value = decode(
            self.http
                .get(format!("{}/api/sessions/{}", self.base, self.session))
                .send()
                .await
                .map_err(|e| e.to_string())?,
        )
        .await?;
        snapshot["change"]["snapshot"]["attachment_limits"]["max_image_base64_bytes"]
            .as_u64()
            .ok_or_else(|| "Service returned an invalid session snapshot.".into())
    }

    fn turn(&self, id: Uuid) -> String {
        format!(
            "{}/api/service/sessions/{}/turns/{id}",
            self.base, self.session
        )
    }

    pub(super) async fn submit(&self, input: &Submit) -> Result<(), String> {
        let url = format!("{}/api/service/sessions/{}/turns", self.base, self.session);
        let mut last = String::new();
        for _ in 0..3 {
            match self.http.post(&url).json(input).send().await {
                Ok(response) if response.status().as_u16() == 202 => return Ok(()),
                Ok(response) if !response.status().is_server_error() => {
                    return Err(failure(response).await);
                }
                Ok(response) => last = failure(response).await,
                Err(error) => last = error.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        Err(format!(
            "Service acceptance is uncertain: {last}. Reconnect with the printed token; do not submit the prompt again."
        ))
    }

    pub(super) async fn output(&self, id: Uuid, offset: usize) -> Result<Output, String> {
        let url = format!(
            "{}/output?instance={}&offset={offset}",
            self.turn(id),
            self.identity.instance
        );
        let mut last = String::new();
        for _ in 0..3 {
            match self.http.get(&url).send().await {
                Ok(response) if response.status().is_success() => return decode(response).await.map_err(|error| format!("{error}. The turn may still be running; reconnect with the printed token.")),
                Ok(response) if !response.status().is_server_error() => {
                    return Err(failure(response).await);
                }
                Ok(response) => last = failure(response).await,
                Err(error) => last = error.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        Err(format!(
            "Lost service connection: {last}. The turn may still be running; reconnect with the printed token."
        ))
    }

    pub(super) async fn cancel(&self, id: Uuid) -> Result<(), String> {
        let response = self
            .http
            .post(format!(
                "{}/cancel?instance={}",
                self.turn(id),
                self.identity.instance
            ))
            .send()
            .await
            .map_err(|e| {
                format!("Could not confirm cancellation: {e}. The turn may still be running.")
            })?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(failure(response).await)
        }
    }
}

fn endpoint(value: &str) -> Result<(String, String), String> {
    let url = url::Url::parse(value).map_err(|e| format!("Invalid --server URL: {e}"))?;
    let loopback = match url.host() {
        Some(url::Host::Domain("localhost")) => true,
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    };
    let path = url.path().trim_end_matches('/');
    let profile = path
        .strip_prefix("/profiles/")
        .and_then(|name| myco::core::validate_profile(name).ok());
    if url.scheme() != "http"
        || !loopback
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || profile.is_none()
    {
        return Err("--server requires a loopback URL including /profiles/NAME, without credentials, query, or fragment. Use an SSH tunnel for remote access.".into());
    }
    Ok((
        format!("{}{path}", url.origin().ascii_serialization()),
        profile.unwrap(),
    ))
}

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T, String> {
    if !response.status().is_success() {
        return Err(failure(response).await);
    }
    response
        .json()
        .await
        .map_err(|e| format!("Invalid service response: {e}"))
}

async fn failure(response: Response) -> String {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    format!("Service returned {status}: {body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_requires_an_explicit_profile_on_a_loopback_origin() {
        assert_eq!(
            endpoint("http://localhost:8765/profiles/work/").unwrap(),
            ("http://localhost:8765/profiles/work".into(), "work".into())
        );
        for url in [
            "http://localhost:8765",
            "http://example.com/profiles/work",
            "http://localhost/profiles/work?token=x",
            "http://user@localhost/profiles/work",
            "http://localhost/profiles/work/other",
        ] {
            assert!(endpoint(url).is_err(), "{url}");
        }
    }
}
