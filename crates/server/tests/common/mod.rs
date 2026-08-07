//! Shared fixtures for the server's integration tests: the counter kind
//! (so nothing depends on a shell) and a signed-in router.

use std::sync::Arc;

use myco_instance::{Instance, Kind, KindSpec, Pool, VerbError, VerbSpec};
use myco_runtime::Signals;
use myco_server::auth::AuthStore;
use serde_json::{Value, json};

pub static COUNTER_SPEC: KindSpec = KindSpec {
    kind: "counter",
    version: 1,
    doc: "a number that goes up",
    verbs: &[
        VerbSpec::driven("incr", "add {by} (default 1)"),
        VerbSpec::read("get", "the current value"),
        VerbSpec::read("text", "the value as plain text"),
    ],
    primary_render: "get",
    recommended_context: "text",
};

pub struct CounterKind;
pub struct Counter(i64);

#[async_trait::async_trait]
impl Instance for Counter {
    async fn verb(
        &mut self,
        verb: &str,
        args: Value,
        signals: &Signals,
    ) -> Result<Value, VerbError> {
        match verb {
            "incr" => {
                self.0 += args.get("by").and_then(Value::as_i64).unwrap_or(1);
                signals.bump();
                Ok(json!(self.0))
            }
            "get" => Ok(json!(self.0)),
            "text" => Ok(json!(self.0.to_string())),
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

impl Kind for CounterKind {
    fn spec(&self) -> &'static KindSpec {
        &COUNTER_SPEC
    }
    fn create(&self, args: Value, _: Signals) -> Result<Box<dyn Instance>, VerbError> {
        Ok(Box::new(Counter(
            args.get("start").and_then(Value::as_i64).unwrap_or(0),
        )))
    }
}

/// A router over a counter pool, plus the pool handle and a live bearer
/// token for ada.
pub fn counter_app() -> (axum::Router, Pool, String) {
    let auth = Arc::new(AuthStore::in_memory());
    auth.add_user("ada", "Ada Lovelace").unwrap();
    let token = auth.issue_for("ada").unwrap().access_token;
    let pool = Pool::new();
    pool.register(Arc::new(CounterKind));
    (myco_server::router(pool.clone(), auth), pool, token)
}
