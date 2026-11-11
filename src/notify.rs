//! The desktop notification after a scheduled plan (spec §5.1 step 2), sent
//! over D-Bus to `org.freedesktop.Notifications`; clicking it opens the GUI.

use anyhow::Context;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;
use zbus::{Connection, MatchRule, MessageStream, Variant};

use crate::APP_ID;
use crate::disk::FsUsage;
use crate::fmt::human_bytes;
use crate::plan::Plan;

pub trait Notifier {
    fn notify(&self, summary: &str, body: &str) -> anyhow::Result<u32>;
    /// Like `notify`, but critical urgency where the daemon has the concept.
    fn notify_urgent(&self, summary: &str, body: &str) -> anyhow::Result<u32> {
        self.notify(summary, body)
    }
    /// true if the default action was invoked; false if closed or timed out.
    fn wait_for_click(&self, id: u32, timeout: Duration) -> anyhow::Result<bool>;
}

/// How long `notify_if_worth_it` waits for a click.
pub const WAIT: Duration = Duration::from_secs(12 * 3600);
/// How long `notify_low_space` waits for a click.
pub const LOW_SPACE_WAIT: Duration = Duration::from_secs(3600);

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// Session-bus notifier. `notify` subscribes to the daemon's signals before
/// sending, so `wait_for_click` can't miss a fast click.
#[derive(Default)]
pub struct DbusNotifier {
    signals: Mutex<Option<MessageStream>>,
}

impl DbusNotifier {
    fn send(&self, summary: &str, body: &str, urgency: u8) -> anyhow::Result<u32> {
        let (signals, id) = zbus::block_on(async {
            let conn = Connection::session().await.context("connecting to the session bus")?;
            let rule = MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(DEST)
                .path(PATH)
                .build()?;
            let signals = MessageStream::for_match_rule(rule, &conn, None).await?;
            let hints: HashMap<&str, Variant> =
                HashMap::from([("desktop-entry", Variant::from(APP_ID)), ("urgency", Variant::from(urgency))]);
            let reply = conn
                .call_method(
                    Some(DEST),
                    PATH,
                    Some(DEST),
                    "Notify",
                    &("Cleaner", 0u32, "edit-clear-all-symbolic", summary, body, vec!["default", "Review plan"], hints, 0i32),
                )
                .await
                .context("calling org.freedesktop.Notifications.Notify")?;
            let id: u32 = reply.body().deserialize()?;
            anyhow::Ok((signals, id))
        })?;
        *self.signals.lock().unwrap() = Some(signals);
        Ok(id)
    }
}

impl Notifier for DbusNotifier {
    fn notify(&self, summary: &str, body: &str) -> anyhow::Result<u32> {
        self.send(summary, body, 1)
    }

    fn notify_urgent(&self, summary: &str, body: &str) -> anyhow::Result<u32> {
        self.send(summary, body, 2)
    }

    fn wait_for_click(&self, id: u32, timeout: Duration) -> anyhow::Result<bool> {
        let mut signals =
            self.signals.lock().unwrap().take().context("wait_for_click called before notify")?;
        // The wait runs on its own thread so the timeout is a plain channel
        // timeout; on timeout the thread is left blocked until the process exits.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let clicked: zbus::Result<bool> = zbus::block_on(async {
                loop {
                    let next = std::future::poll_fn(|cx| Pin::new(&mut signals).poll_next(cx)).await;
                    let Some(msg) = next else { return Ok(false) };
                    let msg = msg?;
                    let header = msg.header();
                    match header.member().map(|m| m.as_str()) {
                        Some("ActionInvoked") => {
                            let (n, action): (u32, String) = msg.body().deserialize()?;
                            if n == id && action == "default" {
                                return Ok(true);
                            }
                        }
                        Some("NotificationClosed") => {
                            let (n, _reason): (u32, u32) = msg.body().deserialize()?;
                            if n == id {
                                return Ok(false);
                            }
                        }
                        _ => {}
                    }
                }
            });
            let _ = tx.send(clicked);
        });
        match rx.recv_timeout(timeout) {
            Ok(clicked) => Ok(clicked?),
            Err(_) => Ok(false),
        }
    }
}

/// Summary and body; the copy is fixed by the plan.
pub fn notify_text(plan: &Plan) -> (String, String) {
    (
        format!("{} can be freed", human_bytes(plan.selected_bytes())),
        "cleaner built a plan; nothing has been deleted. Click to review it.".to_string(),
    )
}

#[derive(Debug, PartialEq)]
pub enum NotifyOutcome {
    BelowThreshold,
    Dismissed,
    Opened,
}

/// Notifies if the plan's selected total is at least `threshold`, waits up to
/// `WAIT` for a click, and on a click calls `launch(&plan.id)`.
pub fn notify_if_worth_it(
    plan: &Plan,
    threshold: u64,
    n: &dyn Notifier,
    launch: &mut dyn FnMut(&str),
) -> anyhow::Result<NotifyOutcome> {
    if plan.selected_bytes() < threshold {
        return Ok(NotifyOutcome::BelowThreshold);
    }
    let (summary, body) = notify_text(plan);
    let id = n.notify(&summary, &body)?;
    if n.wait_for_click(id, WAIT)? {
        launch(&plan.id);
        Ok(NotifyOutcome::Opened)
    } else {
        Ok(NotifyOutcome::Dismissed)
    }
}

/// Summary and body for the low-space notification; the copy is fixed by the
/// plan. `low` is non-empty; its first filesystem is the headline.
pub fn low_space_text(low: &[FsUsage], plan: Result<&Plan, String>) -> (String, String) {
    let first = &low[0];
    let used = ((first.total.saturating_sub(first.avail)) as f64 * 100.0 / first.total as f64).round();
    let summary = format!("{} is almost full: {} free ({used}% used)", first.label, human_bytes(first.avail));
    let mut body = match plan {
        Ok(p) => format!("A plan can free {}. Nothing has been deleted.", human_bytes(p.selected_bytes())),
        Err(reason) => {
            format!("The plan could not be saved: {reason}. Nothing has been deleted. Open cleaner to free space.")
        }
    };
    for f in &low[1..] {
        body.push_str(&format!(" Also low: {} ({} free).", f.label, human_bytes(f.avail)));
    }
    (summary, body)
}

/// The low-space notification: always sent (the threshold doesn't apply),
/// urgent, waits `LOW_SPACE_WAIT`; a click calls `launch` with the plan id, or
/// `None` when no plan was saved.
pub fn notify_low_space(
    low: &[FsUsage],
    plan: Result<&Plan, String>,
    n: &dyn Notifier,
    launch: &mut dyn FnMut(Option<&str>),
) -> anyhow::Result<NotifyOutcome> {
    let id = plan.as_ref().ok().map(|p| p.id.clone());
    let (summary, body) = low_space_text(low, plan);
    let nid = n.notify_urgent(&summary, &body)?;
    if n.wait_for_click(nid, LOW_SPACE_WAIT)? {
        launch(id.as_deref());
        Ok(NotifyOutcome::Opened)
    } else {
        Ok(NotifyOutcome::Dismissed)
    }
}

/// `cleaner gui [<id>]` in its own transient user unit. The timer's oneshot
/// service can then exit at once: systemd would kill a child left in its
/// cgroup, and waiting for the window instead would block the next nightly
/// run. `exec` makes systemd-run fail if the GUI can't be started; the GUI's
/// own output and exit status go to the journal under that unit. Without a
/// plan id the unit is `cleaner-gui-lowspace`.
pub fn gui_launch_command(exe: &std::path::Path, id: Option<&str>) -> std::process::Command {
    let mut cmd = std::process::Command::new("systemd-run");
    cmd.args(["--user", "--collect", "--service-type=exec"])
        .arg(format!("--unit=cleaner-gui-{}", id.unwrap_or("lowspace")))
        .arg(exe)
        .arg("gui")
        .args(id);
    cmd
}
