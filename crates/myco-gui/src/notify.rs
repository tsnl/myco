//! Telling somebody a message arrived.
//!
//! A shared session is only shared if you find out when it moves. Two levels,
//! both best-effort — a browser that refuses either one still shows the
//! message in the transcript, which is the part that matters:
//!
//! * the tab title carries an unread count, which needs no permission and
//!   works everywhere;
//! * a desktop notification fires for a message that names you, or for any
//!   message while the tab is in the background.

use web_sys::{Notification, NotificationOptions, NotificationPermission};

/// The tab title without a badge; restored when the count returns to zero.
const BASE_TITLE: &str = "myco";

fn document() -> Option<web_sys::Document> {
    web_sys::window()?.document()
}

/// Is this tab in the background? Notifications for a session the reader is
/// already looking at would be noise.
pub fn hidden() -> bool {
    document().map(|d| d.hidden()).unwrap_or(false)
}

/// Show `n` unread messages in the tab title (`(3) myco`); `0` clears it.
pub fn set_unread(n: u32) {
    let Some(doc) = document() else { return };
    if n == 0 {
        doc.set_title(BASE_TITLE);
    } else {
        doc.set_title(&format!("({n}) {BASE_TITLE}"));
    }
}

/// Ask for notification permission, once, without blocking anything on the
/// answer. A denied prompt simply leaves [`toast`] a no-op.
pub fn request_permission() {
    if Notification::permission() == NotificationPermission::Default {
        let _ = Notification::request_permission();
    }
}

/// Raise a desktop notification, if we are allowed to.
///
/// `tag` collapses repeats: a busy session replaces its own notification
/// instead of stacking one per message.
pub fn toast(title: &str, body: &str, tag: &str) {
    if Notification::permission() != NotificationPermission::Granted {
        return;
    }
    let opts = NotificationOptions::new();
    opts.set_body(body);
    opts.set_tag(tag);
    let _ = Notification::new_with_options(title, &opts);
}
