//! Getting a client from a raw socket to a registered user: `PASS`, `NICK`,
//! `USER`, and the welcome burst that follows.
//!
//! Nothing else may run until this succeeds — see the gate in the dispatcher —
//! because an unregistered connection has no nickname to answer to and no
//! identity to rate-limit.

use std::time::{Duration, Instant};

use crate::callsign::Callsign;
use crate::irc::message::{is_valid_nick, lower, Message};
use crate::irc::numerics as num;

use super::super::state::UserId;
use super::super::{Delivery, Server};
use super::constant_time_eq;

impl Server {
    pub(super) fn cmd_pass(&mut self, uid: &UserId, msg: &Message) {
        let Some(given) = msg.param(0) else {
            self.numeric(
                uid,
                num::ERR_NEEDMOREPARAMS,
                &["PASS", "Not enough parameters"],
            );
            return;
        };
        let ok = self
            .connection_password
            .as_ref()
            .map(|p| constant_time_eq(p, given))
            .unwrap_or(true);
        if let Some(u) = self.state.user_mut(uid) {
            u.pass_ok = ok;
        }
    }

    pub(super) fn cmd_nick(&mut self, uid: &UserId, msg: &Message) {
        let Some(nick) = msg.param(0).map(|s| s.to_string()) else {
            self.numeric(uid, num::ERR_NONICKNAMEGIVEN, &["No nickname given"]);
            return;
        };
        if !is_valid_nick(&nick, self.config.server.max_nick_len) {
            self.numeric(
                uid,
                num::ERR_ERRONEUSNICKNAME,
                &[&nick, "Erroneous nickname"],
            );
            return;
        }
        // Callsign-shaped nicks belong to RF stations. RFC 1459 casemapping
        // makes `\` the uppercase of `|`, so SM0ABC\7 must be rejected too,
        // as must the AX.25 form SM0ABC-7.
        if Callsign::reserved_from_nick(&nick).is_some() {
            self.numeric(
                uid,
                num::ERR_ERRONEUSNICKNAME,
                &[
                    &nick,
                    "Callsign nicknames are reserved; use CALLSIGN to identify",
                ],
            );
            return;
        }
        let was_registered = self.state.user(uid).map(|u| u.registered).unwrap_or(false);
        let old = self
            .state
            .user(uid)
            .map(|u| u.nick.clone())
            .unwrap_or_default();
        // Own nick, including a case-only change. `nick_taken` is true for
        // ourselves, and treating that as 433 made `/nick alice` and `/nick
        // Alice` fail — and a case change going through the full path would
        // have dropped IDENTIFY and CALLSIGN.
        if !old.is_empty() && old != "*" && lower(&old) == lower(&nick) {
            if old != nick {
                let prefix = self.state.user(uid).map(|u| u.prefix()).unwrap_or_default();
                let _ = self.state.set_nick(uid, &nick);
                if was_registered {
                    let d = Delivery::NickChange {
                        old_nick: old,
                        prefix,
                        new_nick: nick,
                    };
                    self.broadcast_peers(uid, &d, true);
                }
            }
            if !was_registered {
                self.try_complete_registration(uid);
            }
            return;
        }

        if self.state.nick_taken(&nick) {
            self.numeric(
                uid,
                num::ERR_NICKNAMEINUSE,
                &[&nick, "Nickname is already in use"],
            );
            return;
        }
        let prefix = self.state.user(uid).map(|u| u.prefix()).unwrap_or_default();
        if !self.state.set_nick(uid, &nick) {
            self.numeric(
                uid,
                num::ERR_NICKNAMEINUSE,
                &[&nick, "Nickname is already in use"],
            );
            return;
        }
        let claimed = self.accounts.is_registered(&nick);
        let timeout = Duration::from_secs(self.config.accounts.identify_timeout_secs);
        if let Some(u) = self.state.user_mut(uid) {
            u.got_nick = true;
            u.nick_identified = false;
            // The callsign belongs to a nick, not to the TCP session. Keeping
            // it across NICK would let mallory radiate (and hold +v on +r)
            // as alice's SM0ABC.
            u.callsign = None;
            u.identify_by = if claimed {
                Some(Instant::now() + timeout)
            } else {
                None
            };
        }
        if claimed {
            self.notice_user(
                uid,
                &format!(
                    "This nick is registered. IDENTIFY <password> within {} seconds or it will be released.",
                    self.config.accounts.identify_timeout_secs
                ),
            );
        }

        if was_registered {
            let d = Delivery::NickChange {
                old_nick: old,
                prefix,
                new_nick: nick.clone(),
            };
            self.broadcast_peers(uid, &d, true);
            self.refresh_privileges(uid);
        } else {
            self.try_complete_registration(uid);
        }
    }

    pub(super) fn cmd_user(&mut self, uid: &UserId, msg: &Message) {
        if self.state.user(uid).map(|u| u.registered).unwrap_or(false) {
            self.numeric(uid, num::ERR_ALREADYREGISTERED, &["You may not reregister"]);
            return;
        }
        if msg.params.len() < 4 {
            self.numeric(
                uid,
                num::ERR_NEEDMOREPARAMS,
                &["USER", "Not enough parameters"],
            );
            return;
        }
        let Some(username) = username_from_user_param(&msg.params[0]) else {
            self.numeric(
                uid,
                num::ERR_ERRONEUSNICKNAME,
                &[
                    &msg.params[0],
                    "Username may not contain ! or @ or control characters",
                ],
            );
            return;
        };
        if let Some(u) = self.state.user_mut(uid) {
            u.username = username;
            u.realname = msg.params[3].clone();
            u.got_user = true;
        }
        self.try_complete_registration(uid);
    }

    pub(super) fn try_complete_registration(&mut self, uid: &UserId) {
        let Some(user) = self.state.user(uid).cloned() else {
            return;
        };
        if user.registered || !user.got_nick || !user.got_user {
            return;
        }
        if self.connection_password.is_some() && !user.pass_ok {
            self.numeric(uid, num::ERR_PASSWDMISMATCH, &["Password incorrect"]);
            if let UserId::Ip(id) = uid {
                self.send_raw(*id, "ERROR :Closing link (bad password)".into());
            }
            self.quit_user(uid, "Bad password");
            return;
        }
        if let Some(u) = self.state.user_mut(uid) {
            u.registered = true;
        }
        if let Some(u) = self.state.user(uid) {
            self.audit.event(
                "register",
                &[("nick", &u.nick), ("user", &u.username), ("host", &u.host)],
            );
        }

        let server = self.server_name().to_string();
        let network = self.config.server.network.clone();
        self.numeric(
            uid,
            num::RPL_WELCOME,
            &[&format!(
                "Welcome to the {network} amateur radio IRC network {}",
                user.nick
            )],
        );
        self.numeric(
            uid,
            num::RPL_YOURHOST,
            &[&format!(
                "Your host is {server}, running rfircd {}",
                env!("CARGO_PKG_VERSION")
            )],
        );
        self.numeric(
            uid,
            num::RPL_CREATED,
            &["This server was created at startup"],
        );
        self.numeric(
            uid,
            num::RPL_MYINFO,
            &[
                &server,
                &format!("rfircd-{}", env!("CARGO_PKG_VERSION")),
                "iow",
                "mnrtkl",
            ],
        );

        let radio = if self.radio.available() { "ON" } else { "OFF" };
        // Each token is its own middle parameter. A single space-separated
        // string would be scrubbed into `CHANTYPES=#&_PREFIX=…` and clients
        // would not see ISUPPORT tokens at all.
        let isupport = [
            "CHANTYPES=#&".to_string(),
            "PREFIX=(ov)@+".to_string(),
            format!("NICKLEN={}", self.config.server.max_nick_len),
            "CHANNELLEN=50".to_string(),
            "CASEMAPPING=rfc1459".to_string(),
            format!("NETWORK={network}"),
            "MAXTARGETS=1".to_string(),
            "TOPICLEN=200".to_string(),
            "CHANMODES=k,l,r,mnt".to_string(),
            format!("RADIO={radio}"),
            format!("RFCALL={}", self.config.radio.callsign),
            "are supported by this server".to_string(),
        ];
        let isupport: Vec<&str> = isupport.iter().map(|s| s.as_str()).collect();
        self.numeric(uid, num::RPL_ISUPPORT, &isupport);

        self.send_lusers(uid);
        self.send_motd(uid);

        if user.listen_only {
            self.notice_user(
                uid,
                "This connection is listen-only (plaintext from off the machine). \
                 Connect with TLS to speak, identify, OPER, or control the radio. \
                 Everything that reaches the air is still in the clear.",
            );
        }

        let status = self.radio.status_line();
        self.notice_user(uid, &status);
        self.notice_user(
            uid,
            "On +r channels CALLSIGN grants +v (speak on IRC). A control operator \
             must RADIO GRANT a registered nick before that nick's messages are \
             radiated. REGISTER/IDENTIFY keep the nick and the grant across restarts.",
        );
        if self.accounts.is_registered(&user.nick) {
            self.notice_user(
                uid,
                &format!(
                    "{} is registered. IDENTIFY <password> within {} seconds.",
                    user.nick, self.config.accounts.identify_timeout_secs
                ),
            );
        }
    }

    pub(super) fn send_lusers(&mut self, uid: &UserId) {
        let total = self.state.users.len();
        let rf = self.state.users.keys().filter(|u| u.is_rf()).count();
        let text = format!("There are {total} users online, {rf} of them on RF");
        self.numeric(uid, num::RPL_LUSERCLIENT, &[&text]);
        let me = format!("I have {} clients and 0 servers", total - rf);
        self.numeric(uid, num::RPL_LUSERME, &[&me]);
    }

    pub(super) fn send_motd(&mut self, uid: &UserId) {
        if self.config.server.motd.is_empty() {
            self.numeric(uid, num::ERR_NOMOTD, &["MOTD File is missing"]);
            return;
        }
        let server = self.server_name().to_string();
        self.numeric(
            uid,
            num::RPL_MOTDSTART,
            &[&format!("- {server} Message of the day -")],
        );
        for line in self.config.server.motd.clone() {
            self.numeric(uid, num::RPL_MOTD, &[&format!("- {line}")]);
        }
        self.numeric(uid, num::RPL_ENDOFMOTD, &["End of /MOTD command"]);
    }
}

/// USER ident as it appears in `nick!user@host`. `!` and `@` would make the
/// prefix unparseable; control characters do not belong there either.
fn username_from_user_param(raw: &str) -> Option<String> {
    let s: String = raw.chars().take(10).collect();
    if s.is_empty()
        || s.chars()
            .any(|c| c == '!' || c == '@' || c < ' ' || c == '\u{7f}')
    {
        return None;
    }
    Some(s)
}
