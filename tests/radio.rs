//! The transmit subsystem on its own.
//!
//! `tests/rf.rs` drives the bridge and `tests/tnc.rs` drives the link; this
//! sits between them and exercises [`Radio`] directly — the decisions about
//! whether something may be transmitted, what it costs, and when the station
//! owes the band its callsign.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rfircd::airc::{encode_fields, AircFrame, Kind};
use rfircd::ax25::kiss::{self, KissDecoder};
use rfircd::ax25::tnc::{self, TncConfig};
use rfircd::ax25::Ax25Frame;
use rfircd::callsign::Callsign;
use rfircd::config::Config;
use rfircd::server::{IdentifyResult, Radio, TxClass};
use tokio::io::{AsyncReadExt, DuplexStream};

const CONFIG: &str = r##"
[server]
name = "radio.test"

[listen]
bind = []

[radio]
enabled = true
callsign = "SK0MT-1"
destination = "AIRC"
id_interval_secs = 60
paclen = 128

[radio.tnc]
kind = "loopback"
tx_pacing_ms = 0

[radio.duty]
enabled = true
baud = 9600
txdelay_ms = 10
txtail_ms = 10
max_duty_percent = 50

[accounts]
file = "target/test-radio-nicks.json"

[[channels]]
name = "#rf"
rf = true
"##;

struct Harness {
    radio: Radio,
    far: DuplexStream,
    _rx: tokio::sync::mpsc::Receiver<Ax25Frame>,
    decoder: KissDecoder,
}

impl Harness {
    fn new() -> Self {
        Self::with(CONFIG)
    }

    fn with(text: &str) -> Self {
        let config = Arc::new(Config::from_toml(text).unwrap());
        let (link, far) = TncConfig::loopback_link();
        let (handle, rx) = tnc::spawn(TncConfig::from_config(&config, link));
        Harness {
            radio: Radio::new(config, Some(handle)),
            far,
            _rx: rx,
            decoder: KissDecoder::new(4096),
        }
    }

    /// A radio with no TNC at all.
    fn headless(text: &str) -> Radio {
        let config = Arc::new(Config::from_toml(text).unwrap());
        Radio::new(config, None)
    }

    async fn transmitted(&mut self) -> Vec<AircFrame> {
        self.transmitted_ax()
            .await
            .into_iter()
            .filter_map(|ax| AircFrame::decode(&ax.info).ok())
            .collect()
    }

    async fn transmitted_ax(&mut self) -> Vec<Ax25Frame> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(Duration::from_millis(800), self.far.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    for kf in self.decoder.push(&buf[..n]) {
                        if kf.command != kiss::CMD_DATA {
                            continue;
                        }
                        if let Ok(ax) = Ax25Frame::decode(&kf.payload) {
                            out.push(ax);
                        }
                    }
                }
                _ => break,
            }
        }
        out
    }
}

fn station() -> Callsign {
    "SM0ABC-7".parse().unwrap()
}

// ------------------------------------------------------------------ broadcast

#[tokio::test]
async fn an_empty_broadcast_still_puts_one_frame_on_the_air() {
    let mut h = Harness::new();
    // A PING-style message with no fields is still a message; it must not
    // become zero frames and vanish.
    h.radio.broadcast(Kind::Id, Vec::new(), TxClass::Control);
    let sent = h.transmitted().await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].kind, Kind::Id);
    assert_eq!(sent[0].frag_total, 1);
    assert!(sent[0].payload.is_empty());
}

#[tokio::test]
async fn a_broadcast_too_large_to_fragment_is_refused_not_truncated() {
    let mut h = Harness::new();
    // AIRC numbers fragments in one octet, so 255 is the ceiling. A payload
    // past that cannot be sent at all; sending the first 255 fragments would
    // put a message on the air that can never be reassembled — pure airtime
    // for nothing.
    let per_frame = h.radio.max_payload();
    let huge = vec![b'x'; per_frame * 300];
    h.radio.broadcast(Kind::Msg, huge, TxClass::Chat);

    assert!(
        h.transmitted().await.is_empty(),
        "an unsendable message must not go out as a partial one"
    );
    assert_eq!(
        h.radio.stats.rf_frames_dropped, 1,
        "and the operator should be able to see that it happened"
    );
}

#[tokio::test]
async fn a_long_broadcast_is_fragmented_and_every_piece_is_numbered() {
    let mut h = Harness::new();
    let per_frame = h.radio.max_payload();
    let payload = vec![b'y'; per_frame * 3 + 7];
    h.radio.broadcast(Kind::Msg, payload.clone(), TxClass::Chat);

    let sent = h.transmitted().await;
    assert_eq!(sent.len(), 4, "{} fragments", sent.len());
    let seq = sent[0].seq;
    let mut rebuilt = Vec::new();
    for (i, f) in sent.iter().enumerate() {
        assert_eq!(f.seq, seq, "every fragment of a message shares its seq");
        assert_eq!(f.frag_index, i as u8);
        assert_eq!(f.frag_total, 4);
        rebuilt.extend_from_slice(&f.payload);
    }
    assert_eq!(rebuilt, payload);
}

// ------------------------------------------------------------------ admission

#[tokio::test]
async fn acknowledgements_are_never_rationed() {
    let h = Harness::new();
    // Set the published backlog directly rather than racing the TNC task,
    // which drains as fast as it is fed on a loopback link.
    let budget = h.radio.backlog_budget().as_millis() as u64;
    h.radio
        .airtime()
        .unwrap()
        .queued_ms
        .store(budget * 3 / 4, std::sync::atomic::Ordering::Relaxed);

    assert!(
        !h.radio.backlog_has_room(200, TxClass::Chat),
        "chat may fill only half the budget, so at three quarters it is shut out"
    );
    assert!(
        !h.radio.backlog_has_room(200, TxClass::Direct),
        "a private message may fill seven tenths"
    );
    assert!(
        h.radio.backlog_has_room(200, TxClass::Control),
        "control traffic keeps room above that"
    );
    assert!(
        h.radio.backlog_has_room(200, TxClass::Ack),
        "an ACK is one short frame that prevents a long retransmission; it must \
         never wait behind a backlog of chat"
    );
}

#[tokio::test]
async fn a_full_transmit_queue_is_counted_not_hidden() {
    let mut h = Harness::new();
    // The TNC channel is 64 deep; push far past it.
    for i in 0..500 {
        h.radio.transmit_direct(
            &station(),
            AircFrame::new(Kind::Msg, i, vec![0x41; 100]),
            TxClass::Ack,
        );
    }
    assert!(
        h.radio.stats.rf_frames_dropped > 0,
        "frames the TNC could not take should be counted so RADIO QUEUE shows them"
    );
    assert!(
        h.radio.stats.rf_frames_tx > 0,
        "and some should have got through"
    );
}

#[tokio::test]
async fn a_reliable_message_that_does_not_fit_is_refused_before_it_is_committed() {
    let text = CONFIG.replace(
        "id_interval_secs = 60",
        "id_interval_secs = 60\nmax_queued_airtime_secs = 1",
    );
    let mut h = Harness::with(&text);
    // Fill what `Direct` may occupy.
    for i in 0..40 {
        h.radio.transmit_direct(
            &station(),
            AircFrame::new(Kind::Msg, i, vec![0x41; 120]),
            TxClass::Ack,
        );
    }
    let before = h.radio.stats.rf_frames_refused;
    h.radio.unicast(
        &station(),
        Kind::Msg,
        encode_fields(&["#rf", "alice", "hello"]),
        true,
        TxClass::Direct,
    );
    assert!(
        h.radio.stats.rf_frames_refused > before,
        "admission must refuse before the session layer starts an ACK timer, or the \
         message costs up to max_retries transmissions instead of one"
    );
    assert!(
        h.radio.sessions.peer(&station()).is_none(),
        "nothing should have been committed to the session layer"
    );
}

// ------------------------------------------------------------- identification

#[tokio::test]
async fn a_station_that_has_transmitted_identifies_when_the_interval_passes() {
    let text = CONFIG.replace("id_interval_secs = 60", "id_interval_secs = 10");
    let mut h = Harness::with(&text);
    // Drive the clock explicitly: `maybe_identify` also resets its own timer
    // when the interval elapses with nothing to say, so two calls with
    // near-identical timestamps are not the same as two intervals apart.
    let start = Instant::now();

    // Nothing transmitted yet: identifying now would just be QRM.
    h.radio.maybe_identify(start + Duration::from_secs(11));
    assert!(
        h.transmitted().await.is_empty(),
        "an idle station has nothing to identify"
    );

    h.radio
        .broadcast(Kind::Msg, encode_fields(&["#rf", "a", "b"]), TxClass::Chat);
    let _ = h.transmitted().await;

    h.radio.maybe_identify(start + Duration::from_secs(22));
    let sent = h.transmitted().await;
    assert!(
        sent.iter().any(|f| f.kind == Kind::Id),
        "a station that has transmitted must identify: {sent:?}"
    );

    // And having identified, it does not do so again until it transmits.
    h.radio.maybe_identify(start + Duration::from_secs(33));
    assert!(
        h.transmitted().await.is_empty(),
        "identifying an idle station is just more QRM"
    );
}

#[tokio::test]
async fn identification_is_counted_as_airtime_like_anything_else() {
    let mut h = Harness::new();
    h.radio
        .broadcast(Kind::Msg, encode_fields(&["#rf", "a", "b"]), TxClass::Chat);
    let _ = h.transmitted().await;
    let before = h.radio.stats.rf_bytes_tx;

    h.radio.id_if_needed();
    let sent = h.transmitted().await;
    assert!(sent.iter().any(|f| f.kind == Kind::Id));
    assert!(
        h.radio.stats.rf_bytes_tx > before,
        "an ID is key-down time and has to appear in the totals"
    );
}

#[tokio::test]
async fn identification_is_addressed_to_id() {
    let mut h = Harness::new();
    h.radio
        .broadcast(Kind::Msg, encode_fields(&["#rf", "a", "b"]), TxClass::Chat);
    let _ = h.transmitted().await;

    h.radio.id_if_needed();
    let sent = h.transmitted_ax().await;
    let id = sent
        .iter()
        .find(|ax| AircFrame::decode(&ax.info).map(|f| f.kind) == Ok(Kind::Id))
        .expect("an ID frame should have gone out");
    assert_eq!(
        id.destination.call.to_string(),
        "ID",
        "identification is addressed to ID, not the protocol address: {}",
        id.to_monitor_line()
    );
}

#[tokio::test]
async fn radio_id_is_rate_limited_unless_the_station_owes_one() {
    let mut h = Harness::new();
    assert_eq!(h.radio.identify_now(), IdentifyResult::Sent);
    let _ = h.transmitted().await;
    assert_eq!(
        h.radio.identify_now(),
        IdentifyResult::RateLimited,
        "a second RADIO ID inside the interval is how an OPER cooks a QMX"
    );
}

#[tokio::test]
async fn held_mail_is_not_destroyed_when_the_transmitter_is_off() {
    use rfircd::server::mailbox::StoredMessage;

    let mut h = Harness::new();
    h.radio
        .mailbox
        .store(
            &station(),
            StoredMessage {
                from: "alice".into(),
                text: "keep this".into(),
                notice: false,
                truncated: false,
                stored_at: Instant::now(),
            },
        )
        .unwrap();
    h.radio.enabled = false;
    h.radio.flush_mailbox(&station());
    assert_eq!(
        h.radio.mailbox.depth(&station()),
        1,
        "RADIO OFF must not take the only copy out of the mailbox"
    );
}

#[tokio::test]
async fn held_mail_is_not_destroyed_when_the_session_queue_is_full() {
    use rfircd::server::mailbox::StoredMessage;

    let mut h = Harness::new();
    for _ in 0..20 {
        h.radio.unicast(
            &station(),
            Kind::Msg,
            encode_fields(&["SM0ABC|7", "alice", "filler"]),
            true,
            TxClass::Direct,
        );
    }
    assert!(
        !h.radio.sessions.can_accept(&station()),
        "test setup: the session queue should be full"
    );
    h.radio
        .mailbox
        .store(
            &station(),
            StoredMessage {
                from: "alice".into(),
                text: "keep this too".into(),
                notice: false,
                truncated: false,
                stored_at: Instant::now(),
            },
        )
        .unwrap();
    h.radio.flush_mailbox(&station());
    assert_eq!(
        h.radio.mailbox.depth(&station()),
        1,
        "a full session queue must leave held mail where it is"
    );
}

// ------------------------------------------------------------------ reporting

#[tokio::test]
async fn the_status_line_says_which_of_the_several_reasons_applies() {
    // No radio configured at all.
    let off = Harness::headless(&CONFIG.replace("enabled = true", "enabled = false"));
    assert!(
        off.status_line().contains("disabled"),
        "{}",
        off.status_line()
    );

    // Configured, but no TNC attached.
    let headless = Harness::headless(CONFIG);
    let line = headless.status_line();
    assert!(line.contains("no TNC attached"), "{line}");
    assert!(
        line.contains("SK0MT-1"),
        "the operator needs the callsign: {line}"
    );

    // Attached, but the operator turned it off.
    let mut h = Harness::new();
    assert!(h.radio.status_line().contains("transmitter ON"));
    h.radio.enabled = false;
    assert!(h.radio.status_line().contains("transmitter OFF"));
    h.radio.enabled = true;

    // Attached and cooling down.
    let air = h.radio.airtime().unwrap();
    air.cooling_ms
        .store(45_000, std::sync::atomic::Ordering::Relaxed);
    let line = h.radio.status_line();
    assert!(line.contains("PA cooling for 45s"), "{line}");
}

#[tokio::test]
async fn stations_heard_are_counted() {
    let mut h = Harness::new();
    assert_eq!(h.radio.peers_heard(), 0);
    let now = Instant::now();
    h.radio.sessions.force_touch(&station(), now);
    h.radio
        .sessions
        .force_touch(&"SM0XYZ-1".parse().unwrap(), now);
    assert_eq!(h.radio.peers_heard(), 2);
    assert!(h.radio.status_line().contains("2 RF station"));
}

// --------------------------------------------------------------------- retries

#[tokio::test]
async fn a_retransmission_is_sent_without_a_fresh_admission_check() {
    let text = CONFIG.replace(
        "id_interval_secs = 60",
        "id_interval_secs = 60\nack_timeout_secs = 1\nmax_queued_airtime_secs = 1",
    );
    let mut h = Harness::with(&text);
    h.radio.unicast(
        &station(),
        Kind::Msg,
        encode_fields(&["SM0ABC|7", "alice", "answer me"]),
        true,
        TxClass::Direct,
    );
    let sent = h.transmitted().await;
    let seq = sent.first().expect("the original should have gone out").seq;
    h.radio.sessions.on_keyed(&station(), seq, Instant::now());

    // The backlog is now full, but the exchange is already part-way done:
    // dropping the retry would leave the peer waiting for something that will
    // never arrive.
    for i in 0..40 {
        h.radio.transmit_direct(
            &station(),
            AircFrame::new(Kind::Msg, 500 + i, vec![0x41; 120]),
            TxClass::Ack,
        );
    }
    let _ = h.transmitted().await;

    let outcome = h
        .radio
        .sessions
        .tick(Instant::now() + Duration::from_secs(5));
    assert!(
        !outcome.transmit.is_empty(),
        "the session layer wants a retry"
    );
    for (call, frame) in outcome.transmit {
        h.radio.transmit_to(&call, frame);
    }
    assert!(
        h.radio.stats.rf_frames_refused == 0,
        "a retransmission is not new traffic and must not be refused as such"
    );
}

#[tokio::test]
async fn radio_off_does_not_burn_ack_retries() {
    let mut h = Harness::new();
    h.radio.unicast(
        &station(),
        Kind::Msg,
        encode_fields(&["SM0ABC|7", "alice", "answer me"]),
        true,
        TxClass::Direct,
    );
    let _ = h.transmitted().await;
    h.radio.enabled = false;

    // Well past every retry deadline. Same condition the server tick uses.
    let now = Instant::now() + Duration::from_secs(120);
    let out = h
        .radio
        .sessions
        .tick_retries(now, h.radio.available() && !h.radio.interlock_down());
    assert!(out.transmit.is_empty());
    assert!(
        out.lost.is_empty(),
        "RADIO OFF must not declare the station lost while the operator may turn it back on"
    );
    assert!(h.radio.sessions.peer(&station()).is_some());
}

// --------------------------------------------------------------------- mailbox

#[tokio::test]
async fn a_shortened_held_message_is_flagged_as_shortened_on_the_air() {
    use rfircd::airc::frame::flags;
    use rfircd::server::mailbox::StoredMessage;

    let mut h = Harness::new();
    h.radio
        .mailbox
        .store(
            &station(),
            StoredMessage {
                from: "alice".into(),
                text: "this was cut short".into(),
                notice: false,
                truncated: true,
                stored_at: Instant::now(),
            },
        )
        .unwrap();

    h.radio.flush_mailbox(&station());
    let sent = h.transmitted().await;
    let stored = sent
        .iter()
        .find(|f| f.kind == Kind::Stored)
        .expect("the held message should go out");
    assert!(
        stored.flags & flags::TRUNCATED != 0,
        "the station should be told it is not seeing the whole message"
    );
}

#[tokio::test]
async fn flushing_an_empty_mailbox_transmits_nothing() {
    let mut h = Harness::new();
    h.radio.flush_mailbox(&station());
    assert!(h.transmitted().await.is_empty());
}
