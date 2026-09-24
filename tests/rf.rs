//! The radio side of the bridge: what the gateway does with each kind of frame
//! it hears, and what it refuses to do.
//!
//! These drive the `Server` directly with decoded AX.25 frames rather than
//! going through the TNC task, so a test can assert on one frame at a time
//! without waiting for pacing or airtime.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rfircd::airc::{encode_fields, AircFrame, Kind};
use rfircd::ax25::tnc::{self, TncConfig};
use rfircd::ax25::Ax25Frame;
use rfircd::callsign::Callsign;
use rfircd::config::Config;
use rfircd::server::state::{ClientId, UserId};
use rfircd::server::{Event, Server};
use tokio::sync::mpsc;

const CONFIG: &str = r##"
[server]
name = "rf.test"
motd = ["packet here"]

[listen]
bind = []

[radio]
enabled = true
callsign = "SK0MT-1"
destination = "AIRC"
id_interval_secs = 60
paclen = 128
presence_notices = true

[radio.tnc]
kind = "loopback"
# No enforced gap between transmissions: these tests read the loopback with
# short timeouts, and pacing is not what they are checking. At the 1500 ms
# default a frame that *was* transmitted looks like one that was not.
tx_pacing_ms = 0

[radio.duty]
enabled = true
baud = 9600
txdelay_ms = 10
txtail_ms = 10
max_duty_percent = 50

[policy]
rf_msgs_per_min = 600
rf_burst = 100
rf_channel_msgs_per_min = 600
rf_channel_burst = 100
ip_cmds_per_min = 6000
ip_cmd_burst = 500

[accounts]
file = "target/test-rf-nicks.json"

[[channels]]
name = "#rf"
topic = "bridged"
rf = true

[[channels]]
name = "#local"

[[opers]]
name = "root"
password = "operpass1"
"##;

struct Rf {
    server: Server,
    rx: Vec<(ClientId, mpsc::Receiver<String>)>,
    seq: u16,
    /// The loopback TNC's far end. Never read, but it has to stay alive:
    /// dropping it closes the fake radio and the gateway decides it has no
    /// transmitter — which quietly turns off the mailbox and every other
    /// path that asks "can we radiate?".
    _far: tokio::io::DuplexStream,
    _rf_rx: mpsc::Receiver<Ax25Frame>,
    decoder: rfircd::ax25::kiss::KissDecoder,
}

impl Rf {
    fn new() -> Self {
        Self::with(CONFIG)
    }

    fn with(text: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = format!(
            "target/test-rf-nicks-{}-{}.json",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let text = text.replace("target/test-rf-nicks.json", &path);
        let config = Arc::new(Config::from_toml(&text).unwrap());
        let (link, far) = TncConfig::loopback_link();
        let (handle, rf_rx) = tnc::spawn(TncConfig::from_config(&config, link));
        Rf {
            server: Server::new(config, Some(handle)).unwrap(),
            rx: Vec::new(),
            seq: 1,
            _far: far,
            _rf_rx: rf_rx,
            decoder: rfircd::ax25::kiss::KissDecoder::new(4096),
        }
    }

    fn client(&mut self, id: ClientId, nick: &str) -> ClientId {
        let (out, rx) = mpsc::channel(4096);
        self.server.handle(Event::Connected {
            id,
            host: format!("10.1.{}.{}", id / 256, id % 256),
            listen_only: false,
            out,
            hangup: None,
        });
        self.rx.push((id, rx));
        self.send(id, &format!("NICK {nick}"));
        self.send(id, &format!("USER {nick} 0 * :{nick}"));
        self.drain(id);
        id
    }

    fn send(&mut self, id: ClientId, line: &str) {
        self.server.handle(Event::Line {
            id,
            line: line.to_string(),
        });
    }

    fn drain(&mut self, id: ClientId) -> Vec<String> {
        let mut out = Vec::new();
        for (cid, rx) in self.rx.iter_mut() {
            if *cid == id {
                while let Ok(line) = rx.try_recv() {
                    out.push(line);
                }
            }
        }
        out
    }

    /// A station transmits an AIRC frame addressed to the gateway.
    fn heard(&mut self, from: &str, kind: Kind, fields: &[&str]) {
        self.heard_to(from, "SK0MT-1", kind, fields)
    }

    fn heard_to(&mut self, from: &str, to: &str, kind: Kind, fields: &[&str]) {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1).max(1);
        let airc = AircFrame::new(kind, seq, encode_fields(fields));
        let ax = Ax25Frame::ui(
            from.parse().unwrap(),
            to.parse().unwrap(),
            &[],
            airc.encode(),
        )
        .unwrap();
        self.server.handle(Event::Rf(ax));
    }

    /// A raw frame, for the cases that are not well-formed AIRC.
    fn heard_raw(&mut self, frame: Ax25Frame) {
        self.server.handle(Event::Rf(frame));
    }

    /// Everything the gateway has put on the air since the last call.
    async fn transmitted_raw(&mut self) -> Vec<Ax25Frame> {
        use tokio::io::AsyncReadExt;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(Duration::from_millis(150), self._far.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    for kf in self.decoder.push(&buf[..n]) {
                        if kf.command != rfircd::ax25::kiss::CMD_DATA {
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

    async fn transmitted(&mut self) -> Vec<AircFrame> {
        self.transmitted_raw()
            .await
            .into_iter()
            .filter_map(|ax| AircFrame::decode(&ax.info).ok())
            .collect()
    }

    fn heard_aprs(&mut self, from: &str, dest: &str, info: &[u8]) {
        let ax = Ax25Frame::ui(
            from.parse().unwrap(),
            dest.parse().unwrap(),
            &[],
            info.to_vec(),
        )
        .unwrap();
        self.heard_raw(ax);
    }

    fn station(&self, call: &str) -> Option<UserId> {
        let c: Callsign = call.parse().unwrap();
        let uid = UserId::Rf(c);
        self.server.state.user(&uid).map(|u| u.id.clone())
    }
}

// ------------------------------------------------------------ what gets ignored

#[tokio::test]
async fn frames_that_are_not_ours_are_ignored() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    // Not a UI frame.
    let mut not_ui = Ax25Frame::ui(
        "SM0ABC-7".parse().unwrap(),
        "SK0MT-1".parse().unwrap(),
        &[],
        AircFrame::new(Kind::Join, 1, encode_fields(&["#rf"])).encode(),
    )
    .unwrap();
    not_ui.control = 0x00; // an I frame
    rf.heard_raw(not_ui);
    assert!(
        rf.station("SM0ABC-7").is_none(),
        "non-UI frames are not AIRC"
    );

    // Right control field, wrong PID.
    let mut wrong_pid = Ax25Frame::ui(
        "SM0ABC-7".parse().unwrap(),
        "SK0MT-1".parse().unwrap(),
        &[],
        AircFrame::new(Kind::Join, 2, encode_fields(&["#rf"])).encode(),
    )
    .unwrap();
    wrong_pid.pid = Some(0xCF); // NET/ROM
    rf.heard_raw(wrong_pid);
    assert!(rf.station("SM0ABC-7").is_none());

    // Our own transmission coming back through a digipeater.
    rf.heard("SK0MT-1", Kind::Join, &["#rf"]);
    assert!(
        rf.station("SK0MT-1").is_none(),
        "we do not answer ourselves"
    );

    // Addressed to someone else.
    rf.heard_to("SM0ABC-7", "SK0AA-9", Kind::Join, &["#rf"]);
    assert!(rf.station("SM0ABC-7").is_none());

    assert!(rf.drain(a).is_empty(), "none of that should reach IRC");
}

#[tokio::test]
async fn an_implausible_callsign_is_ignored() {
    let mut rf = Rf::new();
    // "NOCALL" has no digit, so it is not a callsign anyone was issued.
    rf.heard("NOCALL", Kind::Hello, &[]);
    assert!(rf.station("NOCALL").is_none());
}

#[tokio::test]
async fn a_denied_station_gets_nothing() {
    let text = CONFIG.replace("[policy]", "[policy]\ndeny_callsigns = [\"SM0BAD\"]");
    let mut rf = Rf::with(&text);
    rf.heard("SM0BAD-7", Kind::Hello, &[]);
    assert!(
        rf.station("SM0BAD-7").is_none(),
        "the deny list covers every SSID"
    );

    rf.heard("SM0ABC-7", Kind::Hello, &[]);
    assert!(rf.station("SM0ABC-7").is_some());
}

#[tokio::test]
async fn an_allow_list_excludes_everyone_else() {
    let text = CONFIG.replace("[policy]", "[policy]\nallow_callsigns = [\"SM0ABC\"]");
    let mut rf = Rf::with(&text);
    rf.heard("SM0XYZ-1", Kind::Hello, &[]);
    assert!(rf.station("SM0XYZ-1").is_none());
    rf.heard("SM0ABC-3", Kind::Hello, &[]);
    assert!(rf.station("SM0ABC-3").is_some());
}

// ------------------------------------------------------------- the frame kinds

#[tokio::test]
async fn hello_registers_a_station() {
    let mut rf = Rf::new();
    rf.heard("SM0ABC-7", Kind::Hello, &["rfirc-station/1"]);
    assert!(rf.station("SM0ABC-7").is_some());
    let u = rf
        .server
        .state
        .by_nick("SM0ABC|7")
        .expect("nick from callsign");
    assert_eq!(u.username, "rf");
    assert!(u.registered);
    assert_eq!(
        u.callsign.as_ref().map(|c| c.to_string()),
        Some("SM0ABC-7".into())
    );
}

#[tokio::test]
async fn join_part_and_quit_are_visible_on_irc() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    let lines = rf.drain(a);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("SM0ABC|7") && l.contains("JOIN #rf")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("+v")),
        "a callsign is voiced on a bridged channel: {lines:?}"
    );

    rf.heard("SM0ABC-7", Kind::Part, &["#rf", "going qrt"]);
    let lines = rf.drain(a);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("PART #rf") && l.contains("going qrt")),
        "{lines:?}"
    );

    // Re-join, then quit outright.
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);
    rf.heard("SM0ABC-7", Kind::Quit, &["73 all"]);
    let lines = rf.drain(a);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("QUIT") && l.contains("73 all")),
        "{lines:?}"
    );
    assert!(rf.station("SM0ABC-7").is_none());
}

#[tokio::test]
async fn a_quit_with_no_reason_still_reads_sensibly() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);

    rf.heard("SM0ABC-7", Kind::Quit, &[""]);
    assert!(
        rf.drain(a).iter().any(|l| l.contains("Signed off")),
        "an empty reason gets a default rather than an empty QUIT"
    );
}

#[tokio::test]
async fn parting_a_channel_the_station_is_not_in_is_harmless() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Hello, &[]);
    rf.drain(a);

    rf.heard("SM0ABC-7", Kind::Part, &["#rf"]);
    rf.heard("SM0ABC-7", Kind::Part, &["#nowhere"]);
    assert!(rf.station("SM0ABC-7").is_some(), "still on frequency");
}

#[tokio::test]
async fn joining_a_channel_that_is_not_bridged_is_refused() {
    let mut rf = Rf::new();
    rf.heard("SM0ABC-7", Kind::Hello, &[]);
    rf.heard("SM0ABC-7", Kind::Join, &["#local"]);
    let uid = rf.station("SM0ABC-7").unwrap();
    assert!(
        rf.server.state.user(&uid).unwrap().channels.is_empty(),
        "an Internet-only channel is not the station's to join"
    );

    rf.heard("SM0ABC-7", Kind::Join, &["#nosuch"]);
    rf.heard("SM0ABC-7", Kind::Join, &["notachannel"]);
    assert!(rf.server.state.user(&uid).unwrap().channels.is_empty());
}

#[tokio::test]
async fn names_and_ping_need_a_registered_station() {
    let mut rf = Rf::new();
    // Neither should register the station as a side effect.
    rf.heard("SM0ABC-7", Kind::Names, &["#rf"]);
    rf.heard("SM0ABC-7", Kind::Ping, &["token"]);
    assert!(rf.station("SM0ABC-7").is_none());

    rf.heard("SM0ABC-7", Kind::Hello, &[]);
    rf.heard("SM0ABC-7", Kind::Names, &["#rf"]);
    rf.heard("SM0ABC-7", Kind::Ping, &["a-very-long-token-indeed"]);
    assert!(rf.station("SM0ABC-7").is_some());
}

#[tokio::test]
async fn identification_and_replies_from_other_gateways_are_only_logged() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    // ID is informational. The rest are downlink kinds we never act on.
    for kind in [
        Kind::Id,
        Kind::Ack,
        Kind::Welcome,
        Kind::NamesReply,
        Kind::Pong,
        Kind::Presence,
        Kind::Stored,
        Kind::Error,
    ] {
        rf.heard("SM0ABC-7", kind, &["something"]);
    }
    assert!(
        rf.drain(a).is_empty(),
        "none of those should produce IRC traffic"
    );
}

// -------------------------------------------------------------------- messaging

#[tokio::test]
async fn a_channel_message_from_rf_reaches_irc() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);

    rf.heard("SM0ABC-7", Kind::Msg, &["#rf", "good morning"]);
    let lines = rf.drain(a);
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with(":SM0ABC|7!rf@") && l.contains("PRIVMSG #rf :good morning")),
        "{lines:?}"
    );

    rf.heard("SM0ABC-7", Kind::Notice, &["#rf", "a notice"]);
    assert!(rf
        .drain(a)
        .iter()
        .any(|l| l.contains("NOTICE #rf :a notice")));
}

#[tokio::test]
async fn a_broadcast_channel_message_from_rf_reaches_irc() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    rf.heard_to("SM0ABC-7", "AIRC", Kind::Msg, &["#rf", "from the hillside"]);
    let lines = rf.drain(a);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("PRIVMSG #rf :from the hillside")),
        "AIRC-destined uplink is how stations talk to each other: {lines:?}"
    );

    rf.heard_to(
        "SK0AA-1",
        "AIRC",
        Kind::Msg,
        &["#rf", "bob", "from the other gateway"],
    );
    assert!(
        !rf.drain(a)
            .iter()
            .any(|l| l.contains("from the other gateway")),
        "a 3-field broadcast is downlink, not uplink"
    );
}

#[tokio::test]
async fn irc_notice_and_ctcp_are_not_put_on_the_air() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);
    let _ = rf.transmitted().await;

    rf.send(a, "NOTICE #rf :radio status chatter");
    let sent = rf.transmitted().await;
    assert!(
        sent.iter().all(|f| !f
            .fields()
            .iter()
            .any(|x| x.contains("radio status chatter"))),
        "NOTICE must stay on IRC: {sent:?}"
    );

    rf.send(a, "PRIVMSG #rf :\u{1}VERSION\u{1}");
    let sent = rf.transmitted().await;
    assert!(
        sent.iter()
            .all(|f| !f.fields().iter().any(|x| x.contains("VERSION"))),
        "CTCP VERSION must not key the transmitter: {sent:?}"
    );

    rf.send(a, "PRIVMSG #rf :\u{1}ACTION waves\u{1}");
    let sent = rf.transmitted().await;
    assert!(
        sent.iter()
            .any(|f| f.fields().iter().any(|x| x.contains("/me waves"))),
        "/me is a message, not a control query: {sent:?}"
    );
}

#[tokio::test]
async fn a_long_action_is_truncated_on_the_air() {
    let text = CONFIG.replace("[policy]", "[policy]\nmax_rf_text_len = 20");
    let mut rf = Rf::with(&text);
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);
    let _ = rf.transmitted().await;

    let rest = "x".repeat(40);
    rf.send(a, &format!("PRIVMSG #rf :\u{1}ACTION {rest}\u{1}"));
    let sent = rf.transmitted().await;
    let bodies: Vec<String> = sent.iter().flat_map(|f| f.fields()).collect();
    assert!(
        bodies
            .iter()
            .any(|b| b.contains("/me ") && b.contains('\u{2026}')),
        "the radiated /me must be the shortened body: {sent:?}"
    );
    assert!(
        bodies.iter().all(|b| !b.contains(&rest)),
        "the unshortened ACTION must not go out: {sent:?}"
    );
}

#[tokio::test]
async fn rf_channel_flood_still_reaches_other_irc_users() {
    let text = CONFIG
        .replace(
            "rf_channel_msgs_per_min = 600",
            "rf_channel_msgs_per_min = 1",
        )
        .replace("rf_channel_burst = 100", "rf_channel_burst = 1");
    let mut rf = Rf::with(&text);
    let a = rf.client(1, "alice");
    let b = rf.client(2, "bob");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.send(a, "JOIN #rf");
    rf.send(b, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);
    rf.drain(b);
    let _ = rf.transmitted().await;

    rf.send(a, "PRIVMSG #rf :first");
    rf.drain(a);
    assert!(
        rf.drain(b).iter().any(|l| l.contains("first")),
        "the first message is delivered"
    );
    let _ = rf.transmitted().await;

    rf.send(a, "PRIVMSG #rf :second");
    let alice = rf.drain(a);
    assert!(
        alice.iter().any(|l| l.contains("Flood protection")),
        "the sender is told: {alice:?}"
    );
    let bob = rf.drain(b);
    assert!(
        bob.iter().any(|l| l.contains("second")),
        "flood protection is for the air, not for IRC: {bob:?}"
    );
    assert!(
        alice.iter().all(|l| !l.contains("Queued for RF")
            && !l.contains("stay on IRC")
            && !l.contains("Messages from users with RF-TX")),
        "flood must not also send the generic air-status notice: {alice:?}"
    );
}

#[tokio::test]
async fn a_message_from_a_station_that_never_joined_still_arrives() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    // A lost JOIN must not silently swallow a QSO.
    rf.heard("SM0XYZ-9", Kind::Msg, &["#rf", "anyone about?"]);
    let lines = rf.drain(a);
    assert!(
        lines.iter().any(|l| l.contains("JOIN #rf")),
        "joined implicitly: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("anyone about?")),
        "{lines:?}"
    );
}

#[tokio::test]
async fn messages_to_channels_the_station_may_not_use_are_refused() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #local");
    rf.heard("SM0ABC-7", Kind::Hello, &[]);
    rf.drain(a);

    rf.heard("SM0ABC-7", Kind::Msg, &["#local", "not allowed here"]);
    rf.heard("SM0ABC-7", Kind::Msg, &["#nosuch", "nowhere"]);
    assert!(
        rf.drain(a).iter().all(|l| !l.contains("not allowed here")),
        "an Internet-only channel does not carry RF traffic"
    );
}

#[tokio::test]
async fn a_private_message_from_rf_reaches_one_irc_user() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    let b = rf.client(2, "bob");
    rf.heard("SM0ABC-7", Kind::Hello, &[]);
    rf.drain(a);
    rf.drain(b);

    rf.heard("SM0ABC-7", Kind::Msg, &["alice", "meet me on 145.500"]);
    assert!(rf.drain(a).iter().any(|l| l.contains("meet me on 145.500")));
    assert!(rf.drain(b).is_empty(), "not a broadcast");

    // To a nick that does not exist.
    rf.heard("SM0ABC-7", Kind::Msg, &["nobody", "hello?"]);
    assert!(rf.drain(a).is_empty());
}

#[tokio::test]
async fn an_empty_message_is_dropped() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);

    // Only control characters: nothing survives sanitising.
    rf.heard("SM0ABC-7", Kind::Msg, &["#rf", "\u{2}\u{f}"]);
    assert!(rf.drain(a).is_empty());
    // A malformed MSG with no text field at all.
    rf.heard("SM0ABC-7", Kind::Msg, &["#rf"]);
    assert!(rf.drain(a).is_empty());
}

#[tokio::test]
async fn a_station_that_floods_is_dropped_not_answered() {
    let text = CONFIG
        .replace("rf_msgs_per_min = 600", "rf_msgs_per_min = 6")
        .replace("rf_burst = 100", "rf_burst = 3");
    let mut rf = Rf::with(&text);
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);

    for i in 0..20 {
        rf.heard("SM0ABC-7", Kind::Msg, &["#rf", &format!("flood {i}")]);
    }
    let delivered = rf.drain(a).iter().filter(|l| l.contains("flood")).count();
    assert!(
        delivered < 20,
        "the token bucket should have dropped most of that: {delivered} got through"
    );
    let call: Callsign = "SM0ABC-7".parse().unwrap();
    assert!(
        rf.server.radio.sessions.peer(&call).unwrap().dropped > 0,
        "drops should be counted so RADIO HEARD can show them"
    );
}

#[tokio::test]
async fn a_quit_reason_from_the_air_cannot_inject_an_irc_line() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);

    // encode_fields strips CR and LF, and the IRC serialiser scrubs them
    // again. Belt and braces, because this is a protocol-injection path into
    // every client in the channel.
    let airc = AircFrame::new(Kind::Quit, 99, b"bye\r\nNOTICE alice :pwned".to_vec());
    let ax = Ax25Frame::ui(
        "SM0ABC-7".parse().unwrap(),
        "SK0MT-1".parse().unwrap(),
        &[],
        airc.encode(),
    )
    .unwrap();
    rf.heard_raw(ax);

    for line in rf.drain(a) {
        assert!(!line.contains('\r') && !line.contains('\n'), "{line:?}");
        assert!(
            !line.starts_with(":rf.test NOTICE alice :pwned"),
            "injected a server notice: {line:?}"
        );
    }
}

// ----------------------------------------------------------------- housekeeping

#[tokio::test]
async fn a_station_that_goes_quiet_is_dropped() {
    let text = CONFIG.replace(
        "id_interval_secs = 60",
        "id_interval_secs = 60\npeer_idle_timeout_secs = 1",
    );
    let mut rf = Rf::with(&text);
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);
    assert!(rf.station("SM0ABC-7").is_some());

    std::thread::sleep(Duration::from_millis(1100));
    rf.server.handle(Event::Tick);
    assert!(
        rf.station("SM0ABC-7").is_none(),
        "a station we have not heard from is not on frequency"
    );
    assert!(rf.drain(a).iter().any(|l| l.contains("Signal lost")));
}

#[tokio::test]
async fn an_operator_can_remove_a_station() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);

    rf.send(a, "RADIO KICK SM0ABC-7");
    assert!(rf.drain(a).iter().any(|l| l.contains("Station removed")));
    assert!(rf.station("SM0ABC-7").is_none());

    rf.heard("SM0ABC-7", Kind::Msg, &["#rf", "back already"]);
    assert!(
        rf.station("SM0ABC-7").is_none(),
        "RADIO KICK must not be undone by the next PRIVMSG"
    );
    assert!(!rf
        .server
        .state
        .channel("#rf")
        .unwrap()
        .members
        .contains_key(&UserId::Rf("SM0ABC-7".parse().unwrap())));

    // HELLO is how they come back.
    rf.heard("SM0ABC-7", Kind::Hello, &[]);
    assert!(rf.station("SM0ABC-7").is_some());

    // And RADIO HEARD reports what is on frequency.
    rf.heard("SM0XYZ-1", Kind::Hello, &[]);
    rf.drain(a);
    rf.send(a, "RADIO HEARD");
    assert!(rf.drain(a).iter().any(|l| l.contains("SM0XYZ-1")));
}

#[tokio::test]
async fn an_operator_can_kick_a_station_from_one_channel() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);

    rf.send(a, "KICK #rf SM0ABC|7 :qrm");
    assert!(rf.drain(a).iter().any(|l| l.contains("KICK #rf SM0ABC|7")));
    assert!(
        rf.station("SM0ABC-7").is_some(),
        "kicked from the channel, still on frequency"
    );
    assert!(!rf
        .server
        .state
        .channel("#rf")
        .unwrap()
        .members
        .contains_key(&UserId::Rf("SM0ABC-7".parse().unwrap())));

    // A following channel message must not silently undo the kick.
    rf.heard("SM0ABC-7", Kind::Msg, &["#rf", "still here"]);
    assert!(
        !rf.server
            .state
            .channel("#rf")
            .unwrap()
            .members
            .contains_key(&UserId::Rf("SM0ABC-7".parse().unwrap())),
        "KICK must stick until the station sends JOIN"
    );
}

#[tokio::test]
async fn whois_on_a_station_reports_what_the_radio_side_knows() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.heard("SM0ABC-7", Kind::Hello, &[]);
    rf.drain(a);

    let lines = {
        rf.send(a, "WHOIS SM0ABC|7");
        rf.drain(a)
    };
    assert!(
        lines.iter().any(|l| l.contains("Radio station SM0ABC-7")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("last heard")),
        "idle time and queue depth belong in WHOIS for a station: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains(" 317 ") && l.contains("SM0ABC|7")),
        "317 must name the IRC nick, not the AX.25 call: {lines:?}"
    );
}

#[tokio::test]
async fn who_on_an_rf_channel_does_not_mark_stations_as_ops() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);

    let lines = {
        rf.send(a, "WHO #rf");
        rf.drain(a)
    };
    let station = lines
        .iter()
        .find(|l| l.contains("SM0ABC|7") && l.contains(" 352 "))
        .unwrap_or_else(|| panic!("WHO should list the station: {lines:?}"));
    assert!(
        station.split_whitespace().any(|f| f == "H+"),
        "a station is voiced on +r, not a channel operator: {station}"
    );
    assert!(
        !station.split_whitespace().any(|f| f == "H@"),
        "WHO @ means chanop: {station}"
    );
}

#[tokio::test]
async fn the_mailbox_holds_and_reports_messages_for_absent_stations() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.drain(a);

    rf.send(a, "PRIVMSG SM0ABC|7 :call me when you are back");
    assert!(rf.drain(a).iter().any(|l| l.contains("Held for delivery")));
    assert_eq!(rf.server.radio.mailbox.len(), 1);

    rf.send(a, "RADIO MAIL");
    assert!(rf.drain(a).iter().any(|l| l.contains("SM0ABC-7")));

    // Held messages expire.
    let much_later = Instant::now() + Duration::from_secs(48 * 3600);
    assert_eq!(rf.server.radio.mailbox.expire(much_later), 1);
    assert!(rf.server.radio.mailbox.is_empty());
}

#[tokio::test]
async fn a_full_mailbox_says_so_rather_than_silently_dropping() {
    // Per-station 1, gateway 2: enough room to reach each limit separately.
    // `store` checks the gateway total first, so a total of 1 would mask the
    // per-station message entirely.
    let text = CONFIG.replace(
        "paclen = 128",
        "paclen = 128\nmailbox_per_station = 1\nmailbox_total = 2",
    );
    let mut rf = Rf::with(&text);
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.drain(a);

    rf.send(a, "PRIVMSG SM0ABC|7 :first");
    rf.drain(a);
    rf.send(a, "PRIVMSG SM0ABC|7 :second");
    assert!(rf
        .drain(a)
        .iter()
        .any(|l| l.contains("as much mail as it can hold")));

    // A second station fills the gateway, and a third is refused for that.
    rf.send(a, "PRIVMSG SM0DEF|2 :for someone else");
    rf.drain(a);
    rf.send(a, "PRIVMSG SM0GHI|4 :no room left");
    assert!(rf
        .drain(a)
        .iter()
        .any(|l| l.contains("gateway mailbox is full")));
}

#[tokio::test]
async fn a_station_appearing_gets_its_held_mail_a_little_at_a_time() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.drain(a);
    for i in 0..4 {
        rf.send(a, &format!("PRIVMSG SM0ABC|7 :held {i}"));
    }
    rf.drain(a);
    assert_eq!(rf.server.radio.mailbox.len(), 4);

    rf.heard("SM0ABC-7", Kind::Hello, &[]);
    assert_eq!(
        rf.server.radio.mailbox.len(),
        3,
        "one message per exchange, not the whole mailbox at once"
    );
}

#[tokio::test]
async fn presence_notices_reach_the_air_only_when_enabled() {
    // The fixture turns them on; check the channel sees joins either way and
    // that the setting is what decides whether they are radiated.
    let mut rf = Rf::new();
    assert!(rf.server.config.radio.presence_notices);
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);

    let b = rf.client(2, "bob");
    rf.send(b, "JOIN #rf");
    let lines = rf.drain(a);
    assert!(lines
        .iter()
        .any(|l| l.contains("bob") && l.contains("JOIN #rf")));
}

#[tokio::test]
async fn the_last_station_leaving_is_announced_to_the_irc_side() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    let lines = rf.drain(a);
    assert!(
        lines.iter().any(|l| l.contains("is on frequency")),
        "IRC users should be told when the channel goes live: {lines:?}"
    );

    rf.heard("SM0ABC-7", Kind::Part, &["#rf"]);
    let lines = rf.drain(a);
    assert!(
        lines.iter().any(|l| l.contains("No RF station remains")),
        "and when it stops: {lines:?}"
    );
}

#[tokio::test]
async fn held_mail_is_not_destroyed_when_the_transmit_queue_is_full() {
    // A message held for hours must not be lost because the backlog happened
    // to be full at the moment the station reappeared. The mailbox is the
    // store-and-forward of last resort; if it silently drops, there is no
    // other copy anywhere.
    let text = CONFIG
        .replace(
            "id_interval_secs = 60",
            "id_interval_secs = 60\nmax_queued_airtime_secs = 1",
        )
        .replace("baud = 9600", "baud = 300");
    let mut rf = Rf::with(&text);
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.drain(a);

    rf.send(a, "PRIVMSG SM0ABC|7 :something worth keeping");
    rf.drain(a);
    assert_eq!(
        rf.server.radio.mailbox.len(),
        1,
        "test setup: the message was held"
    );

    // Fill the transmit backlog so nothing more can be admitted, then let the
    // station appear.
    for i in 0..40 {
        rf.server.radio.transmit_direct(
            &"SK0AA-1".parse().unwrap(),
            AircFrame::new(Kind::Msg, 900 + i, vec![0x41; 120]),
            rfircd::server::TxClass::Chat,
        );
    }
    assert!(
        !rf.server
            .radio
            .backlog_has_room(200, rfircd::server::TxClass::Direct),
        "test setup: the backlog should be full"
    );

    rf.heard("SM0ABC-7", Kind::Hello, &[]);

    assert_eq!(
        rf.server.radio.mailbox.len(),
        1,
        "the held message was taken out of the mailbox and then refused by \
         admission control, so it is gone: neither transmitted nor held"
    );
}

#[tokio::test]
async fn held_mail_is_not_destroyed_when_the_transmitter_is_off() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.drain(a);

    rf.send(a, "PRIVMSG SM0ABC|7 :something worth keeping");
    rf.drain(a);
    assert_eq!(
        rf.server.radio.mailbox.len(),
        1,
        "test setup: the message was held"
    );

    rf.server.radio.enabled = false;
    rf.heard("SM0ABC-7", Kind::Hello, &[]);

    assert_eq!(
        rf.server.radio.mailbox.len(),
        1,
        "a HELLO while the transmitter is OFF must not take the only copy \
         out of the mailbox"
    );
}

#[tokio::test]
async fn a_topic_change_reaches_the_air_once_however_many_stations_are_listening() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.send(a, "JOIN #rf");
    for call in ["SM0ABC-7", "SM0DEF-1", "SM0GHI-2"] {
        rf.heard(call, Kind::Join, &["#rf"]);
    }
    rf.drain(a);
    let _ = rf.transmitted().await;

    rf.send(a, "TOPIC #rf :net at 1900 local");
    let sent = rf.transmitted().await;
    let topics: Vec<_> = sent
        .iter()
        .filter(|f| f.fields().iter().any(|x| x.contains("net at 1900")))
        .collect();
    assert_eq!(
        topics.len(),
        1,
        "one transmission reaches every station in range; sending it per station \
         would be three times the airtime for the same information: {sent:?}"
    );

    // Setting it to the same value again is not a reason to key up.
    rf.drain(a);
    rf.send(a, "TOPIC #rf :net at 1900 local");
    let sent = rf.transmitted().await;
    assert!(
        !sent
            .iter()
            .any(|f| f.fields().iter().any(|x| x.contains("net at 1900"))),
        "an unchanged topic must not be retransmitted"
    );
}

#[tokio::test]
async fn irc_keeps_the_full_topic_when_rf_truncates() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);
    let _ = rf.transmitted().await;

    let long = "the net will meet on the hill at local sunset for traffic and then we all go quiet after the round table chat";
    assert!(long.len() > 64);
    rf.send(a, &format!("TOPIC #rf :{long}"));
    let irc = rf.drain(a);
    assert!(
        irc.iter().any(|l| l.contains(&long)),
        "the TOPIC event on IRC is the stored text, not the RF slice: {irc:?}"
    );
    assert_eq!(
        rf.server.state.channel("#rf").unwrap().topic.as_deref(),
        Some(long),
        "channel state must match what members were told"
    );
    let sent = rf.transmitted().await;
    let air: Vec<_> = sent.iter().filter(|f| f.kind == Kind::Notice).collect();
    assert!(
        air.iter().any(|f| {
            let joined = f.fields().join(" ");
            joined.contains("the net will meet") && !joined.contains(long)
        }),
        "RF still carries a shortened topic: {sent:?}"
    );
}

#[tokio::test]
async fn a_server_notice_to_a_station_is_short_and_not_retried() {
    use rfircd::airc::frame::flags;
    let mut rf = Rf::new();
    rf.heard("SM0ABC-7", Kind::Hello, &[]);
    let _ = rf.transmitted().await;

    // Provoke a notice by asking to speak in a channel that is not bridged.
    rf.heard("SM0ABC-7", Kind::Msg, &["#local", "can you hear me"]);
    let sent = rf.transmitted().await;
    for f in sent
        .iter()
        .filter(|f| f.kind == Kind::Notice || f.kind == Kind::Error)
    {
        assert_eq!(
            f.frag_total, 1,
            "a courtesy notice must fit one frame, not fragment across several"
        );
        assert!(
            f.flags & flags::ACK_REQ == 0,
            "an error or notice is not worth up to max_retries transmissions: {f:?}"
        );
    }
}

#[tokio::test]
async fn presence_notices_go_out_once_when_enabled() {
    let mut rf = Rf::new();
    assert!(
        rf.server.config.radio.presence_notices,
        "fixture enables them"
    );
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    rf.drain(a);
    let _ = rf.transmitted().await;

    let b = rf.client(2, "bob");
    rf.send(b, "JOIN #rf");
    let joined = rf.transmitted().await;
    assert_eq!(
        joined.iter().filter(|f| f.kind == Kind::Presence).count(),
        1,
        "one presence frame for one join: {joined:?}"
    );

    rf.send(b, "PART #rf");
    let parted = rf.transmitted().await;
    let leaving: Vec<_> = parted
        .iter()
        .filter(|f| f.kind == Kind::Presence)
        .filter(|f| f.fields().get(2).map(|s| s == "-").unwrap_or(false))
        .collect();
    assert_eq!(leaving.len(), 1, "and one for the part: {parted:?}");
}

#[tokio::test]
async fn the_message_limit_follows_the_fragment_budget_not_just_the_character_count() {
    // A small paclen with a one-frame budget must lower the effective text
    // limit, whatever `max_rf_text_len` says.
    let text = CONFIG.replace("paclen = 128", "paclen = 64").replace(
        "[policy]",
        "[policy]\nmax_rf_text_len = 250\nmax_rf_fragments = 1",
    );
    let rf = Rf::with(&text);
    let effective = rf.server.policy.config.max_rf_text_len;
    assert!(
        effective < 250,
        "one frame at paclen 64 cannot hold 250 characters, but the limit stayed at \
         {effective}"
    );
    assert!(effective > 0, "and it must not collapse to nothing");
}

// ---------------------------------------------------------------- APRS interop

#[tokio::test]
async fn an_aprs_message_to_the_gateway_reaches_irc() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);
    let _ = rf.transmitted_raw().await;

    // Stock radio: AX.25 dest is a TOCALL, addressee is in the payload.
    rf.heard_aprs(
        "SM0ABC-7",
        "APK004",
        b":SK0MT-1  :#rf hello from the trail{01",
    );
    let lines = rf.drain(a);
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with(":SM0ABC|7!rf@")
                && l.contains("PRIVMSG #rf :hello from the trail")),
        "APRS uplink should show in the channel: {lines:?}"
    );
    assert!(rf.station("SM0ABC-7").is_some());

    let frames = rf.transmitted_raw().await;
    let airc: Vec<_> = frames
        .iter()
        .filter(|f| AircFrame::decode(&f.info).is_ok())
        .map(|f| f.to_monitor_line())
        .collect();
    assert!(
        frames
            .iter()
            .any(|f| { f.destination.call.to_string() == "APRS" && f.info == b":SM0ABC-7 :ack01" }),
        "the radio must get an ACK or it will retry: {frames:?}"
    );
    assert!(
        airc.is_empty(),
        "no AIRC station is listening; do not CQ an AIRC copy: {airc:?}"
    );
}

#[tokio::test]
async fn aprs_retries_are_acked_but_not_re_injected() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    let info = b":SK0MT-1  :#rf once{7";
    rf.heard_aprs("SM0ABC-7", "APRS", info);
    rf.heard_aprs("SM0ABC-7", "APRS", info);
    let lines = rf.drain(a);
    let n: usize = lines
        .iter()
        .filter(|l| l.contains("PRIVMSG #rf") && l.contains("once"))
        .count();
    assert_eq!(n, 1, "a retry is an ACK lost, not a second line: {lines:?}");

    let acks: usize = rf
        .transmitted_raw()
        .await
        .iter()
        .filter(|f| f.info.windows(8).any(|w| w == b":ack7") || f.info.ends_with(b"ack7"))
        .count();
    assert!(acks >= 2, "both copies must be ACKed so the radio stops");
}

#[tokio::test]
async fn aprs_help_does_not_enter_the_channel() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    rf.heard_aprs("SM0ABC-7", "APRS", b":SK0MT-1  :HELP{2");
    assert!(
        !rf.drain(a).iter().any(|l| l.contains("PRIVMSG #rf")),
        "HELP is how-to, not a line of chat"
    );
    let frames = rf.transmitted_raw().await;
    assert!(frames.iter().any(|f| f.info == b":SM0ABC-7 :ack2"));
    assert!(frames.iter().any(|f| {
        let s = String::from_utf8_lossy(&f.info);
        s.contains("send #chan text")
    }));
}

#[tokio::test]
async fn aprs_default_channel_accepts_bare_text() {
    let text = CONFIG.replace(
        "presence_notices = true",
        "presence_notices = true\naprs_channel = \"#rf\"",
    );
    let mut rf = Rf::with(&text);
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    rf.heard_aprs("SM0ABC-7", "APRS", b":SK0MT    :bare text{3");
    assert!(
        rf.drain(a)
            .iter()
            .any(|l| l.contains("PRIVMSG #rf :bare text")),
        "SSID 0 addressee and no # prefix both have to work"
    );
}

#[tokio::test]
async fn aprs_to_someone_else_is_ignored() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);
    let _ = rf.transmitted_raw().await;

    rf.heard_aprs("SM0ABC-7", "APRS", b":N0CALL-9 :#rf secret{1");
    assert!(rf.drain(a).is_empty());
    assert!(rf.station("SM0ABC-7").is_none());
    let tx = rf.transmitted_raw().await;
    assert!(
        tx.is_empty(),
        "must not answer someone else's message: {tx:?}"
    );
}

#[tokio::test]
async fn aprs_disabled_is_silent() {
    let text = CONFIG.replace(
        "presence_notices = true",
        "presence_notices = true\naprs = false",
    );
    let mut rf = Rf::with(&text);
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    rf.heard_aprs("SM0ABC-7", "APK004", b":SK0MT-1  :#rf hello{01");
    rf.heard_aprs("SM0ABC-7", "APRS", b"!5930.00N/01803.00E-QTH");
    assert!(rf.drain(a).is_empty());
    assert!(rf.station("SM0ABC-7").is_none());
}

#[tokio::test]
async fn aprs_is_translated_for_airc_stations_already_in_the_channel() {
    let mut rf = Rf::new();
    rf.heard("SM0XYZ-9", Kind::Join, &["#rf"]);
    let _ = rf.transmitted().await;

    rf.heard_aprs("SM0ABC-7", "APRS", b":SK0MT-1  :#rf from a handheld{4");
    let airc = rf.transmitted().await;
    assert!(
        airc.iter().any(|f| {
            f.kind == Kind::Msg && {
                let fields = f.fields();
                fields.first().map(|s| s.as_str()) == Some("#rf")
                    && fields.last().map(|s| s.as_str()) == Some("from a handheld")
            }
        }),
        "AIRC stations did not hear the APRS frame as chat: {airc:?}"
    );
}

#[tokio::test]
async fn an_aprs_position_reaches_irc_and_stays_off_the_air() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);
    let _ = rf.transmitted_raw().await;

    rf.heard_aprs("SM0ABC-7", "APRS", b"!5930.00N/01803.00E-QTH Kista");
    let lines = rf.drain(a);
    assert!(
        lines.iter().any(|l| l.contains("NOTICE #rf")
            && l.contains("SM0ABC|7")
            && l.contains("59°30.00N")
            && l.contains("QTH Kista")),
        "position should be a channel NOTICE: {lines:?}"
    );
    assert!(rf.station("SM0ABC-7").is_none(), "a beacon is not a join");
    let tx = rf.transmitted_raw().await;
    assert!(tx.is_empty(), "beacons are never retransmitted: {tx:?}");
}

#[tokio::test]
async fn an_aprs_status_beacon_reaches_irc() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    rf.heard_aprs("SM0ABC-7", "APRS", b">Hello from the hill");
    assert!(
        rf.drain(a)
            .iter()
            .any(|l| l.contains("NOTICE #rf") && l.contains("Hello from the hill")),
        "status beacons are regular APRS beacons"
    );
}

#[tokio::test]
async fn a_digipeated_aprs_beacon_is_shown_once() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    let info = b"!5930.00N/01803.00E-once";
    rf.heard_aprs("SM0ABC-7", "APRS", info);
    rf.heard_aprs("SM0ABC-7", "WIDE1-1", info);
    let n: usize = rf
        .drain(a)
        .iter()
        .filter(|l| l.contains("NOTICE #rf") && l.contains("once"))
        .count();
    assert_eq!(n, 1, "the second copy is a digipeat, not a new beacon");
}

/// A callsign on the air is a claim, not an identity, so every limiter keyed
/// on one hands a fresh bucket to a sender that invents a new name per frame.
/// That bought a session-table slot each time — and the table is capped, so a
/// few hundred forged names locked every real station out for the idle
/// timeout. The global first-contact bucket is what bounds it.
///
/// It does not stop a station that transmits continuously from crowding out
/// new arrivals while it is transmitting: nothing at this layer can, that is
/// jamming. What it does is keep the damage proportional to the flood and end
/// it when the flood ends, instead of leaving a full table behind for half an
/// hour.
#[tokio::test]
async fn a_flood_of_forged_callsigns_cannot_fill_the_session_table() {
    let mut rf = Rf::new();
    // An established station, on the air before the flood starts.
    rf.heard("SM0ABC-7", Kind::Hello, &["SM0ABC-7"]);
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    assert!(rf.station("SM0ABC-7").is_some());

    // Plausible, distinct, and none of them real.
    for i in 0..400 {
        let call = format!("SM{}A{:02}", i % 10, i / 10);
        rf.heard(&call, Kind::Hello, &[&call]);
    }

    let stations = rf.server.radio.sessions.peers().count();
    let capacity = rf.server.radio.sessions.config.max_peers;
    assert!(
        stations * 4 < capacity,
        "a forged-callsign flood took {stations} of {capacity} session slots; \
         the first-contact budget exists so it cannot"
    );

    // The station that was already here is untouched — the budget rations
    // first contact, not conversation — and can still be talked to.
    assert!(
        rf.station("SM0ABC-7").is_some(),
        "the flood displaced an established station"
    );
    rf.heard("SM0ABC-7", Kind::Msg, &["#rf", "still here"]);
    assert!(
        rf.server
            .radio
            .sessions
            .peer(&"SM0ABC-7".parse::<Callsign>().unwrap())
            .is_some(),
        "an established station lost its session to the flood"
    );
}

/// The APRS path answers an unknown station, so a received frame buys a
/// transmission. Rationing first contact is what stops that converting a
/// forged-callsign flood into the licensee's airtime.
#[tokio::test]
async fn forged_callsigns_do_not_each_earn_an_aprs_ack() {
    let mut rf = Rf::new();
    for i in 0..200 {
        let call = format!("SM{}B{:02}", i % 10, i / 10);
        rf.heard_aprs(&call, "APRS", b":SK0MT-1  :#rf hello{01");
    }
    let acks = rf
        .transmitted_raw()
        .await
        .into_iter()
        .filter(|ax| ax.info.starts_with(b":"))
        .count();
    assert!(
        acks <= 16,
        "{acks} APRS acks were transmitted for 200 forged callsigns"
    );
}

/// Every other control frame goes through the per-station rate limit; QUIT
/// did not, and it is the cheapest frame to forge — one unacknowledged
/// broadcast removed a station from IRC.
#[tokio::test]
async fn a_forged_quit_is_rate_limited_like_every_other_control_frame() {
    let mut rf = Rf::with(&CONFIG.replace("rf_msgs_per_min = 600", "rf_msgs_per_min = 60"));
    rf.heard("SM0ABC-7", Kind::Hello, &["SM0ABC-7"]);
    rf.heard("SM0ABC-7", Kind::Join, &["#rf"]);
    assert!(rf.station("SM0ABC-7").is_some(), "the station should be on");

    // Spend the station's control budget, then check the QUIT after it is
    // refused rather than acted on.
    for _ in 0..200 {
        rf.heard("SM0ABC-7", Kind::Names, &["#rf"]);
    }
    rf.heard("SM0ABC-7", Kind::Quit, &["bye"]);
    assert!(
        rf.station("SM0ABC-7").is_some(),
        "a QUIT past the rate limit still removed the station"
    );
}

#[tokio::test]
async fn irc_channel_chat_reaches_an_aprs_peer_as_aprs() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.send(a, "JOIN #rf");
    rf.drain(a);
    // APRS HT joins the channel by messaging the gateway.
    rf.heard_aprs("SM0ABC-7", "APRS", b":SK0MT-1  :#rf listening{9");
    let _ = rf.transmitted_raw().await;
    rf.drain(a);

    rf.send(a, "PRIVMSG #rf :hello trail");
    let frames = rf.transmitted_raw().await;
    let aprs: Vec<_> = frames
        .iter()
        .filter(|f| f.destination.call.to_string() == "APRS")
        .collect();
    assert!(
        aprs.iter().any(|f| {
            let s = String::from_utf8_lossy(&f.info);
            s.contains("SM0ABC-7") && s.contains("alice: hello trail") && s.contains('{')
        }),
        "APRS peer must get addressed channel chat: {frames:?}"
    );
    assert!(
        frames
            .iter()
            .filter(|f| AircFrame::decode(&f.info).is_ok())
            .all(|f| {
                AircFrame::decode(&f.info)
                    .ok()
                    .map(|a| a.kind != Kind::Msg)
                    .unwrap_or(true)
            }),
        "no AIRC MSG when only an APRS peer is listening: {frames:?}"
    );
}

#[tokio::test]
async fn irc_private_message_to_aprs_peer_is_aprs() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.send(a, "JOIN #rf");
    rf.heard_aprs("SM0ABC-7", "APRS", b":SK0MT-1  :#rf hi{1");
    let _ = rf.transmitted_raw().await;
    rf.drain(a);

    rf.send(a, "PRIVMSG SM0ABC|7 :direct reply");
    let frames = rf.transmitted_raw().await;
    assert!(
        frames.iter().any(|f| {
            f.destination.call.to_string() == "APRS"
                && String::from_utf8_lossy(&f.info).contains("alice: direct reply")
        }),
        "DM to an APRS peer must be APRS: {frames:?}"
    );
}

#[tokio::test]
async fn aprs_ack_clears_outbound_pending() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.send(a, "JOIN #rf");
    rf.heard_aprs("SM0ABC-7", "APRS", b":SK0MT-1  :#rf hi{1");
    let _ = rf.transmitted_raw().await;

    rf.send(a, "PRIVMSG SM0ABC|7 :ping");
    let frames = rf.transmitted_raw().await;
    let msgid = frames
        .iter()
        .find_map(|f| {
            let s = String::from_utf8_lossy(&f.info);
            let i = s.rfind('{')?;
            Some(s[i + 1..].trim_end_matches('}').to_string())
        })
        .expect("outbound APRS should carry a msgid");

    // HT ACKs; a second DM should go out immediately (not blocked on retry).
    let ack = format!(":SK0MT-1  :ack{msgid}");
    rf.heard_aprs("SM0ABC-7", "APRS", ack.as_bytes());
    rf.send(a, "PRIVMSG SM0ABC|7 :second");
    let frames = rf.transmitted_raw().await;
    assert!(
        frames.iter().any(|f| {
            String::from_utf8_lossy(&f.info).contains("alice: second")
        }),
        "after ACK the next DM should transmit: {frames:?}"
    );
}

#[tokio::test]
async fn aprs_nick_line_is_a_private_message() {
    let mut rf = Rf::new();
    let a = rf.client(1, "alice");
    rf.send(a, "JOIN #rf");
    rf.drain(a);

    rf.heard_aprs("SM0ABC-7", "APRS", b":SK0MT-1  :alice hello from HT{3");
    let lines = rf.drain(a);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("PRIVMSG alice :hello from HT")),
        "nick-addressed APRS should be a query: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("PRIVMSG #rf")),
        "must not also inject into the channel: {lines:?}"
    );
}

#[tokio::test]
async fn rf_mode_aprs_does_not_emit_airc_chat() {
    let text = CONFIG.replace(
        "presence_notices = true",
        "presence_notices = true\nrf_mode = \"aprs\"",
    );
    let mut rf = Rf::with(&text);
    let a = rf.client(1, "alice");
    rf.send(a, "OPER root operpass1");
    rf.send(a, "CALLSIGN SM0XYZ");
    rf.send(a, "JOIN #rf");
    rf.heard("SM0XYZ-9", Kind::Hello, &["SM0XYZ-9"]);
    rf.heard("SM0XYZ-9", Kind::Join, &["#rf"]);
    let _ = rf.transmitted_raw().await;
    rf.drain(a);

    rf.send(a, "PRIVMSG #rf :only aprs please");
    let frames = rf.transmitted_raw().await;
    assert!(
        frames
            .iter()
            .filter(|f| AircFrame::decode(&f.info).is_ok())
            .all(|f| AircFrame::decode(&f.info).unwrap().kind != Kind::Msg),
        "rf_mode=aprs must not radiate AIRC chat: {frames:?}"
    );
    assert!(
        frames.iter().any(|f| {
            f.destination.call.to_string() == "APRS"
                && String::from_utf8_lossy(&f.info).contains("only aprs please")
        }),
        "rf_mode=aprs fans out APRS to RF members: {frames:?}"
    );
}
