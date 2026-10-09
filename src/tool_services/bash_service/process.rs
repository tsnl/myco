//! Keep the process-group identity owned until the retained handle closes.

use super::*;
use std::io;
use std::sync::Weak;

//
// Process ownership
//

/// Observe exit without reaping the leader: its waitable PID pins the process
/// group even when descendants close both output pipes. No other code may wait
/// on this child. The last owner kills the group before releasing that PID.
pub(super) struct ProcessOwner {
    child: Option<Child>,
    closed: crate::core::CancelToken,
    shared: Arc<SessionShared>,
}

impl ProcessOwner {
    pub(super) fn retain(child: Child, shared: Arc<SessionShared>) -> Arc<Self> {
        let owner = Arc::new(Self {
            child: Some(child),
            closed: crate::core::CancelToken::new(),
            shared: shared.clone(),
        });
        spawn_observer(Arc::downgrade(&owner), owner.closed.clone(), shared);
        owner
    }

    pub(super) fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    pub(super) fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        signal_process_group(self.pid(), signal)
    }
}

impl Drop for ProcessOwner {
    fn drop(&mut self) {
        let _ = self.signal(libc::SIGKILL);
        self.closed.cancel();
        // Group signals are finished; the final waiter can release the PID.
        let mut child = self.child.take().expect("owned child");
        match child.try_wait() {
            Ok(Some(status)) => record_status(&self.shared, Ok(status)),
            _ => reap_closed_child(child, self.shared.clone()),
        }
    }
}

fn reap_closed_child(mut child: Child, shared: Arc<SessionShared>) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move { record_status(&shared, child.wait().await) });
    }
    // Without a runtime, Child's kill-on-drop and orphan reaper still apply.
}

fn record_status(shared: &SessionShared, status: io::Result<std::process::ExitStatus>) {
    let observation = status.map(|status| ProcessExit {
        code: status.code(),
        signal: status.signal(),
    });
    record_observation(shared, observation);
}

//
// Non-reaping exit observation
//

struct ProcessExit {
    code: Option<i32>,
    signal: Option<i32>,
}

fn observe_exit(owner: &ProcessOwner) -> io::Result<Option<ProcessExit>> {
    let pid = owner
        .pid()
        .ok_or_else(|| io::Error::other("child has no PID"))?;
    read_exit(pid)
}

fn read_exit(pid: u32) -> io::Result<Option<ProcessExit>> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    loop {
        // SAFETY: info is writable and initialized; WNOWAIT preserves exclusive
        // child ownership while WNOHANG keeps this syscall nonblocking.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    // SAFETY: waitid succeeded and the zeroed no-result case has si_pid == 0.
    let info = unsafe { info.assume_init() };
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let status = unsafe { info.si_status() };
    Ok(Some(ProcessExit {
        code: (info.si_code == libc::CLD_EXITED).then_some(status),
        signal: (info.si_code != libc::CLD_EXITED).then_some(status),
    }))
}

fn spawn_observer(
    owner: Weak<ProcessOwner>,
    closed: crate::core::CancelToken,
    shared: Arc<SessionShared>,
) {
    tokio::spawn(async move {
        // Register before observing so an exit between query and recv wakes us.
        let mut changes =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).ok();
        loop {
            if record_exit(&owner, &shared) {
                return;
            }
            tokio::select! {
                _ = closed.cancelled() => return,
                _ = wait_for_change(&mut changes) => {},
            }
        }
    });
}

fn record_exit(owner: &Weak<ProcessOwner>, shared: &SessionShared) -> bool {
    let Some(owner) = owner.upgrade() else {
        return true;
    };
    let observation = match observe_exit(&owner) {
        Ok(None) => return false,
        Ok(Some(exit)) => Ok(exit),
        Err(error) => Err(error),
    };
    record_observation(shared, observation);
    true
}

fn record_observation(shared: &SessionShared, observation: io::Result<ProcessExit>) {
    let mut buffer = lock_unpoisoned(&shared.buffer);
    buffer.exited = true;
    match observation {
        Ok(exit) => {
            buffer.exit_code = exit.code;
            buffer.exit_signal = exit.signal;
        }
        Err(error) => buffer.stderr.extend_from_slice(
            format!("\nmyco: could not observe process exit: {error}\n").as_bytes(),
        ),
    }
    buffer.record_completion();
    drop(buffer);
    shared.notify.notify_waiters();
}

async fn wait_for_change(changes: &mut Option<tokio::signal::unix::Signal>) {
    if let Some(signal) = changes {
        if signal.recv().await.is_none() {
            *changes = None;
        }
    } else {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
