//! The gateway proper: what happens when a frame arrives from the air.
//!
//! Asymmetry is deliberate. Uplink frames (station -> gateway) are as short as
//! possible because the station is transmitting on battery through a handheld;
//! downlink frames carry the sender's nickname because the receiving station
//! has no other way to know who spoke.
//!
//! * uplink   `MSG [target, text]`
//! * downlink `MSG [target, from, text]`

use std::time::Instant;

use tracing::{debug, info, warn};

use super::state::{User, UserId};
use super::{Delivery, Server, TxClass};
use crate::airc::{encode_fields, AircFrame, Kind};
use crate::aprs::{self, AprsMessage};
use crate::ax25::{frame::PID_NO_L3, Ax25Frame};
use crate::callsign::Callsign;
use crate::irc::message::is_channel_name;
use crate::policy::{sanitize, Verdict};

impl Server {
    pub(crate) fn handle_rf_frame(&mut self, frame: Ax25Frame, now: Instant) {
        self.radio.stats.rf_frames_rx += 1;

        if !frame.is_ui() || frame.pid != Some(PID_NO_L3) {
            debug!(target: "rf::monitor", "{}", frame.to_monitor_line());
            return;
        }
        let src = frame.source.call.clone();

        // Identification is addressed to `ID`, not to us. Log it before the
        // destination check or the Kind::Id arm is dead.
        if frame.destination.call.to_string() == "ID" {
            if let Ok(airc) = AircFrame::decode(&frame.info) {
                if airc.kind == Kind::Id {
                    // Sanitised, not raw: this runs before the source is
                    // checked for being a plausible callsign or a permitted
                    // one, so the text is from anybody within earshot, and
                    // `rf::monitor` is exactly the target an operator turns
                    // on to watch the frequency from a terminal.
                    info!(
                        target: "rf::monitor",
                        %src,
                        "identification: {}",
                        sanitize(&airc.fields().join(" "))
                    );
                }
            }
        }

        // Our own transmission coming back through a digipeater.
        if self.config.gateway_callsign().as_ref() == Some(&src) {
            return;
        }

        // APRS: messages put the addressee in the information field (AX.25
        // dest is a TOCALL). Positions and status are broadcasts. Check
        // both before the dest filter or a Kenwood on frequency is ignored.
        if self.config.radio.aprs {
            if let Some(msg) = AprsMessage::decode(&frame.info) {
                if let Some(gateway) = self.config.gateway_callsign() {
                    if msg.addressed_to(&gateway) {
                        self.handle_aprs_message(&src, msg, now);
                        return;
                    }
                }
                debug!(
                    target: "rf::monitor",
                    "APRS message not for us: {}",
                    frame.to_monitor_line()
                );
                return;
            }
            if let Some(beacon) = aprs::decode_beacon(&frame.destination.call, &frame.info) {
                self.handle_aprs_beacon(&src, beacon.summary, now);
                return;
            }
        }

        // PROTOCOL.md §3.1: a receiver must check the AX.25 destination before
        // processing a frame.
        //
        // Unicast to this gateway is ours. Broadcasts to a protocol address
        // (`AIRC`) are station-to-station channel chat: ingest 2-field MSG as
        // uplink, never ACK, never treat 3-field downlink as uplink (that is
        // the two-gateway loop). Unicast to anyone else is ignored.
        let for_us = self.frame_is_for_us(&frame);
        let broadcast = self.frame_is_protocol_broadcast(&frame);
        if !for_us && !broadcast {
            debug!(
                target: "rf::monitor",
                "not addressed to us: {}",
                frame.to_monitor_line()
            );
            return;
        }

        let airc = match AircFrame::decode(&frame.info) {
            Ok(f) => f,
            Err(_) => {
                // Other traffic shares this channel: NET/ROM, other people's
                // QSOs. Log it for the operator and leave it alone. APRS
                // messages and beacons are handled above.
                debug!(target: "rf::monitor", "{}", frame.to_monitor_line());
                return;
            }
        };

        if src.require_amateur().is_err() {
            warn!(%src, "ignoring AIRC frame from an implausible callsign");
            return;
        }
        if !self.policy.station_allowed(&src) {
            debug!(%src, "station not permitted, ignoring");
            return;
        }
        // A callsign we have never heard is the one an attacker invents. Its
        // per-station bucket is empty of history and its session slot is not
        // yet taken, so both are free to it; this bucket is not.
        if self.radio.sessions.peer(&src).is_none() && !self.policy.first_contact_ok(now) {
            debug!(%src, "first-contact budget spent, ignoring an unknown station");
            return;
        }

        let outcome = self.radio.sessions.on_receive(&src, airc, now, for_us);
        for f in outcome.transmit {
            self.transmit_airc(&src, f);
        }
        if let Some(msg) = outcome.deliver {
            let take = if broadcast {
                uplink_chat_from_broadcast(&msg)
            } else {
                true
            };
            if take {
                self.handle_airc_message(&src, msg, now);
            }
        }
        self.radio.drain_acks();
    }

    /// True when an AX.25 frame is addressed to this gateway.
    fn frame_is_for_us(&self, frame: &Ax25Frame) -> bool {
        self.config
            .gateway_callsign()
            .is_some_and(|call| call == frame.destination.call)
    }

    /// True when the destination is a protocol address such as `AIRC`.
    ///
    /// `ID` is also a protocol address, but identification is logged above
    /// and is never chat.
    fn frame_is_protocol_broadcast(&self, frame: &Ax25Frame) -> bool {
        let dest = &frame.destination.call;
        !dest.looks_like_amateur_call() && dest.to_string() != "ID"
    }

    /// An APRS message whose addressee is this gateway.
    ///
    /// The radio is a stock TNC: it will retry until it hears `ack<msgid>`.
    /// Answering is therefore cheaper than silence. Acks and rejects of *our*
    /// messages are ignored. Channel lines are `#chan text`, or the whole
    /// body if `radio.aprs_channel` is set. AIRC stations in that channel
    /// get a translated broadcast; they did not decode the APRS frame.
    fn handle_aprs_message(&mut self, src: &Callsign, msg: AprsMessage, now: Instant) {
        if src.require_amateur().is_err() {
            warn!(%src, "ignoring APRS message from an implausible callsign");
            return;
        }
        if !self.policy.station_allowed(src) {
            debug!(%src, "station not permitted, ignoring APRS");
            return;
        }
        if msg.is_ack_or_rej() {
            self.radio.on_aprs_ack(src, &msg, now);
            return;
        }
        if !self.policy.rf_station_rate_ok(src, now) {
            warn!(%src, "rate limit exceeded, dropping APRS message");
            if let Some(peer) = self.radio.sessions.peer_mut(src) {
                peer.dropped += 1;
            }
            return;
        }
        // Answering an unknown station is a transmission bought with a
        // received frame, so the first frame from a new callsign is rationed
        // globally — otherwise a flood of forged callsigns converts directly
        // into the licensee's airtime, one ACK at a time.
        if self.radio.sessions.peer(src).is_none() && !self.policy.first_contact_ok(now) {
            debug!(%src, "first-contact budget spent, ignoring an unknown APRS station");
            return;
        }
        if self.radio.sessions.touch(src, now).is_none() {
            self.aprs_ack(src, &msg);
            return;
        }
        if let Some(peer) = self.radio.sessions.peer_mut(src) {
            peer.note_aprs();
        }

        let duplicate = msg
            .msgid
            .as_ref()
            .is_some_and(|id| self.radio.aprs_msgid_seen(src, id, now));

        if self.radio.sessions.is_banned(src) {
            self.aprs_rej_or_ack(src, &msg);
            return;
        }
        if duplicate {
            self.aprs_ack(src, &msg);
            return;
        }
        if let Some(id) = &msg.msgid {
            self.radio.remember_aprs_msgid(src, id, now);
        }

        let text = sanitize(&msg.text);
        if aprs::is_help(&text) {
            self.aprs_ack(src, &msg);
            let hint = if self.config.radio.aprs_channel.is_empty() {
                "send #chan text"
            } else {
                "send #chan text, or text for the default channel"
            };
            self.aprs_reply(src, hint);
            return;
        }

        // `nick text` → IRC query when that nick is online (aprsmsg-like).
        if let Some((first, rest)) = text.split_once(' ') {
            if !is_channel_name(first) {
                if let Some(target) = self.state.by_nick(first).cloned() {
                    let body = sanitize(rest.trim());
                    if body.is_empty() {
                        self.aprs_ack(src, &msg);
                        self.aprs_reply(src, "send nick text");
                        return;
                    }
                    if self.ensure_rf_user(src) {
                        self.radio.flush_mailbox(src);
                    }
                    let Some(user) = self.state.user(&UserId::Rf(src.clone())).cloned() else {
                        self.aprs_ack(src, &msg);
                        return;
                    };
                    let d = Delivery::Privmsg {
                        from_nick: user.nick.clone(),
                        from_prefix: user.prefix(),
                        target: target.nick.clone(),
                        text: body,
                        notice: false,
                        truncated: false,
                    };
                    self.deliver(&target.id, &d);
                    self.aprs_ack(src, &msg);
                    info!(%src, to = %target.nick, "APRS private message");
                    return;
                }
            }
        }

        let default = self.config.radio.aprs_channel.clone();
        let Some((channel, body)) = aprs::split_channel_line(&text, &default) else {
            self.aprs_ack(src, &msg);
            self.aprs_reply(src, "send #chan text");
            return;
        };

        let display = self.channel_display_name(channel);
        let Some(chan) = self.state.channel(&display).cloned() else {
            self.aprs_ack(src, &msg);
            self.aprs_reply(src, "no such channel");
            return;
        };
        if !chan.rf {
            self.aprs_ack(src, &msg);
            self.aprs_reply(src, "channel is not bridged");
            return;
        }

        if self.ensure_rf_user(src) {
            self.radio.flush_mailbox(src);
        }
        let uid = UserId::Rf(src.clone());
        if !chan.members.contains_key(&uid) {
            if self
                .radio
                .sessions
                .peer(src)
                .map(|p| p.was_kicked_from(&display))
                .unwrap_or(false)
            {
                self.aprs_ack(src, &msg);
                self.aprs_reply(src, "not on that channel");
                return;
            }
            self.rf_join(src, &display, false);
        }

        let Some(user) = self.state.user(&uid).cloned() else {
            self.aprs_ack(src, &msg);
            return;
        };
        let uid = user.id.clone();
        let airc_others: Vec<Callsign> = self
            .state
            .channel(&display)
            .map(|c| {
                c.members
                    .keys()
                    .filter_map(|m| match m {
                        UserId::Rf(call) if *m != uid => {
                            if self
                                .radio
                                .sessions
                                .peer(call)
                                .map(|p| p.dialect == crate::airc::Dialect::Airc)
                                .unwrap_or(false)
                            {
                                Some(call.clone())
                            } else {
                                None
                            }
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut air_text = body.to_string();
        let mut truncated = false;
        let mut to_airc = !airc_others.is_empty();
        if to_airc {
            match self.policy.screen_outbound(&air_text) {
                Verdict::Allow(t) => air_text = t,
                Verdict::Truncated(t) => {
                    air_text = t;
                    truncated = true;
                }
                Verdict::Deny(_) => to_airc = false,
            }
        }

        let d = Delivery::Privmsg {
            from_nick: user.nick.clone(),
            from_prefix: user.prefix(),
            target: display.clone(),
            text: if to_airc {
                air_text.clone()
            } else {
                body.to_string()
            },
            notice: false,
            truncated,
        };
        // IRC only here: every station in range already heard the APRS frame.
        // AIRC peers did not, so translate once as AIRC when any are present.
        self.broadcast_channel_ex(&display, &d, Some(&uid), false);
        if to_airc {
            let payload = encode_fields(&[&display, &user.nick, &air_text]);
            let flags = if truncated {
                crate::airc::frame::flags::TRUNCATED
            } else {
                0
            };
            self.radio
                .broadcast_flagged(Kind::Msg, payload, TxClass::Chat, flags);
        }
        self.aprs_ack(src, &msg);
        info!(%src, "APRS message into channel");
    }

    /// A position or status beacon. IRC only: every station on frequency
    /// already heard it, and repeating APRS as AIRC would be a new
    /// transmission of someone else's beacon.
    fn handle_aprs_beacon(&mut self, src: &Callsign, summary: String, now: Instant) {
        if src.require_amateur().is_err() {
            return;
        }
        if !self.policy.station_allowed(src) {
            return;
        }
        if self.radio.sessions.is_banned(src) {
            return;
        }
        if self.radio.aprs_beacon_seen(src, &summary, now) {
            return;
        }
        let Some(channel) = self.config.aprs_listen_channel().map(str::to_string) else {
            return;
        };
        let display = self.channel_display_name(&channel);
        let Some(chan) = self.state.channel(&display) else {
            return;
        };
        if !chan.rf {
            return;
        }
        let text = sanitize(&summary);
        if text.is_empty() {
            return;
        }
        self.radio.remember_aprs_beacon(src, &summary, now);
        let nick = src.to_nick();
        let prefix = format!("{nick}!rf@{src}.ax25");
        let line = crate::irc::message::Message::new("NOTICE", vec![display.clone(), text])
            .with_prefix(prefix)
            .to_string();
        for uid in self.state.members(&display) {
            if let UserId::Ip(id) = uid {
                self.send_raw(id, line.clone());
            }
        }
    }

    fn aprs_ack(&mut self, src: &Callsign, msg: &AprsMessage) {
        let Some(id) = msg.msgid.as_deref() else {
            return;
        };
        self.radio.transmit_ui(
            &aprs::ax25_destination(),
            aprs::ack_info(src, id),
            TxClass::Ack,
            src,
        );
    }

    fn aprs_rej_or_ack(&mut self, src: &Callsign, msg: &AprsMessage) {
        let Some(id) = msg.msgid.as_deref() else {
            return;
        };
        self.radio.transmit_ui(
            &aprs::ax25_destination(),
            aprs::rej_info(src, id),
            TxClass::Ack,
            src,
        );
    }

    fn aprs_reply(&mut self, src: &Callsign, text: &str) {
        self.radio.transmit_ui(
            &aprs::ax25_destination(),
            aprs::reply_info(src, text),
            TxClass::Control,
            src,
        );
    }

    fn handle_airc_message(&mut self, src: &Callsign, msg: AircFrame, now: Instant) {
        let fields = msg.fields();
        match msg.kind {
            Kind::Hello => {
                if !self.rf_ctrl_ok(src, now) {
                    return;
                }
                self.radio.sessions.lift_ban(src);
                let created = self.ensure_rf_user(src);
                let name = self.server_name().to_string();
                let motd = self
                    .config
                    .server
                    .motd
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "welcome".into());
                // One MOTD line, hard-capped. A gateway's welcome banner is
                // the operator's prose; the air does not have room for it.
                let motd: String = motd.chars().take(48).collect();
                self.radio.unicast(
                    src,
                    Kind::Welcome,
                    encode_fields(&[&name, &motd]),
                    true,
                    TxClass::Control,
                );
                if created {
                    info!(%src, "station registered");
                }
                self.radio.flush_mailbox(src);
            }
            Kind::Join => {
                if !self.rf_ctrl_ok(src, now) {
                    return;
                }
                self.radio.sessions.lift_ban(src);
                let Some(channel) = fields.first().cloned() else {
                    return;
                };
                if self.ensure_rf_user(src) {
                    self.radio.flush_mailbox(src);
                }
                self.rf_join(src, &channel, true);
            }
            Kind::Part => {
                if !self.rf_ctrl_ok(src, now) {
                    return;
                }
                let Some(channel) = fields.first().cloned() else {
                    return;
                };
                let uid = UserId::Rf(src.clone());
                let display = self.channel_display_name(&channel);
                let reason = crate::policy::sanitize(&fields.get(1).cloned().unwrap_or_default());
                if self.state.user(&uid).is_some() {
                    self.rf_part(&uid, &display, reason);
                }
                if let Some(peer) = self.radio.sessions.peer_mut(src) {
                    peer.channels.remove(&display);
                }
            }
            Kind::Quit => {
                // Rate-limited like every other control frame. It was not,
                // and a QUIT is the cheapest frame there is to forge: one
                // unacknowledged broadcast removes a station from IRC.
                if !self.rf_ctrl_ok(src, now) {
                    return;
                }
                let uid = UserId::Rf(src.clone());
                let reason = crate::policy::sanitize(
                    &fields
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "Signed off".into()),
                );
                let reason = if reason.is_empty() {
                    "Signed off".into()
                } else {
                    reason
                };
                self.quit_user(&uid, &reason);
            }
            Kind::Msg | Kind::Notice => {
                // Downlink is [target, from, text]. Uplink is [target, text].
                // A 3-field MSG is another gateway's broadcast, not a station
                // talking to us — treating it as uplink is the two-gateway loop.
                if fields.len() >= 3 {
                    return;
                }
                let (Some(target), Some(text)) = (fields.first(), fields.get(1)) else {
                    return;
                };
                self.rf_message(
                    src,
                    &target.clone(),
                    &text.clone(),
                    msg.kind == Kind::Notice,
                    now,
                );
            }
            Kind::Names => {
                if !self.rf_ctrl_ok(src, now) {
                    return;
                }
                if !self
                    .radio
                    .sessions
                    .peer(src)
                    .map(|p| p.registered)
                    .unwrap_or(false)
                {
                    return;
                }
                let Some(channel) = fields.first().cloned() else {
                    return;
                };
                let display = self.channel_display_name(&channel);
                let names = self.names_for_air(&display);
                self.radio.unicast(
                    src,
                    Kind::NamesReply,
                    encode_fields(&[&display, &names]),
                    false,
                    TxClass::Control,
                );
            }
            Kind::Ping => {
                if !self.rf_ctrl_ok(src, now) {
                    return;
                }
                if !self
                    .radio
                    .sessions
                    .peer(src)
                    .map(|p| p.registered)
                    .unwrap_or(false)
                {
                    return;
                }
                let token: String = fields.first().cloned().unwrap_or_default();
                let token: String = token.chars().take(8).collect();
                self.radio.unicast(
                    src,
                    Kind::Pong,
                    encode_fields(&[&token]),
                    false,
                    TxClass::Control,
                );
            }
            Kind::Id => {}
            // Replies we never expect to receive, and ACKs, which the session
            // layer has already consumed.
            Kind::Ack
            | Kind::Welcome
            | Kind::NamesReply
            | Kind::Pong
            | Kind::Presence
            | Kind::Stored
            | Kind::Error => {}
        }
    }

    /// Create the IRC-side presence for a station heard on the air.
    /// Returns true if the user was newly created.
    fn ensure_rf_user(&mut self, call: &Callsign) -> bool {
        let uid = UserId::Rf(call.clone());
        if self.state.user(&uid).is_some() {
            return false;
        }
        let mut user = User::new(uid.clone(), format!("{call}.ax25"), Instant::now());
        user.username = "rf".into();
        user.realname = format!("{call} via {}", self.config.radio.callsign);
        user.callsign = Some(call.clone());
        user.registered = true;
        self.state.insert_user(user);

        // Nick collisions should not happen (callsign-shaped nicks are
        // reserved) but a station must never be locked out because of one.
        let mut nick = call.to_nick();
        let mut suffix = 1;
        while !self.state.set_nick(&uid, &nick) {
            nick = format!("{}_{suffix}", call.to_nick());
            suffix += 1;
            if suffix > 9 {
                self.state.remove_user(&uid);
                return false;
            }
        }
        if let Some(peer) = self.radio.sessions.touch(call, Instant::now()) {
            peer.registered = true;
        }
        true
    }

    fn rf_join(&mut self, call: &Callsign, channel: &str, airc_reply: bool) {
        let uid = UserId::Rf(call.clone());
        if !is_channel_name(channel) {
            self.rf_error(call, "403", "no such channel");
            return;
        }
        let display = self.channel_display_name(channel);
        let Some(chan) = self.state.channel(&display).cloned() else {
            self.rf_error(call, "403", "no such channel");
            return;
        };
        if !chan.rf {
            self.rf_error(call, "404", "channel is not bridged to RF");
            return;
        }
        if self.state.join(&uid, &display).is_none() {
            return;
        }
        if let Some(peer) = self.radio.sessions.peer_mut(call) {
            peer.channels.insert(display.clone());
            peer.clear_kicked(&display);
        }
        let flags = self
            .state
            .channel(&display)
            .and_then(|c| c.members.get(&uid).copied())
            .unwrap_or_default();
        let (nick, prefix) = self
            .state
            .user(&uid)
            .map(|u| (u.nick.clone(), u.prefix()))
            .unwrap_or_default();
        let d = Delivery::Join {
            nick: nick.clone(),
            prefix,
            channel: display.clone(),
        };
        // Other stations on frequency heard the JOIN themselves.
        self.broadcast_channel_ex(&display, &d, Some(&uid), false);
        if flags.voice {
            let server = self.server_name().to_string();
            self.announce_mode(&display, &server, "+v", &[&nick]);
        }
        if self
            .state
            .channel(&display)
            .map(|c| c.has_rf_members())
            .unwrap_or(false)
        {
            let call_s = call.to_string();
            self.notice_rf_audience(
                &display,
                &format!(
                    "RF station {call_s} is on frequency. Messages from +v users will be radiated."
                ),
            );
        }

        // Deliberately *not* the member list. A station that joins wants to
        // know it is in; it did not ask who else is here, and on a 300 baud
        // channel a roll call is seconds of airtime nobody requested. The
        // count is one field and answers the question people actually have.
        // `NAMES` gets the list, capped, when it is asked for.
        // APRS radios cannot decode AIRC: skip the reply for them.
        if !airc_reply {
            return;
        }
        let count = self
            .state
            .channel(&display)
            .map(|c| c.members.len())
            .unwrap_or(0)
            .to_string();
        let topic: String = self
            .state
            .channel(&display)
            .and_then(|c| c.topic.clone())
            .unwrap_or_default()
            .chars()
            .take(64)
            .collect();
        self.radio.unicast(
            call,
            Kind::NamesReply,
            encode_fields(&[&display, &format!("{count} here"), &topic]),
            true,
            TxClass::Control,
        );
    }

    /// A short error back to a station.
    ///
    /// Unreliable on purpose. A reliable error is ACK-requested and retried up
    /// to `max_retries` times, so "no such channel" would cost four
    /// transmissions — more airtime than the message that provoked it. If it
    /// is lost, the station simply sees no reply, which is the same
    /// information.
    pub(crate) fn rf_error(&mut self, dst: &Callsign, code: &str, text: &str) {
        self.radio.unicast(
            dst,
            Kind::Error,
            encode_fields(&[code, text]),
            false,
            TxClass::Control,
        );
    }

    /// Member list trimmed to something a 300 baud channel can afford.
    ///
    /// Two caps, because either alone can be defeated: at most
    /// `radio.rf_names_max` names, and at most 160 octets. `names_of` is
    /// otherwise unbounded — a hundred IRC users is over a kilobyte, which
    /// fragments into a long reliable exchange with retries.
    /// Only ever sent in reply to an explicit `NAMES`.
    fn names_for_air(&self, channel: &str) -> String {
        const BUDGET: usize = 160;
        let limit = self.config.radio.rf_names_max;
        let all = self.state.names_of(channel);
        let mut out = String::new();
        let mut shown = 0usize;
        for n in &all {
            if shown >= limit || out.len() + n.len() + 1 > BUDGET {
                break;
            }
            if !out.is_empty() {
                out.push(',');
            }
            out.push_str(n);
            shown += 1;
        }
        if shown < all.len() {
            out.push_str(&format!(",+{} more", all.len() - shown));
        }
        out
    }

    fn rf_part(&mut self, uid: &UserId, channel: &str, reason: String) {
        let Some(user) = self.state.user(uid).cloned() else {
            return;
        };
        if !self
            .state
            .channel(channel)
            .map(|c| c.members.contains_key(uid))
            .unwrap_or(false)
        {
            return;
        }
        let d = Delivery::Part {
            nick: user.nick.clone(),
            prefix: user.prefix(),
            channel: channel.to_string(),
            reason: if reason.is_empty() {
                "Leaving".into()
            } else {
                reason
            },
        };
        self.broadcast_channel_ex(channel, &d, Some(uid), false);
        self.state.part(uid, channel);
        if !self
            .state
            .channel(channel)
            .map(|c| c.has_rf_members())
            .unwrap_or(true)
        {
            self.notice_rf_audience(
                channel,
                "No RF station remains in this channel. Messages stay on IRC until one joins.",
            );
        }
    }

    fn rf_message(&mut self, src: &Callsign, target: &str, text: &str, notice: bool, now: Instant) {
        if !self.policy.rf_station_rate_ok(src, now) {
            // Do not answer a flood with more transmissions; just drop it and
            // let the operator see it in the log.
            warn!(%src, "rate limit exceeded, dropping message");
            if let Some(peer) = self.radio.sessions.peer_mut(src) {
                peer.dropped += 1;
            }
            return;
        }
        let text = sanitize(text);
        if text.is_empty() {
            return;
        }
        if self.radio.sessions.is_banned(src) {
            self.rf_error(src, "442", "removed by control operator");
            return;
        }
        if self.ensure_rf_user(src) {
            self.radio.flush_mailbox(src);
        }
        let uid = UserId::Rf(src.clone());
        let Some(user) = self.state.user(&uid).cloned() else {
            return;
        };

        if is_channel_name(target) {
            let display = self.channel_display_name(target);
            let Some(chan) = self.state.channel(&display).cloned() else {
                self.rf_error(src, "403", "no such channel");
                return;
            };
            if !chan.rf {
                self.rf_error(src, "404", "channel is not bridged to RF");
                return;
            }
            // Be forgiving of a lost JOIN, but not of a kick: inventing a JOIN
            // from a PRIVMSG made both `KICK` and `RADIO KICK` a no-op.
            if !chan.members.contains_key(&uid) {
                if self
                    .radio
                    .sessions
                    .peer(src)
                    .map(|p| p.was_kicked_from(&display))
                    .unwrap_or(false)
                {
                    self.rf_error(src, "442", "you're not on that channel");
                    return;
                }
                self.rf_join(src, &display, true);
            }
            let mut text = text;
            let mut truncated = false;
            let mut repeat = self.config.radio.repeat_rf_traffic;
            if repeat {
                match self.policy.screen_outbound(&text) {
                    Verdict::Allow(t) => text = t,
                    Verdict::Truncated(t) => {
                        text = t;
                        truncated = true;
                    }
                    Verdict::Deny(_) => repeat = false,
                }
            }
            let d = Delivery::Privmsg {
                from_nick: user.nick.clone(),
                from_prefix: user.prefix(),
                target: display.clone(),
                text,
                notice,
                truncated,
            };
            // Every station in range already heard this transmission. We only
            // repeat it if the operator has enabled store-and-forward for
            // hidden stations, and even then it is one extra transmission —
            // never of something policy refused.
            self.broadcast_channel_ex(&display, &d, Some(&uid), repeat);
            return;
        }

        // Private message to a nickname.
        let Some(target_id) = self.find_target(target) else {
            self.rf_error(src, "401", "no such nick");
            return;
        };
        let mut truncated = false;
        let text = match self.policy.screen_outbound(&text) {
            Verdict::Allow(t) => t,
            Verdict::Truncated(t) => {
                truncated = true;
                t
            }
            Verdict::Deny(_) => {
                if target_id.is_rf() {
                    return;
                }
                text
            }
        };
        let d = Delivery::Privmsg {
            from_nick: user.nick.clone(),
            from_prefix: user.prefix(),
            target: target.to_string(),
            text,
            notice,
            truncated,
        };
        self.deliver(&target_id, &d);
    }

    fn rf_ctrl_ok(&mut self, src: &Callsign, now: Instant) -> bool {
        if self.policy.rf_station_rate_ok(src, now) {
            true
        } else {
            debug!(%src, "control-frame rate limit, ignoring");
            false
        }
    }

    /// Transmit a single AIRC frame to one station (used for ACKs, which must
    /// not go through the session queue).
    fn transmit_airc(&mut self, dst: &Callsign, frame: AircFrame) {
        // ACKs are the cheapest airtime there is: one short frame that stops
        // the sender retransmitting a long one. They are never rationed.
        self.radio.transmit_direct(dst, frame, TxClass::Ack);
    }
}

/// Channel chat from a station is a broadcast `MSG`/`NOTICE` with the uplink
/// shape `[target, text]`. Anything else on a protocol address is another
/// gateway's downlink, a HELLO that belongs on unicast, or noise.
fn uplink_chat_from_broadcast(msg: &AircFrame) -> bool {
    matches!(msg.kind, Kind::Msg | Kind::Notice) && msg.fields().len() == 2
}
