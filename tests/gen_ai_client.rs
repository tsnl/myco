use std::time::Duration;

use futures_util::StreamExt;
use myco::blob::{Blob, BlobStore, MediaType};
use myco::gen_ai::{
    Config, Error, Event, GenAiClient, InputContentPart, Message, MessageKind, Request,
};
use tokio::{net::TcpListener, time::timeout};

fn request() -> Request {
    Request {
        model: "test-model".into(),
        messages: vec![Message::new(MessageKind::User {
            content: vec![InputContentPart::Text {
                content: "hello".into(),
            }],
        })],
        max_output_tokens: 64,
        ..Default::default()
    }
}

fn client(endpoint: String) -> GenAiClient {
    GenAiClient::new(
        Config::OpenAiResponses {
            endpoint,
            api_key: "test-key".into(),
        },
        BlobStore::default(),
    )
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

#[test]
fn dropping_a_generation_and_client_preserves_blobs_for_other_handles() {
    let client = client("http://127.0.0.1:1/responses".into());
    let store = client.blobs().clone();
    let blob = Blob {
        media_type: MediaType::Png,
        data: vec![0, 1, 2].into(),
    };
    let reference = store.insert(blob.clone()).unwrap();
    let mut input = request();
    input.messages = vec![Message::new(MessageKind::User {
        content: vec![InputContentPart::Image { blob: reference }],
    })];
    let generation = client.generate(input).unwrap();
    drop(generation);
    drop(client);
    assert_eq!(store.get(reference), Ok(blob));
}
