//! Messages, and the gate between an IRC user and the transmitter.
//!
//! [`chat_for_air`] is the allowlist for IRC bodies. The radio only emits
//! deliveries that are on that same list (chat, topic, optional presence).
//! A new event type does not transmit until it is named. [`Server::screen_for_air`]
//! is then the gate for privilege, callsign and airtime. Every refusal happens
//! before the message is committed to the radio queue, and every one of them
//! says something to the sender.

use std::time::Instant;

use crate::irc::message::{is_channel_name, parse_ctcp, Message};
use crate::irc::numerics as num;
use crate::policy::Verdict;

use super::super::state::UserId;
use super::super::{Delivery, Server, TxClass};
use super::Screened;

/// When a screened message would actually reach the air.
///
/// The difference matters for exactly one check. Everything else — privilege,
/// callsign, rate limit, content — applies the same either way.
#[derive(Copy, Clone, Debug)]
enum AirTiming {
    /// Queued for transmission now, so the airtime backlog applies.
    Now,
    /// Held for a station that is not on frequency. It will be transmitted
    /// whenever that station is next heard.
    Held,
}

/// Text that is on the air allowlist: ordinary PRIVMSG, and `/me` (CTCP ACTION).
///
/// NOTICE, VERSION, DCC, PING, and anything else is not listed, so it stays
/// on IRC. Adding a new CTCP kind does not put it on the air until it is
/// named here.
fn chat_for_air(text: &str, notice: bool) -> Option<String> {
    if notice {
        return None;
    }
    match parse_ctcp(text) {
        Some((cmd, rest)) if cmd.eq_ignore_ascii_case("ACTION") => Some(format!("/me {rest}")),
        Some(_) => None,
        None => Some(text.to_string()),
    }
}

/// Put the screened air body back into the IRC delivery.
///
/// `/me` is screened as `/me rest` and may be shortened. Rebuild the CTCP
/// ACTION so clients still highlight it, with the same (possibly truncated)
/// rest that will be radiated. Leaving the original CTCP in place was how a
/// long `/me` could be flagged truncated and still go out in full.
fn delivery_text_after_screen(original: &str, screened: Screened) -> (String, bool) {
    if parse_ctcp(original).is_some_and(|(cmd, _)| cmd.eq_ignore_ascii_case("ACTION")) {
        if let Some(rest) = screened.text.strip_prefix("/me ") {
            return (format!("\u{1}ACTION {rest}\u{1}"), screened.truncated);
        }
        if screened.text == "/me" {
            return ("\u{1}ACTION\u{1}".into(), screened.truncated);
        }
    }
    (screened.text, screened.truncated)
}

impl Server {
    pub(super) fn cmd_privmsg(&mut self, uid: &UserId, msg: &Message, notice: bool) {
        let Some(target) = msg.param(0).map(|s| s.to_string()) else {
            self.numeric(uid, num::ERR_NOSUCHNICK, &["*", "No recipient given"]);
            return;
        };
        let Some(text) = msg.param(1).map(|s| s.to_string()) else {
            self.numeric(uid, num::ERR_NOTEXTTOSEND, &["No text to send"]);
            return;
        };
        if text.is_empty() {
            self.numeric(uid, num::ERR_NOTEXTTOSEND, &["No text to send"]);
            return;
        }
        let Some(sender) = self.state.user(uid).cloned() else {
            return;
        };

        if is_channel_name(&target) {
            let Some(chan) = self.state.channel(&target).cloned() else {
                self.numeric(uid, num::ERR_NOSUCHCHANNEL, &[&target, "No such channel"]);
                return;
            };
            let member = chan.members.get(uid).copied();
            if member.is_none() {
                self.numeric(
                    uid,
                    num::ERR_CANNOTSENDTOCHAN,
                    &[&chan.name, "Cannot send to channel"],
                );
                return;
            }
            if chan.moderated && !member.map(|f| f.op || f.voice).unwrap_or(false) {
                self.numeric(
                    uid,
                    num::ERR_CANNOTSENDTOCHAN,
                    &[
                        &chan.name,
                        "Channel is moderated (+m); identify with CALLSIGN for +v",
                    ],
                );
                return;
            }

            let mut text = text;
            let mut truncated = false;
            let air = chat_for_air(&text, notice);
            // Keep the empty-channel lock for ordinary users: there is nobody
            // listening, so their chat stays on IRC. RF-TX (OPER or GRANT) may
            // still CQ, otherwise a station on frequency can never hear the
            // gateway and join.
            let mut allow_rf = air.is_some()
                && chan.rf
                && self.radio.available()
                && (chan.has_rf_members()
                    || (self.config.radio.rf_mode.is_airc() && self.user_may_tx_rf(uid)));
            let mut rf_flooded = false;
            if allow_rf {
                if !self.policy.rf_channel_rate_ok(
                    &format!("{}:{}", self.rate_key(uid), chan.name),
                    Instant::now(),
                ) {
                    self.notice_user(
                        uid,
                        "Flood protection: too many messages on the RF channel. Slow down.",
                    );
                    self.audit.event(
                        "flood_rf",
                        &[("nick", &sender.nick), ("channel", &chan.name)],
                    );
                    // The cap is on radiation, not on the internet side.
                    allow_rf = false;
                    rf_flooded = true;
                }
            }
            if allow_rf {
                match self.screen_for_air(uid, air.as_deref().unwrap_or(&text)) {
                    Some(s) => {
                        (text, truncated) = delivery_text_after_screen(&text, s);
                    }
                    None => allow_rf = false,
                }
            } else if !rf_flooded && chan.rf && air.is_some() {
                let why = self.channel_air_line(&chan.name);
                self.notice_air_reason(uid, &chan.name, &why);
            }
            let d = Delivery::Privmsg {
                from_nick: sender.nick.clone(),
                from_prefix: sender.prefix(),
                target: chan.name.clone(),
                text,
                notice,
                truncated,
            };
            self.broadcast_channel_ex(&chan.name, &d, Some(uid), allow_rf);
            if allow_rf && self.config.radio.notice_air_relay && !notice {
                // Say "queued", not "relayed". On a duty-limited channel the
                // two are different, sometimes by a minute, and a sender who
                // is told the truth will not repeat themselves — which is the
                // cheapest airtime saving available.
                let eta = self.radio.eta();
                let when = if eta.as_secs() < 2 {
                    "going out now".to_string()
                } else {
                    format!("about {}s of queue ahead of it", eta.as_secs())
                };
                let n = chan.rf_member_count();
                let audience = if n == 0 {
                    "CQ: no RF station in the channel yet.".to_string()
                } else {
                    format!("{n} station(s) on frequency.")
                };
                self.notice_user(
                    uid,
                    &format!(
                        "Queued for RF ({}), {when}. {audience}",
                        self.config.radio.callsign
                    ),
                );
            }
            return;
        }

        let Some(target_id) = self.find_target(&target) else {
            self.offer_mailbox(uid, &sender.nick, &target, &text, notice);
            return;
        };
        let mut text = text;
        let mut truncated = false;
        if target_id.is_rf() {
            let Some(air) = chat_for_air(&text, notice) else {
                self.notice_user(uid, "Only PRIVMSG chat (including /me) is put on the air.");
                return;
            };
            match self.screen_for_air(uid, &air) {
                Some(s) => {
                    (text, truncated) = delivery_text_after_screen(&text, s);
                }
                None => return,
            }
        }
        if let Some(away) = self.state.user(&target_id).and_then(|u| u.away.clone()) {
            self.numeric(uid, num::RPL_AWAY, &[&target, &away]);
        }
        let d = Delivery::Privmsg {
            from_nick: sender.nick.clone(),
            from_prefix: sender.prefix(),
            target: target.clone(),
            text,
            notice,
            truncated,
        };
        self.deliver(&target_id, &d);
    }

    /// A message to a station that is not currently in range. If it names a
    /// plausible callsign and the mailbox is enabled, hold it; otherwise this
    /// is an ordinary "no such nick".
    pub(super) fn offer_mailbox(
        &mut self,
        uid: &UserId,
        from: &str,
        target: &str,
        text: &str,
        notice: bool,
    ) {
        let Some(air) = chat_for_air(text, notice) else {
            self.numeric(uid, num::ERR_NOSUCHNICK, &[target, "No such nick/channel"]);
            return;
        };
        let call = crate::callsign::Callsign::from_nick(target)
            .ok()
            .filter(|c| c.looks_like_amateur_call());
        let Some(call) = call else {
            self.numeric(uid, num::ERR_NOSUCHNICK, &[target, "No such nick/channel"]);
            return;
        };
        if !self.radio.mailbox.enabled || !self.config.radio.enabled {
            self.numeric(uid, num::ERR_NOSUCHNICK, &[target, "No such nick/channel"]);
            return;
        }
        let Some(screened) = self.screen_for_mailbox(uid, &air) else {
            return;
        };
        let message = crate::server::mailbox::StoredMessage {
            from: from.to_string(),
            text: screened.text,
            truncated: screened.truncated,
            notice,
            stored_at: Instant::now(),
        };
        match self.radio.mailbox.store(&call, message) {
            Ok(depth) => {
                let hours = self.config.radio.mailbox_ttl_secs / 3600;
                self.notice_user(
                    uid,
                    &format!(
                        "{call} is not on frequency. Held for delivery when the station is \
                         next heard ({depth} waiting, dropped after {hours}h)."
                    ),
                );
            }
            Err(e) => {
                let reason = match e {
                    crate::server::mailbox::StoreError::Disabled => "the mailbox is disabled",
                    crate::server::mailbox::StoreError::StationFull => {
                        "that station already has as much mail as it can hold"
                    }
                    crate::server::mailbox::StoreError::GatewayFull => {
                        "the gateway mailbox is full"
                    }
                };
                self.notice_user(uid, &format!("Not held for {call}: {reason}."));
            }
        }
    }

    /// Common gate for anything an IP user wants to put on the air. Returns
    /// the text to transmit, or `None` if it must not be transmitted (the
    /// user has already been told why).
    ///
    /// Every refusal below happens *before* the message is committed to the
    /// radio queue, and every one of them says something to the sender. The
    /// alternative — accept it, queue it, and let the transmitter drop it two
    /// minutes later — is the worst of both worlds: the sender believes it
    /// went out, and the airtime was reserved for nothing.
    pub(super) fn screen_for_air(&mut self, uid: &UserId, text: &str) -> Option<Screened> {
        self.screen(uid, text, AirTiming::Now)
    }

    /// As [`Server::screen_for_air`], for a message that will be held until
    /// the station is next heard.
    pub(super) fn screen_for_mailbox(&mut self, uid: &UserId, text: &str) -> Option<Screened> {
        self.screen(uid, text, AirTiming::Held)
    }

    fn screen(&mut self, uid: &UserId, text: &str, timing: AirTiming) -> Option<Screened> {
        if !self.radio.available() {
            self.notice_user(
                uid,
                "The transmitter is off; your message stayed on the wire.",
            );
            return None;
        }
        if !self.user_may_tx_rf(uid) {
            self.notice_user(
                uid,
                "Not relayed to RF: you do not have RF-TX privilege. \
                 Register this nick, then ask a control operator to RADIO GRANT it. \
                 CALLSIGN is also required. Until then your messages stay on IRC.",
            );
            return None;
        }
        let identified = self.state.user(uid).and_then(|u| u.callsign.clone());
        if identified.is_none() {
            self.notice_user(
                uid,
                "Not relayed to RF: identify with CALLSIGN <yourcall> first. \
                 Everything transmitted here is third-party traffic under the \
                 gateway licensee's responsibility.",
            );
            return None;
        }
        if let Some(call) = &identified {
            if !self.policy.station_allowed(call) {
                self.notice_user(
                    uid,
                    "Not relayed to RF: your callsign is not permitted on this gateway.",
                );
                return None;
            }
        }
        if !self.policy.ip_rate_ok(&self.rate_key(uid), Instant::now()) {
            // Quote the actual link speed. "1200 bits per second" was
            // hardcoded, which is wrong on every HF gateway.
            let baud = self.config.radio.duty.baud;
            self.notice_user(
                uid,
                &format!(
                    "Not relayed to RF: you are sending faster than the channel can carry. \
                     Slow down; the radio side is {baud} bits per second."
                ),
            );
            return None;
        }
        let mut truncated = false;
        let screened = match self.policy.screen_outbound(text) {
            Verdict::Allow(t) => t,
            Verdict::Truncated(t) => {
                truncated = true;
                self.notice_user(
                    uid,
                    &format!(
                        "Your message was shortened to {} characters before transmission. \
                         The radio side carries sentences, not paragraphs.",
                        self.policy.config.max_rf_text_len
                    ),
                );
                t
            }
            Verdict::Deny(reason) => {
                self.notice_user(uid, reason);
                return None;
            }
        };

        // Last gate, and the one that protects the transmitter: is there room
        // in the airtime backlog? Checked here rather than at the transmitter
        // because from here we can still tell the sender. The payload also
        // carries the channel name and the sender's nick, so allow for those.
        //
        // It does not apply to a message being held: that will be transmitted
        // whenever the station next appears, which may be hours away, so the
        // backlog as it stands this instant says nothing about it. Refusing on
        // those grounds would mean a momentarily busy channel silently turned
        // off store-and-forward — exactly when it is most wanted.
        if matches!(timing, AirTiming::Held) {
            return Some(Screened {
                text: screened,
                truncated,
            });
        }
        if self.radio.interlock_down() {
            self.notice_user(
                uid,
                "Not put on the air: the safety interlock is holding the transmitter. \
                 Your message stayed on IRC.",
            );
            return None;
        }
        let octets = self.radio.wire_octets(screened.len() + 40);
        if !self.radio.backlog_has_room(octets, TxClass::Chat) {
            let queued = self.radio.eta().as_secs();
            self.notice_user(
                uid,
                &format!(
                    "Not put on the air: the transmit queue is {queued}s deep and the duty-cycle \
                     limit will not clear it in time. Your message was delivered on IRC. \
                     Try again shortly — or say it shorter."
                ),
            );
            self.radio.stats.rf_frames_refused += 1;
            self.audit
                .event("rf_backlog_refused", &[("octets", &octets.to_string())]);
            return None;
        }
        Some(Screened {
            text: screened,
            truncated,
        })
    }
}

#[cfg(test)]
mod chat_allowlist {
    use super::chat_for_air;

    #[test]
    fn only_privmsg_chat_and_action() {
        assert_eq!(chat_for_air("hello", false).as_deref(), Some("hello"));
        assert_eq!(
            chat_for_air("\u{1}ACTION waves\u{1}", false).as_deref(),
            Some("/me waves")
        );
        assert!(chat_for_air("hello", true).is_none());
        assert!(chat_for_air("\u{1}VERSION\u{1}", false).is_none());
        assert!(chat_for_air("\u{1}PING 1\u{1}", false).is_none());
    }

    #[test]
    fn a_truncated_action_keeps_the_shortened_body() {
        use super::super::Screened;
        use super::delivery_text_after_screen;

        let original = format!("\u{1}ACTION {}\u{1}", "x".repeat(40));
        let screened = Screened {
            text: format!("/me {}\u{2026}", "x".repeat(10)),
            truncated: true,
        };
        let (text, truncated) = delivery_text_after_screen(&original, screened);
        assert!(truncated);
        assert!(text.contains("\u{1}ACTION "));
        assert!(text.contains('\u{2026}'));
        assert!(
            !text.contains(&"x".repeat(40)),
            "the unshortened rest must not survive: {text:?}"
        );
    }
}
