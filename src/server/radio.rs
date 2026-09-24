//! Putting something on the air.
//!
//! Every transmission the gateway makes goes through this module, and that is
//! the point: the rules about airtime are only worth anything if there is no
//! second way round them. The invariants it owns are
//!
//! * **Admission.** A message is priced in seconds of key-down time and
//!   checked against the backlog budget for its class *before* the session
//!   layer accepts it — because once that happens an ACK timer is running and
//!   the message costs up to `max_retries` transmissions, not one.
//! * **Identification.** The station knows whether it owes the band a
//!   callsign, and identification is the one thing that outranks the operator
//!   inhibit (though not the safety interlock).
//! * **Fragmentation.** How many frames a payload becomes, and therefore what
//!   it really costs, is known here and nowhere else.
//!
//! The airtime governor itself lives in [`crate::ax25::airtime`], inside the
//! TNC task. This module decides *whether* to hand it something; the governor
//! decides *when* that something is keyed.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::airc::{encode_fields, AircFrame, Dialect, Kind, SessionConfig, Sessions};
use crate::aprs::{self, AprsMessage};
use crate::ax25::{AirtimeShared, Ax25Frame, Class, Keyed, TncHandle};
use crate::callsign::Callsign;
use crate::config::{Config, RfMode};

use super::mailbox::Mailbox;

/// Outbound APRS chat with stop-and-wait ACK per destination.
#[derive(Default)]
struct AprsOutbound {
    next_id: u32,
    pending: HashMap<Callsign, AprsInFlight>,
    waiting: HashMap<Callsign, VecDeque<AprsQueued>>,
}

struct AprsInFlight {
    msgid: String,
    info: Vec<u8>,
    class: TxClass,
    attempts: u32,
    next_retry: Instant,
}

struct AprsQueued {
    text: String,
    class: TxClass,
}

impl AprsOutbound {
    fn next_msgid(&mut self) -> String {
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        // 1–5 alphanumeric; base-36 keeps ids short for the 67-char budget.
        let mut n = self.next_id;
        let mut s = String::new();
        for _ in 0..5 {
            let d = (n % 36) as u8;
            s.insert(
                0,
                if d < 10 {
                    char::from(b'0' + d)
                } else {
                    char::from(b'A' + (d - 10))
                },
            );
            n /= 36;
            if n == 0 {
                break;
            }
        }
        s
    }
}

/// What a frame is *for*, which decides how much of the transmit backlog it
/// may occupy.
///
/// A single FIFO is the wrong shape for a shared, thermally limited channel:
/// a burst of channel chat would fill it and the ACK that would have ended a
/// retry cycle waits behind ten seconds of gossip — costing more airtime than
/// the chat did. Each class may fill only a fraction of the backlog budget,
/// so protocol traffic always has room and conversation is what gets squeezed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TxClass {
    /// Acknowledgements. Cheap, and every one of them prevents a retransmit.
    Ack,
    /// Session control: WELCOME, NAMES replies, errors, PONG.
    Control,
    /// Addressed to one station: private messages and held mail.
    Direct,
    /// Channel conversation. The largest source of traffic and the most
    /// tolerant of being dropped, so it is squeezed first.
    Chat,
}

impl TxClass {
    /// Fraction of the backlog budget this class may occupy.
    fn allowance(self) -> f64 {
        match self {
            TxClass::Ack => 1.0,
            TxClass::Control => 0.85,
            TxClass::Direct => 0.7,
            TxClass::Chat => 0.5,
        }
    }

    fn scheduler_class(self) -> Class {
        match self {
            TxClass::Ack => Class::Ack,
            TxClass::Control => Class::Control,
            TxClass::Direct => Class::Direct,
            TxClass::Chat => Class::Chat,
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub rf_frames_rx: u64,
    pub rf_frames_tx: u64,
    pub rf_frames_dropped: u64,
    /// Frames refused before they were queued, because the backlog for their
    /// class was already full.
    pub rf_frames_refused: u64,
    pub rf_bytes_tx: u64,
    pub ip_connections: u64,
}

/// The station's transmit side.
pub struct Radio {
    config: Arc<Config>,
    tnc: Option<TncHandle>,
    /// This station's own callsign and digipeater path, parsed once.
    ///
    /// Both used to be re-derived from the configuration strings on every
    /// single transmission, which is parsing in the hot path for a value that
    /// cannot change. Resolving them here also means a frame can always be
    /// built: the configuration has already been validated, so there is no
    /// per-transmission failure to handle.
    source: Option<Callsign>,
    path: Vec<Callsign>,
    /// Per-station sequencing, ACKs and reassembly.
    pub sessions: Sessions,
    /// Messages held for stations that are out of range.
    pub mailbox: Mailbox,
    pub stats: Stats,
    /// Runtime kill switch (`RADIO OFF`). The control operator must be able to
    /// stop the station radiating immediately, without killing the IRC side.
    pub enabled: bool,
    last_id: Option<Instant>,
    /// Set by any transmission, cleared by identifying. An automatically
    /// controlled station must identify the series of transmissions it made,
    /// and must not identify when it has made none — that is just QRM.
    transmitted_since_id: bool,
    keyed_rx: Option<tokio::sync::mpsc::Receiver<Keyed>>,
    /// APRS `{msgid}` values already injected, so a radio's retries do not
    /// flood the channel. Keyed by (source callsign, msgid).
    aprs_msgid: HashMap<(Callsign, String), Instant>,
    /// Recent APRS beacon payloads, so a digipeated copy is not a second line.
    aprs_beacon: HashMap<(Callsign, String), Instant>,
    /// Outbound APRS chat waiting for ACK (one in flight per destination).
    aprs_out: AprsOutbound,
}

/// Result of an operator or automatic identification attempt.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IdentifyResult {
    Sent,
    RateLimited,
    NotQueued,
}

impl Radio {
    const APRS_MSGID_TTL: Duration = Duration::from_secs(30 * 60);
    const APRS_MSGID_MAX: usize = 256;
    const APRS_BEACON_TTL: Duration = Duration::from_secs(90);
    const APRS_BEACON_MAX: usize = 256;

    pub fn new(config: Arc<Config>, tnc: Option<TncHandle>) -> Self {
        let sessions = Sessions::new(SessionConfig {
            paclen: config.radio.paclen,
            ack_timeout: Duration::from_secs(config.radio.ack_timeout_secs),
            max_retries: config.radio.max_retries,
            peer_idle_timeout: Duration::from_secs(config.radio.peer_idle_timeout_secs),
            max_peers: config.radio.max_peers,
            ..Default::default()
        });
        let mailbox = Mailbox::new(
            config.radio.mailbox_enabled,
            config.radio.mailbox_per_station,
            config.radio.mailbox_total,
            Duration::from_secs(config.radio.mailbox_ttl_secs),
        );
        let keyed_rx = tnc.as_ref().and_then(|t| t.take_keyed());
        Self {
            enabled: config.radio.enabled && tnc.is_some(),
            source: config.gateway_callsign(),
            path: config.rf_path(),
            config,
            tnc,
            sessions,
            mailbox,
            stats: Stats::default(),
            last_id: None,
            transmitted_since_id: false,
            keyed_rx,
            aprs_msgid: HashMap::new(),
            aprs_beacon: HashMap::new(),
            aprs_out: AprsOutbound::default(),
        }
    }

    /// Start ACK clocks for frames the TNC has just keyed.
    pub fn drain_keyed(&mut self, now: Instant) {
        let events: Vec<Keyed> = match &mut self.keyed_rx {
            Some(rx) => {
                let mut out = Vec::new();
                while let Ok(k) = rx.try_recv() {
                    out.push(k);
                }
                out
            }
            None => Vec::new(),
        };
        for k in events {
            self.sessions.on_keyed(&k.dest, k.seq, now);
        }
    }

    /// Live airtime counters and the hard transmit inhibit, if there is a TNC.
    pub fn airtime(&self) -> Option<&Arc<AirtimeShared>> {
        self.tnc.as_ref().map(|t| t.airtime())
    }

    /// Stop transmitting now, discarding whatever is already queued. Unlike
    /// `enabled`, which only stops us queueing more, this reaches the frames
    /// already handed to the TNC task.
    pub fn set_tx_inhibit(&self, inhibit: bool) {
        if let Some(tnc) = self.tnc.as_ref() {
            tnc.set_inhibit(inhibit);
        }
    }

    /// Stations currently heard on the air.
    pub fn peers_heard(&self) -> usize {
        self.sessions.peers().count()
    }

    /// The largest payload one AIRC frame can carry at this paclen.
    pub fn max_payload(&self) -> usize {
        self.sessions.config.max_payload()
    }

    pub fn available(&self) -> bool {
        self.enabled && self.tnc.is_some()
    }

    /// The safety interlock is holding the transmitter. Distinct from
    /// [`Radio::available`]: `RADIO OFF` is an operator decision, this is
    /// "it is not safe to key up".
    pub fn interlock_down(&self) -> bool {
        self.airtime().is_some_and(|a| a.interlock_failed())
    }

    /// Unreliable one-to-many transmission addressed to the protocol's
    /// destination address. Every station in range hears it once.
    pub fn broadcast(&mut self, kind: Kind, payload: Vec<u8>, class: TxClass) {
        self.broadcast_flagged(kind, payload, class, 0)
    }

    /// Reliable one-to-one transmission with ACK and retry.
    pub fn unicast(
        &mut self,
        dst: &Callsign,
        kind: Kind,
        payload: Vec<u8>,
        reliable: bool,
        class: TxClass,
    ) {
        self.unicast_flagged(dst, kind, payload, reliable, class, 0);
    }

    pub fn transmit_to(&mut self, dst: &Callsign, frame: AircFrame) {
        // A retransmission the session layer has already decided on. It is
        // finishing an exchange that is part-way done, so it is not subject to
        // fresh admission control — dropping it here would leave the peer
        // waiting for something that will never arrive.
        self.transmit_direct(dst, frame, TxClass::Control);
    }

    /// Standalone ACKs that were not piggybacked onto a later unicast.
    pub fn drain_acks(&mut self) {
        for (dst, frame) in self.sessions.drain_acks() {
            self.transmit_direct(&dst, frame, TxClass::Ack);
        }
    }

    /// Octets this payload will actually put on the wire once fragmented.
    ///
    /// Fragmentation is not free and the naive estimate hides it: each
    /// fragment carries its own AIRC header *and* a full AX.25 address field,
    /// so a payload one octet over the limit costs a whole extra frame — plus
    /// its TXDELAY and TXTAIL, which the governor prices separately.
    pub fn wire_octets(&self, payload: usize) -> usize {
        // AX.25 addresses (source, destination, up to two digipeaters),
        // control, PID and FCS.
        let per_frame = crate::airc::frame::HEADER_LEN + 7 * (2 + self.config.radio.path.len()) + 4;
        let max = self.sessions.config.max_payload();
        let fragments = payload.div_ceil(max).max(1);
        payload + fragments * per_frame
    }

    /// Airtime the transmit queue may hold before new traffic is refused.
    pub fn backlog_budget(&self) -> Duration {
        Duration::from_secs(self.config.radio.max_queued_airtime_secs)
    }

    /// Is there room in the backlog for `octets` of this class?
    ///
    /// This is the decision point that matters. Refusing here means the sender
    /// finds out immediately and can say something shorter or wait; accepting
    /// and then dropping the frame two minutes later at the transmitter means
    /// the message vanished and nobody knows.
    pub fn backlog_has_room(&self, octets: usize, class: TxClass) -> bool {
        let Some(tnc) = self.tnc.as_ref() else {
            return false;
        };
        let budget = self.backlog_budget().mul_f64(class.allowance());
        tnc.queued() + tnc.airtime_for(octets) <= budget
    }

    /// How long a message queued now would wait before it is on the air.
    pub fn eta(&self) -> Duration {
        self.tnc.as_ref().map(|t| t.eta()).unwrap_or_default()
    }

    pub fn transmit_direct(&mut self, dest: &Callsign, frame: AircFrame, class: TxClass) {
        let expects_reply = frame.wants_ack();
        self.enqueue_ui(
            dest,
            frame.encode(),
            class,
            expects_reply,
            &dest.to_string(),
        );
    }

    /// A raw UI information field (APRS ACK/REJ/system reply). `account` is
    /// the station this transmission is *for*, so their APRS traffic shares a
    /// fairness bucket with their AIRC traffic rather than collapsing onto
    /// the `APRS` destination address.
    pub fn transmit_ui(
        &mut self,
        dest: &Callsign,
        info: Vec<u8>,
        class: TxClass,
        account: &Callsign,
    ) {
        self.enqueue_ui(dest, info, class, false, &account.to_string());
    }

    /// Queue an APRS chat line to `to` (msgid + ACK/retry). Body is truncated
    /// to the APRS text budget. Returns false if the transmitter is unavailable.
    pub fn enqueue_aprs_chat(
        &mut self,
        to: &Callsign,
        text: &str,
        class: TxClass,
        now: Instant,
    ) -> bool {
        if !self.available() || self.interlock_down() {
            return false;
        }
        let text: String = text.chars().take(67).collect();
        if text.is_empty() {
            return false;
        }
        if self.aprs_out.pending.contains_key(to) {
            let q = self.aprs_out.waiting.entry(to.clone()).or_default();
            if q.len() >= 8 {
                self.stats.rf_frames_refused += 1;
                return false;
            }
            q.push_back(AprsQueued { text, class });
            return true;
        }
        self.start_aprs_chat(to, text, class, now)
    }

    fn start_aprs_chat(
        &mut self,
        to: &Callsign,
        text: String,
        class: TxClass,
        now: Instant,
    ) -> bool {
        let msgid = self.aprs_out.next_msgid();
        let info = aprs::message_info(to, &text, &msgid);
        if !self.backlog_has_room(info.len() + 20, class) {
            self.stats.rf_frames_refused += 1;
            return false;
        }
        let dest = aprs::ax25_destination();
        self.transmit_ui(&dest, info.clone(), class, to);
        let timeout = Duration::from_secs(self.config.radio.ack_timeout_secs.max(1));
        self.aprs_out.pending.insert(
            to.clone(),
            AprsInFlight {
                msgid,
                info,
                class,
                attempts: 1,
                next_retry: now + timeout,
            },
        );
        true
    }

    /// An `ack`/`rej` from `from` addressed to this gateway.
    pub fn on_aprs_ack(&mut self, from: &Callsign, msg: &AprsMessage, now: Instant) {
        let Some(id) = aprs::control_msgid(msg) else {
            return;
        };
        let Some(pending) = self.aprs_out.pending.get(from) else {
            return;
        };
        if pending.msgid != id {
            return;
        }
        self.aprs_out.pending.remove(from);
        let next = self
            .aprs_out
            .waiting
            .get_mut(from)
            .and_then(|q| q.pop_front());
        if self
            .aprs_out
            .waiting
            .get(from)
            .is_some_and(|q| q.is_empty())
        {
            self.aprs_out.waiting.remove(from);
        }
        if let Some(next) = next {
            let _ = self.start_aprs_chat(from, next.text, next.class, now);
        }
    }

    /// Retransmit unanswered APRS chat; drop after `max_retries`.
    pub fn tick_aprs(&mut self, now: Instant) {
        if !self.available() || self.interlock_down() {
            return;
        }
        let max_retries = self.config.radio.max_retries.max(1);
        let timeout = Duration::from_secs(self.config.radio.ack_timeout_secs.max(1));
        let due: Vec<Callsign> = self
            .aprs_out
            .pending
            .iter()
            .filter(|(_, p)| now >= p.next_retry)
            .map(|(c, _)| c.clone())
            .collect();
        for call in due {
            let Some(mut pending) = self.aprs_out.pending.remove(&call) else {
                continue;
            };
            if pending.attempts >= max_retries {
                debug!(%call, "APRS chat gave up waiting for ACK");
                self.stats.rf_frames_dropped += 1;
                let next = self
                    .aprs_out
                    .waiting
                    .get_mut(&call)
                    .and_then(|q| q.pop_front());
                if self
                    .aprs_out
                    .waiting
                    .get(&call)
                    .is_some_and(|q| q.is_empty())
                {
                    self.aprs_out.waiting.remove(&call);
                }
                if let Some(next) = next {
                    let _ = self.start_aprs_chat(&call, next.text, next.class, now);
                }
                continue;
            }
            pending.attempts += 1;
            let backoff = timeout.saturating_mul(pending.attempts.min(4));
            pending.next_retry = now + backoff;
            let dest = aprs::ax25_destination();
            self.transmit_ui(&dest, pending.info.clone(), pending.class, &call);
            self.aprs_out.pending.insert(call, pending);
        }
    }

    /// Whether outbound chat to this station should use APRS encoding.
    pub fn wants_aprs(&self, call: &Callsign) -> bool {
        if self.config.radio.rf_mode.is_aprs() {
            return true;
        }
        self.sessions
            .peer(call)
            .is_some_and(|p| p.dialect == Dialect::Aprs)
    }

    pub fn rf_mode(&self) -> RfMode {
        self.config.radio.rf_mode
    }

    fn enqueue_ui(
        &mut self,
        dest: &Callsign,
        info: Vec<u8>,
        class: TxClass,
        expects_reply: bool,
        account: &str,
    ) {
        let (Some(tnc), Some(source)) = (self.tnc.as_ref(), self.source.clone()) else {
            return;
        };
        if !self.enabled {
            return;
        }
        if tnc.airtime().interlock_failed() {
            // Do not pile retries into a transmitter that cannot key up.
            // Whatever is already in the TNC queue is being held there.
            return;
        }
        let ax = match Ax25Frame::ui(source, dest.clone(), &self.path, info) {
            Ok(f) => f,
            Err(e) => {
                warn!("cannot build AX.25 frame: {e}");
                return;
            }
        };
        let len = ax.encode().len();
        if tnc.enqueue(ax, class.scheduler_class(), account, expects_reply) {
            self.stats.rf_frames_tx += 1;
            self.stats.rf_bytes_tx += len as u64;
            self.transmitted_since_id = true;
        } else {
            self.stats.rf_frames_dropped += 1;
        }
    }

    /// True when we have already injected this (source, msgid) recently.
    pub fn aprs_msgid_seen(&self, src: &Callsign, msgid: &str, now: Instant) -> bool {
        self.aprs_msgid
            .get(&(src.clone(), msgid.to_string()))
            .is_some_and(|at| now.saturating_duration_since(*at) < Self::APRS_MSGID_TTL)
    }

    pub fn remember_aprs_msgid(&mut self, src: &Callsign, msgid: &str, now: Instant) {
        self.expire_aprs_msgids(now);
        if self.aprs_msgid.len() >= Self::APRS_MSGID_MAX {
            if let Some(oldest) = self
                .aprs_msgid
                .iter()
                .min_by_key(|(_, at)| *at)
                .map(|(k, _)| k.clone())
            {
                self.aprs_msgid.remove(&oldest);
            }
        }
        self.aprs_msgid
            .insert((src.clone(), msgid.to_string()), now);
    }

    pub fn expire_aprs_msgids(&mut self, now: Instant) {
        let ttl = Self::APRS_MSGID_TTL;
        self.aprs_msgid
            .retain(|_, at| now.saturating_duration_since(*at) < ttl);
        let beacon_ttl = Self::APRS_BEACON_TTL;
        self.aprs_beacon
            .retain(|_, at| now.saturating_duration_since(*at) < beacon_ttl);
    }

    /// True when this exact beacon was already shown, typically a digipeat.
    pub fn aprs_beacon_seen(&self, src: &Callsign, summary: &str, now: Instant) -> bool {
        self.aprs_beacon
            .get(&(src.clone(), summary.to_string()))
            .is_some_and(|at| now.saturating_duration_since(*at) < Self::APRS_BEACON_TTL)
    }

    pub fn remember_aprs_beacon(&mut self, src: &Callsign, summary: &str, now: Instant) {
        if self.aprs_beacon.len() >= Self::APRS_BEACON_MAX {
            if let Some(oldest) = self
                .aprs_beacon
                .iter()
                .min_by_key(|(_, at)| *at)
                .map(|(k, _)| k.clone())
            {
                self.aprs_beacon.remove(&oldest);
            }
        }
        self.aprs_beacon
            .insert((src.clone(), summary.to_string()), now);
    }

    /// Deliver mail held for a station we have just heard from.
    ///
    /// A few at a time, not the whole mailbox. Ten held messages released the
    /// instant a HELLO arrives is a minute of near-continuous transmitting
    /// caused by one short frame from a station that may be in range for
    /// thirty seconds. The rest go out on the next thing we hear from them, so
    /// the station's own activity paces the delivery — which is also the only
    /// evidence we have that it is still listening.
    pub fn flush_mailbox(&mut self, call: &Callsign) {
        let depth = self.mailbox.depth(call);
        if depth == 0 {
            return;
        }
        // The mailbox is the last copy. If we cannot radiate — transmitter
        // off, no TNC, interlock down — leave the mail where it is. A HELLO
        // still arrives while `RADIO OFF` (receive keeps running), and taking
        // the message out then handing it to a unicast that returns immediately
        // would destroy it.
        if !self.available() || self.airtime().is_some_and(|a| a.tx_blocked()) {
            debug!(%call, "holding mail back: the transmitter is not available");
            return;
        }
        let batch = self.config.radio.mailbox_flush_batch.max(1);
        let now = Instant::now();
        let nick = call.to_nick();
        let mut sent = 0;
        for _ in 0..batch {
            // Look before taking. A held message is the only copy there is —
            // it may have been waiting hours — so it must not leave the
            // mailbox unless there is somewhere for it to go. Taking it first
            // and letting admission control refuse it afterwards destroys it:
            // neither transmitted nor held, and nobody told.
            let Some(m) = self.mailbox.peek(call) else {
                break;
            };
            let age = m.age(now).as_secs().to_string();
            if self.wants_aprs(call) {
                // APRS HTs do not speak STORED; deliver as ordinary chat.
                let body = if m.from.is_empty() {
                    m.text.clone()
                } else {
                    format!("{}: {}", m.from, m.text)
                };
                let body = if m.truncated {
                    format!("{body}…")
                } else {
                    body
                };
                let info_len = body.len().min(67) + 20;
                if !self.backlog_has_room(info_len, TxClass::Direct) {
                    debug!(%call, "holding mail back: the transmit backlog is full");
                    break;
                }
                if !self.enqueue_aprs_chat(call, &body, TxClass::Direct, now) {
                    debug!(%call, "holding mail back: the transmitter refused it");
                    break;
                }
                self.mailbox.drop_front(call);
                sent += 1;
                continue;
            }
            let payload = encode_fields(&[&nick, &m.from, &m.text, &age]);
            let flags = if m.truncated {
                crate::airc::frame::flags::TRUNCATED
            } else {
                0
            };
            if !self.backlog_has_room(self.wire_octets(payload.len()), TxClass::Direct) {
                debug!(%call, "holding mail back: the transmit backlog is full");
                break;
            }
            if !self.sessions.can_accept(call) {
                debug!(%call, "holding mail back: the station's session queue is full");
                break;
            }
            // Hand it to the session layer first. Only then is it safe to
            // discard the mailbox copy: unicast can still refuse if the
            // interlock dropped between the checks above and the send.
            if !self.unicast_flagged(call, Kind::Stored, payload, true, TxClass::Direct, flags) {
                debug!(%call, "holding mail back: the transmitter refused it");
                break;
            }
            self.mailbox.drop_front(call);
            sent += 1;
        }
        let remaining = depth.saturating_sub(sent);
        if remaining > 0 {
            debug!(%call, "{remaining} held message(s) still waiting");
        }
    }

    pub fn maybe_identify(&mut self, now: Instant) {
        if self.tnc.is_none() {
            return;
        }
        if !self.transmitted_since_id {
            return;
        }
        // Periodic identification waits for the interval after the last ID.
        // A failed sign-off (`RADIO OFF` queued an ID that did not fit) retries
        // every tick: `available()` is now false, so the old guard never tried
        // again. The first ID after transmitting is due immediately — a licence
        // wants the transmissions identified, not a wait from process start.
        if self.enabled {
            if let Some(at) = self.last_id {
                if now.duration_since(at) < self.config.id_interval() {
                    return;
                }
            }
        }
        let _ = self.send_id();
    }

    /// Identify now if we have transmitted since the last ID. Called before
    /// the transmitter is taken off the air (shutdown, `RADIO OFF`): an
    /// automatically controlled station must identify at the end of a series
    /// of transmissions, not only every ten minutes.
    pub fn id_if_needed(&mut self) {
        if self.transmitted_since_id && self.tnc.is_some() {
            self.send_id();
        }
    }

    /// Identify now. Used by `RADIO ID` and by the automatic ID path.
    ///
    /// Returns whether the frame was handed to the TNC. A failure leaves the
    /// "owes an ID" flag set so a later attempt still has something to say.
    /// Operator `RADIO ID` is rate-limited to `id_interval` unless the station
    /// currently owes an identification (it has transmitted since the last ID).
    pub fn identify_now(&mut self) -> IdentifyResult {
        if !self.transmitted_since_id {
            if let Some(at) = self.last_id {
                if Instant::now().duration_since(at) < self.config.id_interval() {
                    return IdentifyResult::RateLimited;
                }
            }
        }
        if self.send_id() {
            IdentifyResult::Sent
        } else {
            IdentifyResult::NotQueued
        }
    }

    fn send_id(&mut self) -> bool {
        let text = format!(
            "{} {}",
            self.config.radio.callsign, self.config.radio.id_text
        );
        let payload = encode_fields(&[&text]);
        let dest: Callsign = "ID".parse().unwrap();
        let seq = self.sessions.next_seq();
        let frame = AircFrame::new(Kind::Id, seq, payload);
        if self.transmit_id(&dest, frame) {
            self.transmitted_since_id = false;
            self.last_id = Some(Instant::now());
            debug!("station identification transmitted");
            true
        } else {
            false
        }
    }

    /// Identification goes out on the TNC's priority path, so it is not held
    /// behind a backlog and is not discarded by the transmit inhibit. This is
    /// the one frame the station is obliged to send.
    fn transmit_id(&mut self, dest: &Callsign, frame: AircFrame) -> bool {
        let (Some(tnc), Some(source)) = (self.tnc.as_ref(), self.source.clone()) else {
            return false;
        };
        // Queue even when the interlock is down: the TNC holds the ID until
        // it is safe to key up. Refusing here meant `RADIO OFF` during a
        // failed SWR check never signed off — `enabled` went false and the
        // obligation was stranded.
        let ax = match Ax25Frame::ui(source, dest.clone(), &self.path, frame.encode()) {
            Ok(f) => f,
            Err(e) => {
                warn!("cannot build station ID frame: {e}");
                return false;
            }
        };
        let len = ax.encode().len();
        if tnc.try_send_id(ax) {
            self.stats.rf_frames_tx += 1;
            self.stats.rf_bytes_tx += len as u64;
            true
        } else {
            self.stats.rf_frames_dropped += 1;
            false
        }
    }

    pub fn status_line(&self) -> String {
        if !self.config.radio.enabled {
            return "Radio gateway is disabled. This is a plain IRC server; nothing is radiated."
                .into();
        }
        let call = &self.config.radio.callsign;
        // Most specific reason first. "No TNC" used to sit below the
        // `enabled` check, which made it unreachable — a radio with no TNC is
        // never enabled — so an operator whose modem was missing was told the
        // transmitter was OFF, which reads as "somebody ran RADIO OFF" rather
        // than "the thing it talks to is not there".
        if self.tnc.is_none() {
            return format!(
                "Radio gateway: no TNC attached. Station {call}. Nothing is being radiated."
            );
        }
        if !self.enabled {
            return format!(
                "Radio gateway: transmitter OFF. Station {call}. Nothing is being radiated."
            );
        }
        if self
            .airtime()
            .map(|a| a.interlock_failed())
            .unwrap_or(false)
        {
            return format!(
                "Radio gateway: station {call}, transmitter BLOCKED by the safety interlock. \
                 Nothing is being radiated. Station identification is held until it is safe."
            );
        }
        let duty = self
            .airtime()
            .map(|a| format!(" {:.0}% duty.", a.duty_percent()))
            .unwrap_or_default();
        let cooling = self
            .airtime()
            .map(|a| a.cooling_ms.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0);
        let cooling = if cooling > 0 {
            format!(" PA cooling for {}s.", cooling / 1000)
        } else {
            String::new()
        };
        format!(
            "Radio gateway: transmitter ON, station {call}, {} RF station(s) heard, {} frames TX / {} RX ({} bytes on air).{duty}{cooling}",
            self.sessions.peers().count(),
            self.stats.rf_frames_tx,
            self.stats.rf_frames_rx,
            self.stats.rf_bytes_tx
        )
    }

    /// As [`Radio::broadcast`], with AIRC frame flags — currently only
    /// [`crate::airc::frame::flags::TRUNCATED`], so a receiving station can
    /// show that it is not seeing the whole message.
    pub fn broadcast_flagged(&mut self, kind: Kind, payload: Vec<u8>, class: TxClass, flags: u8) {
        if !self.available() || self.interlock_down() {
            return;
        }
        let seq = self.sessions.next_seq();
        let max = self.sessions.config.max_payload();
        let chunks: Vec<Vec<u8>> = if payload.is_empty() {
            vec![Vec::new()]
        } else {
            payload.chunks(max).map(|c| c.to_vec()).collect()
        };
        if chunks.len() > u8::MAX as usize {
            self.stats.rf_frames_dropped += 1;
            return;
        }
        if !self.backlog_has_room(self.wire_octets(payload.len()), class) {
            self.stats.rf_frames_refused += 1;
            return;
        }
        // Airtime admission is in seconds; the TNC queue is in frames.
        // A burst of fragments can fill the channel after the backlog check
        // still said yes, and broadcasts are not retried.
        if chunks.len()
            > self
                .tnc
                .as_ref()
                .map(|t| t.tx_room_in(class.scheduler_class()))
                .unwrap_or(0)
        {
            self.stats.rf_frames_refused += 1;
            return;
        }
        let total = chunks.len() as u8;
        let dest: Callsign = self
            .config
            .radio
            .destination
            .parse()
            .unwrap_or_else(|_| "AIRC".parse().unwrap());
        for (i, chunk) in chunks.into_iter().enumerate() {
            let mut f = AircFrame::new(kind, seq, chunk).with_flags(flags);
            f.frag_index = i as u8;
            f.frag_total = total;
            self.transmit_direct(&dest, f, class);
        }
    }

    /// As [`Radio::unicast`], with extra AIRC frame flags.
    pub fn unicast_flagged(
        &mut self,
        dst: &Callsign,
        kind: Kind,
        payload: Vec<u8>,
        reliable: bool,
        class: TxClass,
        flags: u8,
    ) -> bool {
        if !self.available() || self.interlock_down() {
            return false;
        }
        // Admission control happens before the session layer sees the message.
        // Once `Sessions::send` accepts it, an ACK timer is running and the
        // message will be retransmitted up to `max_retries` times — so a
        // message admitted when there is no airtime for it does not cost one
        // transmission, it costs four.
        if !self.backlog_has_room(self.wire_octets(payload.len()), class) {
            self.stats.rf_frames_refused += 1;
            return false;
        }
        // Same hole as broadcast: airtime can still fit when the 64-deep
        // frame queue cannot. Reliable traffic that would wait behind an
        // in-flight message does not need a slot yet.
        let queued_behind = reliable && self.sessions.peer(dst).is_some_and(|p| p.awaiting_ack());
        if !queued_behind {
            let max = self.sessions.config.max_payload();
            let fragments = if payload.is_empty() {
                1
            } else {
                payload.len().div_ceil(max).max(1)
            };
            if fragments
                > self
                    .tnc
                    .as_ref()
                    .map(|t| t.tx_room_in(class.scheduler_class()))
                    .unwrap_or(0)
            {
                self.stats.rf_frames_refused += 1;
                return false;
            }
        }
        let now = Instant::now();
        let outcome = self.sessions.enqueue(dst, kind, payload, reliable, now);
        if !outcome.accepted {
            return false;
        }
        if let Some(peer) = self.sessions.peer_mut(dst) {
            peer.note_airc();
        }
        for f in outcome.frames {
            self.transmit_direct(dst, f.with_flags(flags), class);
        }
        true
    }
}
