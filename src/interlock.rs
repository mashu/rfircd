//! External transmit interlock: a safety check rfircd does not itself know
//! how to make.
//!
//! The gateway cannot see the radio. It speaks KISS to a modem — Direwolf, a
//! hardware TNC — and KISS carries frames, not telemetry. There is no SWR
//! reading, no PA temperature and no power meter anywhere in that path, and on
//! a QMX the one port that *could* answer (CAT) is already held by Direwolf
//! for PTT; two processes cannot share a serial port.
//!
//! Rather than pretend otherwise, this module lets the operator supply the
//! check. A command is run on a timer; while it fails, the transmitter is
//! inhibited. What the command measures is the operator's business: SWR from a
//! separate meter, a temperature probe on the finals, a GPIO from a hardware
//! interlock, "is the antenna switch on the dummy load", or a file that a
//! neighbour can touch when they are working on the tower.
//!
//! Two decisions worth stating, because they are the ones that make it a
//! safety feature rather than a status light:
//!
//! * **Fail closed.** A check that cannot be run, times out, or has never run
//!   counts as a failure. The failure mode of an unreadable SWR meter is not
//!   "assume it is fine".
//! * **It blocks station identification too.** Identification jumps the data
//!   queue, not the airtime clock — but a station that must not transmit must
//!   not transmit; a licence requires you to identify transmissions you make,
//!   not to make one.

use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tracing::{info, warn};

use crate::ax25::AirtimeShared;
use crate::config::InterlockConfig;

/// Result of one check, for logging and for `RADIO STATUS`.
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Pass,
    /// The command ran and said no.
    Fail(String),
    /// The command could not be run at all, or took too long. Treated exactly
    /// like a failure.
    Unavailable(String),
}

impl Check {
    pub fn is_pass(&self) -> bool {
        matches!(self, Check::Pass)
    }

    pub fn reason(&self) -> &str {
        match self {
            Check::Pass => "ok",
            Check::Fail(r) | Check::Unavailable(r) => r,
        }
    }
}

/// Run the interlock command once.
pub async fn run_once(config: &InterlockConfig) -> Check {
    let mut cmd = Command::new(&config.command);
    cmd.args(&config.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let timeout = Duration::from_secs(config.timeout_secs.max(1));
    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Check::Unavailable(format!("cannot run {}: {e}", config.command));
        }
        Err(_) => {
            return Check::Unavailable(format!(
                "{} did not answer within {}s",
                config.command,
                timeout.as_secs()
            ));
        }
    };
    if output.status.success() {
        return Check::Pass;
    }
    // The command's own words are the most useful thing to show an operator,
    // so take the last line it printed. It is external output: keep it to one
    // short line and strip anything that could break an IRC message.
    let text = String::from_utf8_lossy(&output.stderr);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        text.into_owned()
    };
    let detail: String = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no output")
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    Check::Fail(format!(
        "{} exited {}: {detail}",
        config.command,
        output.status.code().unwrap_or(-1)
    ))
}

/// Poll the interlock forever, publishing the result into `shared`.
pub fn spawn(config: InterlockConfig, shared: Arc<AirtimeShared>) {
    // Until the first check succeeds, the transmitter stays down.
    shared.interlock_ok.store(false, Ordering::Release);
    shared.wake.notify_waiters();
    info!(
        command = %config.command,
        "transmit interlock enabled; the transmitter stays inhibited until it passes"
    );
    tokio::spawn(async move {
        let interval = Duration::from_secs(config.interval_secs.max(1));
        let mut last_ok: Option<bool> = None;
        loop {
            let check = run_once(&config).await;
            let ok = check.is_pass();
            shared.interlock_ok.store(ok, Ordering::Release);
            // Log transitions, not every poll: this runs every thirty seconds
            // for the life of the process.
            if last_ok != Some(ok) {
                shared.wake.notify_waiters();
                if ok {
                    info!("transmit interlock passed; transmitting is permitted again");
                } else {
                    warn!(
                        "transmit interlock FAILED, transmitter inhibited: {}",
                        check.reason()
                    );
                }
                last_ok = Some(ok);
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(command: &str, args: &[&str]) -> InterlockConfig {
        InterlockConfig {
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            interval_secs: 30,
            timeout_secs: 2,
        }
    }

    #[tokio::test]
    async fn a_successful_command_passes() {
        assert_eq!(run_once(&cfg("true", &[])).await, Check::Pass);
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_own_words() {
        let c = cfg("sh", &["-c", "echo 'SWR 4.2:1 — too high' >&2; exit 1"]);
        let check = run_once(&c).await;
        assert!(!check.is_pass());
        assert!(check.reason().contains("SWR 4.2"), "{}", check.reason());
    }

    #[tokio::test]
    async fn a_missing_command_fails_closed() {
        let check = run_once(&cfg("/nonexistent/swr-check", &[])).await;
        assert!(
            matches!(check, Check::Unavailable(_)),
            "an interlock that cannot be run must not read as a pass"
        );
    }

    #[tokio::test]
    async fn a_hanging_command_times_out_and_fails() {
        let mut c = cfg("sleep", &["30"]);
        c.timeout_secs = 1;
        let check = run_once(&c).await;
        assert!(!check.is_pass());
        assert!(
            check.reason().contains("did not answer"),
            "{}",
            check.reason()
        );
    }
}
