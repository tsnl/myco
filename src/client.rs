//! [`MycoApi`] over HTTP: point [`HttpClient`] at a running `myco --mode
//! serve` and hold it exactly like the in-process server object — same trait,
//! same types, the work just happens on the other side of a socket.

use futures::StreamExt;
use myco_api::{ApiError, ErrorKind, EventStream, MycoApi};

use myco_api as api;

pub struct HttpClient {
    /// e.g. `http://127.0.0.1:7773/api` (no trailing slash).
    base: String,
    /// Bearer token identifying this client; the server attributes every
    /// entry it writes to whoever the token belongs to.
    token: String,
    http: reqwest::Client,
}

impl HttpClient {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token: token.into(),
            http: reqwest::Client::new(),
        }
    }

    /// From `$MYCO_API` / `$MYCO_API_TOKEN`, as exported by `--mode serve`
    /// into the environment tools run in.
    pub fn from_env() -> Result<Self, String> {
        let base = std::env::var("MYCO_API").map_err(|_| "MYCO_API is not set".to_string())?;
        let token =
            std::env::var("MYCO_API_TOKEN").map_err(|_| "MYCO_API_TOKEN is not set".to_string())?;
        Ok(Self::new(base, token))
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.bearer_auth(&self.token)
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T, ApiError> {
        if resp.status().is_success() {
            resp.json::<T>()
                .await
                .map_err(|e| ApiError::new(ErrorKind::Internal, format!("decode: {e}")))
        } else {
            let status = resp.status();
            match resp.json::<ApiError>().await {
                Ok(e) => Err(e),
                Err(_) => Err(ApiError::new(ErrorKind::Internal, format!("http {status}"))),
            }
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        self.send(self.http.get(format!("{}{path}", self.base)))
            .await
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<T, ApiError> {
        Self::decode(self.auth(req).send().await.map_err(transport)?).await
    }
}

fn transport(e: reqwest::Error) -> ApiError {
    ApiError::new(ErrorKind::Internal, format!("transport: {e}"))
}

#[async_trait::async_trait]
impl MycoApi for HttpClient {
    async fn list_sessions(
        &self,
        include_archived: bool,
    ) -> Result<Vec<api::SessionSummary>, ApiError> {
        self.get(&format!("/sessions?include_archived={include_archived}"))
            .await
    }

    async fn create_session(
        &self,
        req: api::CreateSession,
    ) -> Result<api::SessionSummary, ApiError> {
        self.send(self.http.post(format!("{}/sessions", self.base)).json(&req))
            .await
    }

    async fn session_detail(&self, id: &str) -> Result<api::SessionDetail, ApiError> {
        self.get(&format!("/sessions/{id}")).await
    }

    async fn update_session(
        &self,
        id: &str,
        req: api::UpdateSession,
    ) -> Result<api::SessionSummary, ApiError> {
        self.send(
            self.http
                .patch(format!("{}/sessions/{id}", self.base))
                .json(&req),
        )
        .await
    }

    async fn post_message(&self, id: &str, req: api::PostMessage) -> Result<api::Poll, ApiError> {
        self.send(
            self.http
                .post(format!("{}/sessions/{id}/messages", self.base))
                .json(&req),
        )
        .await
    }

    async fn poll(&self, id: &str, since: usize) -> Result<api::Poll, ApiError> {
        self.get(&format!("/sessions/{id}/poll?since={since}"))
            .await
    }

    async fn events(&self, id: &str) -> Result<EventStream, ApiError> {
        let resp = self
            .auth(self.http.get(format!("{}/sessions/{id}/events", self.base)))
            .send()
            .await
            .map_err(transport)?;
        if !resp.status().is_success() {
            let status = resp.status();
            return match resp.json::<ApiError>().await {
                Ok(e) => Err(e),
                Err(_) => Err(ApiError::new(ErrorKind::Internal, format!("http {status}"))),
            };
        }
        // Parse the SSE byte stream: accumulate lines, emit on `data:`.
        let stream = resp
            .bytes_stream()
            .scan(String::new(), |buf, chunk| {
                let mut out: Vec<api::StreamEvent> = Vec::new();
                if let Ok(chunk) = chunk {
                    buf.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(nl) = buf.find('\n') {
                        let line: String = buf.drain(..=nl).collect();
                        let line = line.trim_end();
                        if let Some(data) = line.strip_prefix("data:")
                            && let Ok(ev) =
                                serde_json::from_str::<api::StreamEvent>(data.trim_start())
                        {
                            out.push(ev);
                        }
                    }
                }
                futures::future::ready(Some(out))
            })
            .flat_map(futures::stream::iter);
        Ok(Box::pin(stream))
    }

    async fn cancel(&self, id: &str) -> Result<api::Poll, ApiError> {
        self.send(
            self.http
                .post(format!("{}/sessions/{id}/cancel", self.base)),
        )
        .await
    }

    async fn compact(&self, id: &str) -> Result<api::Poll, ApiError> {
        self.send(
            self.http
                .post(format!("{}/sessions/{id}/compact", self.base)),
        )
        .await
    }

    async fn retire(&self, id: &str) -> Result<api::Poll, ApiError> {
        self.send(
            self.http
                .delete(format!("{}/sessions/{id}/live", self.base)),
        )
        .await
    }

    async fn models(&self) -> Result<api::Models, ApiError> {
        self.get("/models").await
    }

    async fn whoami(&self) -> Result<api::Identity, ApiError> {
        self.get("/whoami").await
    }

    async fn shells(&self, id: &str) -> Result<api::Shells, ApiError> {
        self.get(&format!("/sessions/{id}/shells")).await
    }

    async fn shell_tail(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        from: u64,
    ) -> Result<api::ShellTailChunk, ApiError> {
        self.get(&format!("/sessions/{id}/shells/{host}/{shell}?from={from}"))
            .await
    }

    async fn shell_input(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        data: String,
    ) -> Result<api::Shell, ApiError> {
        self.send(
            self.http
                .post(format!(
                    "{}/sessions/{id}/shells/{host}/{shell}/input",
                    self.base
                ))
                .json(&api::ShellInput { data }),
        )
        .await
    }

    async fn shell_lock(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        lock: api::ShellLockMode,
    ) -> Result<api::Shell, ApiError> {
        self.send(
            self.http
                .post(format!(
                    "{}/sessions/{id}/shells/{host}/{shell}/lock",
                    self.base
                ))
                .json(&api::ShellLockRequest { lock }),
        )
        .await
    }

    async fn shell_screen(
        &self,
        id: &str,
        host: &str,
        shell: &str,
    ) -> Result<api::ShellScreen, ApiError> {
        self.get(&format!("/sessions/{id}/shells/{host}/{shell}/screen"))
            .await
    }
}
