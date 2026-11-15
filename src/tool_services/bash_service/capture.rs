//! Output capture: pipe readers feeding a shared buffer, exec capture
//! under an accumulation cap, and middle-elided rendering.

use super::*;

/// How long exec waits for its pipe readers after the child is gone. The
/// readers only EOF when *every* write end closes, so a backgrounded
/// grandchild (`sleep 300 & echo hi`) would otherwise stall the result for
/// its whole lifetime — or forever, for a daemon.
const EXEC_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// In-memory accumulation cap per exec stream. `max_bytes` caps what a call
/// *returns*; without this a flooding child (`yes`, a verbose build) grows
/// the capture buffer without bound for the exec's whole lifetime. Head and
/// rolling tail are kept so the final middle-elision still sees both ends.
pub(super) const EXEC_CAPTURE_CAP: usize = 512 * 1024;

/// One exec stream captured under the accumulation cap: the first
/// `EXEC_CAPTURE_CAP / 2` bytes, a rolling tail of the same size, and a count
/// of bytes dropped at the seam between them.
#[derive(Default)]
pub(super) struct CappedCapture {
    pub(super) head: Vec<u8>,
    pub(super) tail: Vec<u8>,
    pub(super) omitted: usize,
}

impl CappedCapture {
    pub(super) fn push(&mut self, chunk: &[u8]) {
        let head_cap = EXEC_CAPTURE_CAP / 2;
        let tail_cap = EXEC_CAPTURE_CAP - head_cap;
        let mut rest = chunk;
        if self.head.len() < head_cap {
            let take = (head_cap - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if rest.is_empty() {
            return;
        }
        self.tail.extend_from_slice(rest);
        // Drop-oldest in blocks (not per chunk) so a flood costs a few
        // memmoves, not one per 4 KiB read.
        if self.tail.len() > tail_cap + 64 * 1024 {
            let excess = self.tail.len() - tail_cap;
            self.tail.drain(..excess);
            self.omitted += excess;
        }
    }
}

pub(super) fn render_exec_capture(shared: &SessionShared, max_bytes: usize) -> (String, String) {
    let buffer = lock_unpoisoned(&shared.buffer);
    let (stdout, stderr) = buffer
        .exec_capture
        .as_ref()
        .expect("foreground exec capture");
    (
        render_capture(stdout, max_bytes),
        render_capture(stderr, max_bytes),
    )
}

/// The shared middle-elision shape: verbatim head, one honest marker, tail.
fn middle_elision(
    head: &[u8],
    tail: &[u8],
    omitted: usize,
    total: usize,
    max_bytes: usize,
) -> String {
    format!(
        "{}\n[... {omitted} bytes omitted ({total} bytes total, max_bytes={max_bytes}); \
         filter with grep/head/tail, redirect to a file, or raise max_bytes ...]\n{}",
        String::from_utf8_lossy(head),
        String::from_utf8_lossy(tail),
    )
}

/// Render a captured exec stream under the *return* cap `max_bytes`, folding
/// any capture-time loss into the same middle-elision marker.
///
/// Unlike session output (buffered; the agent can `read` the rest later),
/// exec output is gone once the call returns, so the cut must keep the most
/// informative parts — and those are the ends: build/test tools print the
/// root-cause error first and the failure summary last. Cuts are byte-exact;
/// a split UTF-8 char renders as U+FFFD at the seam.
pub(super) fn render_capture(capture: &CappedCapture, max_bytes: usize) -> String {
    if capture.omitted == 0 {
        let mut all = Vec::with_capacity(capture.head.len() + capture.tail.len());
        all.extend_from_slice(&capture.head);
        all.extend_from_slice(&capture.tail);
        return truncate_middle_lossy(&all, max_bytes);
    }
    let total = capture.head.len() + capture.omitted + capture.tail.len();
    let head_keep = max_bytes.div_ceil(2).min(capture.head.len());
    let tail_keep = (max_bytes / 2).min(capture.tail.len());
    let omitted = total - head_keep - tail_keep;
    middle_elision(
        &capture.head[..head_keep],
        &capture.tail[capture.tail.len() - tail_keep..],
        omitted,
        total,
        max_bytes,
    )
}

/// Cap contiguous bytes at `max_bytes`, keeping the head and tail halves and
/// eliding the middle with a marker (see [`render_capture`]).
pub(super) fn truncate_middle_lossy(bytes: &[u8], max_bytes: usize) -> String {
    if bytes.len() <= max_bytes {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let head_len = max_bytes.div_ceil(2);
    let tail_len = max_bytes / 2;
    let omitted = bytes.len() - head_len - tail_len;
    middle_elision(
        &bytes[..head_len],
        &bytes[bytes.len() - tail_len..],
        omitted,
        bytes.len(),
        max_bytes,
    )
}

/// Kill a foreground exec's process group, reap its leader, and render what
/// it printed.
pub(super) async fn kill_and_capture(
    child: &mut Child,
    pid: Option<u32>,
    readers: [tokio::task::JoinHandle<()>; 2],
    shared: &SessionShared,
    max_bytes: usize,
) -> (String, String) {
    kill_process_group(pid);
    let _ = child.start_kill();
    let _ = child.wait().await;
    drain_capture(readers).await;
    render_exec_capture(shared, max_bytes)
}

/// Wait up to `EXEC_DRAIN_GRACE` per exec pipe reader, then abort it. Reached
/// with the child already dead, so anything still holding the pipes open is a
/// stray grandchild whose future output we deliberately give up on.
pub(super) async fn drain_capture(readers: [tokio::task::JoinHandle<()>; 2]) {
    for mut task in readers {
        if tokio::time::timeout(EXEC_DRAIN_GRACE, &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
}

/// Per-stream in-memory cap for session output. A long-lived session nobody
/// `read`s (a dev server left running for hours) must not grow resident
/// memory without bound; the newest output is what a late reader wants, so
/// the oldest is dropped — in blocks, so a flood costs a few memmoves.
pub(super) const SESSION_STREAM_CAP: usize = 2 * 1024 * 1024;

/// Enforce [`SESSION_STREAM_CAP`] on one stream; returns bytes dropped.
pub(super) fn cap_session_stream(stream: &mut Vec<u8>) -> usize {
    if stream.len() <= SESSION_STREAM_CAP {
        return 0;
    }
    // Drop down to 3/4 of the cap so the next overflows are amortized.
    let excess = stream.len() - (SESSION_STREAM_CAP - SESSION_STREAM_CAP / 4);
    stream.drain(..excess);
    excess
}

pub(super) fn spawn_reader<R>(
    mut reader: R,
    kind: StreamKind,
    shared: Arc<SessionShared>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let mut b = lock_unpoisoned(&shared.buffer);
                    let dropped = if let Some((stdout, stderr)) = &mut b.exec_capture {
                        match kind {
                            StreamKind::Stdout => stdout,
                            StreamKind::Stderr => stderr,
                        }
                        .push(&buf[..n]);
                        0
                    } else {
                        match kind {
                            StreamKind::Stdout => {
                                b.stdout.extend_from_slice(&buf[..n]);
                                cap_session_stream(&mut b.stdout)
                            }
                            StreamKind::Stderr => {
                                b.stderr.extend_from_slice(&buf[..n]);
                                cap_session_stream(&mut b.stderr)
                            }
                        }
                    };
                    b.dropped_bytes = b.dropped_bytes.saturating_add(dropped);
                    b.total_bytes = b.total_bytes.saturating_add(n);
                    drop(b);
                    shared.notify.notify_waiters();
                }
                Err(_) => break,
            }
        }
        let mut b = lock_unpoisoned(&shared.buffer);
        b.eof_streams += 1;
        b.record_completion();
        drop(b);
        shared.notify.notify_waiters();
    })
}
