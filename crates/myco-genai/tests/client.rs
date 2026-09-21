use std::future::ready;

use myco_genai::{Client, Config, Error, Event, Message, Request};

fn request() -> Request {
    Request::new("test-model", vec![Message::User("hello".into())], 64)
}

fn client(endpoint: String) -> Client {
    Client::new(Config::OpenAi {
        endpoint,
        api_key: "test-key".into(),
    })
    .unwrap()
}

#[tokio::test]
async fn observer_failure_prevents_request_dispatch() {
    let client = client("http://127.0.0.1:1/responses".into());
    let result = client
        .generate(request(), |event| {
            let Event::Request { body, .. } = event else {
                panic!("request must be first")
            };
            assert_eq!(body["input"][0]["content"], "hello");
            ready(Err("could not record request".into()))
        })
        .await;
    assert!(
        matches!(result, Err(Error::Observer(error)) if error.to_string() == "could not record request")
    );
}

#[tokio::test]
async fn awaiting_request_observation_does_not_open_a_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = client(format!(
        "http://{}/responses",
        listener.local_addr().unwrap()
    ));
    let (entered, observed) = tokio::sync::oneshot::channel();
    let mut entered = Some(entered);
    let task = tokio::spawn(async move {
        client
            .generate(request(), move |_| {
                entered.take().unwrap().send(()).unwrap();
                std::future::pending()
            })
            .await
    });
    observed.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn an_unpolled_generation_does_not_call_the_observer() {
    let client = client("http://127.0.0.1:1/responses".into());
    let mut count = 0;
    let generation = client.generate(request(), |_| {
        count += 1;
        ready(Ok(()))
    });
    drop(generation);
    assert_eq!(count, 0);
}
