use std::time::Duration;

use futures_util::StreamExt;
use myco::model::{Config, Error, Event, GenAiClient, Message, Request};
use tokio::{net::TcpListener, time::timeout};

fn request() -> Request {
    Request::new("test-model", vec![Message::User("hello".into())], 64)
}

fn client(endpoint: String) -> GenAiClient {
    GenAiClient::new(Config::OpenAi {
        endpoint,
        api_key: "test-key".into(),
    })
    .unwrap()
}

#[tokio::test]
async fn an_unpolled_generation_never_contacts_the_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = client(format!(
        "http://{}/responses",
        listener.local_addr().unwrap()
    ));
    let generation = client.generate(request()).unwrap();
    assert!(
        timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    drop(generation);
    assert!(
        timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn consuming_only_the_request_never_dispatches_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = client(format!(
        "http://{}/responses",
        listener.local_addr().unwrap()
    ));
    let mut generation = client.generate(request()).unwrap();
    let Event::Request { body, .. } = generation.next().await.unwrap().unwrap() else {
        panic!("request must be first");
    };
    assert_eq!(body["input"][0]["content"], "hello");
    assert!(!body.to_string().contains("test-key"));
    assert!(
        timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    drop(generation);
    assert!(
        timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn invalid_options_fail_before_a_stream_is_returned_without_a_runtime() {
    let client = client("http://127.0.0.1:1/responses".into());
    let mut request = request();
    request.driver_options.insert("stream".into(), false.into());
    assert!(matches!(
        client.generate(request),
        Err(Error::InvalidRequest(_))
    ));
}
