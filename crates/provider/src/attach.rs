//! The pool's half of the bus envelope: [`attach`] adopts a provider's
//! instances into a local [`Pool`] and relays until the stream dies.
//! The counterpart of [`serve`](crate::serve), and deliberately its
//! mirror: rows upsert, marks fold, events forward, gone forgets — and
//! when the stream ends, every adopted row is handed back at once
//! ([`Pool::drop_remotes_from`]), because EOF is death and death is
//! removal.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use myco_instance::{Pool, Principal, RemoteCall, VerbError};
use myco_wire as wire;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};

type Pending = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Result<Value, VerbError>>>>>;

/// The clonable handle onto one attached provider: verbs forward through
/// it (it is the adopted rows' [`RemoteCall`]), and creation goes here
/// because creation is the one operation that is not a verb. A dead
/// stream answers [`VerbError::Gone`] — the same word a dead local cell
/// uses, because to callers it is the same fact.
#[derive(Clone)]
pub struct Link {
    out: tokio::sync::mpsc::Sender<wire::ToProvider>,
    pending: Pending,
    seq: Arc<AtomicU64>,
}

impl Link {
    async fn roundtrip(
        &self,
        build: impl FnOnce(u64) -> wire::ToProvider,
    ) -> Result<Value, VerbError> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seq, tx);
        if self.out.send(build(seq)).await.is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&seq);
            return Err(VerbError::Gone);
        }
        rx.await.unwrap_or(Err(VerbError::Gone))
    }

    /// Create an instance on the provider. The reply's row comes back for
    /// immediate use; the row *frame* follows on the stream and upserts
    /// idempotently, so callers need not wait for the listing to agree.
    pub async fn create(
        &self,
        creator: &Principal,
        kind: &str,
        project: &str,
        title: &str,
        args: Value,
    ) -> Result<wire::InstanceInfo, VerbError> {
        let row = self
            .roundtrip(|seq| wire::ToProvider::Create {
                seq,
                kind: kind.into(),
                project: project.into(),
                title: title.into(),
                creator: creator.clone(),
                args,
            })
            .await?;
        serde_json::from_value(row).map_err(|_| VerbError::Failed {
            why: "the provider answered a create with something that is not a row".into(),
        })
    }
}

#[async_trait::async_trait]
impl RemoteCall for Link {
    async fn call(
        &self,
        caller: &Principal,
        id: &str,
        verb: &str,
        args: Value,
    ) -> Result<Value, VerbError> {
        self.roundtrip(|seq| wire::ToProvider::Call {
            seq,
            id: id.into(),
            caller: caller.clone(),
            verb: verb.into(),
            args,
        })
        .await
    }
}

/// A provider handshaken and adopted, not yet relaying: hold the offers
/// and the [`Link`], then hand the rest to [`Attached::run`]. Dropping
/// this — before or during `run` — is the cleanup: pending calls answer
/// gone, adopted rows leave the pool.
pub struct Attached<R> {
    pub name: String,
    pub kinds: Vec<wire::KindOffer>,
    pub link: Link,
    lines: tokio::io::Lines<BufReader<R>>,
    pool: Pool,
    origin: String,
    call: Arc<dyn RemoteCall>,
    writer: tokio::task::JoinHandle<()>,
}

/// Handshake with a provider and adopt everything it announced. The
/// provider speaks first; a wrong protocol closes without answering
/// (there is nothing to negotiate). `origin` is the local instance that
/// owns this stream — a host — and becomes the adopted rows' parent and
/// their cleanup key.
pub async fn attach<R, W>(
    pool: Pool,
    origin: &str,
    reader: R,
    writer: W,
) -> std::io::Result<Attached<R>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    let hello = match lines.next_line().await? {
        Some(line) => wire::decode::<wire::ToPool>(&line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the stream ended before hello",
            ));
        }
    };
    let wire::ToPool::Hello {
        protocol,
        name,
        kinds,
        rows,
    } = hello
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the provider must speak hello first",
        ));
    };
    if protocol != wire::PROTOCOL {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("protocol {protocol}, expected {}", wire::PROTOCOL),
        ));
    }

    let mut writer = writer;
    writer
        .write_all(
            wire::encode(&wire::ToProvider::Hello {
                protocol: wire::PROTOCOL,
            })
            .as_bytes(),
        )
        .await?;
    writer.flush().await?;

    let (out, mut out_rx) = tokio::sync::mpsc::channel::<wire::ToProvider>(256);
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if writer.write_all(wire::encode(&frame).as_bytes()).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });

    let link = Link {
        out,
        pending: Arc::new(Mutex::new(HashMap::new())),
        seq: Arc::new(AtomicU64::new(1)),
    };
    let call: Arc<dyn RemoteCall> = Arc::new(link.clone());
    for row in rows {
        pool.adopt(origin, row, Arc::clone(&call));
    }

    Ok(Attached {
        name,
        kinds,
        link,
        lines,
        pool,
        origin: origin.to_string(),
        call,
        writer: writer_task,
    })
}

impl<R> Attached<R>
where
    R: AsyncRead + Unpin,
{
    /// Relay frames until the stream ends. Consumes self, so the return —
    /// or a cancellation anywhere inside — runs the drop cleanup exactly
    /// once.
    pub async fn run(mut self) -> std::io::Result<()> {
        loop {
            let line = match self.lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => return Ok(()),
                Err(e) => return Err(e),
            };
            let frame = wire::decode::<wire::ToPool>(&line)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            match frame {
                wire::ToPool::Hello { .. } => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "hello twice",
                    ));
                }
                wire::ToPool::Reply { seq, outcome } => {
                    let waiter = self
                        .link
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&seq);
                    if let Some(tx) = waiter {
                        let _ = tx.send(outcome.into_result());
                    }
                }
                wire::ToPool::Row { info } => {
                    self.pool.adopt(&self.origin, info, Arc::clone(&self.call));
                }
                wire::ToPool::Mark { id, watermark } => {
                    self.pool.remote_mark(&id, watermark);
                }
                wire::ToPool::Event { id, name, data } => {
                    self.pool
                        .remote_event(&id, myco_runtime::Event { name, data });
                }
                wire::ToPool::Gone { id } => {
                    self.pool.drop_remote(&id);
                }
                wire::ToPool::Lagged => {
                    // Doubt travels tagged with the stream's owner; fresh
                    // rows follow from the provider and upsert on arrival.
                    self.pool.remote_event(
                        &self.origin,
                        myco_runtime::Event {
                            name: "lagged".into(),
                            data: Value::Null,
                        },
                    );
                }
            }
        }
    }
}

impl<R> Drop for Attached<R> {
    fn drop(&mut self) {
        self.writer.abort();
        self.pool.drop_remotes_from(&self.origin);
        let mut pending = self
            .link
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(VerbError::Gone));
        }
    }
}
