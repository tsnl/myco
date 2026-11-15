//! Keep the process-group identity owned until the retained handle closes.

use super::*;
use std::io;
use std::path::PathBuf;
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

    pub(super) fn enforce_deadline(
        self: &Arc<Self>,
        deadline: tokio::time::Instant,
        timeout_ms: u64,
    ) {
        let owner = Arc::downgrade(self);
        let closed = self.closed.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = closed.cancelled() => return,
                _ = tokio::time::sleep_until(deadline) => {},
            }
            if let Some(owner) = owner.upgrade() {
                owner.expire(timeout_ms);
            }
        });
    }

    fn expire(&self, timeout_ms: u64) {
        let mut buffer = lock_unpoisoned(&self.shared.buffer);
        // A completed leader and closed pipes do not prove its descendants
        // exited. The retained leader still pins this group's identity.
        match self.signal(libc::SIGKILL) {
            Ok(()) if !buffer.is_finished() => buffer.exec_timeout_ms = Some(timeout_ms),
            Ok(()) => {}
            Err(error) => {
                buffer.stderr.extend_from_slice(
                    format!("\nmyco: could not enforce exec timeout_ms={timeout_ms}: {error}\n")
                        .as_bytes(),
                );
                cap_session_stream(&mut buffer.stderr);
            }
        }
        drop(buffer);
        self.shared.notify.notify_waiters();
    }

    pub(super) fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// The leader's current directory. The unreaped leader pins its PID, so
    /// the lookup cannot name a recycled process.
    pub(super) fn working_directory(&self) -> Option<PathBuf> {
        process_directory(self.pid()?)
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

#[cfg(target_os = "linux")]
fn process_directory(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
fn process_directory(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    // SAFETY: an all-zero proc_vnodepathinfo is a valid plain-data value.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: the buffer is exactly `size` writable bytes of the requested flavor.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    let path: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .as_flattened()
        .iter()
        .map(|byte| *byte as u8)
        .take_while(|byte| *byte != 0)
        .collect();
    (!path.is_empty()).then(|| PathBuf::from(std::ffi::OsString::from_vec(path)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_directory(_pid: u32) -> Option<PathBuf> {
    None
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
