//! Control-operator audit trail: who connected, what was claimed, what went on
//! the air.
//!
//! The record is written from a dedicated task, not from the server actor.
//! That matters more than it sounds: the actor is a single task that every
//! connection and every received frame passes through, so a blocking
//! `write` + `flush` in it is a stall for *everybody* — and this log is
//! written on every connect, every callsign claim and every transmitted
//! frame. A bounded channel keeps the actor's side to a pointer copy.
//!
//! The channel is bounded on purpose. If the writer cannot keep up (a full
//! disk, a network filesystem gone away) the right answer is to drop audit
//! lines and say how many were lost, not to grow a queue until the process
//! dies or to block the server behind the filesystem.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use tracing::warn;

/// Audit lines buffered before the server starts dropping them.
const QUEUE: usize = 4096;

/// A handle onto the audit trail. Cheap to clone, so each subsystem can hold
/// one rather than reaching back through the server for it.
#[derive(Clone)]
pub struct Audit {
    tx: Option<mpsc::Sender<String>>,
    /// Lines dropped because the writer fell behind. Reported to the log so a
    /// gap in the audit trail is never silent. Shared across clones: the
    /// count is a property of the log, not of who is writing to it.
    dropped: Arc<AtomicU64>,
}

impl Audit {
    /// Open the audit log. With no path, events still reach `tracing` and
    /// nothing is spawned.
    pub fn open(path: Option<&str>) -> Self {
        let Some(path) = path else {
            return Self {
                tx: None,
                dropped: Arc::new(AtomicU64::new(0)),
            };
        };
        let file = match open_append(path) {
            Ok(f) => f,
            Err(e) => {
                warn!(path, "cannot open audit log: {e}");
                return Self {
                    tx: None,
                    dropped: Arc::new(AtomicU64::new(0)),
                };
            }
        };
        tracing::info!(path, "audit log enabled");
        let (tx, rx) = mpsc::channel::<String>(QUEUE);
        spawn_writer(file, rx);
        Self {
            tx: Some(tx),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// One line, `unix_ms event k=v k=v ...`. Values with spaces are quoted.
    pub fn event(&self, kind: &str, fields: &[(&str, &str)]) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let line = format_event(ts, kind, fields);
        tracing::info!(target: "rfircd::audit", "{line}");
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        if tx.try_send(line).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            // Complain once per doubling, so a failing disk is visible without
            // the complaint itself becoming the flood.
            if n.is_power_of_two() {
                warn!("audit log is not keeping up; {n} line(s) dropped so far");
            }
        }
    }

    /// Audit lines lost because the writer could not keep up.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// One physical line. User-controlled values (KICK/KILL reasons) must not
/// split the file, so line breaks and NULs become spaces before quoting.
fn format_event(ts: u128, kind: &str, fields: &[(&str, &str)]) -> String {
    let mut line = format!("{} {}", ts, one_line(kind));
    for (k, v) in fields {
        let k = one_line(k);
        let v = one_line(v);
        if v.chars().any(|c| c.is_whitespace()) {
            line.push_str(&format!(" {k}=\"{}\"", v.replace('"', "'")));
        } else {
            line.push_str(&format!(" {k}={v}"));
        }
    }
    line
}

fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            // Every control character, not the seven that split a line.
            // KICK and KILL reasons are user text and reach this file *and*
            // `tracing`, which is usually a terminal — so an ESC here writes
            // the operator's screen rather than the record of what happened.
            c if c.is_control() => ' ',
            '\u{85}' | '\u{2028}' | '\u{2029}' => ' ',
            // C1: U+009B is an eight-bit CSI.
            c if ('\u{80}'..='\u{9f}').contains(&c) => ' ',
            other => other,
        })
        .collect()
}

/// Drain the channel onto disk. Batches whatever has already arrived into one
/// write, so a burst costs one syscall rather than one per line.
fn spawn_writer(file: File, mut rx: mpsc::Receiver<String>) {
    tokio::task::spawn_blocking(move || {
        let mut file = file;
        while let Some(first) = rx.blocking_recv() {
            let mut batch = first;
            batch.push('\n');
            while let Ok(next) = rx.try_recv() {
                batch.push_str(&next);
                batch.push('\n');
            }
            if let Err(e) = file.write_all(batch.as_bytes()).and_then(|()| file.flush()) {
                warn!("audit log write failed: {e}");
            }
        }
    });
}

fn open_append(path: &str) -> std::io::Result<File> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    // 0600, like the nick database. This file records who connected from
    // where, which callsigns they claimed and every OPER attempt; it is the
    // licensee's record of what their station did, and it was being created
    // at whatever the umask happened to be.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    // `mode` only applies when the file is created, so an existing log keeps
    // whatever it has: tighten it, but do not fail the open over it — losing
    // the audit trail is worse than a permissive one, and the operator may
    // have set the mode deliberately.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = file.metadata() {
            let mut perms = meta.permissions();
            if perms.mode() & 0o077 != 0 {
                perms.set_mode(0o600);
                let _ = file.set_permissions(perms);
            }
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_keeps_one_line_per_event() {
        // No path: nothing is spawned, so this needs no runtime.
        let a = Audit::open(None);
        a.event("kick", &[("reason", "flooding the channel"), ("n", "3")]);
        assert_eq!(a.dropped(), 0);

        let line = format_event(
            1,
            "kick",
            &[("reason", "foo\nbar\u{2028}baz\r"), ("nick", "alice")],
        );
        assert_eq!(line.lines().count(), 1, "{line:?}");
        assert!(!line.contains('\n'), "{line:?}");
        assert!(!line.contains('\r'), "{line:?}");
        assert!(!line.contains('\u{2028}'), "{line:?}");
        assert!(line.contains("foo bar baz"), "{line:?}");
    }

    /// KICK and KILL reasons are user text and go to this file *and* to
    /// `tracing`, which is usually the operator's terminal.
    #[test]
    fn control_characters_never_reach_the_record() {
        let line = format_event(
            1,
            "kill",
            &[
                ("reason", "spam\u{1b}]0;pwned\u{7}\u{1b}[2J"),
                ("nick", "alice"),
            ],
        );
        assert!(
            !line.chars().any(|c| c.is_control()),
            "a control character survived into the audit line: {line:?}"
        );
        assert!(line.contains("spam"), "{line:?}");
        assert!(line.contains("nick=alice"), "{line:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_audit_log_is_mode_600() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "rfircd-audit-mode-{}.log",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let name = path.to_string_lossy().to_string();
        {
            let a = Audit::open(Some(&name));
            a.event("oper", &[("nick", "alice")]);
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "audit log was {mode:o}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn events_reach_the_file() {
        let path = std::env::temp_dir().join(format!(
            "rfircd-audit-{}.log",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let name = path.to_string_lossy().to_string();
        {
            let a = Audit::open(Some(&name));
            a.event("rf_tx", &[("dest", "SM0ABC-7"), ("bytes", "42")]);
            a.event("oper", &[("nick", "alice"), ("host", "127.0.0.1")]);
        }
        // The writer task owns the file; give it a moment to drain.
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if let Ok(text) = std::fs::read_to_string(&path) {
                if text.lines().count() >= 2 {
                    assert!(text.contains("rf_tx dest=SM0ABC-7 bytes=42"), "{text}");
                    assert!(text.contains("oper nick=alice"), "{text}");
                    let _ = std::fs::remove_file(&path);
                    return;
                }
            }
        }
        panic!("audit lines never reached {name}");
    }
}
