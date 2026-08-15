//! The provider's half of the bus envelope: [`serve`] fronts a local
//! [`Pool`] on a byte stream, speaking [`myco_wire`] frames. This is the
//! whole trick of protocol providers — the far process runs the *same*
//! L0/L1 the server runs, with whatever kinds it chooses to register,
//! and the envelope is a relay, not a second implementation. A host is
//! this loop plus `kind-tty`; a toold is this loop plus its own kind.
//!
//! Discipline mirrors the server's WebSocket feed (`server::watch`): one
//! outbound queue serializes every frame onto the writer; watch tasks
//! send coalesced marks per instance; the pool's global feed relays
//! events and drives row upserts. Verb calls apply concurrently (a slow
//! verb must not convoy the stream), which is the same ordering promise
//! concurrent gateway requests already have — none.

use std::collections::HashMap;

pub mod attach;
pub mod host;

pub use attach::{Attached, Link, attach};
pub use host::HostKind;

use myco_instance::{Pool, Principal};
use myco_wire as wire;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};

/// The principal the serve loop itself acts as, for the introspection
/// reads (`sys.meta`) that keep rows fresh. Never used for kind verbs —
/// callers cross the wire verbatim.
fn relay() -> Principal {
    Principal::System("provider".into())
}

/// Serve `pool` on a byte stream until EOF or a protocol error. The
/// provider speaks first: hello with offers and live rows, then the
/// frame loop. Returns `Ok(())` on orderly EOF — the pool hung up — and
/// an error for a stream that broke the treaty (bad frame, wrong
/// protocol), which the caller should treat as fatal rather than retry:
/// the two ends do not negotiate.
pub async fn serve<R, W>(pool: Pool, name: &str, reader: R, writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    let mut writer = writer;

    // One outbound queue: watch tasks, call tasks, and the feed all send
    // here; only this function touches the writer.
    let (out, mut out_rx) = tokio::sync::mpsc::channel::<String>(256);

    let hello = wire::ToPool::Hello {
        protocol: wire::PROTOCOL,
        name: name.to_string(),
        kinds: offers(&pool),
        rows: pool.list(None),
    };
    writer.write_all(wire::encode(&hello).as_bytes()).await?;
    writer.flush().await?;

    // The pool's answer. Anything but a matching hello ends the stream.
    let ack = match lines.next_line().await? {
        Some(line) => wire::decode::<wire::ToProvider>(&line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        None => return Ok(()),
    };
    match ack {
        wire::ToProvider::Hello { protocol } if protocol == wire::PROTOCOL => {}
        wire::ToProvider::Hello { protocol } => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("protocol {protocol}, expected {}", wire::PROTOCOL),
            ));
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the pool must answer hello with hello",
            ));
        }
    }

    // Marks for everything already alive; new instances join on birth.
    let mut watches: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    for row in pool.list(None) {
        watch(&pool, &mut watches, row.id, &out);
    }

    let mut feed = pool.events();
    let result = loop {
        tokio::select! {
            incoming = lines.next_line() => {
                let line = match incoming {
                    Ok(Some(line)) => line,
                    Ok(None) => break Ok(()),
                    Err(e) => break Err(e),
                };
                let frame = match wire::decode::<wire::ToProvider>(&line) {
                    Ok(frame) => frame,
                    Err(e) => {
                        break Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e));
                    }
                };
                match frame {
                    wire::ToProvider::Hello { .. } => {
                        break Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "hello twice",
                        ));
                    }
                    wire::ToProvider::Call { seq, id, caller, verb, args } => {
                        // Concurrent on purpose: a slow verb (a piped
                        // command draining) must not convoy every other
                        // reply behind it.
                        let pool = pool.clone();
                        let out = out.clone();
                        tokio::spawn(async move {
                            let outcome = pool.call(&caller, &id, &verb, args).await;
                            let reply = wire::ToPool::Reply { seq, outcome: outcome.into() };
                            let _ = out.send(wire::encode(&reply)).await;
                        });
                    }
                    wire::ToProvider::Create { seq, kind, project, title, creator, args } => {
                        let outcome = pool
                            .create(&creator, &kind, &project, &title, args)
                            .map(|info| serde_json::to_value(info).expect("rows serialize"));
                        let reply = wire::ToPool::Reply { seq, outcome: outcome.into() };
                        if out.send(wire::encode(&reply)).await.is_err() {
                            break Ok(());
                        }
                    }
                }
            }
            event = feed.recv() => {
                match event {
                    Ok((id, event)) => {
                        relay_event(&pool, &mut watches, &out, id, event).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Frames were lost before the wire; say so, then
                        // re-sync rows — the same recovery the feed
                        // doctrine demands of every lagging consumer.
                        let _ = out.send(wire::encode(&wire::ToPool::Lagged)).await;
                        for row in pool.list(None) {
                            watch(&pool, &mut watches, row.id.clone(), &out);
                            let _ = out
                                .send(wire::encode(&wire::ToPool::Row { info: row }))
                                .await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break Ok(()),
                }
            }
            frame = out_rx.recv() => {
                match frame {
                    Some(f) => {
                        if writer.write_all(f.as_bytes()).await.is_err() {
                            break Ok(());
                        }
                        if writer.flush().await.is_err() {
                            break Ok(());
                        }
                    }
                    None => break Ok(()),
                }
            }
        }
    };

    for (_, task) in watches {
        task.abort();
    }
    result
}

/// One pool event onto the wire, plus its side effects on rows and
/// watches: births get a row and a mark loop, meta changes refresh the
/// row, removal sends gone (the watch task starves out on its own).
async fn relay_event(
    pool: &Pool,
    watches: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    out: &tokio::sync::mpsc::Sender<String>,
    id: String,
    event: myco_runtime::Event,
) {
    let name = event.name.clone();
    let _ = out
        .send(wire::encode(&wire::ToPool::Event {
            id: id.clone(),
            name: event.name,
            data: event.data,
        }))
        .await;
    match name.as_str() {
        "created" | "renamed" | "driver" => {
            // The row is state, freshly read — never reconstructed from
            // the event that announced it changed.
            if let Ok(meta) = pool.call(&relay(), &id, "sys.meta", Value::Null).await
                && let Ok(info) = serde_json::from_value::<wire::InstanceInfo>(meta)
            {
                let _ = out.send(wire::encode(&wire::ToPool::Row { info })).await;
            }
            watch(pool, watches, id, out);
        }
        "removed" => {
            let _ = out.send(wire::encode(&wire::ToPool::Gone { id })).await;
        }
        _ => {}
    }
}

/// Ensure a mark loop runs for `id`. Level-triggered from 0, so the pool
/// side gets a first mark immediately — the same no-race contract the
/// WebSocket watch keeps.
fn watch(
    pool: &Pool,
    watches: &mut HashMap<String, tokio::task::JoinHandle<()>>,
    id: String,
    out: &tokio::sync::mpsc::Sender<String>,
) {
    if watches.contains_key(&id) {
        return;
    }
    let pool = pool.clone();
    let out = out.clone();
    watches.insert(
        id.clone(),
        tokio::spawn(async move {
            let mut seen = 0;
            loop {
                match pool.changed(&id, seen).await {
                    Ok(watermark) => {
                        seen = watermark;
                        let frame = wire::ToPool::Mark {
                            id: id.clone(),
                            watermark,
                        };
                        if out.send(wire::encode(&frame)).await.is_err() {
                            return;
                        }
                    }
                    // Removed: the feed's gone frame is the announcement;
                    // this loop just ends.
                    Err(_) => return,
                }
            }
        }),
    );
}

fn offers(pool: &Pool) -> Vec<wire::KindOffer> {
    pool.kinds()
        .into_iter()
        .map(|spec| wire::KindOffer {
            kind: spec.kind.to_string(),
            version: spec.version,
            spec: serde_json::to_value(spec).expect("specs serialize"),
        })
        .collect()
}

#[cfg(test)]
mod tests;
