//! Stress and robustness: feed the parsers what a real channel feeds them.
//!
//! Everything here decodes input the gateway does not control — bytes off the
//! air, lines off a socket. The property being checked is always the same one:
//! *no input causes a panic, a hang, or unbounded memory*, and whatever does
//! decode round-trips faithfully. A packet channel supplies corrupt frames for
//! free, so this is not a hypothetical.
//!
//! The generator is a xorshift PRNG rather than a fuzzing framework: it needs
//! no extra dependency, it is deterministic (a failure reproduces from the
//! printed seed), and it runs in CI in under a second.

use std::time::{Duration, Instant};

use rfircd::airc::frame::{AircFrame, Kind};
use rfircd::airc::{encode_fields, SessionConfig, Sessions};
use rfircd::aprs::{AprsBeacon, AprsMessage};
use rfircd::ax25::kiss::{self, KissDecoder};
use rfircd::ax25::{Address, Ax25Frame};
use rfircd::callsign::Callsign;
use rfircd::config::PolicyConfig;
use rfircd::irc::message::{is_channel_name, is_valid_nick, lower, Message};
use rfircd::policy::{looks_like_ciphertext, sanitize, Policy};

/// Deterministic, dependency-free noise.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.byte()).collect()
    }
}

#[test]
fn ax25_decoding_survives_arbitrary_bytes() {
    let mut rng = Rng::new(0x9E3779B97F4A7C15);
    for i in 0..200_000 {
        let len = rng.below(80);
        let buf = rng.bytes(len);
        // The contract is only that it returns rather than panicking. Anything
        // that does decode must re-encode to something that decodes again.
        if let Ok(frame) = Ax25Frame::decode(&buf) {
            let re = frame.encode();
            assert!(
                Ax25Frame::decode(&re).is_ok(),
                "iteration {i}: a decoded frame did not survive a round trip: {buf:02x?}"
            );
        }
    }
}

#[test]
fn aprs_decoding_survives_arbitrary_bytes() {
    let mut rng = Rng::new(0xA9B8C7D6E5F40123);
    for i in 0..200_000 {
        let len = rng.below(80);
        let buf = rng.bytes(len);
        if let Some(msg) = AprsMessage::decode(&buf) {
            let re = msg.encode();
            let back = AprsMessage::decode(&re)
                .unwrap_or_else(|| panic!("iteration {i}: encoded form did not decode: {re:02x?}"));
            assert_eq!(back.addressee, msg.addressee, "iteration {i}");
            assert_eq!(back.kind, msg.kind, "iteration {i}");
        }
        let _ = AprsBeacon::decode(&buf);
        let dest: Callsign = "APRS".parse().unwrap();
        let _ = rfircd::aprs::decode_beacon(&dest, &buf);
    }
}

/// A field of exactly `len` bytes drawn from `alphabet`, padded with ASCII
/// when the next character would overshoot the width.
fn field_of(rng: &mut Rng, alphabet: &[char], len: usize) -> String {
    let mut s = String::new();
    while s.len() < len {
        let c = alphabet[rng.below(alphabet.len())];
        if s.len() + c.len_utf8() <= len {
            s.push(c);
        } else {
            s.push('x');
        }
    }
    s
}

/// Uniform random bytes never reach the interesting arms.
///
/// `decode_uncompressed` is only entered by an information field shaped
/// `!ddmm.ddN/`, which random generation produces with probability around
/// 1e-12 — so a panic living behind that prefix survived 200k iterations of
/// the sweep above indefinitely. This builds the skeleton and randomises only
/// the fields inside it, which is where parsers actually go wrong.
#[test]
fn aprs_decoding_survives_well_formed_skeletons() {
    // Characters chosen to straddle the byte offsets the coordinate
    // formatter slices at: a coordinate field is nine octets, and only the
    // latitude is validated, so multi-byte characters land mid-slice.
    // `E`/`W` and `N`/`S` matter: the coordinate formatter is only reached by
    // a field that ends in a hemisphere letter, so an alphabet without them
    // reproduces the blind spot this test exists to close.
    const FILL: &[char] = &[
        'E',
        'W',
        'N',
        'S',
        'A',
        '0',
        '9',
        '.',
        ' ',
        '-',
        '\u{e9}',
        '\u{4e2d}',
        '\u{10348}',
        '\u{7f}',
    ];
    let mut rng = Rng::new(0x5DEECE66D);
    let dest: Callsign = "APRS".parse().unwrap();
    for _ in 0..100_000 {
        // Drawn before the match so the generator never borrows `rng`
        // twice in one call.
        let tail = rng.below(24);
        let idlen = rng.below(6);
        let mut info: Vec<u8> = Vec::new();
        match rng.below(6) {
            // Uncompressed position: `!` lat(8) sym lon(9) comment.
            0 | 1 => {
                info.push(*[b'!', b'=', b'/', b'@'].get(rng.below(4)).unwrap());
                info.extend_from_slice(field_of(&mut rng, FILL, 8).as_bytes());
                info.push(if rng.below(2) == 0 { b'/' } else { b'\\' });
                info.extend_from_slice(field_of(&mut rng, FILL, 9).as_bytes());
                info.extend_from_slice(field_of(&mut rng, FILL, tail).as_bytes());
            }
            // The same, but with the latitude that passes `uncompressed_lat`,
            // so the longitude path is always reached.
            2 | 3 => {
                info.push(b'!');
                info.extend_from_slice(b"5930.00N");
                info.push(b'/');
                info.extend_from_slice(field_of(&mut rng, FILL, 9).as_bytes());
                info.extend_from_slice(field_of(&mut rng, FILL, tail).as_bytes());
            }
            // Status, compressed position, Mic-E.
            4 => {
                info.push(*[b'>', b'_', b'`', b'\''].get(rng.below(4)).unwrap());
                info.extend_from_slice(field_of(&mut rng, FILL, tail).as_bytes());
            }
            // Addressed message: `:ADDRESSEE:text{id`.
            _ => {
                info.push(b':');
                info.extend_from_slice(field_of(&mut rng, FILL, 9).as_bytes());
                info.push(b':');
                info.extend_from_slice(field_of(&mut rng, FILL, tail).as_bytes());
                if rng.below(2) == 0 {
                    info.push(b'{');
                    info.extend_from_slice(field_of(&mut rng, FILL, idlen).as_bytes());
                }
            }
        }
        // The contract is the same as the random sweep: return, never panic.
        let _ = AprsBeacon::decode(&info);
        let _ = AprsMessage::decode(&info);
        let _ = rfircd::aprs::decode_beacon(&dest, &info);
        let _ = rfircd::aprs::decode_mice_frame("593P00", &info);
    }
}

#[test]
fn ax25_round_trips_for_every_well_formed_frame() {
    let mut rng = Rng::new(12345);
    for _ in 0..20_000 {
        let digis: Vec<Callsign> = (0..rng.below(3))
            .map(|_| {
                let n = rng.below(10);
                format!("SK{n}MT-{}", rng.below(16)).parse().unwrap()
            })
            .collect();
        let n = rng.below(200);
        let info = rng.bytes(n);
        let frame = Ax25Frame::ui(
            "SM0ABC-7".parse().unwrap(),
            "AIRC".parse().unwrap(),
            &digis,
            info.clone(),
        )
        .unwrap();
        let back = Ax25Frame::decode(&frame.encode()).expect("our own frames must decode");
        assert_eq!(back, frame);
        assert_eq!(back.info, info);
    }
}

#[test]
fn the_address_field_rejects_what_it_cannot_represent() {
    let mut rng = Rng::new(777);
    for _ in 0..100_000 {
        let buf = rng.bytes(7);
        if let Ok(addr) = Address::decode(&buf) {
            let mut out = Vec::new();
            addr.encode(&mut out);
            assert_eq!(
                Address::decode(&out).unwrap(),
                addr,
                "an address must survive its own encoding"
            );
        }
    }
}

#[test]
fn the_kiss_decoder_never_exceeds_its_limit_or_hangs() {
    let mut rng = Rng::new(0xDEADBEEF);
    let mut dec = KissDecoder::new(256);
    for _ in 0..2_000 {
        // A mix of structure and noise: FEND and FESC appear far more often
        // than chance would give them, which is what exercises the escape
        // handling and the resynchronisation.
        let len = rng.below(512);
        let chunk: Vec<u8> = (0..len)
            .map(|_| match rng.below(6) {
                0 => kiss::FEND,
                1 => kiss::FESC,
                2 => kiss::TFEND,
                3 => kiss::TFESC,
                _ => rng.byte(),
            })
            .collect();
        for frame in dec.push(&chunk) {
            assert!(
                frame.payload.len() <= 256,
                "the decoder handed back a frame past its own limit"
            );
        }
    }

    // After all that, it still decodes a good frame.
    let wire = kiss::encode(0, kiss::CMD_DATA, b"still working");
    let mut dec = KissDecoder::new(256);
    assert_eq!(dec.push(&wire)[0].payload, b"still working");
}

#[test]
fn kiss_round_trips_any_payload() {
    let mut rng = Rng::new(24680);
    for _ in 0..20_000 {
        let n = rng.below(200);
        let payload = rng.bytes(n);
        let port = (rng.below(16)) as u8;
        let command = (rng.below(16)) as u8;
        let wire = kiss::encode(port, command, &payload);
        let mut dec = KissDecoder::new(1024);
        let frames = dec.push(&wire);
        assert_eq!(frames.len(), 1, "one frame in, one frame out");
        assert_eq!(frames[0].payload, payload);
        assert_eq!(frames[0].port, port);
        assert_eq!(frames[0].command, command);
    }
}

#[test]
fn airc_decoding_survives_arbitrary_bytes() {
    let mut rng = Rng::new(0xC0FFEE);
    for _ in 0..200_000 {
        let n = rng.below(64);
        let mut buf = rng.bytes(n);
        // Half the time, make it look like AIRC so the header path is reached
        // rather than bailing on the magic.
        if buf.len() >= 2 && rng.below(2) == 0 {
            buf[0] = b'A';
            buf[1] = b'1';
        }
        if let Ok(frame) = AircFrame::decode(&buf) {
            assert!(frame.frag_total > 0 && frame.frag_index < frame.frag_total);
            assert_eq!(
                AircFrame::decode(&frame.encode()).unwrap(),
                frame,
                "a decoded AIRC frame must survive a round trip"
            );
            // Fields are lossy-decoded UTF-8, so this must never panic and
            // never produce a separator that would re-split differently.
            for f in frame.fields() {
                assert!(!f.contains('\u{1f}'));
            }
        }
    }
}

#[test]
fn field_encoding_cannot_smuggle_separators_or_line_breaks() {
    let mut rng = Rng::new(13579);
    for _ in 0..20_000 {
        let (la, lb) = (rng.below(20), rng.below(20));
        let a = String::from_utf8_lossy(&rng.bytes(la)).into_owned();
        let b = String::from_utf8_lossy(&rng.bytes(lb)).into_owned();
        let payload = encode_fields(&[&a, &b]);
        assert!(!payload.contains(&b'\r'));
        assert!(!payload.contains(&b'\n'));
        assert!(!payload.contains(&0));
        let frame = AircFrame::new(Kind::Msg, 1, payload);
        assert!(
            frame.fields().len() <= 2,
            "a field must not be able to split itself into more fields"
        );
    }
}

#[test]
fn the_session_layer_survives_a_hostile_peer() {
    let mut rng = Rng::new(0xABCDEF);
    let cfg = SessionConfig {
        paclen: 64,
        max_peers: 8,
        max_reasm: 2,
        max_queue: 4,
        ..Default::default()
    };
    let mut s = Sessions::new(cfg);
    let calls: Vec<Callsign> = (0..20)
        .map(|i| format!("SM{}ABC-{}", i % 10, i % 16).parse().unwrap())
        .collect();

    let start = Instant::now();
    let mut now = start;
    for i in 0..100_000 {
        let call = &calls[rng.below(calls.len())];
        let kind = match rng.below(8) {
            0 => Kind::Hello,
            1 => Kind::Join,
            2 => Kind::Msg,
            3 => Kind::Ack,
            4 => Kind::Part,
            5 => Kind::Names,
            6 => Kind::Ping,
            _ => Kind::Quit,
        };
        let seq = (rng.next() % 65536) as u16;
        let n = rng.below(80);
        let mut f = AircFrame::new(kind, seq, rng.bytes(n));
        f.flags = rng.byte();
        // Fragment headers are validated on decode; here they are set directly,
        // so cover the legal range and let reassembly deal with the rest.
        f.frag_total = 1 + (rng.below(8)) as u8;
        f.frag_index = (rng.below(f.frag_total as usize)) as u8;
        s.on_receive(call, f, now, true);

        if i % 500 == 0 {
            now += Duration::from_secs(1);
            s.tick(now);
        }
    }

    assert!(
        s.peers().count() <= 8,
        "the peer table grew past max_peers: a flood of unique callsigns is free to produce"
    );
    for p in s.peers() {
        assert!(p.queue_depth() <= 5, "a peer queue grew past its bound");
    }
}

#[test]
fn irc_parsing_survives_arbitrary_lines() {
    let mut rng = Rng::new(0x5EED);
    for _ in 0..200_000 {
        let n = rng.below(600);
        let raw = rng.bytes(n);
        let line = String::from_utf8_lossy(&raw);
        if let Some(msg) = Message::parse(&line) {
            // A parsed message must serialise to exactly one IRC line, or an
            // RF-originated field becomes an injection into every client.
            let out = msg.to_string();
            assert!(
                !out.contains('\r') && !out.contains('\n') && !out.contains('\0'),
                "serialising produced a line break: {out:?}"
            );
            assert!(!msg.command.is_empty());
        }
        // The helpers are called on the same untrusted input.
        let _ = lower(&line);
        let _ = is_channel_name(&line);
        let _ = is_valid_nick(&line, 30);
    }
}

#[test]
fn message_round_trips_survive_reparsing() {
    let mut rng = Rng::new(0x1234ABCD);
    for _ in 0..20_000 {
        let n = 1 + rng.below(4);
        let params: Vec<String> = (0..n)
            .map(|_| {
                let n = 1 + rng.below(12);
                String::from_utf8_lossy(&rng.bytes(n)).into_owned()
            })
            .collect();
        let msg = Message::new("PRIVMSG", params).with_prefix("nick!user@host");
        let line = msg.to_string();
        let back = Message::parse(&line).expect("our own output must parse");
        assert_eq!(back.command, "PRIVMSG");
        assert_eq!(
            back.params.len(),
            msg.params.len(),
            "the parameter count changed across a round trip, so every field after \
             the culprit shifted: {:?} -> {line:?} -> {:?}",
            msg.params,
            back.params
        );
    }
}

#[test]
fn the_outbound_screen_never_panics_and_always_bounds_length() {
    let mut rng = Rng::new(0xFEEDFACE);
    let policy = Policy::new(PolicyConfig {
        max_rf_text_len: 40,
        ..Default::default()
    });
    for _ in 0..100_000 {
        let n = rng.below(300);
        let raw = rng.bytes(n);
        let text = String::from_utf8_lossy(&raw).into_owned();
        let cleaned = sanitize(&text);
        assert!(!cleaned.contains('\r') && !cleaned.contains('\n'));
        assert!(
            !cleaned.starts_with(' ') && !cleaned.ends_with(' '),
            "sanitised text is trimmed: {cleaned:?}"
        );
        let _ = looks_like_ciphertext(&text);
        match policy.screen_outbound(&text) {
            rfircd::policy::Verdict::Allow(t) => assert!(t.chars().count() <= 40),
            rfircd::policy::Verdict::Truncated(t) => assert!(
                t.chars().count() <= 40,
                "a truncated message must respect the cap: {} chars",
                t.chars().count()
            ),
            rfircd::policy::Verdict::Deny(_) => {}
        }
    }
}

#[test]
fn callsign_parsing_survives_arbitrary_text() {
    let mut rng = Rng::new(0x0BADC0DE);
    for _ in 0..200_000 {
        let n = rng.below(20);
        let raw = rng.bytes(n);
        let text = String::from_utf8_lossy(&raw).into_owned();
        if let Ok(c) = text.parse::<Callsign>() {
            // Display and FromStr are inverses, and the nick form round trips.
            assert_eq!(c.to_string().parse::<Callsign>().unwrap(), c);
            assert_eq!(Callsign::from_nick(&c.to_nick()).unwrap(), c);
            assert!(c.base().len() <= 6 && c.ssid() <= 15);
        }
        let _ = Callsign::reserved_from_nick(&text);
        let _ = Callsign::from_nick(&text);
    }
}

#[test]
fn a_reserved_callsign_nick_can_never_be_claimed_by_an_ip_user() {
    // Every spelling of a callsign an IRC client could type must be caught,
    // including the RFC 1459 casemapping equivalents.
    let mut rng = Rng::new(0x1CE);
    for _ in 0..50_000 {
        let base = format!(
            "{}{}{}",
            (b'A' + rng.below(26) as u8) as char,
            (b'A' + rng.below(26) as u8) as char,
            rng.below(10)
        );
        let ssid = rng.below(16);
        for spelling in [
            base.to_string(),
            format!("{base}-{ssid}"),
            format!("{base}|{ssid}"),
            format!("{base}\\{ssid}"),
            base.to_lowercase(),
        ] {
            if ssid == 0 && spelling.contains(['-', '|', '\\']) {
                continue;
            }
            assert!(
                Callsign::reserved_from_nick(&spelling).is_some(),
                "{spelling} should be reserved for an RF station"
            );
        }
    }
}

#[test]
fn a_middle_parameter_containing_a_space_cannot_split_the_message() {
    // Middle parameters are space-delimited on the wire, so one that contains
    // a space silently becomes two. This is reachable from configuration:
    // `server.name` is used as the topic setter, and RPL_TOPICWHOTIME puts
    // the setter in a middle position.
    let m = Message::new(
        "333",
        vec![
            "alice".into(),
            "#rf".into(),
            "My Gateway".into(), // the setter, with a space
            "1700000000".into(),
        ],
    )
    .with_prefix("server.example");
    let line = m.to_string();
    let back = Message::parse(&line).expect("our own output must parse");
    assert_eq!(
        back.params.len(),
        m.params.len(),
        "a middle parameter split the message into extra parameters: {line}"
    );
    assert_eq!(
        back.params[3], "1700000000",
        "the timestamp moved because an earlier parameter split: {line}"
    );
}

#[test]
fn a_trailing_parameter_keeps_its_spacing() {
    // The trailing parameter is everything after the colon, so it can hold
    // runs of spaces. Collapsing them mangles anything aligned — a table, a
    // code snippet, ASCII art — for no safety benefit.
    let m = Message::new("PRIVMSG", vec!["#rf".into(), "column1    column2".into()])
        .with_prefix("alice!a@h");
    let back = Message::parse(&m.to_string()).unwrap();
    assert_eq!(
        back.params[1], "column1    column2",
        "the message text lost its spacing"
    );
}

/// Two session layers talking to each other over a lossy, reordering,
/// duplicating channel — which is what a shared half-duplex radio channel is.
///
/// The property: every message the sender commits to reliably is either
/// delivered exactly once or given up on. Never delivered twice, never
/// silently mangled, and the reassembly never hands up a payload that is not
/// one of the messages that was sent.
#[test]
fn reliable_delivery_survives_loss_reordering_and_duplication() {
    let mut rng = Rng::new(0x5AFE7ED5);
    for trial in 0..200 {
        let cfg = SessionConfig {
            paclen: 24 + rng.below(40),
            ack_timeout: Duration::from_secs(5),
            max_retries: 6,
            max_queue: 32,
            ..Default::default()
        };
        let mut tx = Sessions::new(cfg.clone());
        let mut rx = Sessions::new(cfg.clone());
        let a: Callsign = "SM0ABC-7".parse().unwrap();
        let b: Callsign = "SK0MT-1".parse().unwrap();

        // Distinct payloads so a mix-up is visible.
        let messages: Vec<Vec<u8>> = (0..6)
            .map(|i| format!("message number {i} {}", "x".repeat(rng.below(60))).into_bytes())
            .collect();

        let mut now = Instant::now();
        let mut in_flight: Vec<AircFrame> = Vec::new();
        let mut delivered: Vec<Vec<u8>> = Vec::new();
        let mut queued = messages.clone();

        for _ in 0..400 {
            // Offer the next message when nothing is in flight for it.
            if !queued.is_empty() {
                let m = queued.remove(0);
                in_flight.extend(tx.send(&b, Kind::Msg, m, true, now));
            }

            // The channel: drop a third, duplicate a sixth, reorder the rest.
            let mut arriving = std::mem::take(&mut in_flight);
            if arriving.len() > 1 && rng.below(2) == 0 {
                let i = rng.below(arriving.len());
                let j = rng.below(arriving.len());
                arriving.swap(i, j);
            }
            for f in arriving {
                if rng.below(3) == 0 {
                    continue; // lost
                }
                let repeats = if rng.below(6) == 0 { 2 } else { 1 };
                for _ in 0..repeats {
                    let out = rx.on_receive(&a, f.clone(), now, true);
                    if let Some(msg) = out.deliver {
                        delivered.push(msg.payload);
                    }
                    // Follow-ups after a received ACK (the next queued message).
                    in_flight.extend(out.transmit);
                    // ACKs travel back over the same lossy channel.
                    for (_, ack) in rx.drain_acks() {
                        if rng.below(4) != 0 {
                            let back = tx.on_receive(&b, ack, now, true);
                            in_flight.extend(back.transmit);
                        }
                    }
                }
            }

            now += Duration::from_secs(6);
            let tick = tx.tick(now);
            in_flight.extend(tick.transmit.into_iter().map(|(_, f)| f));
            let rx_tick = rx.tick(now);
            for (_, ack) in rx_tick.transmit {
                if rng.below(4) != 0 {
                    let back = tx.on_receive(&b, ack, now, true);
                    in_flight.extend(back.transmit);
                }
            }
        }

        // Nothing invented, nothing corrupted.
        for d in &delivered {
            assert!(
                messages.contains(d),
                "trial {trial}: reassembly produced a payload that was never sent: \
                 {} bytes",
                d.len()
            );
        }
        // Nothing delivered twice.
        let mut seen = delivered.clone();
        seen.sort();
        let before = seen.len();
        seen.dedup();
        assert_eq!(
            seen.len(),
            before,
            "trial {trial}: a message was delivered more than once despite the \
             duplicate-suppression window"
        );
    }
}

/// The peer table and every per-peer structure stay bounded no matter what a
/// station does, including a station that never acknowledges anything.
#[test]
fn a_peer_that_never_acknowledges_is_eventually_given_up_on() {
    let cfg = SessionConfig {
        ack_timeout: Duration::from_secs(4),
        max_retries: 3,
        ..Default::default()
    };
    let mut s = Sessions::new(cfg);
    let call: Callsign = "SM0ABC-7".parse().unwrap();
    let mut now = Instant::now();

    let frames = s.send(&call, Kind::Msg, b"hello".to_vec(), true, now);
    assert_eq!(frames.len(), 1);
    s.on_keyed(&call, frames[0].seq, now);
    let mut transmissions = 1;
    let mut lost = false;
    for _ in 0..40 {
        now += Duration::from_secs(5);
        let out = s.tick(now);
        transmissions += out.transmit.len();
        if !out.lost.is_empty() {
            lost = true;
            break;
        }
    }
    assert!(
        lost,
        "a station that never answers must eventually be dropped"
    );
    assert!(
        transmissions <= 5,
        "{transmissions} transmissions for one message: max_retries is 3, and every \
         extra one is airtime spent on a station that is not listening"
    );
    assert_eq!(s.peers().count(), 0, "the peer should have been forgotten");
}

/// The airtime governor's own arithmetic, under values a configuration file
/// can actually produce.
#[test]
fn the_governor_is_sane_across_the_configurable_range() {
    use rfircd::ax25::airtime::{Governor, TxDecision, HARD_MAX_DUTY};
    use rfircd::config::DutyConfig;

    let mut rng = Rng::new(0xA1147123);
    for _ in 0..2_000 {
        let duty = DutyConfig {
            enabled: true,
            baud: [50, 300, 1200, 9600, 19200][rng.below(5)],
            txdelay_ms: (rng.below(255) * 10) as u64,
            txtail_ms: (rng.below(255) * 10) as u64,
            window_secs: 1 + rng.below(3600) as u64,
            max_duty_percent: 1 + rng.below(50) as u32,
            max_continuous_secs: 1 + rng.below(300) as u64,
            cooldown_secs: rng.below(600) as u64,
            hourly_airtime_secs: rng.below(3600) as u64,
            max_hold_secs: 1 + rng.below(600) as u64,
            allow_nonstandard_baud: true,
        };
        let air = duty.to_airtime();
        // The clamp holds regardless of what was asked for.
        assert!(air.effective_duty() <= HARD_MAX_DUTY);

        // Only exercise configurations the validator would accept. It
        // deliberately rejects a duty allowance too small to fit one frame,
        // because such a station could never transmit at all.
        if air.check_hardware_safe().is_err() {
            continue;
        }
        let mut g = Governor::new(air.clone());
        let octets = 16 + rng.below(300);
        let allowance = air.window.mul_f64(air.effective_duty());
        if g.airtime_for(octets) > allowance {
            continue;
        }
        // Costing a frame never panics and never returns nonsense.
        let cost = g.airtime_for(octets);
        assert!(cost >= air.txdelay + air.txtail);
        assert!(cost < Duration::from_secs(3600));

        // A decision is reached, and a deferral is always a finite wait.
        let now = Instant::now();
        match g.check(octets, now) {
            TxDecision::Send => {
                let keyed = g.record(octets, now);
                assert_eq!(keyed, cost);
            }
            TxDecision::Defer(d, _) => assert!(d <= Duration::from_secs(3600)),
        }
        // The read-only estimate is finite. It can legitimately be as long as
        // the rolling hour plus one frame when the hourly budget is spent —
        // `max_hold` drops the frame long before then — but it must never run
        // away.
        let clear = g.time_until_clear(octets, now);
        assert!(
            clear <= Duration::from_secs(3600) + g.airtime_for(octets),
            "an estimate of {clear:?} is longer than the longest window in play"
        );
    }
}
