//! Client queries that do not change server state: `VERSION`, `TIME`, `ADMIN`,
//! `INFO`, `HELP`, `STATS`, `LINKS`, and the IRCv3 `CAP` handshake.
//!
//! These exist so a modern IRC client can finish connecting and ask ordinary
//! questions. They never key the transmitter.

use crate::irc::message::Message;
use crate::irc::numerics as num;
use crate::server::state::{ClientId, UserId};
use crate::server::Server;

impl Server {
    pub(super) fn cmd_cap(&mut self, uid: &UserId, msg: &Message) {
        let UserId::Ip(id) = *uid else {
            return;
        };
        let sub = msg.param(0).unwrap_or("").to_ascii_uppercase();
        let nick = self
            .state
            .user(uid)
            .map(|u| {
                if u.registered {
                    u.nick.clone()
                } else {
                    "*".into()
                }
            })
            .unwrap_or_else(|| "*".into());
        let name = self.server_name().to_string();
        match sub.as_str() {
            "LS" | "LIST" => {
                // No IRCv3 capabilities. An empty list plus CAP END from the
                // client is the handshake every modern client already knows.
                self.cap_line(id, &name, &nick, &sub, "");
            }
            "REQ" => {
                let asked = msg.param(1).unwrap_or("").to_string();
                self.cap_line(id, &name, &nick, "NAK", &asked);
            }
            "END" => {}
            _ => {}
        }
    }

    fn cap_line(&mut self, id: ClientId, server: &str, nick: &str, verb: &str, rest: &str) {
        self.send_raw(
            id,
            Message::new(
                "CAP",
                vec![nick.to_string(), verb.to_string(), rest.to_string()],
            )
            .with_prefix(server.to_string())
            .to_string(),
        );
    }

    pub(super) fn cmd_version(&mut self, uid: &UserId) {
        let ver = format!("rfircd-{}", env!("CARGO_PKG_VERSION"));
        let server = self.server_name().to_string();
        self.numeric(
            uid,
            num::RPL_VERSION,
            &[
                &ver,
                &server,
                "IRC + AX.25 gateway. See HELP. Air is always in the clear.",
            ],
        );
    }

    pub(super) fn cmd_time(&mut self, uid: &UserId) {
        let server = self.server_name().to_string();
        let now = utc_now_rfc2822();
        self.numeric(uid, num::RPL_TIME, &[&server, &now]);
    }

    pub(super) fn cmd_admin(&mut self, uid: &UserId) {
        let server = self.server_name().to_string();
        self.numeric(uid, num::RPL_ADMINME, &[&server, "Administrative info"]);
        self.numeric(
            uid,
            num::RPL_ADMINLOC1,
            &["This is an amateur-radio packet gateway, not a public IRC network."],
        );
        self.numeric(
            uid,
            num::RPL_ADMINLOC2,
            &["The control operator is whoever can OPER on this process."],
        );
        self.numeric(
            uid,
            num::RPL_ADMINEMAIL,
            &["See MOTD and the station licence."],
        );
    }

    pub(super) fn cmd_info(&mut self, uid: &UserId) {
        for line in [
            concat!(
                "rfircd ",
                env!("CARGO_PKG_VERSION"),
                " — IRC server with an AX.25 gateway"
            ),
            "Single server; no linking, no services, no DCC, no on-air encryption.",
            "The air is an allowlist: PRIVMSG chat and /me, plus TOPIC. Anything",
            "not named stays on IRC — a new event type does not transmit until listed.",
            "https://github.com/mashu/rfircd",
        ] {
            self.numeric(uid, num::RPL_INFO, &[line]);
        }
        self.numeric(uid, num::RPL_ENDOFINFO, &["End of /INFO list"]);
    }

    pub(super) fn cmd_help(&mut self, uid: &UserId, msg: &Message) {
        let topic = msg.param(0).unwrap_or("index").to_ascii_lowercase();
        self.numeric(uid, num::RPL_HELPSTART, &[&topic, "Help"]);
        let body: &[&str] = match topic.as_str() {
            "air" | "radio" | "rf" => &[
                "Allowlist: PRIVMSG chat and /me, plus TOPIC (AIRC mode only).",
                "JOIN/PART only if presence_notices is on (AIRC). APRS messages",
                "to the gateway enter a +r channel or query a nick. Replies and",
                "channel chat reach APRS HTs as addressed APRS when needed.",
                "positions/status beacons are IRC NOTICE only, never retransmitted.",
                "NOTICE, CTCP, MODE, KICK, numerics stay on IRC until listed.",
            ],
            _ => &[
                "NICK USER PASS QUIT PING PONG JOIN PART PRIVMSG NOTICE TOPIC",
                "NAMES LIST WHO WHOIS WHOWAS MODE MOTD LUSERS AWAY KICK INVITE",
                "ISON USERHOST VERSION TIME ADMIN INFO HELP STATS LINKS CAP",
                "OPER CALLSIGN REGISTER IDENTIFY UNREGISTER RADIO",
                "OPER also: KILL ACCOUNTS DROPNICK UNCLAIM KLINE UNKLINE KLINES PASSWD",
                "HELP AIR — what is transmitted vs what stays on IRC",
            ],
        };
        for line in body {
            self.numeric(uid, num::RPL_HELPTXT, &[&topic, line]);
        }
        self.numeric(uid, num::RPL_ENDOFHELP, &[&topic, "End of /HELP"]);
    }

    pub(super) fn cmd_stats(&mut self, uid: &UserId, msg: &Message) {
        let what = msg
            .param(0)
            .map(|s| s.chars().next().unwrap_or('u'))
            .unwrap_or('u')
            .to_ascii_lowercase();
        let letter = what.to_string();
        match what {
            'u' => {
                let up = self.uptime().as_secs();
                let text = format!(
                    "Server Up {days} days {hours:02}:{mins:02}:{secs:02}",
                    days = up / 86400,
                    hours = (up % 86400) / 3600,
                    mins = (up % 3600) / 60,
                    secs = up % 60,
                );
                self.numeric(uid, num::RPL_STATSUPTIME, &[&text]);
            }
            _ => self.notice_user(uid, "STATS u (uptime)"),
        }
        self.numeric(uid, num::RPL_ENDOFSTATS, &[&letter, "End of /STATS"]);
    }

    pub(super) fn cmd_links(&mut self, uid: &UserId) {
        let server = self.server_name().to_string();
        self.numeric(
            uid,
            num::RPL_LINKS,
            &[&server, &server, "0 rfircd (no server linking)"],
        );
        self.numeric(uid, num::RPL_ENDOFLINKS, &["*", "End of /LINKS list"]);
    }
}

/// UTC clock as `Www, DD Mon YYYY HH:MM:SS +0000` without a time crate.
fn utc_now_rfc2822() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d, hh, mm, ss) = civil_utc(t);
    const WDAY: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // 1970-01-01 was Thursday.
    let wday = WDAY[((t / 86400) % 7) as usize];
    format!(
        "{wday}, {d:02} {mon} {y} {hh:02}:{mm:02}:{ss:02} +0000",
        mon = MON[m as usize - 1]
    )
}

fn civil_utc(unix: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (unix / 86400) as i64;
    let rem = unix % 86400;
    let hh = (rem / 3600) as u32;
    let mm = ((rem % 3600) / 60) as u32;
    let ss = (rem % 60) as u32;
    // Howard Hinnant's civil_from_days, Unix epoch = 719468 days from 0000-03-01.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, hh, mm, ss)
}

#[cfg(test)]
mod tests {
    use super::civil_utc;

    #[test]
    fn unix_epoch_is_the_first_of_january_1970() {
        assert_eq!(civil_utc(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_utc(86400 + 3661), (1970, 1, 2, 1, 1, 1));
    }
}
