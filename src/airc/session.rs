//! Per-station session state: sequencing, fragmentation, retransmission,
//! duplicate suppression and reassembly.
//!
//! The module is deliberately pure - it never touches the clock or the radio
//! itself, it is driven with an explicit `now` and returns the frames the
//! caller should transmit. That makes the awkward parts (timeouts, retries,
//! reassembly races) testable without a radio or a sleeping test.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::callsign::Callsign;

use super::frame::{flags, AircFrame, Kind, HEADER_LEN};

#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// AX.25 information-field budget in octets (`paclen`). 256 is the AX.25
    /// default; 128 gets through a noisy channel far more often.
    pub paclen: usize,
    pub ack_timeout: Duration,
    pub max_retries: u32,
    pub reassembly_timeout: Duration,
    /// A station we have not heard from in this long is forgotten (and its
    /// IRC-side ghost is removed).
    pub peer_idle_timeout: Duration,
    /// Messages queued per station before we start refusing.
    pub max_queue: usize,
    /// How many recent sequence numbers to remember per station for dedup.
    pub dedup_window: usize,
    /// Stations we will remember at once. Further callsigns are ignored until
    /// an idle peer expires, so a flood of unique sources cannot grow forever.
    pub max_peers: usize,
    /// Incomplete fragmented messages kept per station.
    pub max_reasm: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            paclen: 128,
            ack_timeout: Duration::from_secs(12),
            max_retries: 3,
            reassembly_timeout: Duration::from_secs(60),
            peer_idle_timeout: Duration::from_secs(30 * 60),
            max_queue: 16,
            dedup_window: 64,
            max_peers: 256,
            max_reasm: 4,
        }
    }
}

impl SessionConfig {
    pub fn max_payload(&self) -> usize {
        self.paclen.saturating_sub(HEADER_LEN).max(1)
    }
}

struct Pending {
    frames: Vec<AircFrame>,
    /// Fragment `i` has been reported received. A 2-octet ACK sets every bit;
    /// a bitmap ACK sets the bits it carries.
    acked: Vec<bool>,
    attempts: u32,
    /// `None` until the TNC reports the frame keyed. A backlog must not
    /// manufacture an ACK timeout.
    next_retry: Option<Instant>,
}

struct Reassembly {
    kind: Kind,
    flags: u8,
    parts: Vec<Option<Vec<u8>>>,
    started: Instant,
    want_ack: bool,
}

/// An ACK we owe this station. Held so the next unicast can carry it rather
/// than keying up a second time.
struct OwedAck {
    seq: u16,
    /// `None` means the whole message arrived (2-octet ACK). `Some` is a
    /// selective bitmap: bit *i* set when fragment *i* is in hand.
    bitmap: Option<Vec<u8>>,
}

/// Which application protocol a station has been speaking on the air.
///
/// New peers default to [`Dialect::Aprs`] until they send AIRC; an AIRC
/// `HELLO` (or any AIRC frame) upgrades them to [`Dialect::Airc`] for the
/// rest of the peer lifetime. Used so a bridged channel can fan out public
/// chat as addressed APRS to stock HTs without forcing the whole gateway
/// into APRS mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Dialect {
    Aprs,
    Airc,
}

/// One station heard on the air.
pub struct Peer {
    pub call: Callsign,
    pub channels: HashSet<String>,
    pub first_heard: Instant,
    pub last_heard: Instant,
    /// Set once the station has sent HELLO and passed policy checks.
    pub registered: bool,
    /// Frames dropped for this peer, for `RADIO HEARD`.
    pub dropped: u64,
    /// Last application dialect observed from this station.
    pub dialect: Dialect,
    pending: Option<Pending>,
    owed_ack: Option<OwedAck>,
    queue: VecDeque<Vec<AircFrame>>,
    seen: VecDeque<u16>,
    reasm: HashMap<u16, Reassembly>,
    /// Channels this station was KICKed from. A following MSG must not
    /// silently rejoin — that made KICK a no-op on RF.
    kicked: HashSet<String>,
    /// Session epoch from the last HELLO. A new epoch is a restart: clear
    /// the dedup window rather than ACKing seq 1 forever.
    epoch: u16,
}

impl Peer {
    fn new(call: Callsign, now: Instant) -> Self {
        Self {
            call,
            channels: HashSet::new(),
            first_heard: now,
            last_heard: now,
            registered: false,
            dropped: 0,
            dialect: Dialect::Aprs,
            pending: None,
            owed_ack: None,
            queue: VecDeque::new(),
            seen: VecDeque::new(),
            reasm: HashMap::new(),
            kicked: HashSet::new(),
            epoch: 0,
        }
    }

    /// Sticky upgrade: once a station speaks AIRC it stays AIRC.
    pub fn note_airc(&mut self) {
        self.dialect = Dialect::Airc;
    }

    /// Mark as APRS unless it has already spoken AIRC.
    pub fn note_aprs(&mut self) {
        if self.dialect != Dialect::Airc {
            self.dialect = Dialect::Aprs;
        }
    }

    pub fn was_kicked_from(&self, channel: &str) -> bool {
        self.kicked.contains(channel)
    }

    pub fn mark_kicked(&mut self, channel: &str) {
        self.kicked.insert(channel.to_string());
        self.channels.remove(channel);
    }

    pub fn clear_kicked(&mut self, channel: &str) {
        self.kicked.remove(channel);
    }

    pub fn queue_depth(&self) -> usize {
        self.queue.len() + usize::from(self.pending.is_some())
    }

    /// True when a reliable message is on the air or waiting for ACK.
    pub fn awaiting_ack(&self) -> bool {
        self.pending.is_some()
    }
}

#[derive(Default)]
pub struct RxOutcome {
    /// A complete message from the station, ready for the bridge.
    pub deliver: Option<AircFrame>,
    /// Frames to put on the air right now (the next queued message after an
    /// ACK released the in-flight one). Standalone ACKs are held on the peer
    /// and collected with [`Sessions::drain_acks`].
    pub transmit: Vec<AircFrame>,
    /// True if the frame was a duplicate we had already processed.
    pub duplicate: bool,
}

#[derive(Default)]
pub struct TickOutcome {
    pub transmit: Vec<(Callsign, AircFrame)>,
    /// Stations that stopped acknowledging or went quiet; the bridge removes
    /// their IRC presence.
    pub lost: Vec<Callsign>,
}

/// Result of [`Sessions::enqueue`].
pub struct SendOutcome {
    pub frames: Vec<AircFrame>,
    pub accepted: bool,
}

impl SendOutcome {
    fn dropped() -> Self {
        Self {
            frames: Vec::new(),
            accepted: false,
        }
    }
}

pub struct Sessions {
    pub config: SessionConfig,
    peers: HashMap<Callsign, Peer>,
    /// One sequence space for everything this station transmits.
    ///
    /// It has to be shared rather than per destination: a receiver
    /// deduplicates on (source, seq), and it sees our unicast traffic and our
    /// broadcasts as one stream. Per-peer counters would hand out the same
    /// sequence number twice and the second message would be silently
    /// discarded as a duplicate.
    next_seq: u16,
    /// Peers removed by [`Sessions::force_touch`] to make room. The server
    /// turns these into IRC QUITs; without that, the user stays in channels
    /// forever because idle expiry only walks the session table.
    evicted: Vec<Callsign>,
    /// Callsigns a control operator removed with `RADIO KICK`. Independent of
    /// the peer table: `forget` would otherwise let the next MSG recreate them.
    radio_kicked: HashSet<Callsign>,
}

impl Sessions {
    pub fn new(config: SessionConfig) -> Self {
        Self {
            config,
            peers: HashMap::new(),
            next_seq: 1,
            evicted: Vec::new(),
            radio_kicked: HashSet::new(),
        }
    }

    /// Stations dropped from the table to make room for an outgoing message.
    pub fn take_evicted(&mut self) -> Vec<Callsign> {
        std::mem::take(&mut self.evicted)
    }

    /// `RADIO KICK` removes the station and refuses to invent a JOIN from the
    /// next PRIVMSG. HELLO or JOIN lets them back.
    pub fn ban(&mut self, call: &Callsign) {
        self.radio_kicked.insert(call.clone());
    }

    pub fn lift_ban(&mut self, call: &Callsign) {
        self.radio_kicked.remove(call);
    }

    pub fn is_banned(&self, call: &Callsign) -> bool {
        self.radio_kicked.contains(call)
    }

    /// Allocate the next outgoing sequence number. Public because broadcasts
    /// are not addressed to a peer but must share the same space.
    pub fn next_seq(&mut self) -> u16 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if self.next_seq == 0 {
            self.next_seq = 1;
        }
        seq
    }

    pub fn peer(&self, call: &Callsign) -> Option<&Peer> {
        self.peers.get(call)
    }

    pub fn peer_mut(&mut self, call: &Callsign) -> Option<&mut Peer> {
        self.peers.get_mut(call)
    }

    pub fn peers(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values()
    }

    pub fn touch(&mut self, call: &Callsign, now: Instant) -> Option<&mut Peer> {
        if !self.peers.contains_key(call) && self.peers.len() >= self.config.max_peers {
            return None;
        }
        let peer = self
            .peers
            .entry(call.clone())
            .or_insert_with(|| Peer::new(call.clone(), now));
        peer.last_heard = now;
        Some(peer)
    }

    /// Unlike [`Sessions::touch`], evicts the quietest peer if the table is
    /// full so a legitimate outgoing message is never dropped on the floor.
    pub fn force_touch(&mut self, call: &Callsign, now: Instant) -> Option<&mut Peer> {
        if self.config.max_peers == 0 {
            return None;
        }
        if !self.peers.contains_key(call) && self.peers.len() >= self.config.max_peers {
            if let Some(oldest) = self
                .peers
                .iter()
                .min_by_key(|(_, p)| p.last_heard)
                .map(|(c, _)| c.clone())
            {
                self.peers.remove(&oldest);
                self.evicted.push(oldest);
            }
        }
        self.touch(call, now)
    }

    pub fn forget(&mut self, call: &Callsign) {
        self.peers.remove(call);
    }

    /// Handle a frame received from `src`.
    ///
    /// Inbound traffic uses [`Sessions::touch`], not [`Sessions::force_touch`]:
    /// a flood of unique callsigns must not evict a quiet real station.
    /// Outgoing messages still force a slot so a legitimate TX is never
    /// dropped on the floor.
    ///
    /// `addressed_to_us` is the AX.25 destination check. Broadcasts (to
    /// `AIRC` / `ID`) must never be ACKed: a single `ACK_REQ` bit on a
    /// broadcast would make every station in range key up at once.
    pub fn on_receive(
        &mut self,
        src: &Callsign,
        frame: AircFrame,
        now: Instant,
        addressed_to_us: bool,
    ) -> RxOutcome {
        let cfg = self.config.clone();
        let Some(peer) = self.touch(src, now) else {
            return RxOutcome::default();
        };
        peer.note_airc();
        let mut out = RxOutcome::default();

        if frame.kind == Kind::Hello {
            let epoch = hello_epoch(&frame);
            if epoch != peer.epoch {
                peer.seen.clear();
                peer.reasm.clear();
                peer.pending = None;
                peer.owed_ack = None;
                peer.queue.clear();
                peer.epoch = epoch;
            }
        }

        if let Some(acked) = frame.piggyback_seq {
            out.transmit.extend(apply_ack(peer, acked, None, now, &cfg));
        }

        if frame.kind == Kind::Ack {
            if let Some((acked, bitmap)) = parse_ack_payload(&frame.payload) {
                out.transmit
                    .extend(apply_ack(peer, acked, bitmap, now, &cfg));
            }
            return out;
        }

        if peer.seen.contains(&frame.seq) {
            out.duplicate = true;
            // A duplicate of an already-delivered message is still ACKed:
            // the usual reason for a repeat is that our ACK was lost.
            // Incomplete fragments are not in `seen` and are not ACKed.
            if addressed_to_us && frame.wants_ack() {
                owe_ack(peer, frame.seq, None);
            }
            // HELLO is the start of a session. A restarted station whose
            // sequence space overlapped (no epoch, or the same seq as last
            // time) must not be ACKed and ignored for half an hour. A true
            // retry of HELLO only costs a second WELCOME.
            if frame.kind == Kind::Hello {
                out.duplicate = false;
                out.deliver = Some(frame);
            }
            return out;
        }

        if frame.frag_total == 1 {
            if addressed_to_us && frame.wants_ack() {
                owe_ack(peer, frame.seq, None);
            }
            remember_seq(peer, frame.seq, cfg.dedup_window);
            out.deliver = Some(frame);
            return out;
        }

        // Fragmented message: stash and wait for the rest. A 2-octet ACK is
        // sent only when the last missing fragment arrives. A RETRY of an
        // incomplete set earns a bitmap so the sender can fill holes without
        // repeating what we already have.
        if frame.payload.len() > cfg.max_payload() {
            return out;
        }
        if !peer.reasm.contains_key(&frame.seq) && peer.reasm.len() >= cfg.max_reasm {
            if let Some(oldest) = peer
                .reasm
                .iter()
                .min_by_key(|(_, r)| r.started)
                .map(|(s, _)| *s)
            {
                peer.reasm.remove(&oldest);
            }
        }
        let want_ack = addressed_to_us && frame.wants_ack();
        let entry = peer.reasm.entry(frame.seq).or_insert_with(|| Reassembly {
            kind: frame.kind,
            flags: frame.flags,
            parts: vec![None; frame.frag_total as usize],
            started: now,
            want_ack,
        });
        if entry.parts.len() != frame.frag_total as usize {
            *entry = Reassembly {
                kind: frame.kind,
                flags: frame.flags,
                parts: vec![None; frame.frag_total as usize],
                started: now,
                want_ack,
            };
        }
        entry.want_ack |= want_ack;
        if frame.flags & flags::ACK_REQ != 0 {
            entry.flags |= flags::ACK_REQ;
        }
        entry.parts[frame.frag_index as usize] = Some(frame.payload.clone());
        let complete = entry.parts.iter().all(|p| p.is_some());
        if !complete {
            if want_ack && frame.flags & flags::RETRY != 0 {
                let bitmap = bitmap_from_parts(&entry.parts);
                owe_ack(peer, frame.seq, Some(bitmap));
            }
            return out;
        }

        let mut payload = Vec::new();
        for part in entry.parts.iter().flatten() {
            payload.extend_from_slice(part);
        }
        let (kind, flg, want) = (entry.kind, entry.flags, entry.want_ack);
        peer.reasm.remove(&frame.seq);
        remember_seq(peer, frame.seq, cfg.dedup_window);
        if want || (addressed_to_us && (frame.wants_ack() || flg & flags::ACK_REQ != 0)) {
            owe_ack(peer, frame.seq, None);
        }
        out.deliver = Some(AircFrame::new(kind, frame.seq, payload).with_flags(flg));
        out
    }

    /// Standalone ACKs that were not piggybacked onto a later unicast.
    pub fn drain_acks(&mut self) -> Vec<(Callsign, AircFrame)> {
        let mut out = Vec::new();
        for (call, peer) in self.peers.iter_mut() {
            if let Some(owed) = peer.owed_ack.take() {
                out.push((call.clone(), owed.to_frame()));
            }
        }
        out
    }

    /// Would a reliable send to `dst` be accepted rather than dropped?
    ///
    /// Held mail uses this so a message is not taken out of the mailbox only
    /// to be refused by a full per-station queue. Unreliable traffic is never
    /// queued, so it is always accepted here.
    pub fn can_accept(&self, dst: &Callsign) -> bool {
        match self.peers.get(dst) {
            Some(peer) if peer.pending.is_some() => peer.queue.len() < self.config.max_queue,
            _ => true,
        }
    }

    /// Queue a message for `dst`, fragmenting as needed. Returns the frames to
    /// transmit immediately (empty if something is already in flight and this
    /// message had to be queued behind it).
    pub fn send(
        &mut self,
        dst: &Callsign,
        kind: Kind,
        payload: Vec<u8>,
        reliable: bool,
        now: Instant,
    ) -> Vec<AircFrame> {
        self.enqueue(dst, kind, payload, reliable, now).frames
    }

    /// As [`Sessions::send`], but says whether the message was accepted.
    ///
    /// Empty frames used to mean both "queued behind something in flight" and
    /// "dropped". Held mail needs the difference: only the latter must leave
    /// the message in the mailbox.
    pub fn enqueue(
        &mut self,
        dst: &Callsign,
        kind: Kind,
        payload: Vec<u8>,
        reliable: bool,
        now: Instant,
    ) -> SendOutcome {
        let cfg = self.config.clone();
        let seq = self.next_seq();
        let Some(peer) = self.force_touch(dst, now) else {
            return SendOutcome::dropped();
        };
        let send_now = !reliable || peer.pending.is_none();
        // Only a complete ACK piggybacks, and only onto a unicast we are
        // actually sending now. A broadcast with PIGGYACK would be processed
        // by every station in range as an ACK of that seq, which is not
        // theirs to complete.
        let piggy = if send_now && reliable && cfg.max_payload() > 2 {
            match peer.owed_ack.take() {
                Some(o) if o.bitmap.is_none() => Some(o.seq),
                other => {
                    peer.owed_ack = other;
                    None
                }
            }
        } else {
            None
        };
        let chunks = chunk_payload(&payload, cfg.max_payload(), piggy.is_some());
        if chunks.len() > u8::MAX as usize {
            if let Some(s) = piggy {
                peer.owed_ack = Some(OwedAck {
                    seq: s,
                    bitmap: None,
                });
            }
            peer.dropped += 1;
            return SendOutcome::dropped();
        }
        let total = chunks.len() as u8;
        let frames: Vec<AircFrame> = chunks
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                let mut f = AircFrame::new(kind, seq, chunk);
                f.frag_index = i as u8;
                f.frag_total = total;
                if reliable {
                    f.flags |= flags::ACK_REQ;
                }
                f
            })
            .collect();

        if !reliable {
            return SendOutcome {
                frames,
                accepted: true,
            };
        }
        if peer.pending.is_some() {
            if let Some(s) = piggy {
                peer.owed_ack = Some(OwedAck {
                    seq: s,
                    bitmap: None,
                });
            }
            if peer.queue.len() >= cfg.max_queue {
                peer.dropped += 1;
                return SendOutcome::dropped();
            }
            peer.queue.push_back(frames);
            return SendOutcome {
                frames: Vec::new(),
                accepted: true,
            };
        }
        let stored = frames.clone();
        let mut out = start_pending(peer, stored, now, &cfg);
        if let Some(acked) = piggy {
            if let Some(first) = out.first_mut() {
                first.attach_piggyback(acked);
            }
        }
        SendOutcome {
            frames: out,
            accepted: true,
        }
    }

    /// Drive timers: retransmissions, reassembly expiry, idle stations.
    pub fn tick(&mut self, now: Instant) -> TickOutcome {
        self.tick_retries(now, true)
    }

    /// As [`Sessions::tick`], but when `retry` is false the session does not
    /// retransmit or give up. Used while the transmitter cannot key up —
    /// interlock down *or* `RADIO OFF` — because burning ACK attempts against
    /// a held or purged queue would declare the station lost.
    pub fn tick_retries(&mut self, now: Instant, retry: bool) -> TickOutcome {
        let cfg = self.config.clone();
        let mut out = TickOutcome::default();
        let mut giving_up = Vec::new();

        for (call, peer) in self.peers.iter_mut() {
            let expired: Vec<u16> = peer
                .reasm
                .iter()
                .filter(|(_, r)| now.duration_since(r.started) >= cfg.reassembly_timeout)
                .map(|(s, _)| *s)
                .collect();
            for seq in expired {
                if let Some(r) = peer.reasm.remove(&seq) {
                    if r.want_ack {
                        out.transmit.push((
                            call.clone(),
                            ack_with_bitmap(seq, &bitmap_from_parts(&r.parts)),
                        ));
                    }
                }
            }

            if now.duration_since(peer.last_heard) > cfg.peer_idle_timeout {
                out.lost.push(call.clone());
                continue;
            }

            if !retry {
                continue;
            }

            let Some(pending) = peer.pending.as_mut() else {
                continue;
            };
            let Some(deadline) = pending.next_retry else {
                continue;
            };
            if now < deadline {
                continue;
            }
            if pending.attempts >= cfg.max_retries {
                giving_up.push(call.clone());
                continue;
            }
            pending.attempts += 1;
            pending.next_retry = Some(now + backoff(&cfg, pending.attempts));
            for (i, f) in pending.frames.iter().enumerate() {
                if pending.acked.get(i).copied().unwrap_or(false) {
                    continue;
                }
                let mut f = f.clone();
                f.flags |= flags::RETRY;
                out.transmit.push((call.clone(), f));
            }
        }

        for call in giving_up {
            if let Some(peer) = self.peers.get_mut(&call) {
                peer.pending = None;
                peer.dropped += 1;
                if let Some(next) = peer.queue.pop_front() {
                    for f in start_pending(peer, next, now, &cfg) {
                        out.transmit.push((call.clone(), f));
                    }
                } else {
                    // Nothing left to say and the station is not answering.
                    out.lost.push(call.clone());
                }
            }
        }

        for call in &out.lost {
            self.peers.remove(call);
        }
        out
    }

    /// The TNC has keyed a unicast frame. Start (or restart) the ACK clock
    /// from this moment, not from enqueue.
    pub fn on_keyed(&mut self, dest: &Callsign, seq: u16, now: Instant) {
        let cfg = self.config.clone();
        let Some(peer) = self.peers.get_mut(dest) else {
            return;
        };
        let Some(pending) = peer.pending.as_mut() else {
            return;
        };
        if pending.frames.first().map(|f| f.seq) != Some(seq) {
            return;
        }
        let attempt = pending.attempts.max(1);
        pending.next_retry = Some(now + backoff(&cfg, attempt));
    }
}

fn ack_for(seq: u16) -> AircFrame {
    AircFrame::new(Kind::Ack, seq, seq.to_be_bytes().to_vec())
}

fn ack_with_bitmap(seq: u16, bitmap: &[u8]) -> AircFrame {
    let mut payload = seq.to_be_bytes().to_vec();
    payload.extend_from_slice(bitmap);
    AircFrame::new(Kind::Ack, seq, payload)
}

impl OwedAck {
    fn to_frame(&self) -> AircFrame {
        match &self.bitmap {
            None => ack_for(self.seq),
            Some(b) => ack_with_bitmap(self.seq, b),
        }
    }
}

fn owe_ack(peer: &mut Peer, seq: u16, bitmap: Option<Vec<u8>>) {
    // A complete ACK replaces a partial one for the same seq; a newer seq
    // (we process one frame at a time) replaces whatever was held.
    peer.owed_ack = Some(OwedAck { seq, bitmap });
}

fn parse_ack_payload(payload: &[u8]) -> Option<(u16, Option<&[u8]>)> {
    if payload.len() < 2 {
        return None;
    }
    let seq = u16::from_be_bytes([payload[0], payload[1]]);
    if payload.len() == 2 {
        Some((seq, None))
    } else {
        Some((seq, Some(&payload[2..])))
    }
}

fn bit_set(bits: &[u8], i: usize) -> bool {
    let byte = i / 8;
    let mask = 1u8 << (i % 8);
    bits.get(byte).is_some_and(|b| b & mask != 0)
}

fn bitmap_from_parts(parts: &[Option<Vec<u8>>]) -> Vec<u8> {
    let n = parts.len().div_ceil(8).max(1);
    let mut b = vec![0u8; n];
    for (i, p) in parts.iter().enumerate() {
        if p.is_some() {
            b[i / 8] |= 1 << (i % 8);
        }
    }
    b
}

fn chunk_payload(payload: &[u8], max: usize, piggy: bool) -> Vec<Vec<u8>> {
    if payload.is_empty() {
        return vec![Vec::new()];
    }
    let first_max = if piggy {
        max.saturating_sub(2).max(1)
    } else {
        max
    };
    if payload.len() <= first_max {
        return vec![payload.to_vec()];
    }
    let mut out = vec![payload[..first_max].to_vec()];
    for c in payload[first_max..].chunks(max) {
        out.push(c.to_vec());
    }
    out
}

/// Mark fragments acknowledged. A 2-octet ACK (no bitmap) completes the
/// message. A bitmap retransmits only the holes, immediately, if anything
/// new was reported.
fn apply_ack(
    peer: &mut Peer,
    seq: u16,
    bitmap: Option<&[u8]>,
    now: Instant,
    cfg: &SessionConfig,
) -> Vec<AircFrame> {
    let missing = {
        let Some(pending) = peer.pending.as_mut() else {
            return Vec::new();
        };
        if pending.frames.first().map(|f| f.seq) != Some(seq) {
            return Vec::new();
        }
        if let Some(bits) = bitmap {
            let mut progress = false;
            for (i, flag) in pending.acked.iter_mut().enumerate() {
                if bit_set(bits, i) && !*flag {
                    *flag = true;
                    progress = true;
                }
            }
            if !pending.acked.iter().all(|x| *x) {
                if !progress {
                    return Vec::new();
                }
                pending.attempts = 0;
                pending.next_retry = None;
                Some(
                    pending
                        .frames
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !pending.acked[*i])
                        .map(|(_, f)| {
                            let mut f = f.clone();
                            f.flags |= flags::RETRY;
                            f
                        })
                        .collect::<Vec<_>>(),
                )
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some(frames) = missing {
        return frames;
    }
    peer.pending = None;
    if let Some(next) = peer.queue.pop_front() {
        start_pending(peer, next, now, cfg)
    } else {
        Vec::new()
    }
}

fn start_pending(
    peer: &mut Peer,
    frames: Vec<AircFrame>,
    _now: Instant,
    _cfg: &SessionConfig,
) -> Vec<AircFrame> {
    let n = frames.len();
    peer.pending = Some(Pending {
        frames: frames.clone(),
        acked: vec![false; n],
        attempts: 0,
        next_retry: None,
    });
    frames
}

fn hello_epoch(frame: &AircFrame) -> u16 {
    frame
        .field(1)
        .and_then(|s| u16::from_str_radix(s.trim(), 16).ok())
        .unwrap_or(0)
}

fn backoff(cfg: &SessionConfig, attempt: u32) -> Duration {
    // Linear backoff. Exponential is wrong on a shared half-duplex channel:
    // the usual cause of loss is a collision, and waiting minutes just makes
    // the QSO unusable.
    cfg.ack_timeout * attempt.min(4)
}

fn remember_seq(peer: &mut Peer, seq: u16, window: usize) {
    peer.seen.push_back(seq);
    while peer.seen.len() > window {
        peer.seen.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airc::frame::encode_fields;

    fn call() -> Callsign {
        "SM0ABC-7".parse().unwrap()
    }

    fn take_acks(s: &mut Sessions) -> Vec<AircFrame> {
        s.drain_acks().into_iter().map(|(_, f)| f).collect()
    }

    #[test]
    fn fragments_and_reassembles() {
        let cfg = SessionConfig {
            paclen: HEADER_LEN + 4,
            ..Default::default()
        };
        let mut tx = Sessions::new(cfg.clone());
        let mut rx = Sessions::new(cfg);
        let now = Instant::now();

        let frames = tx.send(&call(), Kind::Msg, b"abcdefghij".to_vec(), true, now);
        assert_eq!(frames.len(), 3);

        let mut delivered = None;
        for f in frames {
            let out = rx.on_receive(&call(), f, now, true);
            if out.deliver.is_some() {
                delivered = out.deliver;
            }
        }
        assert_eq!(delivered.unwrap().payload, b"abcdefghij");
    }

    #[test]
    fn duplicates_are_suppressed_but_still_acked() {
        let mut rx = Sessions::new(SessionConfig::default());
        let now = Instant::now();
        let f =
            AircFrame::new(Kind::Msg, 9, encode_fields(&["#rf", "hi"])).with_flags(flags::ACK_REQ);

        let first = rx.on_receive(&call(), f.clone(), now, true);
        assert!(first.deliver.is_some());
        assert!(first.transmit.is_empty());
        assert_eq!(take_acks(&mut rx).len(), 1);

        let second = rx.on_receive(&call(), f, now, true);
        assert!(second.deliver.is_none());
        assert!(second.duplicate);
        assert_eq!(
            take_acks(&mut rx).len(),
            1,
            "a repeat means our ACK was lost"
        );
    }

    #[test]
    fn retransmits_then_gives_up() {
        let cfg = SessionConfig {
            ack_timeout: Duration::from_secs(10),
            max_retries: 2,
            ..Default::default()
        };
        let mut s = Sessions::new(cfg);
        let mut now = Instant::now();
        let frames = s.send(&call(), Kind::Msg, b"x".to_vec(), true, now);
        assert_eq!(frames.len(), 1);
        s.on_keyed(&call(), frames[0].seq, now);

        now += Duration::from_secs(11);
        assert_eq!(s.tick(now).transmit.len(), 1);
        now += Duration::from_secs(21);
        assert_eq!(s.tick(now).transmit.len(), 1);
        now += Duration::from_secs(31);
        let out = s.tick(now);
        assert!(out.transmit.is_empty());
        assert_eq!(out.lost, vec![call()]);
    }

    #[test]
    fn a_blocked_transmitter_does_not_burn_retry_attempts() {
        let cfg = SessionConfig {
            ack_timeout: Duration::from_secs(10),
            max_retries: 2,
            ..Default::default()
        };
        let mut s = Sessions::new(cfg);
        let mut now = Instant::now();
        let frames = s.send(&call(), Kind::Msg, b"x".to_vec(), true, now);
        assert_eq!(frames.len(), 1);

        // Well past every retry deadline, but the frame has not keyed, so
        // there is no retry clock yet.
        now += Duration::from_secs(120);
        let out = s.tick_retries(now, false);
        assert!(out.transmit.is_empty());
        assert!(
            out.lost.is_empty(),
            "giving up would drop a message still held in the TNC"
        );
        assert!(
            s.peer(&call()).is_some(),
            "the session must still be waiting"
        );

        // Key-down starts the clock. A retry is not due until ack_timeout later.
        s.on_keyed(&call(), frames[0].seq, now);
        let out = s.tick_retries(now, true);
        assert!(out.transmit.is_empty());
        now += Duration::from_secs(11);
        let out = s.tick_retries(now, true);
        assert_eq!(out.transmit.len(), 1);
        assert!(out.lost.is_empty());
    }

    #[test]
    fn ack_releases_the_queue() {
        let mut s = Sessions::new(SessionConfig::default());
        let now = Instant::now();
        let first = s.send(&call(), Kind::Msg, b"one".to_vec(), true, now);
        let queued = s.send(&call(), Kind::Msg, b"two".to_vec(), true, now);
        assert!(queued.is_empty(), "second message waits for the first ACK");

        let seq = first[0].seq;
        let ack = AircFrame::new(Kind::Ack, seq, seq.to_be_bytes().to_vec());
        let out = s.on_receive(&call(), ack, now, true);
        assert_eq!(out.transmit.len(), 1);
        assert_eq!(out.transmit[0].payload, b"two");
    }

    #[test]
    fn sequence_numbers_are_not_reused_across_peers() {
        let mut s = Sessions::new(SessionConfig::default());
        let now = Instant::now();
        let other: Callsign = "SM0XYZ".parse().unwrap();
        let a = s.send(&call(), Kind::Msg, b"one".to_vec(), false, now)[0].seq;
        let b = s.send(&other, Kind::Msg, b"two".to_vec(), false, now)[0].seq;
        let c = s.next_seq();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn queue_is_bounded() {
        let cfg = SessionConfig {
            max_queue: 2,
            ..Default::default()
        };
        let mut s = Sessions::new(cfg);
        let now = Instant::now();
        for _ in 0..5 {
            s.send(&call(), Kind::Msg, b"spam".to_vec(), true, now);
        }
        let peer = s.peer(&call()).unwrap();
        assert_eq!(peer.queue_depth(), 3);
        assert_eq!(peer.dropped, 2);
        assert!(
            !s.can_accept(&call()),
            "a full queue must be visible before the next send refuses"
        );
    }

    #[test]
    fn reliable_fragments_are_acked_only_when_complete() {
        let cfg = SessionConfig {
            paclen: HEADER_LEN + 4,
            ..Default::default()
        };
        let mut tx = Sessions::new(cfg.clone());
        let mut rx = Sessions::new(cfg);
        let now = Instant::now();
        let frames = tx.send(&call(), Kind::Msg, b"abcdefghij".to_vec(), true, now);
        assert_eq!(frames.len(), 3);

        let first = rx.on_receive(&call(), frames[0].clone(), now, true);
        assert!(first.deliver.is_none());
        assert!(
            first.transmit.is_empty() && take_acks(&mut rx).is_empty(),
            "ACKing fragment 0 lets the sender drop the rest"
        );

        let second = rx.on_receive(&call(), frames[1].clone(), now, true);
        assert!(second.deliver.is_none());
        assert!(take_acks(&mut rx).is_empty());

        let last = rx.on_receive(&call(), frames[2].clone(), now, true);
        assert_eq!(last.deliver.unwrap().payload, b"abcdefghij");
        let acks = take_acks(&mut rx);
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].kind, Kind::Ack);
        assert_eq!(acks[0].payload, frames[0].seq.to_be_bytes());
    }

    #[test]
    fn peer_table_is_bounded() {
        let cfg = SessionConfig {
            max_peers: 2,
            ..Default::default()
        };
        let mut s = Sessions::new(cfg);
        let now = Instant::now();
        let a: Callsign = "SM0AAA-1".parse().unwrap();
        let b: Callsign = "SM0BBB-1".parse().unwrap();
        let c: Callsign = "SM0CCC-1".parse().unwrap();
        assert!(s.touch(&a, now).is_some());
        assert!(s.touch(&b, now).is_some());
        assert!(
            s.touch(&c, now).is_none(),
            "a third peer must not grow the table"
        );
        s.force_touch(&c, now);
        assert_eq!(s.peers().count(), 2);
        assert_eq!(
            s.take_evicted().len(),
            1,
            "the quietest station must be reported so IRC can drop the ghost"
        );
    }

    #[test]
    fn inbound_frames_do_not_evict_a_live_peer() {
        let cfg = SessionConfig {
            max_peers: 2,
            ..Default::default()
        };
        let mut s = Sessions::new(cfg);
        let now = Instant::now();
        let a: Callsign = "SM0AAA-1".parse().unwrap();
        let b: Callsign = "SM0BBB-1".parse().unwrap();
        let c: Callsign = "SM0CCC-1".parse().unwrap();
        let hello = AircFrame::new(Kind::Hello, 1, Vec::new());
        s.on_receive(&a, hello.clone(), now, true);
        s.on_receive(&b, hello.clone(), now, true);
        let out = s.on_receive(&c, hello, now, true);
        assert!(
            out.deliver.is_none(),
            "a new callsign against a full table is ignored, not a reason to drop someone"
        );
        assert_eq!(s.peers().count(), 2);
        assert!(s.peer(&a).is_some() && s.peer(&b).is_some());
        assert!(
            s.take_evicted().is_empty(),
            "inbound must not produce IRC QUITs for stations that are still on frequency"
        );
    }

    #[test]
    fn a_broadcast_is_never_acked() {
        let mut rx = Sessions::new(SessionConfig::default());
        let now = Instant::now();
        let f =
            AircFrame::new(Kind::Msg, 9, encode_fields(&["#rf", "hi"])).with_flags(flags::ACK_REQ);
        let out = rx.on_receive(&call(), f, now, false);
        assert!(out.deliver.is_some());
        assert!(
            out.transmit.is_empty() && take_acks(&mut rx).is_empty(),
            "ACK_REQ on a broadcast must not make every station key up"
        );
    }

    #[test]
    fn hello_with_a_new_epoch_clears_the_dedup_window() {
        let mut rx = Sessions::new(SessionConfig::default());
        let now = Instant::now();
        let first =
            AircFrame::new(Kind::Msg, 1, encode_fields(&["#rf", "old"])).with_flags(flags::ACK_REQ);
        assert!(rx.on_receive(&call(), first, now, true).deliver.is_some());

        let hello = AircFrame::new(Kind::Hello, 2, encode_fields(&["client/1", "00AB"]));
        assert!(rx.on_receive(&call(), hello, now, true).deliver.is_some());

        let again = AircFrame::new(Kind::Msg, 1, encode_fields(&["#rf", "after restart"]))
            .with_flags(flags::ACK_REQ);
        let out = rx.on_receive(&call(), again, now, true);
        assert!(
            out.deliver.is_some(),
            "a new epoch must forget the old seq window, not ACK-and-drop seq 1"
        );
        assert!(!out.duplicate);
    }

    #[test]
    fn hello_without_epoch_is_still_delivered_if_seq_was_seen() {
        let mut rx = Sessions::new(SessionConfig::default());
        let now = Instant::now();
        let first =
            AircFrame::new(Kind::Msg, 1, encode_fields(&["#rf", "old"])).with_flags(flags::ACK_REQ);
        assert!(rx.on_receive(&call(), first, now, true).deliver.is_some());

        let hello = AircFrame::new(Kind::Hello, 1, encode_fields(&["legacy-client"]));
        let out = rx.on_receive(&call(), hello, now, true);
        assert!(
            out.deliver.is_some(),
            "a restarted station with no epoch must not be wedged by seq 1"
        );
        assert!(!out.duplicate);
    }

    #[test]
    fn a_lost_middle_fragment_is_the_only_one_retried() {
        let cfg = SessionConfig {
            paclen: HEADER_LEN + 4,
            ack_timeout: Duration::from_secs(10),
            max_retries: 3,
            ..Default::default()
        };
        let mut tx = Sessions::new(cfg.clone());
        let mut rx = Sessions::new(cfg);
        let mut now = Instant::now();
        let frames = tx.send(&call(), Kind::Msg, b"abcdefghij".to_vec(), true, now);
        assert_eq!(frames.len(), 3);
        tx.on_keyed(&call(), frames[0].seq, now);

        rx.on_receive(&call(), frames[0].clone(), now, true);
        rx.on_receive(&call(), frames[2].clone(), now, true);
        assert!(
            take_acks(&mut rx).is_empty(),
            "holes are not a complete ACK"
        );

        now += Duration::from_secs(11);
        let retry = tx.tick(now);
        assert_eq!(retry.transmit.len(), 3, "first timeout still repeats all");
        let retried: Vec<_> = retry.transmit.into_iter().map(|(_, f)| f).collect();
        // First timeout still repeats everything. A RETRY of fragment 0 while
        // 0 and 2 are already in hand produces a bitmap, not a complete ACK.
        let sack = rx.on_receive(&call(), retried[0].clone(), now, true);
        assert!(sack.deliver.is_none());
        let acks = take_acks(&mut rx);
        assert_eq!(acks.len(), 1);
        assert!(
            acks[0].payload.len() > 2,
            "a RETRY of an incomplete set is a bitmap, not a complete ACK"
        );

        let missing = tx.on_receive(&call(), acks[0].clone(), now, true);
        assert_eq!(missing.transmit.len(), 1);
        assert_eq!(missing.transmit[0].frag_index, 1);

        let last = rx.on_receive(&call(), missing.transmit[0].clone(), now, true);
        assert_eq!(last.deliver.unwrap().payload, b"abcdefghij");
        let done = take_acks(&mut rx);
        assert_eq!(done.len(), 1);
        assert_eq!(
            done[0].payload.len(),
            2,
            "completion is still a 2-octet ACK"
        );
    }

    #[test]
    fn a_two_byte_ack_still_completes_every_fragment() {
        let cfg = SessionConfig {
            paclen: HEADER_LEN + 4,
            ..Default::default()
        };
        let mut s = Sessions::new(cfg);
        let now = Instant::now();
        let frames = s.send(&call(), Kind::Msg, b"abcdefghij".to_vec(), true, now);
        assert_eq!(frames.len(), 3);
        let seq = frames[0].seq;
        let ack = AircFrame::new(Kind::Ack, seq, seq.to_be_bytes().to_vec());
        let out = s.on_receive(&call(), ack, now, true);
        assert!(out.transmit.is_empty());
        assert!(
            !s.peer(&call()).unwrap().awaiting_ack(),
            "legacy 2-octet ACK must still release the whole message"
        );
    }

    #[test]
    fn an_owed_ack_piggybacks_on_the_next_unicast() {
        let mut s = Sessions::new(SessionConfig::default());
        let now = Instant::now();
        let incoming =
            AircFrame::new(Kind::Hello, 3, encode_fields(&["client"])).with_flags(flags::ACK_REQ);
        assert!(s.on_receive(&call(), incoming, now, true).deliver.is_some());

        let welcome = s.send(
            &call(),
            Kind::Welcome,
            encode_fields(&["gw", "hi"]),
            true,
            now,
        );
        assert_eq!(welcome.len(), 1);
        assert_eq!(welcome[0].piggyback_seq, Some(3));
        assert!(
            take_acks(&mut s).is_empty(),
            "the ACK rode on WELCOME; nothing left to key up for"
        );
    }

    #[test]
    fn an_owed_ack_goes_out_alone_if_nothing_else_is_queued() {
        let mut s = Sessions::new(SessionConfig::default());
        let now = Instant::now();
        let incoming =
            AircFrame::new(Kind::Msg, 9, encode_fields(&["#rf", "hi"])).with_flags(flags::ACK_REQ);
        s.on_receive(&call(), incoming, now, true);
        let acks = take_acks(&mut s);
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].kind, Kind::Ack);
        assert_eq!(acks[0].payload, 9u16.to_be_bytes());
    }

    #[test]
    fn a_piggybacked_ack_releases_the_senders_queue() {
        let mut s = Sessions::new(SessionConfig::default());
        let now = Instant::now();
        let first = s.send(&call(), Kind::Msg, b"one".to_vec(), true, now);
        let queued = s.send(&call(), Kind::Msg, b"two".to_vec(), true, now);
        assert!(queued.is_empty());
        let seq = first[0].seq;
        let mut reply = AircFrame::new(Kind::Msg, 50, encode_fields(&["#rf", "ok"]));
        reply.attach_piggyback(seq);
        let out = s.on_receive(&call(), reply, now, true);
        assert_eq!(out.transmit.len(), 1);
        assert_eq!(out.transmit[0].payload, b"two");
    }

    #[test]
    fn reassembly_timeout_sends_a_partial_ack() {
        let cfg = SessionConfig {
            paclen: HEADER_LEN + 4,
            reassembly_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let mut tx = Sessions::new(cfg.clone());
        let mut rx = Sessions::new(cfg);
        let mut now = Instant::now();
        let frames = tx.send(&call(), Kind::Msg, b"abcdefghij".to_vec(), true, now);
        rx.on_receive(&call(), frames[0].clone(), now, true);
        now += Duration::from_secs(6);
        let tick = rx.tick(now);
        assert_eq!(tick.transmit.len(), 1);
        assert_eq!(tick.transmit[0].1.kind, Kind::Ack);
        assert!(
            tick.transmit[0].1.payload.len() > 2,
            "the sender needs a bitmap, not a silent drop"
        );
    }
}
