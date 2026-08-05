//! When the agent speaks, and when it stays out of the way.
//!
//! A session with one person in it is a private line: everything said there is
//! said to the agent, exactly as before. A session with two people in it is a
//! room, and an agent that answers every line talks over the people using it.
//! The rule is explicit address — these tests pin both halves, over HTTP, with
//! two real token holders.

use std::sync::Arc;

use myco::server::Server;
use myco_api::{Author, EntryBody};
use myco_api::{Content, TurnEndReason};
use myco_auth::AuthStore;
use myco_models::{GenerateOutput, GenerativeModel};
use myco_test_support::ScriptedModel;
use rocket::http::{ContentType, Header, Status};
use rocket::local::asynchronous::Client;

const CONFIG_TOML: &str = r#"
model = "fake"

[gateways.g]
protocol = "openai-completions"
base_url = "http://127.0.0.1:9/v1"
auth = "dummy"

[models.fake]
gateway = "g"
context_window = 100000
"#;

const ADA_PASSWORD: &str = "ada's long enough password";
const GRACE_PASSWORD: &str = "grace's long enough password";

const ROSTER_TOML: &str = r#"
[[users]]
id = "ada"
name = "Ada Lovelace"

[[users]]
id = "grace"
name = "Grace Hopper"
"#;

fn test_server_with(factory: myco::server::ModelFactory) -> Arc<Server> {
    let config = myco::config::Config::resolve_with(
        Default::default(),
        |k| (k == "USER").then(|| "ada".to_string()),
        |_, _| myco::config::parse_file_config_str(CONFIG_TOML),
        |_| myco::config::parse_file_roster_str(ROSTER_TOML),
        || Ok(Vec::new()),
        |_| Err("no auth files in tests".into()),
    )
    .expect("test config resolves");
    let auth = Arc::new(AuthStore::in_memory().with_work_factor(1));
    let server = Server::with_model_factory_and_auth(config, factory, auth);
    server
        .auth()
        .set_password("ada", ADA_PASSWORD)
        .expect("chpass");
    server
        .auth()
        .set_password("grace", GRACE_PASSWORD)
        .expect("chpass");
    server
}

fn test_server() -> Arc<Server> {
    test_server_with(Box::new(|_, _, _, _| {
        // Every reply is distinguishable from a user message, which is all
        // these tests need to count turns.
        let reply = || GenerateOutput {
            content: vec![Content::Text {
                text: "agent reply".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        };
        Ok(ScriptedModel::new(vec![reply(), reply(), reply(), reply()])
            as Arc<dyn GenerativeModel>)
    }))
}

async fn client() -> Client {
    let figment = rocket::Config::figment().merge(("log_level", "off"));
    Client::tracked(myco::web::rocket(test_server(), figment))
        .await
        .expect("rocket builds")
}

fn bearer(token: &str) -> Header<'static> {
    Header::new("Authorization", format!("Bearer {token}"))
}

async fn login(c: &Client, id: &str, password: &str) -> String {
    let resp = c
        .post("/api/auth/token")
        .header(ContentType::Form)
        .body(format!(
            "grant_type=password&username={id}&password={password}"
        ))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok, "login for {id}");
    let token: myco_api::AccessToken = resp.into_json().await.expect("token response");
    token.access_token
}

async fn new_session(c: &Client, token: &str) -> String {
    let created = c
        .post("/api/sessions")
        .header(bearer(token))
        .json(&myco_api::CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .dispatch()
        .await;
    assert_eq!(created.status(), Status::Ok);
    let summary: myco_api::SessionSummary = created.into_json().await.expect("summary");
    summary.id
}

/// Post as `token`, returning the server's own verdict: `busy` is true exactly
/// when the message woke the agent.
async fn post(c: &Client, token: &str, id: &str, text: &str) -> bool {
    let resp = c
        .post(format!("/api/sessions/{id}/messages"))
        .header(bearer(token))
        .json(&myco_api::PostMessage { text: text.into() })
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok, "post {text:?}");
    let poll: myco_api::Poll = resp.into_json().await.expect("poll");
    poll.busy
}

async fn poll(c: &Client, token: &str, id: &str) -> myco_api::Poll {
    let resp = c
        .get(format!("/api/sessions/{id}/poll?since=0"))
        .header(bearer(token))
        .dispatch()
        .await;
    assert_eq!(resp.status(), Status::Ok);
    resp.into_json().await.expect("poll")
}

/// Wait for the session to go idle with `pred` satisfied.
async fn settle(
    c: &Client,
    token: &str,
    id: &str,
    pred: impl Fn(&myco_api::Poll) -> bool,
) -> myco_api::Poll {
    for _ in 0..200 {
        let p = poll(c, token, id).await;
        if !p.busy && pred(&p) {
            return p;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("session never settled");
}

fn agent_replies(p: &myco_api::Poll) -> usize {
    p.entries
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Agent { .. }))
        .count()
}

fn user_messages(p: &myco_api::Poll) -> Vec<(&str, String)> {
    p.entries
        .iter()
        .filter_map(|e| match (&e.author, &e.body) {
            (Author::User { id, .. }, EntryBody::User { content }) => {
                Some((id.as_str(), myco_api::content_text(content)))
            }
            _ => None,
        })
        .collect()
}

/// Nobody else has posted, so there is nobody else to be talking to. The
/// pre-multiplayer behaviour is the behaviour: say anything, get an answer.
#[tokio::test]
async fn a_solo_session_answers_without_being_addressed() {
    let _home = myco_test_support::temp_home("mp-solo");
    let c = client().await;
    let ada = login(&c, "ada", ADA_PASSWORD).await;
    let id = new_session(&c, &ada).await;

    assert!(
        post(&c, &ada, &id, "hello").await,
        "a message in a solo session wakes the agent"
    );
    let p = settle(&c, &ada, &id, |p| agent_replies(p) == 1).await;
    assert_eq!(agent_replies(&p), 1);
}

/// The claim: once two people are in the room, a line between them is not a
/// prompt. It is recorded, it is not answered.
#[tokio::test]
async fn the_agent_does_not_interject_when_users_talk_to_each_other() {
    let _home = myco_test_support::temp_home("mp-quiet");
    let c = client().await;
    let ada = login(&c, "ada", ADA_PASSWORD).await;
    let grace = login(&c, "grace", GRACE_PASSWORD).await;
    let id = new_session(&c, &ada).await;

    // Ada opens the session alone, so this one is for the agent.
    assert!(post(&c, &ada, &id, "getting started").await);
    settle(&c, &ada, &id, |p| agent_replies(p) == 1).await;

    // Grace joins. From here the session is shared.
    assert!(
        !post(&c, &grace, &id, "@ada did you see the build?").await,
        "a message addressed to a person must not wake the agent"
    );
    assert!(
        !post(&c, &ada, &id, "yes, looking now").await,
        "a reply between people must not wake the agent"
    );

    // Both messages are recorded, in order, attributed — and unanswered.
    let p = settle(&c, &ada, &id, |p| user_messages(p).len() == 3).await;
    assert_eq!(
        agent_replies(&p),
        1,
        "the agent answered a message that was not for it: {:?}",
        p.entries
    );
    let msgs = user_messages(&p);
    assert_eq!(msgs[1].0, "grace");
    assert_eq!(msgs[1].1, "@ada did you see the build?");
    assert_eq!(msgs[2].0, "ada");
    assert_eq!(msgs[2].1, "yes, looking now");
}

/// The other half: in that same room, naming the agent still reaches it, and
/// it can see what it overheard while it was quiet.
#[tokio::test]
async fn naming_the_agent_in_a_shared_session_wakes_it() {
    let _home = myco_test_support::temp_home("mp-address");
    let c = client().await;
    let ada = login(&c, "ada", ADA_PASSWORD).await;
    let grace = login(&c, "grace", GRACE_PASSWORD).await;
    let id = new_session(&c, &ada).await;

    assert!(post(&c, &ada, &id, "opening").await);
    settle(&c, &ada, &id, |p| agent_replies(p) == 1).await;

    assert!(!post(&c, &grace, &id, "@ada one sec").await);
    settle(&c, &ada, &id, |p| user_messages(p).len() == 2).await;

    assert!(
        post(&c, &grace, &id, "@myco what do you make of it?").await,
        "naming the agent wakes it"
    );
    let p = settle(&c, &ada, &id, |p| agent_replies(p) == 2).await;

    // The quiet message is in the history the agent was given, ahead of the
    // one that woke it: overheard, not lost.
    let msgs = user_messages(&p);
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1].1, "@ada one sec");
    assert_eq!(msgs[2].1, "@myco what do you make of it?");
}

/// The model key the session is running as is an address too, so `@fake`
/// reaches this session's agent — and a different model's key does not.
#[tokio::test]
async fn the_model_key_addresses_the_agent_but_another_key_does_not() {
    let _home = myco_test_support::temp_home("mp-modelkey");
    let c = client().await;
    let ada = login(&c, "ada", ADA_PASSWORD).await;
    let grace = login(&c, "grace", GRACE_PASSWORD).await;
    let id = new_session(&c, &ada).await;

    assert!(post(&c, &ada, &id, "opening").await);
    settle(&c, &ada, &id, |p| agent_replies(p) == 1).await;

    // Grace makes it a room, then addresses a model that is not this one.
    assert!(!post(&c, &grace, &id, "@haiku are you there?").await);
    // ...and then the one that is.
    assert!(post(&c, &grace, &id, "@fake are you there?").await);
    let p = settle(&c, &ada, &id, |p| agent_replies(p) == 2).await;
    assert_eq!(user_messages(&p).len(), 3);
}

/// The other half of a shared session: everyone has to *see* the message.
///
/// The feed carries it as its own record — author, text, timestamp — and
/// carries it whether or not the agent will answer. That is what lets a client
/// place a person's message in the transcript on its own terms rather than
/// appending text to whatever the agent last said, and it is why a message
/// between two people is not invisible to a third.
#[tokio::test]
async fn a_message_reaches_watchers_on_the_feed_as_its_own_record() {
    use futures::StreamExt;
    use myco_api::{MycoApi, PostMessage, StreamEvent};

    let _home = myco_test_support::temp_home("mp-feed");
    let server = test_server();
    let ada = server.as_user(Author::User {
        id: "ada".into(),
        name: "Ada Lovelace".into(),
    });
    let grace = server.as_user(Author::User {
        id: "grace".into(),
        name: "Grace Hopper".into(),
    });

    let s = ada
        .create_session(myco_api::CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .await
        .expect("create");

    // Ada opens the session so grace's message is one between people.
    ada.post_message(
        &s.id,
        PostMessage {
            text: "opening".into(),
        },
    )
    .await
    .expect("post");
    for _ in 0..200 {
        let p = ada.poll(&s.id, 0).await.expect("poll");
        if !p.busy
            && p.entries
                .iter()
                .any(|e| matches!(e.body, EntryBody::Agent { .. }))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    // Ada is watching when grace speaks.
    let mut feed = ada.events(&s.id).await.expect("subscribe");
    let busy = grace
        .post_message(
            &s.id,
            PostMessage {
                text: "@ada quick question".into(),
            },
        )
        .await
        .expect("post")
        .busy;
    assert!(!busy, "a message between people does not wake the agent");

    // The message itself arrives — not a text delta, not a turn marker.
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(ev) = feed.next().await {
            if let StreamEvent::Message { .. } = ev {
                return ev;
            }
        }
        panic!("the feed closed without delivering the message");
    })
    .await
    .expect("a message event within five seconds");

    let StreamEvent::Message { entry, wakes_agent } = event else {
        unreachable!("filtered above");
    };
    assert!(!wakes_agent, "the feed reports that no turn follows");
    assert_eq!(entry.text(), "@ada quick question");
    match entry.author {
        Author::User { id, name } => {
            assert_eq!(id, "grace");
            assert_eq!(name, "Grace Hopper");
        }
        other => panic!("expected a user author, got {other:?}"),
    }

    // And it becomes durable, so a reader who reloads sees it too.
    //
    // Not instantly: the event goes out ahead of the queue, and the write
    // happens when the session's task drains the note. That ordering is the
    // point — a message is visible immediately even if the agent is busy — and
    // it is why a client keeps a delivered-but-unpersisted message on screen
    // across a poll instead of letting the poll erase it.
    let mut persisted = false;
    for _ in 0..200 {
        let p = ada.poll(&s.id, 0).await.expect("poll");
        if p.entries.iter().any(|e| e.text() == "@ada quick question") {
            persisted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(persisted, "the message never reached the transcript");
}

// ---------------------------------------------------------------------------
// Pre-emption: what happens to a message posted while a turn is running
// ---------------------------------------------------------------------------

use myco_models::{GenerateError, Message, MessagePart};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A model whose `gate_call`-th generate call signals `entered` and then
/// blocks until `release` gets a permit — the deterministic stand-in for "the
/// agent is mid-turn while somebody posts".
struct GatedModel {
    inner: Arc<dyn GenerativeModel>,
    gate_call: usize,
    calls: AtomicUsize,
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
}

impl GenerativeModel for GatedModel {
    fn generate(
        &self,
        input: &[Message],
    ) -> myco_core::AsyncStream<Result<MessagePart, GenerateError>> {
        use futures::StreamExt;
        let inner = self.inner.generate(input);
        if self.calls.fetch_add(1, Ordering::SeqCst) != self.gate_call {
            return inner;
        }
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(
            futures::stream::once(async move {
                entered.add_permits(1);
                let _ = release.acquire().await;
            })
            .flat_map(|_| futures::stream::iter(Vec::new()))
            .chain(inner),
        )
    }
}

/// Three rounds: a tool call, a gated tool call, a final answer. The unknown
/// tool name makes each round a fast error result — the boundaries are what
/// the test needs, not the tools.
fn gated_two_round_factory(
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
) -> myco::server::ModelFactory {
    Box::new(move |_, _, _, _| {
        let round = |id: &str| GenerateOutput {
            content: vec![],
            tool_uses: vec![myco_api::ToolUse {
                id: id.into(),
                name: "no_such_tool".into(),
                input: serde_json::json!({}),
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        };
        let inner = ScriptedModel::new(vec![
            round("call_1"),
            round("call_2"),
            GenerateOutput {
                content: vec![Content::Text {
                    text: "read everything".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            },
        ]);
        Ok(Arc::new(GatedModel {
            inner,
            gate_call: 1,
            calls: AtomicUsize::new(0),
            entered: entered.clone(),
            release: release.clone(),
        }) as Arc<dyn GenerativeModel>)
    })
}

/// The pre-emption contract: a message that addresses the agent mid-turn is
/// folded into the turn in flight at its next well-formed boundary and
/// answered by it — one turn, not a second one queued behind the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_direct_message_mid_turn_preempts_the_turn_in_flight() {
    use myco_api::{MycoApi, PostMessage};

    let _home = myco_test_support::temp_home("mp-preempt");
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let server = test_server_with(gated_two_round_factory(entered.clone(), release.clone()));
    let ada = server.as_user(Author::User {
        id: "ada".into(),
        name: "Ada Lovelace".into(),
    });

    let s = ada
        .create_session(myco_api::CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .await
        .expect("create");
    let live = server.get_live(&s.id).await.expect("live");
    let turns_before = *live.turns.borrow();

    ada.post_message(
        &s.id,
        PostMessage {
            text: "start".into(),
        },
    )
    .await
    .expect("post");

    // The turn is now provably mid-flight: its second generate is blocked.
    entered.acquire().await.expect("gate entered").forget();
    let p = ada
        .post_message(
            &s.id,
            PostMessage {
                text: "@myco also consider the flange".into(),
            },
        )
        .await
        .expect("post");
    assert!(p.busy, "a direct message mid-turn keeps the session busy");

    release.add_permits(1);
    let p = settle_api(&ada, &s.id, |p| {
        p.entries.iter().any(|e| e.text() == "read everything")
    })
    .await;

    // One turn answered both messages.
    let live = server.get_live(&s.id).await.expect("live");
    assert_eq!(
        *live.turns.borrow(),
        turns_before + 1,
        "the mid-turn message must fold into the running turn, not queue its own"
    );

    // The folded message sits in the transcript ahead of the final answer,
    // where the model actually read it.
    let folded_at = p
        .entries
        .iter()
        .position(|e| e.text() == "@myco also consider the flange")
        .expect("the mid-turn message is in the transcript");
    let answer_at = p
        .entries
        .iter()
        .position(|e| e.text() == "read everything")
        .expect("the final answer is in the transcript");
    assert!(folded_at < answer_at, "entries={:?}", p.entries);
}

/// The room rule must hold while messages are still queued behind a running
/// turn: shared-ness counts everyone whose message has been *accepted*, not
/// just what has reached disk. Grace joins mid-turn; ada's next unaddressed
/// line must not wake the agent — and a note posted mid-turn still reports
/// the session busy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_room_rule_counts_messages_still_queued_behind_a_turn() {
    use futures::StreamExt;
    use myco_api::{MycoApi, PostMessage, StreamEvent};

    let _home = myco_test_support::temp_home("mp-queued-room");
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let server = test_server_with(gated_two_round_factory(entered.clone(), release.clone()));
    let ada = server.as_user(Author::User {
        id: "ada".into(),
        name: "Ada Lovelace".into(),
    });
    let grace = server.as_user(Author::User {
        id: "grace".into(),
        name: "Grace Hopper".into(),
    });

    let s = ada
        .create_session(myco_api::CreateSession {
            model: None,
            parent_session: None,
            fork: false,
        })
        .await
        .expect("create");
    let mut feed = ada.events(&s.id).await.expect("subscribe");
    let live = server.get_live(&s.id).await.expect("live");
    let turns_before = *live.turns.borrow();

    ada.post_message(
        &s.id,
        PostMessage {
            text: "start".into(),
        },
    )
    .await
    .expect("post");
    entered.acquire().await.expect("gate entered").forget();

    // Grace joins while the turn runs. Nothing she says is on disk yet.
    let p = grace
        .post_message(
            &s.id,
            PostMessage {
                text: "@ada watching this".into(),
            },
        )
        .await
        .expect("post");
    assert!(
        p.busy,
        "a note posted mid-turn must not report the agent idle"
    );

    // Ada replies to grace, unaddressed. The stale-snapshot bug made this
    // wake the agent, because grace's message had not reached the session
    // file yet.
    ada.post_message(
        &s.id,
        PostMessage {
            text: "sounds good".into(),
        },
    )
    .await
    .expect("post");

    // The feed's own verdicts: neither mid-turn message wakes the agent.
    let mut verdicts = std::collections::HashMap::new();
    let collect = async {
        while let Some(ev) = feed.next().await {
            if let StreamEvent::Message { entry, wakes_agent } = ev {
                verdicts.insert(entry.text(), wakes_agent);
                if verdicts.len() == 3 {
                    break;
                }
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), collect)
        .await
        .expect("all three messages reach the feed");
    assert_eq!(verdicts.get("start"), Some(&true));
    assert_eq!(verdicts.get("@ada watching this"), Some(&false));
    assert_eq!(
        verdicts.get("sounds good"),
        Some(&false),
        "an unaddressed line in a room that just became shared must not wake the agent"
    );

    release.add_permits(1);
    let p = settle_api(&ada, &s.id, |p| {
        p.entries.iter().any(|e| e.text() == "read everything")
    })
    .await;

    // Still one turn, and both mid-turn messages are recorded.
    let live = server.get_live(&s.id).await.expect("live");
    assert_eq!(*live.turns.borrow(), turns_before + 1);
    assert!(p.entries.iter().any(|e| e.text() == "@ada watching this"));
    assert!(p.entries.iter().any(|e| e.text() == "sounds good"));
}

/// In-process settle helper for tests driving the `Server` object directly.
async fn settle_api(
    api: &dyn myco_api::MycoApi,
    id: &str,
    pred: impl Fn(&myco_api::Poll) -> bool,
) -> myco_api::Poll {
    for _ in 0..200 {
        let p = api.poll(id, 0).await.expect("poll");
        if !p.busy && pred(&p) {
            return p;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("session never settled");
}
