//! End-to-end tests: a fake TNC on one side, a fake IRC client on the other,
//! and the real server in between.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rfircd::airc::{encode_fields, flags, AircFrame, Kind};
use rfircd::ax25::kiss::{self, KissDecoder};
use rfircd::ax25::tnc::{self, TncConfig};
use rfircd::ax25::Ax25Frame;
use rfircd::callsign::Callsign;
use rfircd::config::Config;
use rfircd::server::state::ClientId;
use rfircd::server::{Event, Server};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;

/// A fast link with the airtime governor ON.
///
/// Every fixture in this file includes it. The governor stays enabled in tests
/// on purpose — it once stopped being wired to the TNC at all and the tests
/// could not tell — but these are 9600 baud with a short key-up, so a frame is
/// about 150 ms and the limits do not dominate tests that are about something
/// else. Tests that *are* about the governor override these with tighter ones.
const FAST_LINK: &str = r##"
tx_pacing_ms = 0

[radio.duty]
enabled = true
baud = 9600
txdelay_ms = 10
txtail_ms = 10
window_secs = 600
max_duty_percent = 50
max_continuous_secs = 60
cooldown_secs = 60
hourly_airtime_secs = 0
max_hold_secs = 120
"##;

/// Policy limits raised out of the way, so a test can isolate the airtime
/// layer beneath them.
///
/// Four independent things can stop a message reaching the air, and they fire
/// in this order: the IRC-side flood cap (`[policy]`), the RF-TX privilege and
/// content screen, airtime admission control, and finally the governor
/// deferring at the transmitter. A test that wants to prove the fourth has to
/// get past the first.
const NO_FLOOD_CAP: &str = r##"
rf_channel_msgs_per_min = 6000
rf_channel_burst = 200
ip_to_rf_msgs_per_min = 6000
ip_to_rf_burst = 200
ip_cmds_per_min = 6000
ip_cmd_burst = 500
"##;

/// Expand the `__FAST_LINK__` placeholder. Call this before customising a
/// fixture, so a test edits the settings it can actually see.
fn config_text(base: &str) -> String {
    base.replace("__FAST_LINK__", FAST_LINK)
}

const CONFIG: &str = r##"
[server]
name = "test.gateway"
motd = ["packet radio here"]

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
__FAST_LINK__

[policy]

[accounts]
file = "target/test-nicks.json"
identify_timeout_secs = 60

[[channels]]
name = "#rf"
topic = "gateway channel"
rf = true

[[channels]]
name = "#local"

[[opers]]
name = "root"
password = "operpass1"
"##;

struct Harness {
    server: Server,
    far: DuplexStream,
    rf_rx: mpsc::Receiver<Ax25Frame>,
    client_rx: mpsc::Receiver<String>,
    decoder: KissDecoder,
}

const CLIENT: ClientId = 1;

impl Harness {
    async fn new() -> Self {
        Self::from_toml(CONFIG).await
    }

    async fn from_toml(text: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let text = text
            .replace(
                "target/test-nicks.json",
                &format!("target/test-nicks-{pid}-{n}.json"),
            )
            .replace(
                "target/test-nicks-grant.json",
                &format!("target/test-nicks-grant-{pid}-{n}.json"),
            );
        let config = Arc::new(Config::from_toml(&config_text(&text)).unwrap());
        // The same TncConfig the server builds, with a loopback link
        // substituted. Anything derived from the configuration file — paclen,
        // pacing, and the whole `[radio.duty]` section — therefore reaches the
        // TNC in tests exactly as it does in production.
        let (link, far) = TncConfig::loopback_link();
        let (handle, rf_rx) = tnc::spawn(TncConfig::from_config(&config, link));
        let mut server = Server::new(config, Some(handle)).unwrap();

        let (out, client_rx) = mpsc::channel(1024);
        server.handle(Event::Connected {
            id: CLIENT,
            host: "127.0.0.1".into(),
            listen_only: false,
            out,
            hangup: None,
        });
        let mut h = Self {
            server,
            far,
            rf_rx,
            client_rx,
            decoder: KissDecoder::new(1024),
        };
        h.send("NICK alice");
        h.send("USER alice 0 * :Alice");
        h
    }

    fn send(&mut self, line: &str) {
        self.server.handle(Event::Line {
            id: CLIENT,
            line: line.to_string(),
        });
    }

    fn connect_extra(&mut self, id: ClientId, nick: &str) -> mpsc::Receiver<String> {
        let (out, rx) = mpsc::channel(1024);
        self.server.handle(Event::Connected {
            id,
            // A distinct host per client: `max_conns_per_host` would
            // otherwise refuse everything past the eighth, and a test that
            // silently has one user in the channel proves nothing.
            host: format!("10.0.{}.{}", id / 256, id % 256),
            listen_only: false,
            out,
            hangup: None,
        });
        self.server.handle(Event::Line {
            id,
            line: format!("NICK {nick}"),
        });
        self.server.handle(Event::Line {
            id,
            line: format!("USER {nick} 0 * :{nick}"),
        });
        rx
    }

    fn oper_and_callsign(&mut self) {
        self.send("OPER root operpass1");
        self.send("CALLSIGN SM0XYZ");
    }

    fn drain_client(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(line) = self.client_rx.try_recv() {
            out.push(line);
        }
        out
    }

    /// Put a frame on the air as if a station had transmitted it, and let the
    /// server process it.
    async fn station_transmits(&mut self, from: &str, frame: AircFrame) {
        // Stations unicast to the gateway callsign; see PROTOCOL.md §3.1.
        self.transmits_to(from, "SK0MT-1", frame).await
    }

    /// As above, but addressed wherever the caller says. Used to check that
    /// the gateway ignores traffic that is not its business.
    async fn transmits_to(&mut self, from: &str, to: &str, frame: AircFrame) {
        let call: Callsign = from.parse().unwrap();
        let ax = Ax25Frame::ui(call, to.parse().unwrap(), &[], frame.encode()).unwrap();
        let wire = kiss::encode(0, kiss::CMD_DATA, &ax.encode());
        self.far.write_all(&wire).await.unwrap();
        self.far.flush().await.unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), self.rf_rx.recv())
            .await
            .expect("TNC task did not deliver the frame")
            .expect("TNC channel closed");
        self.server.handle(Event::Rf(received));
    }

    /// Acknowledge a frame the gateway sent us.
    async fn ack(&mut self, frame: &AircFrame) {
        self.station_transmits(
            "SM0ABC-7",
            AircFrame::new(Kind::Ack, frame.seq, frame.seq.to_be_bytes().to_vec()),
        )
        .await;
    }

    /// Collect everything the gateway has transmitted so far.
    async fn transmitted(&mut self) -> Vec<(Ax25Frame, AircFrame)> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(Duration::from_millis(500), self.far.read(&mut buf)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => {
                    for kf in self.decoder.push(&buf[..n]) {
                        // The gateway also pushes KISS parameter frames
                        // (TXDELAY, TXTAIL, full-duplex off) at connect. Only
                        // data frames carry AX.25.
                        if kf.command != kiss::CMD_DATA {
                            continue;
                        }
                        let ax = Ax25Frame::decode(&kf.payload).unwrap();
                        if let Ok(airc) = AircFrame::decode(&ax.info) {
                            out.push((ax, airc));
                        }
                    }
                }
                Ok(Err(e)) => panic!("loopback read failed: {e}"),
            }
        }
        out
    }
}

#[tokio::test]
async fn registration_and_local_channel() {
    let mut h = Harness::new().await;
    let lines = h.drain_client();
    assert!(lines.iter().any(|l| l.contains(" 001 alice")));
    assert!(lines.iter().any(|l| l.contains("packet radio here")));

    h.send("JOIN #local");
    let lines = h.drain_client();
    assert!(lines.iter().any(|l| l.contains("JOIN #local")));
    assert!(lines.iter().any(|l| l.contains(" 366 ")));
}

#[tokio::test]
async fn station_joins_and_appears_on_irc() {
    let mut h = Harness::new().await;
    h.send("JOIN #rf");
    h.drain_client();

    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])),
    )
    .await;

    let lines = h.drain_client();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("SM0ABC|7") && l.contains("JOIN #rf")),
        "IRC users should see the station join: {lines:?}"
    );

    // The gateway confirms the join, but does NOT read out the member list:
    // nobody asked for it, and a roll call is airtime.
    let tx = h.transmitted().await;
    let reply = tx
        .iter()
        .find(|(_, a)| a.kind == Kind::NamesReply)
        .expect("expected a join confirmation");
    assert_eq!(reply.0.destination.call.to_string(), "SM0ABC-7");
    assert_eq!(reply.0.source.call.to_string(), "SK0MT-1");
    let fields = reply.1.fields();
    assert!(
        !fields[1].contains("alice"),
        "the member list was sent unasked: {fields:?}"
    );
    assert!(
        fields[1].contains("here"),
        "the join confirmation should carry a member count: {fields:?}"
    );
}

#[tokio::test]
async fn message_from_rf_reaches_irc_without_retransmission() {
    let mut h = Harness::new().await;
    h.send("JOIN #rf");
    h.drain_client();
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])),
    )
    .await;
    h.transmitted().await;
    h.drain_client();

    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(
            Kind::Msg,
            2,
            encode_fields(&["#rf", "good morning from the hilltop"]),
        ),
    )
    .await;

    let lines = h.drain_client();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("PRIVMSG #rf :good morning from the hilltop")),
        "{lines:?}"
    );

    let tx = h.transmitted().await;
    assert!(
        tx.iter().all(|(_, a)| a.kind != Kind::Msg),
        "a message heard on the air must not be transmitted back onto it"
    );
}

#[tokio::test]
async fn irc_message_needs_a_callsign_before_it_is_transmitted() {
    let mut h = Harness::new().await;
    h.send("JOIN #rf");
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])),
    )
    .await;
    h.transmitted().await;
    h.drain_client();

    h.send("PRIVMSG #rf :hello radio");
    let lines = h.drain_client();
    assert!(
        lines
            .iter()
            .any(|l| l.contains(" 404 ") || l.contains("+m")),
        "unidentified users must not speak on #rf: {lines:?}"
    );
    let tx = h.transmitted().await;
    assert!(tx.iter().all(|(_, a)| a.kind != Kind::Msg));

    h.send("CALLSIGN SM0XYZ");
    h.drain_client();
    h.send("PRIVMSG #rf :hello radio");
    let lines = h.drain_client();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("RF-TX") || l.contains("not have RF-TX")),
        "CALLSIGN alone must not radiate: {lines:?}"
    );
    let tx = h.transmitted().await;
    assert!(tx.iter().all(|(_, a)| a.kind != Kind::Msg));

    h.send("OPER root operpass1");
    h.drain_client();
    h.send("PRIVMSG #rf :hello radio");
    h.drain_client();

    let tx = h.transmitted().await;
    let (ax, airc) = tx
        .iter()
        .find(|(_, a)| a.kind == Kind::Msg)
        .unwrap_or_else(|| {
            panic!(
                "OPER + CALLSIGN should put the message on the air; got {:?}",
                tx.iter().map(|(_, a)| a.kind).collect::<Vec<_>>()
            )
        });
    assert_eq!(ax.destination.call.to_string(), "AIRC");
    assert_eq!(airc.fields(), vec!["#rf", "alice", "hello radio"]);
}

#[tokio::test]
async fn ciphertext_is_not_transmitted() {
    let mut h = Harness::new().await;
    h.oper_and_callsign();
    h.send("JOIN #rf");
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])),
    )
    .await;
    h.transmitted().await;
    h.drain_client();

    let mut bob_rx = h.connect_extra(2, "bob");
    let _ = drain_rx(&mut bob_rx);
    h.server.handle(Event::Line {
        id: 2,
        line: "JOIN #rf".into(),
    });
    let _ = drain_rx(&mut bob_rx);

    h.send("PRIVMSG #rf :U2FsdGVkX1+8xQ2mZ9pKbNvYwErTyUiOpAsDfGhJkLzXcVbNm1234567890AbCdEf");
    let lines = h.drain_client();
    assert!(
        lines.iter().any(|l| l.contains("obscure their meaning")),
        "{lines:?}"
    );
    let tx = h.transmitted().await;
    assert!(tx.iter().all(|(_, a)| a.kind != Kind::Msg));

    let bob_lines = drain_rx(&mut bob_rx);
    assert!(
        bob_lines
            .iter()
            .any(|l| l.contains("PRIVMSG #rf") && l.contains("U2FsdGVk")),
        "ciphertext must still reach other IRC clients: {bob_lines:?}"
    );
}

fn drain_rx(rx: &mut mpsc::Receiver<String>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(line) = rx.try_recv() {
        out.push(line);
    }
    out
}

#[tokio::test]
async fn private_message_to_a_station_is_acknowledged_and_retried() {
    let mut h = Harness::new().await;
    h.oper_and_callsign();
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Hello, 1, encode_fields(&[""])),
    )
    .await;
    // The welcome is sent reliably, so the station must acknowledge it before
    // anything else can be unicast to it.
    let welcome = h
        .transmitted()
        .await
        .into_iter()
        .find(|(_, a)| a.kind == Kind::Welcome)
        .expect("a station that says HELLO gets a welcome");
    h.ack(&welcome.1).await;
    h.drain_client();

    h.send("PRIVMSG SM0ABC|7 :direct message");
    let tx = h.transmitted().await;
    let (ax, airc) = tx
        .iter()
        .find(|(_, a)| a.kind == Kind::Msg)
        .expect("private messages are unicast to the station");
    assert_eq!(ax.destination.call.to_string(), "SM0ABC-7");
    assert!(airc.wants_ack());
    assert_eq!(airc.fields(), vec!["SM0ABC|7", "alice", "direct message"]);

    // The station acknowledges; nothing more is transmitted.
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Ack, airc.seq, airc.seq.to_be_bytes().to_vec()),
    )
    .await;
    for _ in 0..3 {
        h.server.handle(Event::Tick);
    }
    let tx = h.transmitted().await;
    assert!(
        tx.iter().all(|(_, a)| a.kind != Kind::Msg),
        "an acknowledged message must not be retransmitted"
    );
}

#[tokio::test]
async fn station_frames_from_implausible_callsigns_are_ignored() {
    let mut h = Harness::new().await;
    h.send("JOIN #rf");
    h.drain_client();

    h.station_transmits(
        "NOCALL",
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])),
    )
    .await;
    let lines = h.drain_client();
    assert!(lines.iter().all(|l| !l.contains("JOIN #rf")), "{lines:?}");
}

#[tokio::test]
async fn messages_are_held_for_a_station_that_is_out_of_range() {
    let mut h = Harness::new().await;
    h.oper_and_callsign();
    h.drain_client();

    // Nobody by that name is on frequency, but it is a plausible callsign.
    h.send("PRIVMSG SM0ABC|7 :meet on 145.500 at 1900");
    let lines = h.drain_client();
    assert!(
        lines.iter().any(|l| l.contains("Held for delivery")),
        "{lines:?}"
    );
    let tx = h.transmitted().await;
    assert!(
        tx.iter().all(|(_, a)| a.kind != Kind::Stored),
        "nothing should be transmitted to a station we cannot hear"
    );

    // The station appears. The welcome goes first and is acknowledged; the
    // held message follows it out of the per-station queue.
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Hello, 1, encode_fields(&[""])),
    )
    .await;
    let welcome = h
        .transmitted()
        .await
        .into_iter()
        .find(|(_, a)| a.kind == Kind::Welcome)
        .expect("a station that says HELLO gets a welcome");
    h.ack(&welcome.1).await;
    let tx = h.transmitted().await;
    let (ax, airc) = tx
        .iter()
        .find(|(_, a)| a.kind == Kind::Stored)
        .expect("held mail should be delivered on first contact");
    assert_eq!(ax.destination.call.to_string(), "SM0ABC-7");
    let fields = airc.fields();
    assert_eq!(fields[0], "SM0ABC|7");
    assert_eq!(fields[1], "alice");
    assert_eq!(fields[2], "meet on 145.500 at 1900");
    assert!(fields[3].parse::<u64>().is_ok(), "age in seconds");
    assert!(airc.wants_ack());
}

#[tokio::test]
async fn unknown_nicknames_that_are_not_callsigns_still_fail_normally() {
    let mut h = Harness::new().await;
    h.send("CALLSIGN SM0XYZ");
    h.drain_client();
    h.send("PRIVMSG nosuchperson :hello");
    let lines = h.drain_client();
    assert!(lines.iter().any(|l| l.contains(" 401 ")), "{lines:?}");
}

#[tokio::test]
async fn callsign_nicks_are_reserved_including_casemapping_and_ax25_form() {
    let mut h = Harness::new().await;
    h.drain_client();
    h.send("NICK SM0ABC|7");
    let lines = h.drain_client();
    assert!(lines.iter().any(|l| l.contains(" 432 ")), "{lines:?}");
    h.send("NICK SM0ABC\\7");
    let lines = h.drain_client();
    assert!(lines.iter().any(|l| l.contains(" 432 ")), "{lines:?}");
    h.send("NICK SM0ABC-7");
    let lines = h.drain_client();
    assert!(lines.iter().any(|l| l.contains(" 432 ")), "{lines:?}");
}

#[tokio::test]
async fn rf_quit_reason_cannot_inject_irc_lines() {
    let mut h = Harness::new().await;
    h.send("JOIN #rf");
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])),
    )
    .await;
    h.drain_client();

    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Quit, 2, b"gone\r\nNOTICE alice :injected".to_vec()),
    )
    .await;
    let lines = h.drain_client();
    assert!(
        lines.iter().any(|l| l.contains("QUIT")),
        "expected a QUIT: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .all(|l| l.contains("QUIT") || !l.contains("NOTICE alice :injected")),
        "CRLF in a QUIT reason must not become a new IRC command: {lines:?}"
    );
}

#[tokio::test]
async fn callsign_alone_does_not_radiate() {
    let mut h = Harness::new().await;
    h.send("CALLSIGN SM0XYZ");
    h.send("JOIN #rf");
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])),
    )
    .await;
    h.transmitted().await;
    h.drain_client();

    h.send("PRIVMSG #rf :hello radio");
    let lines = h.drain_client();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("RF-TX") || l.contains("not have RF-TX")),
        "CALLSIGN alone must not radiate: {lines:?}"
    );
    let tx = h.transmitted().await;
    assert!(
        tx.iter().all(|(_, a)| a.kind != Kind::Msg),
        "ordinary IRC clients must not key the transmitter"
    );
}

#[tokio::test]
async fn rf_tx_can_cq_when_no_station_has_joined() {
    let mut h = Harness::new().await;
    h.oper_and_callsign();
    h.send("JOIN #rf");
    h.drain_client();
    let _ = h.transmitted().await;

    h.send("PRIVMSG #rf :cq cq anyone on frequency");
    let lines = h.drain_client();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("Queued for RF") && l.contains("CQ")),
        "the sender should be told this is a CQ into an empty channel: {lines:?}"
    );
    let tx = h.transmitted().await;
    let msg = tx.iter().find(|(_, a)| a.kind == Kind::Msg);
    assert!(
        msg.is_some(),
        "OPER + CALLSIGN must be able to CQ so a station on frequency can hear the gateway: {:?}",
        tx.iter().map(|(_, a)| a.kind).collect::<Vec<_>>()
    );
    assert_eq!(
        msg.unwrap().1.fields(),
        vec!["#rf", "alice", "cq cq anyone on frequency"]
    );
}

#[tokio::test]
async fn without_rf_tx_an_empty_channel_stays_on_irc() {
    let mut h = Harness::new().await;
    h.send("CALLSIGN SM0XYZ");
    h.send("JOIN #rf");
    h.drain_client();
    let _ = h.transmitted().await;

    h.send("PRIVMSG #rf :hello");
    let lines = h.drain_client();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("No RF station") || l.contains("can still CQ")),
        "the empty-channel lock must still apply without RF-TX: {lines:?}"
    );
    let tx = h.transmitted().await;
    assert!(
        tx.iter().all(|(_, a)| a.kind != Kind::Msg),
        "ordinary chat must not key an empty frequency: {:?}",
        tx.iter().map(|(_, a)| a.kind).collect::<Vec<_>>()
    );

    h.send("PRIVMSG #rf :hello again");
    let again = h.drain_client();
    assert!(
        again
            .iter()
            .all(|l| !l.contains("No RF station") && !l.contains("can still CQ")),
        "the same explanation must not be repeated on every line: {again:?}"
    );
}

#[tokio::test]
async fn grant_requires_a_registered_nick_then_survives_identify() {
    let toml = r##"
[server]
name = "test.gateway"
[listen]
bind = []
[radio]
enabled = true
callsign = "SK0MT-1"
destination = "AIRC"
id_interval_secs = 60
[radio.tnc]
kind = "loopback"
__FAST_LINK__
[accounts]
file = "target/test-nicks-grant.json"
[[channels]]
name = "#rf"
rf = true
[[opers]]
name = "root"
password = "operpass1"
"##;
    let mut h = Harness::from_toml(toml).await;
    h.send("JOIN #rf");
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])),
    )
    .await;
    h.transmitted().await;
    h.drain_client();

    let mut oper_rx = h.connect_extra(9, "rootop");
    h.server.handle(Event::Line {
        id: 9,
        line: "OPER root operpass1".into(),
    });
    let _ = drain_rx(&mut oper_rx);
    h.server.handle(Event::Line {
        id: 9,
        line: "RADIO GRANT alice".into(),
    });
    let grant_fail = drain_rx(&mut oper_rx);
    assert!(
        grant_fail.iter().any(|l| l.contains("not registered")),
        "{grant_fail:?}"
    );

    h.send("REGISTER secret12");
    h.drain_client();
    h.server.handle(Event::Line {
        id: 9,
        line: "RADIO GRANT alice".into(),
    });
    let grant_ok = drain_rx(&mut oper_rx);
    assert!(
        grant_ok
            .iter()
            .any(|l| l.contains("stored in the nick file")),
        "{grant_ok:?}"
    );

    h.send("CALLSIGN SM0XYZ");
    h.drain_client();
    h.send("PRIVMSG #rf :after grant");
    h.drain_client();
    let tx = h.transmitted().await;
    assert!(
        tx.iter()
            .any(|(_, a)| a.kind == Kind::Msg
                && a.fields().last() == Some(&"after grant".to_string())),
        "{tx:?}"
    );
}

#[tokio::test]
async fn frames_addressed_elsewhere_are_ignored() {
    let mut h = Harness::new().await;
    h.drain_client();

    // A JOIN to AIRC is not channel chat; the gateway still ignores it.
    h.transmits_to(
        "SM0ABC-7",
        "AIRC",
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])),
    )
    .await;
    // Addressed to a different station's callsign entirely.
    h.transmits_to(
        "SM0XYZ-9",
        "SK0AA-1",
        AircFrame::new(Kind::Join, 2, encode_fields(&["#rf"])),
    )
    .await;

    let lines = h.drain_client();
    assert!(
        !lines.iter().any(|l| l.contains("JOIN")),
        "a frame not addressed to this gateway was acted on: {lines:?}"
    );
    assert!(
        h.transmitted().await.is_empty(),
        "the gateway answered traffic that was not addressed to it"
    );
}

#[tokio::test]
async fn a_second_gateways_downlink_does_not_start_a_loop() {
    let mut h = Harness::new().await;
    h.drain_client();

    // Exactly what another rfircd on the same frequency puts on the air:
    // a downlink MSG, [channel, from, text], addressed to AIRC. Read as an
    // uplink it would look like a message from SK0AA-1 to "#rf" saying
    // "bob" — which we would then relay and transmit, and so would they.
    h.transmits_to(
        "SK0AA-1",
        "AIRC",
        AircFrame::new(
            Kind::Msg,
            7,
            encode_fields(&["#rf", "bob", "hello from the other gateway"]),
        ),
    )
    .await;

    assert!(
        h.transmitted().await.is_empty(),
        "we answered another gateway's broadcast; two gateways would key each other forever"
    );
    let lines = h.drain_client();
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("hello from the other gateway")),
        "another gateway's downlink was relayed to IRC: {lines:?}"
    );
}

#[tokio::test]
async fn a_station_broadcast_reaches_irc_without_being_repeated() {
    let mut h = Harness::new().await;
    h.send("JOIN #rf");
    h.drain_client();
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])),
    )
    .await;
    let _ = h.transmitted().await;

    h.transmits_to(
        "SM0ABC-7",
        "AIRC",
        AircFrame::new(Kind::Msg, 4, encode_fields(&["#rf", "from the hillside"])),
    )
    .await;

    let lines = h.drain_client();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("PRIVMSG #rf :from the hillside")),
        "broadcast uplink must still reach IRC: {lines:?}"
    );
    let tx = h.transmitted().await;
    assert!(
        !tx.iter().any(|(_, a)| a.kind == Kind::Msg),
        "the gateway must not retransmit what every station already heard: {tx:?}"
    );
}

#[tokio::test]
async fn a_welcome_piggybacks_the_hello_ack() {
    let mut h = Harness::new().await;
    h.drain_client();
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Hello, 1, encode_fields(&[""])).with_flags(flags::ACK_REQ),
    )
    .await;
    let tx = h.transmitted().await;
    let welcome = tx
        .iter()
        .find(|(_, a)| a.kind == Kind::Welcome)
        .map(|(_, a)| a);
    assert!(welcome.is_some(), "HELLO still gets a WELCOME: {tx:?}");
    assert_eq!(
        welcome.unwrap().piggyback_seq,
        Some(1),
        "ACK+WELCOME as two key-ups burns the PA for nothing"
    );
    assert!(
        !tx.iter().any(|(_, a)| a.kind == Kind::Ack),
        "a standalone ACK next to WELCOME is the old two-frame handshake: {tx:?}"
    );
}

#[tokio::test]
async fn radio_off_identifies_and_purges_the_queue() {
    let mut h = Harness::new().await;
    h.oper_and_callsign();
    h.send("JOIN #rf");
    h.station_transmits("SM0ABC-7", AircFrame::new(Kind::Hello, 1, Vec::new()))
        .await;
    h.drain_client();
    let before = h.transmitted().await;
    assert!(
        !before.is_empty(),
        "the HELLO should have been answered on the air, so the station owes an ID"
    );

    h.send("RADIO OFF");
    let sent = h.transmitted().await;
    assert!(
        sent.iter().any(|(_, f)| f.kind == Kind::Id),
        "a station that has transmitted must identify before it goes quiet: {:?}",
        sent.iter().map(|(_, f)| f.kind).collect::<Vec<_>>()
    );

    // Nothing more may reach the air after that.
    h.send("PRIVMSG #rf :this must not be transmitted");
    assert!(
        h.transmitted().await.is_empty(),
        "the transmitter kept radiating after RADIO OFF"
    );
}

#[tokio::test]
async fn names_reply_to_rf_is_bounded() {
    let mut h = Harness::new().await;
    h.drain_client();
    // Fill the channel with IP users so an unbounded NAMES would be huge.
    for i in 0..60u64 {
        let mut rx = h.connect_extra(100 + i, &format!("op_{i:02}"));
        h.server.handle(Event::Line {
            id: 100 + i,
            line: "JOIN #rf".into(),
        });
        let _ = drain_rx(&mut rx);
    }
    h.station_transmits("SM0ABC-7", AircFrame::new(Kind::Hello, 1, Vec::new()))
        .await;
    h.ack(&AircFrame::new(Kind::Welcome, 0, Vec::new())).await;
    let _ = h.transmitted().await;

    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 2, encode_fields(&["#rf"])),
    )
    .await;
    let sent = h.transmitted().await;
    for (_, f) in &sent {
        if f.kind == Kind::NamesReply {
            assert!(
                f.payload.len() < 400,
                "a NAMES reply of {} octets is minutes of airtime at 300 baud",
                f.payload.len()
            );
        }
    }
}

/// A realistic QRP HF gateway: 300 baud, a 5 % duty limit, a 60 s backlog.
/// A busy channel must not be able to commit the transmitter to more than the
/// backlog holds, and the senders whose messages do not fit must be told.
#[tokio::test]
async fn a_busy_channel_fills_the_backlog_and_then_is_refused() {
    let toml = config_text(CONFIG)
        .replace("baud = 9600", "baud = 300")
        .replace("max_duty_percent = 50", "max_duty_percent = 5")
        .replace("max_continuous_secs = 60", "max_continuous_secs = 10")
        .replace("cooldown_secs = 60", "cooldown_secs = 300")
        // Raise the IRC-side flood cap out of the way: this test is about the
        // airtime layer underneath it.
        .replace("[policy]", &format!("[policy]{NO_FLOOD_CAP}"));
    let mut h = Harness::from_toml(&toml).await;
    h.oper_and_callsign();
    h.send("JOIN #rf");
    h.station_transmits("SM0ABC-7", AircFrame::new(Kind::Hello, 1, Vec::new()))
        .await;
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 2, encode_fields(&["#rf"])),
    )
    .await;
    assert!(
        h.server.state.channel("#rf").unwrap().has_rf_members(),
        "test setup: without a station on frequency nothing is bound for the air"
    );
    h.drain_client();
    let _ = h.transmitted().await;

    let body = "x".repeat(150);
    for _ in 0..20 {
        h.send(&format!("PRIVMSG #rf :{body}"));
    }
    let notices: Vec<String> = h.drain_client();

    let queued = notices
        .iter()
        .filter(|l| l.contains("Queued for RF"))
        .count();
    let refused = notices
        .iter()
        .filter(|l| l.contains("transmit queue is"))
        .count();
    assert!(queued > 0, "nothing was accepted at all: {notices:?}");
    assert!(
        refused > 0,
        "20 full-length messages at 300 baud is far more than a 60s backlog holds, \
         but none was refused: {notices:?}"
    );
    assert_eq!(
        queued + refused,
        20,
        "every message should have got one answer or the other"
    );
    assert!(
        h.server.radio.stats.rf_frames_refused as usize == refused,
        "the counter and the notices should agree"
    );

    // The backlog is bounded by airtime, and by the share a chat message may
    // occupy — half of the 60 s budget.
    let air = h.server.radio.airtime().unwrap();
    assert!(
        air.queued() <= Duration::from_secs(31),
        "the transmit queue holds {:?}, past the chat share of the 60s budget",
        air.queued()
    );
    assert!(
        air.duty_percent() > 0.0,
        "the governor recorded no airtime — is [radio.duty] reaching the TNC?"
    );

    // And the operator can see all of it.
    h.drain_client();
    h.send("RADIO QUEUE");
    let lines = h.drain_client();
    assert!(
        lines.iter().any(|l| l.contains("transmit queue")),
        "{lines:?}"
    );
}

/// The asymmetry, stated as a test: an IRC user sees their message instantly,
/// and is told honestly that the radio side is behind — with an estimate that
/// grows as the queue does.
#[tokio::test]
async fn senders_are_given_a_growing_and_honest_estimate() {
    let toml = config_text(CONFIG)
        .replace("baud = 9600", "baud = 300")
        .replace(
            "id_interval_secs = 60",
            "id_interval_secs = 60\nmax_queued_airtime_secs = 600",
        )
        .replace("[policy]", &format!("[policy]{NO_FLOOD_CAP}"));
    let mut h = Harness::from_toml(&toml).await;
    h.oper_and_callsign();
    h.send("JOIN #rf");
    h.station_transmits("SM0ABC-7", AircFrame::new(Kind::Hello, 1, Vec::new()))
        .await;
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 2, encode_fields(&["#rf"])),
    )
    .await;
    h.drain_client();
    let _ = h.transmitted().await;

    let body = "y".repeat(140);
    for _ in 0..8 {
        h.send(&format!("PRIVMSG #rf :{body}"));
    }
    let notices = h.drain_client();

    // Every message reached IRC immediately — that side is free.
    assert_eq!(
        notices
            .iter()
            .filter(|l| l.contains("PRIVMSG #rf") && l.contains(&body))
            .count(),
        0,
        "the sender does not get their own message echoed back"
    );
    assert_eq!(
        h.server.radio.stats.rf_frames_refused, 0,
        "a 600s backlog should admit all eight"
    );

    // The estimate they were quoted grows with the queue rather than
    // claiming each one is going out now.
    let etas: Vec<u64> = notices
        .iter()
        .filter_map(|l| l.split("about ").nth(1))
        .filter_map(|rest| rest.split('s').next())
        .filter_map(|n| n.parse().ok())
        .collect();
    assert!(etas.len() >= 4, "expected queue estimates: {notices:?}");
    assert!(
        etas.windows(2).all(|w| w[1] >= w[0]),
        "the estimate must not go backwards as the queue grows: {etas:?}"
    );
    assert!(
        etas.last().unwrap() > etas.first().unwrap(),
        "eight full-length messages at 300 baud is a real queue: {etas:?}"
    );
    assert!(
        h.server.radio.airtime().unwrap().queued() > Duration::from_secs(1),
        "the queued airtime should reflect what was accepted"
    );
}

#[tokio::test]
async fn config_rejects_a_duty_budget_that_can_never_pass_a_frame() {
    // 1 % of 60 s is 600 ms; a 128-octet frame at 300 baud is several
    // seconds. Nothing would ever be transmitted, and the operator would be
    // left wondering why. Catch it at startup instead.
    let toml = config_text(CONFIG)
        .replace("baud = 9600", "baud = 300")
        .replace("window_secs = 600", "window_secs = 60")
        .replace("max_duty_percent = 50", "max_duty_percent = 1");
    let err = Config::from_toml(&toml).unwrap_err().to_string();
    assert!(err.contains("duty"), "{err}");
}

#[tokio::test]
async fn member_lists_are_sent_only_when_asked_and_are_capped() {
    let mut h = Harness::new().await;
    h.drain_client();
    for i in 0..40u64 {
        let mut rx = h.connect_extra(200 + i, &format!("ham_{i:02}"));
        h.server.handle(Event::Line {
            id: 200 + i,
            line: "JOIN #rf".into(),
        });
        let _ = drain_rx(&mut rx);
    }

    assert!(
        h.server.state.channel("#rf").unwrap().members.len() > 30,
        "test setup: only {} members in #rf",
        h.server.state.channel("#rf").unwrap().members.len()
    );
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])),
    )
    .await;
    let tx = h.transmitted().await;
    for (_, f) in &tx {
        if f.kind == Kind::NamesReply {
            let fields = f.fields();
            assert!(
                !fields[1].contains("ham_"),
                "JOIN leaked the member list: {fields:?}"
            );
        }
    }

    // Now ask for it explicitly. It arrives, but capped.
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Names, 2, encode_fields(&["#rf"])),
    )
    .await;
    let tx = h.transmitted().await;
    let reply = tx
        .iter()
        .find(|(_, a)| a.kind == Kind::NamesReply)
        .expect("an explicit NAMES should be answered");
    let names = reply.1.fields()[1].clone();
    let listed = names.split(',').filter(|n| n.contains("ham_")).count();
    assert!(listed <= 8, "{listed} names is more than rf_names_max");
    assert!(
        names.contains("more"),
        "the truncation should be visible: {names}"
    );
    assert!(
        reply.1.payload.len() <= 200,
        "a NAMES reply of {} octets is too much airtime",
        reply.1.payload.len()
    );
}

#[tokio::test]
async fn held_mail_is_delivered_a_little_at_a_time() {
    let mut h = Harness::new().await;
    h.oper_and_callsign();
    h.drain_client();
    // Five messages held for a station that is not on frequency.
    for i in 0..5 {
        h.send(&format!("PRIVMSG SM0ABC|7 :held message {i}"));
    }
    h.drain_client();

    // The station appears. Its welcome goes out first and is acknowledged;
    // only then does held mail leave the per-station queue — and only one
    // message of it, not the whole mailbox.
    h.station_transmits("SM0ABC-7", AircFrame::new(Kind::Hello, 1, Vec::new()))
        .await;
    let welcome = h
        .transmitted()
        .await
        .into_iter()
        .find(|(_, a)| a.kind == Kind::Welcome)
        .expect("a station that says HELLO gets a welcome");
    h.ack(&welcome.1).await;

    let tx = h.transmitted().await;
    let stored = tx.iter().filter(|(_, a)| a.kind == Kind::Stored).count();
    assert_eq!(
        stored, 1,
        "held mail should drip, not flood: {stored} messages went out at once"
    );
    assert!(
        h.server.radio.mailbox.depth(&"SM0ABC-7".parse().unwrap()) > 0,
        "the rest should still be waiting, not discarded"
    );
}

#[tokio::test]
async fn a_full_backlog_is_refused_out_loud_not_dropped_silently() {
    // One second of queue: the second message has nowhere to go.
    let toml = config_text(CONFIG).replace(
        "id_interval_secs = 60",
        "id_interval_secs = 60\nmax_queued_airtime_secs = 1",
    );
    let mut h = Harness::from_toml(&toml).await;
    h.oper_and_callsign();
    h.send("JOIN #rf");
    h.station_transmits("SM0ABC-7", AircFrame::new(Kind::Hello, 1, Vec::new()))
        .await;
    h.station_transmits(
        "SM0ABC-7",
        AircFrame::new(Kind::Join, 2, encode_fields(&["#rf"])),
    )
    .await;
    h.drain_client();

    // Fire messages without letting the TNC task drain the queue.
    for i in 0..30 {
        h.send(&format!("PRIVMSG #rf :message number {i}"));
    }
    let lines = h.drain_client();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("transmit queue") || l.contains("Queued for RF")),
        "the sender was told nothing about what happened to their message: {lines:?}"
    );
}

#[tokio::test]
async fn the_safety_interlock_stops_everything_including_identification() {
    let mut h = Harness::new().await;
    h.oper_and_callsign();
    h.send("JOIN #rf");
    h.station_transmits("SM0ABC-7", AircFrame::new(Kind::Hello, 1, Vec::new()))
        .await;
    h.drain_client();
    let _ = h.transmitted().await;

    // An SWR check (or a temperature probe, or a tower interlock) has failed.
    h.server
        .radio
        .airtime()
        .unwrap()
        .interlock_ok
        .store(false, std::sync::atomic::Ordering::Relaxed);

    h.send("PRIVMSG #rf :is anyone there");
    h.send("RADIO ID");
    assert!(
        h.transmitted().await.is_empty(),
        "the interlock must stop identification too: if it is not safe to key up,          it is not safe to key up for an ID"
    );
    h.drain_client();
    h.send("RADIO STATUS");
    let lines = h.drain_client();
    assert!(
        lines.iter().any(|l| l.contains("interlock")),
        "the operator should be told why nothing is going out: {lines:?}"
    );

    // Interlock recovers; the sign-off ID that was held should go out on
    // the airtime clock. Do not RADIO ID again — that is now rate-limited.
    h.server
        .radio
        .airtime()
        .unwrap()
        .interlock_ok
        .store(true, std::sync::atomic::Ordering::Release);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let sent = h.transmitted().await;
    let id = sent
        .iter()
        .find(|(_, f)| f.kind == Kind::Id)
        .expect("RADIO ID should put an identification frame on the air");
    assert_eq!(
        id.0.destination.call.to_string(),
        "ID",
        "RADIO ID must use the identification address, not the protocol broadcast: {}",
        id.0.to_monitor_line()
    );
}

#[tokio::test]
async fn opers_can_see_the_unsent_queue_and_retune_the_limits() {
    let mut h = Harness::new().await;
    h.oper_and_callsign();
    h.drain_client();

    h.send("RADIO QUEUE");
    let lines = h.drain_client();
    assert!(
        lines.iter().any(|l| l.contains("transmit queue")),
        "RADIO QUEUE should report the transmit backlog: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("Held for stations out of range")),
        "RADIO QUEUE should account for held mail too: {lines:?}"
    );

    // Turn the duty cycle down mid-session.
    h.send("RADIO LIMIT DUTY 10");
    let lines = h.drain_client();
    assert!(lines.iter().any(|l| l.contains("10%")), "{lines:?}");
    assert_eq!(
        h.server.radio.airtime().unwrap().duty_limit(0.25),
        0.10,
        "the override should be in force"
    );

    // Asking for more than the ceiling gets the ceiling.
    h.send("RADIO LIMIT DUTY 90");
    h.drain_client();
    assert_eq!(h.server.radio.airtime().unwrap().duty_limit(0.25), 0.5);

    h.send("RADIO LIMIT DUTY off");
    h.drain_client();
    assert_eq!(h.server.radio.airtime().unwrap().duty_limit(0.25), 0.25);
}

#[tokio::test]
async fn a_non_oper_cannot_retune_the_transmitter() {
    let mut h = Harness::new().await;
    h.send("CALLSIGN SM0XYZ");
    h.drain_client();
    h.send("RADIO LIMIT DUTY 50");
    h.send("RADIO QUEUE");
    let lines = h.drain_client();
    assert!(
        lines.iter().filter(|l| l.contains(" 481 ")).count() >= 2,
        "both should need control-operator privilege: {lines:?}"
    );
}

#[tokio::test]
async fn user_created_channels_do_not_accumulate() {
    let mut h = Harness::new().await;
    h.drain_client();
    let before = h.server.state.channels.len();
    for i in 0..15 {
        h.send(&format!("JOIN #scratch{i}"));
    }
    assert!(h.server.state.channels.len() > before);
    h.server.handle(Event::Disconnected {
        id: 1,
        reason: "gone".into(),
    });
    assert_eq!(
        h.server.state.channels.len(),
        before,
        "channels created with JOIN must not outlive their last member"
    );
    assert!(
        h.server.state.channel("#rf").is_some() && h.server.state.channel("#local").is_some(),
        "configured channels must survive being empty"
    );
}

#[tokio::test]
async fn the_server_is_capped_in_total_not_just_per_host() {
    let toml = config_text(CONFIG).replace("bind = []", "bind = []\nmax_clients = 4");
    let mut h = Harness::from_toml(&toml).await;
    h.drain_client();
    // Each from a different host, so only the global cap can stop them.
    let mut accepted = 0;
    for i in 0..10u64 {
        let mut rx = h.connect_extra(50 + i, &format!("guest_{i}"));
        let lines = drain_rx(&mut rx);
        if !lines.iter().any(|l| l.contains("Server is full")) {
            accepted += 1;
        }
    }
    assert!(
        accepted < 10,
        "a distributed connection flood is under the per-host limit on every host"
    );
}

#[tokio::test]
async fn kiss_timing_parameters_are_pushed_to_the_tnc() {
    // The governor prices every frame using txdelay/txtail, so the modem had
    // better be using the same numbers. Check the bytes, not the intent.
    let toml = config_text(CONFIG)
        .replace("tx_pacing_ms = 0", "tx_pacing_ms = 0\nslottime = 10")
        .replace("txdelay_ms = 10", "txdelay_ms = 400")
        .replace("txtail_ms = 10", "txtail_ms = 250");
    let mut h = Harness::from_toml(&toml).await;

    // Read whatever the TNC task wrote at connect, before any traffic.
    let mut buf = [0u8; 512];
    let mut params: Vec<(u8, Vec<u8>)> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && params.len() < 4 {
        match tokio::time::timeout(Duration::from_millis(200), h.far.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                for kf in h.decoder.push(&buf[..n]) {
                    if kf.command != kiss::CMD_DATA {
                        params.push((kf.command, kf.payload));
                    }
                }
            }
            _ => break,
        }
    }

    let get = |cmd: u8| {
        params
            .iter()
            .find(|(c, _)| *c == cmd)
            .map(|(_, p)| p.clone())
    };
    // KISS carries these in 10 ms units.
    assert_eq!(
        get(kiss::CMD_TXDELAY),
        Some(vec![40]),
        "TXDELAY was not pushed: the modem and the airtime model would disagree. Got {params:?}"
    );
    assert_eq!(get(kiss::CMD_TXTAIL), Some(vec![25]), "{params:?}");
    assert_eq!(
        get(kiss::CMD_FULLDUPLEX),
        Some(vec![0]),
        "full duplex must be explicitly forced off: {params:?}"
    );
    assert_eq!(get(kiss::CMD_SLOTTIME), Some(vec![10]), "{params:?}");
}

#[tokio::test]
async fn a_config_using_the_old_txdelay_key_is_told_where_it_went() {
    let toml = config_text(CONFIG).replace("tx_pacing_ms = 0", "tx_pacing_ms = 0\ntxdelay = 30");
    let err = Config::from_toml(&toml).unwrap_err().to_string();
    assert!(
        err.contains("radio.duty.txdelay_ms") && err.contains("300"),
        "an operator upgrading needs to be told where the setting went and in \
         what units, not just that the key is unknown: {err}"
    );
}

#[tokio::test]
async fn the_station_client_shares_the_gateways_airtime_limits() {
    // The station is a human typing rather than an automatic service, but it
    // is the same QRP radio at the same baud rate, so the same check applies.
    use rfircd::ax25::AirtimeConfig;
    let bad = AirtimeConfig {
        max_continuous: Duration::from_secs(60),
        cooldown: Duration::from_secs(5),
        ..AirtimeConfig::default()
    };
    let err = bad.check_hardware_safe().unwrap_err();
    assert!(err.contains("burst duty cycle"), "{err}");
    assert!(
        AirtimeConfig::default().check_hardware_safe().is_ok(),
        "the shipped defaults must pass their own check"
    );
}
