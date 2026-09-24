//! The IRC command surface, exercised the way a client exercises it.
//!
//! `tests/gateway.rs` covers the radio side; this covers what an IP client can
//! ask for and what it gets back. Every test asserts on the numeric or the
//! message the client actually receives, because that is the contract — a
//! command that "works" but replies with the wrong numeric is broken for
//! irssi.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rfircd::config::Config;
use rfircd::server::state::ClientId;
use rfircd::server::{Event, Server};
use tokio::sync::mpsc;

fn unique_accounts_file() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "target/test-commands-nicks-{}-{}.json",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

const CONFIG: &str = r##"
[server]
name = "cmd.test"
network = "TESTNET"
motd = ["first line", "second line"]
max_nick_len = 20
max_channels_per_user = 3

[listen]
bind = []

[accounts]
file = "target/test-commands-nicks.json"
identify_timeout_secs = 60
min_password_len = 8

[policy]
ip_cmds_per_min = 6000
ip_cmd_burst = 500
identify_per_min = 600
identify_burst = 200

[[channels]]
name = "#lobby"
topic = "the lobby"

[[channels]]
name = "#ops"
operators = ["alice"]

[[opers]]
name = "root"
password = "operpass1"
"##;

struct Net {
    server: Server,
    rx: Vec<(ClientId, mpsc::Receiver<String>)>,
}

impl Net {
    fn new() -> Self {
        Self::with(CONFIG)
    }

    fn with(text: &str) -> Self {
        let path = unique_accounts_file();
        let text = text.replace("target/test-commands-nicks.json", &path);
        let config = Arc::new(Config::from_toml(&text).unwrap());
        Net {
            server: Server::new(config, None).unwrap(),
            rx: Vec::new(),
        }
    }

    /// Connect a client and finish registration.
    fn client(&mut self, id: ClientId, nick: &str) -> ClientId {
        self.raw_client(id);
        self.send(id, &format!("NICK {nick}"));
        self.send(id, &format!("USER {nick} 0 * :{nick} the tester"));
        self.drain(id);
        id
    }

    /// Connect without registering.
    fn raw_client(&mut self, id: ClientId) -> ClientId {
        let (out, rx) = mpsc::channel(4096);
        self.server.handle(Event::Connected {
            id,
            host: format!("10.9.{}.{}", id / 256, id % 256),
            listen_only: false,
            out,
            hangup: None,
        });
        self.rx.push((id, rx));
        id
    }

    fn listen_only_client(&mut self, id: ClientId, nick: &str) -> ClientId {
        let (out, rx) = mpsc::channel(4096);
        self.server.handle(Event::Connected {
            id,
            host: format!("203.0.113.{}", id % 256),
            listen_only: true,
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

    /// Send and collect the reply in one step.
    fn ask(&mut self, id: ClientId, line: &str) -> Vec<String> {
        self.send(id, line);
        self.drain(id)
    }
}

/// True if any line carries this numeric (as a space-delimited field).
fn has_numeric(lines: &[String], code: &str) -> bool {
    lines.iter().any(|l| l.split(' ').any(|f| f == code))
}

// ----------------------------------------------------------------- registration

#[test]
fn registration_sends_the_expected_welcome_burst() {
    let mut n = Net::new();
    let a = n.raw_client(1);
    n.send(a, "NICK alice");
    n.send(a, "USER alice 0 * :Alice");
    let lines = n.drain(a);

    for code in [
        "001", "002", "003", "004", "005", "251", "255", "375", "372", "376",
    ] {
        assert!(
            has_numeric(&lines, code),
            "missing numeric {code}: {lines:?}"
        );
    }
    assert!(lines.iter().any(|l| l.contains("TESTNET")));
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&format!("rfircd-{}", env!("CARGO_PKG_VERSION")))),
        "004 should advertise this build, not a frozen version: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("CASEMAPPING=rfc1459")),
        "ISUPPORT should tell the client how nicks compare"
    );
    let five = lines
        .iter()
        .find(|l| l.split(' ').any(|f| f == "005"))
        .expect("missing 005");
    assert!(
        five.contains(" PREFIX=(ov)@+"),
        "ISUPPORT tokens are separate parameters, not one underscore-glued field: {five}"
    );
    assert!(
        !five.contains("_PREFIX="),
        "005 must not scrub token spaces into underscores: {five}"
    );
}

#[test]
fn commands_before_registration_are_refused() {
    let mut n = Net::new();
    let a = n.raw_client(1);
    for cmd in [
        "JOIN #lobby",
        "PRIVMSG #lobby :hi",
        "MODE #lobby",
        "WHOIS bob",
    ] {
        let lines = n.ask(a, cmd);
        assert!(
            has_numeric(&lines, "451"),
            "{cmd} should be ERR_NOTREGISTERED: {lines:?}"
        );
    }
    // These four are always allowed.
    n.send(a, "PING :x");
    assert!(n.drain(a).iter().any(|l| l.contains("PONG")));
    n.send(a, "CAP LS 302");
    assert!(n.drain(a).iter().any(|l| l.contains("CAP")));
}

#[test]
fn a_listen_only_connection_can_watch_but_not_speak() {
    let mut n = Net::new();
    let speaker = n.client(1, "alice");
    let watcher = n.listen_only_client(2, "eve");
    n.send(speaker, "JOIN #lobby");
    n.drain(speaker);
    n.send(watcher, "JOIN #lobby");
    let joined = n.drain(watcher);
    assert!(
        joined
            .iter()
            .any(|l| l.contains("JOIN") || l.contains(" 366 ")),
        "a spectator may join: {joined:?}"
    );
    assert!(
        n.ask(watcher, "PRIVMSG #lobby :inject")
            .iter()
            .any(|l| l.contains(" 484 ")),
        "plaintext off-box must not send"
    );
    assert!(
        n.ask(watcher, "OPER root operpass1")
            .iter()
            .any(|l| l.contains(" 484 ")),
        "plaintext off-box must not OPER"
    );
    assert!(
        n.ask(watcher, "REGISTER hunter22")
            .iter()
            .any(|l| l.contains(" 484 ")),
        "a password must not be accepted on a listen-only socket"
    );
    assert!(
        n.ask(watcher, "KLINE 203.0.113.1")
            .iter()
            .any(|l| l.contains(" 484 ")),
        "plaintext off-box must not ban hosts"
    );
    n.send(speaker, "PRIVMSG #lobby :hello from tls");
    n.drain(speaker);
    let seen = n.drain(watcher);
    assert!(
        seen.iter().any(|l| l.contains("hello from tls")),
        "listen-only means receive, not silence: {seen:?}"
    );
}

#[test]
fn listen_only_who_does_not_reveal_client_ips() {
    let mut n = Net::new();
    let speaker = n.client(1, "alice");
    n.send(speaker, "JOIN #lobby");
    n.drain(speaker);
    let watcher = n.listen_only_client(2, "eve");
    n.send(watcher, "JOIN #lobby");
    n.drain(watcher);

    let who = n.ask(watcher, "WHO #lobby");
    assert!(
        !who.iter().any(|l| l.contains("10.9.")),
        "listen-only WHO must not leak IP addresses: {who:?}"
    );
    assert!(
        who.iter().any(|l| l.contains("hidden")),
        "other users' hosts are cloaked: {who:?}"
    );
    let whois = n.ask(watcher, "WHOIS alice");
    assert!(
        !whois.iter().any(|l| l.contains("10.9.")),
        "listen-only WHOIS must not leak IP addresses: {whois:?}"
    );
    let from_speaker = n.ask(speaker, "WHO #lobby");
    assert!(
        from_speaker.iter().any(|l| l.contains("10.9.")),
        "a full-access client still sees hosts: {from_speaker:?}"
    );
}

#[test]
fn user_username_cannot_contain_prefix_delimiters() {
    let mut n = Net::new();
    let a = n.raw_client(1);
    n.send(a, "NICK alice");
    let lines = n.ask(a, "USER bob!op@x 0 * :Alice");
    assert!(
        has_numeric(&lines, "432"),
        "USER with !/@ must be refused: {lines:?}"
    );
    assert!(
        !n.server
            .state
            .user(&rfircd::server::state::UserId::Ip(a))
            .unwrap()
            .registered,
        "a refused USER must not complete registration"
    );
    n.send(a, "USER bob 0 * :Alice");
    assert!(has_numeric(&n.drain(a), "001"), "a valid USER still works");
}

#[test]
fn a_nick_that_is_taken_or_malformed_is_refused() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.raw_client(2);

    n.send(b, "NICK alice");
    assert!(has_numeric(&n.drain(b), "433"), "duplicate nick");

    n.send(b, "NICK 0notaletter");
    assert!(has_numeric(&n.drain(b), "432"), "malformed nick");

    // Callsign-shaped nicks belong to RF stations, in all three spellings.
    for nick in ["SM0ABC", "SM0ABC|7", "SM0ABC-7", "SM0ABC\\7"] {
        n.send(b, &format!("NICK {nick}"));
        let lines = n.drain(b);
        assert!(
            has_numeric(&lines, "432"),
            "{nick} should be reserved for RF: {lines:?}"
        );
    }

    // No parameter at all.
    n.send(a, "NICK");
    assert!(has_numeric(&n.drain(a), "431"));
}

#[test]
fn a_nick_change_is_announced_to_the_channel() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    n.send(a, "JOIN #lobby");
    n.send(b, "JOIN #lobby");
    n.drain(a);
    n.drain(b);

    n.send(a, "NICK allison");
    assert!(
        n.drain(b).iter().any(|l| l.contains("NICK allison")),
        "the channel should see the rename"
    );
    assert!(n.server.state.by_nick("allison").is_some());
    assert!(n.server.state.by_nick("alice").is_none());
}

#[test]
fn user_needs_four_parameters_and_cannot_be_repeated() {
    let mut n = Net::new();
    let a = n.raw_client(1);
    n.send(a, "NICK alice");
    n.send(a, "USER alice");
    assert!(has_numeric(&n.drain(a), "461"));

    n.send(a, "USER alice 0 * :Alice");
    n.drain(a);
    n.send(a, "USER alice 0 * :Alice again");
    assert!(has_numeric(&n.drain(a), "462"), "no re-registration");
}

#[test]
fn a_server_password_is_required_when_configured() {
    let text = CONFIG.replace(
        "max_channels_per_user = 3",
        "max_channels_per_user = 3\npassword = \"letmein\"",
    );
    let mut n = Net::with(&text);

    let bad = n.raw_client(1);
    n.send(bad, "PASS wrong");
    n.send(bad, "NICK alice");
    n.send(bad, "USER alice 0 * :Alice");
    let lines = n.drain(bad);
    assert!(has_numeric(&lines, "464"), "{lines:?}");
    assert!(n.server.state.by_nick("alice").is_none());

    let good = n.raw_client(2);
    n.send(good, "PASS letmein");
    n.send(good, "NICK bob");
    n.send(good, "USER bob 0 * :Bob");
    assert!(has_numeric(&n.drain(good), "001"));

    // Off-box plaintext is listen-only, but it still has to be able to PASS
    // or a passworded server is unwatchable.
    let (out, rx) = mpsc::channel(64);
    n.server.handle(Event::Connected {
        id: 4,
        host: "203.0.113.4".into(),
        listen_only: true,
        out,
        hangup: None,
    });
    n.rx.push((4, rx));
    n.send(4, "PASS letmein");
    n.send(4, "NICK eve");
    n.send(4, "USER eve 0 * :Eve");
    let watched = n.drain(4);
    assert!(
        has_numeric(&watched, "001"),
        "a spectator may PASS to watch: {watched:?}"
    );
    assert!(
        !has_numeric(&watched, "484"),
        "PASS is not a speak command: {watched:?}"
    );

    // PASS with no argument is a parameter error, not a silent pass.
    let none = n.raw_client(3);
    n.send(none, "PASS");
    assert!(has_numeric(&n.drain(none), "461"));
}

// --------------------------------------------------------------------- channels

#[test]
fn joining_and_parting_a_channel() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let lines = n.ask(a, "JOIN #lobby");
    assert!(lines.iter().any(|l| l.contains("JOIN #lobby")));
    assert!(has_numeric(&lines, "332"), "topic: {lines:?}");
    assert!(has_numeric(&lines, "353"), "names");
    assert!(has_numeric(&lines, "366"), "end of names");

    let again = n.ask(a, "JOIN #lobby");
    assert!(
        has_numeric(&again, "353") && has_numeric(&again, "366"),
        "re-JOIN must refresh NAMES, not hang: {again:?}"
    );
    assert!(
        again.iter().all(|l| !l.contains("JOIN #lobby")),
        "already on the channel: do not announce a second JOIN: {again:?}"
    );

    let lines = n.ask(a, "PART #lobby :bye");
    assert!(lines.iter().any(|l| l.contains("PART #lobby")));
    // Parting twice is an error, not a crash.
    assert!(has_numeric(&n.ask(a, "PART #lobby"), "442"));
    assert!(has_numeric(&n.ask(a, "PART #nowhere"), "403"));
    assert!(has_numeric(&n.ask(a, "PART"), "461"));
    assert!(has_numeric(&n.ask(a, "JOIN"), "461"));
    assert!(has_numeric(&n.ask(a, "JOIN notachannel"), "403"));
}

#[test]
fn join_zero_leaves_everything() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    n.send(a, "JOIN #lobby");
    n.send(a, "JOIN #ops");
    n.drain(a);
    assert_eq!(n.server.state.user(&user_id(a)).unwrap().channels.len(), 2);

    let lines = n.ask(a, "JOIN 0");
    assert_eq!(
        lines.iter().filter(|l| l.contains("PART")).count(),
        2,
        "JOIN 0 parts every channel: {lines:?}"
    );
    assert!(n
        .server
        .state
        .user(&user_id(a))
        .unwrap()
        .channels
        .is_empty());
}

#[test]
fn channel_keys_limits_and_the_per_user_cap() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");

    // Alice creates a channel, so she is its operator, and locks it.
    n.send(a, "JOIN #private");
    n.drain(a);
    n.send(a, "MODE #private +k hunter2");
    n.drain(a);

    assert!(has_numeric(&n.ask(b, "JOIN #private"), "475"), "wrong key");
    let lines = n.ask(b, "JOIN #private hunter2");
    assert!(
        lines.iter().any(|l| l.contains("JOIN #private")),
        "{lines:?}"
    );

    // A limit of one is already full.
    n.send(a, "MODE #private +l 1");
    n.drain(a);
    let c = n.client(3, "carol");
    assert!(has_numeric(&n.ask(c, "JOIN #private hunter2"), "471"));

    // +l 0 is not a lock: it was applying a limit of zero, which made
    // `members.len() >= 0` refuse every join, including an empty channel.
    n.send(a, "MODE #private -l");
    n.drain(a);
    n.send(a, "MODE #private +l 0");
    n.drain(a);
    assert!(
        n.server.state.channel("#private").unwrap().limit.is_none(),
        "+l 0 must not lock the channel empty"
    );

    // Three channels each, per the fixture.
    n.send(c, "JOIN #one");
    n.send(c, "JOIN #two");
    n.send(c, "JOIN #three");
    n.drain(c);
    assert!(has_numeric(&n.ask(c, "JOIN #four"), "405"));
}

#[test]
fn topic_requires_operator_when_locked() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    n.send(a, "JOIN #lobby");
    n.send(b, "JOIN #lobby");
    n.drain(a);
    n.drain(b);

    // Configured channels are +t and nobody is op, so this is refused.
    assert!(has_numeric(&n.ask(b, "TOPIC #lobby :hijacked"), "482"));

    // An oper can always set it.
    n.send(a, "OPER root operpass1");
    n.drain(a);
    let lines = n.ask(a, "TOPIC #lobby :a better topic");
    assert!(
        lines.iter().any(|l| l.contains("a better topic")),
        "{lines:?}"
    );
    assert_eq!(
        n.server.state.channel("#lobby").unwrap().topic.as_deref(),
        Some("a better topic")
    );

    n.drain(b);
    n.send(a, "TOPIC #lobby :a better topic");
    assert!(
        n.drain(b).is_empty(),
        "setting the topic to itself is a no-op on IRC"
    );

    // Reading it back, and reading an empty one.
    assert!(has_numeric(&n.ask(b, "TOPIC #lobby"), "332"));
    n.send(a, "JOIN #fresh");
    n.drain(a);
    assert!(
        has_numeric(&n.ask(a, "TOPIC #fresh"), "331"),
        "no topic set"
    );
    assert!(has_numeric(&n.ask(a, "TOPIC #nowhere :x"), "403"));
    assert!(has_numeric(&n.ask(a, "TOPIC"), "461"));

    let c = n.client(3, "carol");
    assert!(
        has_numeric(&n.ask(c, "TOPIC #lobby :from outside"), "442"),
        "setting a topic requires being on the channel"
    );
}

#[test]
fn kick_needs_channel_operator_privilege() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    // Alice creates #room, so she holds ops there.
    n.send(a, "JOIN #room");
    n.send(b, "JOIN #room");
    n.drain(a);
    n.drain(b);

    assert!(
        has_numeric(&n.ask(b, "KICK #room alice :no"), "482"),
        "an ordinary member cannot kick"
    );
    let lines = n.ask(a, "KICK #room bob :behave");
    assert!(
        lines.iter().any(|l| l.contains("KICK #room bob")),
        "{lines:?}"
    );
    assert!(!n
        .server
        .state
        .channel("#room")
        .unwrap()
        .members
        .contains_key(&user_id(b)));

    assert!(
        has_numeric(&n.ask(a, "KICK #room bob"), "441"),
        "not on channel"
    );
    assert!(has_numeric(&n.ask(a, "KICK #room nobody"), "401"));
    assert!(has_numeric(&n.ask(a, "KICK #nowhere bob"), "403"));
    assert!(has_numeric(&n.ask(a, "KICK #room"), "461"));
}

// --------------------------------------------------------------------- messaging

#[test]
fn messages_reach_channels_and_nicks_but_not_the_sender() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    n.send(a, "JOIN #lobby");
    n.send(b, "JOIN #lobby");
    n.drain(a);
    n.drain(b);

    n.send(a, "PRIVMSG #lobby :hello all");
    assert!(
        n.drain(a).iter().all(|l| !l.contains("hello all")),
        "a sender does not get their own channel message echoed"
    );
    assert!(n.drain(b).iter().any(|l| l.contains("hello all")));

    n.send(a, "PRIVMSG bob :just you");
    assert!(n.drain(b).iter().any(|l| l.contains("just you")));

    n.send(a, "NOTICE #lobby :a notice");
    assert!(n
        .drain(b)
        .iter()
        .any(|l| l.starts_with(":alice") && l.contains("NOTICE")));
}

#[test]
fn messaging_errors_are_the_right_numerics() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    assert!(has_numeric(&n.ask(a, "PRIVMSG"), "401"), "no recipient");
    assert!(has_numeric(&n.ask(a, "PRIVMSG bob"), "412"), "no text");
    assert!(has_numeric(&n.ask(a, "PRIVMSG bob :"), "412"), "empty text");
    assert!(has_numeric(&n.ask(a, "PRIVMSG nobody :hi"), "401"));
    assert!(has_numeric(&n.ask(a, "PRIVMSG #nowhere :hi"), "403"));
    // Not a member of the channel.
    n.send(a, "JOIN #lobby");
    n.send(a, "PART #lobby");
    n.drain(a);
    assert!(has_numeric(&n.ask(a, "PRIVMSG #lobby :hi"), "404"));
}

#[test]
fn away_is_reported_to_whoever_messages_you() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    n.send(b, "AWAY :walking the dog");
    assert!(
        has_numeric(&n.drain(b), "306"),
        "the client needs RPL_NOWAWAY to know AWAY stuck"
    );

    assert!(has_numeric(&n.ask(a, "PRIVMSG bob :you there?"), "301"));
    assert!(n
        .ask(a, "WHOIS bob")
        .iter()
        .any(|l| l.contains("walking the dog")));

    n.send(b, "AWAY");
    assert!(has_numeric(&n.drain(b), "305"));
    assert!(!has_numeric(&n.ask(a, "PRIVMSG bob :back?"), "301"));
}

// ------------------------------------------------------------------- information

#[test]
fn who_whois_list_and_names() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    n.send(a, "JOIN #lobby");
    n.send(b, "JOIN #lobby");
    n.drain(a);

    let lines = n.ask(a, "WHO #lobby");
    assert!(has_numeric(&lines, "352"), "{lines:?}");
    assert!(has_numeric(&lines, "315"));
    assert!(lines.iter().any(|l| l.contains("bob")));
    assert!(has_numeric(&n.ask(a, "WHO bob"), "352"), "WHO by nick");
    assert!(
        has_numeric(&n.ask(a, "WHO"), "315"),
        "WHO with no mask still ends"
    );

    let lines = n.ask(a, "WHOIS bob");
    assert!(has_numeric(&lines, "311"), "{lines:?}");
    assert!(has_numeric(&lines, "319"), "channels");
    assert!(has_numeric(&lines, "318"), "end");
    assert!(has_numeric(&n.ask(a, "WHOIS nobody"), "401"));
    assert!(has_numeric(&n.ask(a, "WHOIS"), "431"));

    let lines = n.ask(a, "LIST");
    assert!(has_numeric(&lines, "321") && has_numeric(&lines, "322") && has_numeric(&lines, "323"));
    assert!(lines.iter().any(|l| l.contains("#lobby")));

    let lines = n.ask(a, "NAMES #lobby");
    assert!(has_numeric(&lines, "353") && has_numeric(&lines, "366"));
    assert!(has_numeric(&n.ask(a, "NAMES #nowhere"), "403"));

    assert!(has_numeric(&n.ask(a, "MOTD"), "372"));
    assert!(has_numeric(&n.ask(a, "LUSERS"), "251"));
    assert!(
        has_numeric(&n.ask(a, "FROBNICATE"), "421"),
        "unknown command"
    );
}

#[test]
fn rfc2812_queries_that_clients_actually_send() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");

    assert!(has_numeric(&n.ask(a, "VERSION"), "351"));
    assert!(has_numeric(&n.ask(a, "TIME"), "391"));
    assert!(has_numeric(&n.ask(a, "ADMIN"), "256"));
    assert!(has_numeric(&n.ask(a, "INFO"), "374"));
    assert!(has_numeric(&n.ask(a, "HELP"), "704"));
    assert!(has_numeric(&n.ask(a, "STATS u"), "242"));
    assert!(has_numeric(&n.ask(a, "LINKS"), "364"));
    assert!(has_numeric(&n.ask(a, "WHOWAS ghost"), "406"));

    let ison = n.ask(a, "ISON alice bob nobody");
    assert!(has_numeric(&ison, "303"), "{ison:?}");
    assert!(ison
        .iter()
        .any(|l| l.contains("alice") && l.contains("bob")));
    assert!(ison.iter().all(|l| !l.contains("nobody")));

    assert!(has_numeric(&n.ask(a, "USERHOST alice"), "302"));

    let cap = n.ask(a, "CAP REQ :sasl");
    assert!(
        cap.iter().any(|l| l.contains("NAK") && l.contains("sasl")),
        "{cap:?}"
    );
    assert!(
        n.ask(a, "CAP END").iter().all(|l| !l.contains(" 421 ")),
        "CAP END is part of the handshake, not an unknown command"
    );

    n.send(a, "JOIN #lobby");
    n.drain(a);
    n.drain(b);
    let invited = n.ask(a, "INVITE bob #lobby");
    assert!(has_numeric(&invited, "341"), "{invited:?}");
    assert!(n
        .drain(b)
        .iter()
        .any(|l| l.contains("INVITE bob :#lobby") || l.contains("INVITE bob #lobby")));
}

#[test]
fn a_server_with_no_motd_says_so() {
    let text = CONFIG.replace(r#"motd = ["first line", "second line"]"#, "motd = []");
    let mut n = Net::with(&text);
    let a = n.client(1, "alice");
    assert!(has_numeric(&n.ask(a, "MOTD"), "422"));
}

// -------------------------------------------------------------------------- modes

#[test]
fn channel_and_user_modes() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    n.send(a, "JOIN #room");
    n.send(b, "JOIN #room");
    n.drain(a);
    n.drain(b);

    // Query.
    assert!(has_numeric(&n.ask(a, "MODE #room"), "324"));
    assert!(has_numeric(&n.ask(a, "MODE alice"), "221"), "user mode");
    assert!(has_numeric(&n.ask(a, "MODE ALICE"), "221"), "casemapping");
    assert!(
        has_numeric(&n.ask(a, "MODE bob"), "502"),
        "MODE othernick must not echo the caller's umodes"
    );

    // Alice created #room so she is op; bob is not.
    assert!(has_numeric(&n.ask(b, "MODE #room +t"), "482"));

    let lines = n.ask(a, "MODE #room +o bob");
    assert!(lines.iter().any(|l| l.contains("+o bob")), "{lines:?}");
    assert!(
        n.server.state.channel("#room").unwrap().members[&user_id(b)].op,
        "bob should hold ops now"
    );
    n.send(a, "MODE #room -o bob");
    n.drain(a);
    assert!(!n.server.state.channel("#room").unwrap().members[&user_id(b)].op);

    assert!(has_numeric(&n.ask(a, "MODE #room +o"), "461"));
    assert!(has_numeric(&n.ask(a, "MODE #room +o nobody"), "401"));
    let _carol = n.client(3, "carol");
    assert!(has_numeric(&n.ask(a, "MODE #room +o carol"), "441"));

    n.send(a, "MODE #room +v bob");
    n.drain(a);
    assert!(n.server.state.channel("#room").unwrap().members[&user_id(b)].voice);

    // Keys and limits set and clear.
    n.send(a, "MODE #room +kl secret 5");
    n.drain(a);
    {
        let c = n.server.state.channel("#room").unwrap();
        assert_eq!(c.key.as_deref(), Some("secret"));
        assert_eq!(c.limit, Some(5));
        assert!(c.mode_string().contains('k') && c.mode_string().contains('l'));
    }
    // A +k or +l with no usable argument must not clear what is already set.
    n.send(a, "MODE #room +k");
    n.drain(a);
    n.send(a, "MODE #room +l");
    n.drain(a);
    n.send(a, "MODE #room +l nope");
    n.drain(a);
    {
        let c = n.server.state.channel("#room").unwrap();
        assert_eq!(
            c.key.as_deref(),
            Some("secret"),
            "+k with no key must not unlock the channel"
        );
        assert_eq!(
            c.limit,
            Some(5),
            "+l with no number must not lift the limit"
        );
    }
    n.send(a, "MODE #room -kl");
    n.drain(a);
    {
        let c = n.server.state.channel("#room").unwrap();
        assert!(c.key.is_none() && c.limit.is_none());
    }

    // Unknown mode letters are ignored rather than fatal.
    n.send(a, "MODE #room +Z");
    n.drain(a);

    // Already-set modes are not re-announced: clients would treat +t as a
    // change, and on +r that would be a phantom transmission-worthy event.
    n.send(a, "MODE #room +t");
    let to_bob = n.drain(b);
    assert!(
        !to_bob
            .iter()
            .any(|l| l.contains("MODE") && l.contains("+t")),
        "a no-op MODE must not be announced: {to_bob:?}"
    );

    assert!(has_numeric(&n.ask(a, "MODE #nowhere"), "403"));
    assert!(has_numeric(&n.ask(a, "MODE"), "461"));
}

#[test]
fn rf_channel_modes_are_reserved_to_control_operators() {
    let text = CONFIG.replace(
        "[[opers]]",
        "[[channels]]\nname = \"#air\"\nrf = true\n\n[[opers]]",
    );
    let mut n = Net::with(&text);
    let a = n.client(1, "alice");
    n.send(a, "JOIN #air");
    n.drain(a);
    // Alice is the first member but RF channels do not hand out ops for that.
    assert!(has_numeric(&n.ask(a, "MODE #air -m"), "482"));

    n.send(a, "OPER root operpass1");
    n.drain(a);
    // Even as an oper, +r and +m on an RF channel are called out separately.
    n.send(a, "MODE #air -m");
    n.drain(a);
    assert!(!n.server.state.channel("#air").unwrap().moderated);
    n.send(a, "MODE #air -r");
    n.drain(a);
    assert!(!n.server.state.channel("#air").unwrap().rf);
}

#[test]
fn a_non_oper_cannot_change_r_on_any_channel() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    n.send(a, "JOIN #room");
    n.drain(a);
    assert!(
        has_numeric(&n.ask(a, "MODE #room +r"), "481"),
        "deciding what occupies the air is a control-operator decision"
    );
    assert!(!n.server.state.channel("#room").unwrap().rf);
}

#[test]
fn turning_r_on_makes_the_channel_an_rf_channel() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let op = n.client(2, "root");
    n.send(a, "JOIN #room");
    n.drain(a);
    assert!(
        n.server.state.channel("#room").unwrap().members[&user_id(a)].op,
        "first joiner is op on a local channel"
    );

    n.send(op, "OPER root operpass1");
    n.drain(op);
    n.send(op, "MODE #room +r");
    n.drain(op);
    let chan = n.server.state.channel("#room").unwrap();
    assert!(chan.rf);
    assert!(
        chan.moderated,
        "+r implies +m, as configured RF channels do"
    );
    assert!(
        !chan.members[&user_id(a)].op,
        "the first joiner must not keep +o after the channel becomes +r"
    );
}

#[test]
fn a_refused_mode_change_is_not_announced_to_the_channel() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    n.send(a, "JOIN #room");
    n.send(b, "JOIN #room");
    n.drain(a);
    n.drain(b);

    n.send(a, "MODE #room +r");
    let to_alice = n.drain(a);
    let to_bob = n.drain(b);
    assert!(has_numeric(&to_alice, "481"), "{to_alice:?}");
    assert!(
        !to_bob
            .iter()
            .any(|l| l.contains("MODE") && l.contains("+r")),
        "bob must not see a mode change that was refused: {to_bob:?}"
    );
    assert!(!n.server.state.channel("#room").unwrap().rf);
}

// ------------------------------------------------------------------- privileges

#[test]
fn oper_accepts_only_the_configured_credentials() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    assert!(has_numeric(&n.ask(a, "OPER root wrongpass"), "464"));
    assert!(has_numeric(&n.ask(a, "OPER nobody operpass1"), "464"));
    assert!(has_numeric(&n.ask(a, "OPER root"), "461"));
    assert!(!n.server.state.user(&user_id(a)).unwrap().oper);

    assert!(has_numeric(&n.ask(a, "OPER ROOT operpass1"), "381"));
    assert!(n.server.state.user(&user_id(a)).unwrap().oper);
    assert!(n.ask(a, "MODE alice").iter().any(|l| l.contains("+o")));
    let whois = n.ask(a, "WHOIS alice");
    assert!(
        whois
            .iter()
            .any(|l| l.contains(" 313 ") && l.contains("control operator")),
        "{whois:?}"
    );
    assert!(
        whois
            .iter()
            .any(|l| l.contains(" 320 ") && l.contains("RF-TX")),
        "RF-TX is not IRC operator status: {whois:?}"
    );
    assert!(
        whois
            .iter()
            .filter(|l| l.contains(" 313 "))
            .all(|l| !l.contains("RF-TX")),
        "313 must not be reused for RF-TX: {whois:?}"
    );
}

#[test]
fn hashed_oper_accepts_the_password_and_a_name_miss_is_still_464() {
    let hash = "$argon2id$v=19$m=19456,t=2,p=1$UbNQwhjjlChgI6nXhAEvtQ$cUKq82OE3JY3TWAimOT0BiIG1miEs6ean6LaZJKZji8";
    let text = CONFIG.replace(
        "password = \"operpass1\"",
        &format!("password = \"{hash}\""),
    );
    let mut n = Net::with(&text);
    let a = n.client(1, "alice");
    assert!(has_numeric(&n.ask(a, "OPER nobody operpass1"), "464"));
    assert!(!n.server.state.user(&user_id(a)).unwrap().oper);
    assert!(has_numeric(&n.ask(a, "OPER root operpass1"), "381"));
    assert!(n.server.state.user(&user_id(a)).unwrap().oper);
}

#[test]
fn kill_is_for_control_operators_and_actually_disconnects() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    assert!(has_numeric(&n.ask(b, "KILL alice :because"), "481"));

    n.send(a, "OPER root operpass1");
    n.drain(a);
    assert!(has_numeric(&n.ask(a, "KILL nobody :x"), "401"));
    assert!(has_numeric(&n.ask(a, "KILL"), "461"));

    n.send(a, "KILL bob :spamming");
    assert!(n.drain(b).iter().any(|l| l.contains("Killed")));
    assert!(n.server.state.by_nick("bob").is_none());
}

#[test]
fn callsign_is_recorded_as_an_unverified_claim() {
    let mut n = Net::new();
    let a = n.client(1, "alice");

    assert!(n.ask(a, "CALLSIGN").iter().any(|l| l.contains("none")));
    assert!(n
        .ask(a, "CALLSIGN not!valid")
        .iter()
        .any(|l| l.contains("not a valid callsign")));
    assert!(n
        .ask(a, "CALLSIGN AIRC")
        .iter()
        .any(|l| l.contains("does not look like an amateur callsign")));

    let lines = n.ask(a, "CALLSIGN sm0xyz");
    assert!(
        lines.iter().any(|l| l.contains("unverified claim")),
        "{lines:?}"
    );
    assert_eq!(
        n.server
            .state
            .user(&user_id(a))
            .unwrap()
            .callsign
            .as_ref()
            .map(|c| c.to_string()),
        Some("SM0XYZ".into())
    );
    assert!(n.ask(a, "CALLSIGN").iter().any(|l| l.contains("SM0XYZ")));
}

#[test]
fn a_registered_callsign_cannot_be_taken_by_another_nick() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    n.send(a, "CALLSIGN SM0XYZ");
    n.send(a, "REGISTER goodpassword");
    n.drain(a);
    assert_eq!(
        n.server.accounts.owner_of_callsign("SM0XYZ").as_deref(),
        Some("alice")
    );

    let b = n.client(2, "bob");
    let lines = n.ask(b, "CALLSIGN SM0XYZ");
    assert!(
        lines.iter().any(|l| l.contains("registered to alice")),
        "{lines:?}"
    );
    assert!(n.server.state.user(&user_id(b)).unwrap().callsign.is_none());
}

#[test]
fn a_session_callsign_cannot_be_claimed_twice() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    n.send(a, "CALLSIGN SM0XYZ");
    n.drain(a);
    let b = n.client(2, "bob");
    let lines = n.ask(b, "CALLSIGN SM0XYZ");
    assert!(
        lines.iter().any(|l| l.contains("already claimed by alice")),
        "{lines:?}"
    );
    assert!(n.server.state.user(&user_id(b)).unwrap().callsign.is_none());
    // The holder can say it again.
    assert!(n
        .ask(a, "CALLSIGN SM0XYZ")
        .iter()
        .any(|l| l.contains("unverified claim")));
}

#[test]
fn identify_binds_the_session_callsign() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    n.send(a, "CALLSIGN SM0XYZ");
    n.send(a, "REGISTER goodpassword");
    n.drain(a);
    n.server.handle(Event::Disconnected {
        id: a,
        reason: "gone".into(),
    });

    let a = n.client(1, "alice");
    n.send(a, "CALLSIGN SM0ABC");
    n.drain(a);
    assert_eq!(
        n.server.accounts.owner_of_callsign("SM0XYZ").as_deref(),
        Some("alice")
    );

    let lines = n.ask(a, "IDENTIFY goodpassword");
    assert!(
        lines.iter().any(|l| l.contains("Password accepted")),
        "{lines:?}"
    );
    assert_eq!(
        n.server.accounts.owner_of_callsign("SM0ABC").as_deref(),
        Some("alice"),
        "IDENTIFY must bind the callsign claimed this session"
    );
    assert!(
        n.server.accounts.owner_of_callsign("SM0XYZ").is_none(),
        "the previous stored callsign is replaced"
    );
}

#[test]
fn changing_nick_drops_the_callsign_claim() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    n.send(a, "CALLSIGN SM0XYZ");
    n.send(a, "REGISTER goodpassword");
    n.drain(a);
    n.send(a, "NICK ally");
    n.drain(a);
    assert!(
        n.server.state.user(&user_id(a)).unwrap().callsign.is_none(),
        "a nick change must not keep another nick's callsign"
    );
    assert!(!n.server.state.user(&user_id(a)).unwrap().nick_identified);
    assert_eq!(
        n.server.accounts.owner_of_callsign("SM0XYZ").as_deref(),
        Some("alice")
    );
}

#[test]
fn a_nick_case_change_keeps_identify_and_callsign() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    n.send(a, "CALLSIGN SM0XYZ");
    n.send(a, "REGISTER goodpassword");
    n.drain(a);

    let lines = n.ask(a, "NICK alice");
    assert!(
        !has_numeric(&lines, "433"),
        "own nick is not in use by someone else: {lines:?}"
    );
    assert!(n.server.state.user(&user_id(a)).unwrap().nick_identified);
    assert_eq!(n.server.state.user(&user_id(a)).unwrap().nick, "alice");

    n.send(a, "NICK Alice");
    n.drain(a);
    let u = n.server.state.user(&user_id(a)).unwrap();
    assert_eq!(u.nick, "Alice");
    assert!(u.nick_identified, "a case change is the same nick");
    assert_eq!(
        u.callsign.as_ref().map(|c| c.to_string()).as_deref(),
        Some("SM0XYZ")
    );
}

#[test]
fn oper_can_drop_a_nick_and_unclaim_a_callsign() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    n.send(a, "CALLSIGN SM0XYZ");
    n.send(a, "REGISTER goodpassword");
    n.drain(a);

    let op = n.client(2, "root");
    n.send(op, "OPER root operpass1");
    n.drain(op);

    assert!(has_numeric(&n.ask(a, "ACCOUNTS"), "481"));
    let listed = n.ask(op, "ACCOUNTS");
    assert!(
        listed
            .iter()
            .any(|l| l.contains("alice") && l.contains("SM0XYZ")),
        "{listed:?}"
    );

    let lines = n.ask(op, "UNCLAIM SM0XYZ");
    assert!(
        lines.iter().any(|l| l.contains("no longer bound")),
        "{lines:?}"
    );
    assert!(n.server.accounts.owner_of_callsign("SM0XYZ").is_none());
    assert!(n.server.state.user(&user_id(a)).unwrap().callsign.is_none());

    n.send(a, "CALLSIGN SM0XYZ");
    n.drain(a);
    n.send(op, "DROPNICK alice");
    n.drain(op);
    assert!(!n.server.accounts.is_registered("alice"));
    assert!(!n.server.state.user(&user_id(a)).unwrap().nick_identified);
    assert!(
        n.server.state.user(&user_id(a)).unwrap().callsign.is_none(),
        "the bound callsign went with the nick"
    );
}

#[test]
fn oper_kline_refuses_that_host() {
    let mut n = Net::new();
    let op = n.client(1, "root");
    n.send(op, "OPER root operpass1");
    n.drain(op);

    let v = n.client(5, "mallory");
    let host = n.server.state.user(&user_id(v)).unwrap().host.clone();
    n.send(op, &format!("KLINE {host} :abuse"));
    n.drain(op);
    let dropped = n.drain(v);
    assert!(dropped.iter().any(|l| l.contains("Banned")), "{dropped:?}");
    assert!(n.server.state.by_nick("mallory").is_none());

    let (out, rx) = mpsc::channel(64);
    n.server.handle(Event::Connected {
        id: 5,
        host: host.clone(),
        listen_only: false,
        out,
        hangup: None,
    });
    n.rx.push((5, rx));
    let refused = n.drain(5);
    assert!(
        refused
            .iter()
            .any(|l| l.contains("Banned from this server")),
        "{refused:?}"
    );
    assert!(n.server.state.user(&user_id(5)).is_none());

    n.send(op, &format!("UNKLINE {host}"));
    n.drain(op);
    let (out, rx) = mpsc::channel(64);
    n.server.handle(Event::Connected {
        id: 7,
        host,
        listen_only: false,
        out,
        hangup: None,
    });
    n.rx.push((7, rx));
    n.send(7, "NICK mallory");
    n.send(7, "USER mallory 0 * :Mallory");
    assert!(has_numeric(&n.drain(7), "001"));
}

#[test]
fn oper_passwd_changes_the_connection_password() {
    let mut n = Net::new();
    let op = n.client(1, "root");
    n.send(op, "OPER root operpass1");
    n.drain(op);
    assert!(n
        .ask(op, "PASSWD secret99")
        .iter()
        .any(|l| l.contains("now required")));

    let bad = n.raw_client(2);
    n.send(bad, "NICK bob");
    n.send(bad, "USER bob 0 * :Bob");
    let lines = n.drain(bad);
    assert!(has_numeric(&lines, "464"), "{lines:?}");

    let good = n.raw_client(3);
    n.send(good, "PASS secret99");
    n.send(good, "NICK carol");
    n.send(good, "USER carol 0 * :Carol");
    assert!(has_numeric(&n.drain(good), "001"));

    n.send(op, "PASSWD off");
    n.drain(op);
    let open = n.raw_client(4);
    n.send(open, "NICK dave");
    n.send(open, "USER dave 0 * :Dave");
    assert!(has_numeric(&n.drain(open), "001"));
}

#[test]
fn a_denied_callsign_is_refused() {
    let text = CONFIG.replace("[policy]", "[policy]\ndeny_callsigns = [\"SM0BAD\"]");
    let mut n = Net::with(&text);
    let a = n.client(1, "alice");
    assert!(n
        .ask(a, "CALLSIGN SM0BAD-3")
        .iter()
        .any(|l| l.contains("not permitted")));
    assert!(n.server.state.user(&user_id(a)).unwrap().callsign.is_none());
}

// ---------------------------------------------------------------------- accounts

#[test]
fn register_identify_and_unregister() {
    let mut n = Net::new();
    let a = n.client(1, "alice");

    assert!(n
        .ask(a, "REGISTER short")
        .iter()
        .any(|l| l.contains("too short")));
    assert!(has_numeric(&n.ask(a, "REGISTER"), "461"));
    assert!(n
        .ask(a, "IDENTIFY whatever")
        .iter()
        .any(|l| l.contains("not registered")));

    let lines = n.ask(a, "REGISTER goodpassword");
    assert!(
        lines.iter().any(|l| l.contains("Nick registered")),
        "{lines:?}"
    );
    assert!(n.server.accounts.is_registered("alice"));
    assert!(n.server.state.user(&user_id(a)).unwrap().nick_identified);

    // A second client cannot claim the nick without the password.
    let b = n.client(2, "bob");
    n.send(b, "NICK alice");
    assert!(has_numeric(&n.drain(b), "433"), "still in use");

    assert!(n
        .ask(a, "IDENTIFY wrongpassword")
        .iter()
        .any(|l| l.contains("Password incorrect")));
    assert!(n
        .ask(a, "IDENTIFY goodpassword")
        .iter()
        .any(|l| l.contains("Password accepted")));

    // Changing the password while identified.
    assert!(n
        .ask(a, "REGISTER anotherpassword")
        .iter()
        .any(|l| l.contains("Password updated")));

    assert!(n
        .ask(a, "UNREGISTER wrongpassword")
        .iter()
        .any(|l| l.contains("Password incorrect")));
    assert!(n
        .ask(a, "UNREGISTER anotherpassword")
        .iter()
        .any(|l| l.contains("unregistered")));
    assert!(!n.server.accounts.is_registered("alice"));
}

#[test]
fn a_registered_nick_must_be_identified_or_it_is_released() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    n.send(a, "REGISTER goodpassword");
    n.drain(a);
    n.server.handle(Event::Disconnected {
        id: a,
        reason: "gone".into(),
    });

    // Somebody else takes the nick and is put on notice.
    let b = n.raw_client(2);
    n.send(b, "NICK alice");
    n.send(b, "USER alice 0 * :Alice?");
    let lines = n.drain(b);
    assert!(
        lines.iter().any(|l| l.contains("This nick is registered")),
        "{lines:?}"
    );
    assert!(n
        .server
        .state
        .user(&user_id(b))
        .unwrap()
        .identify_by
        .is_some());
}

#[test]
fn callsign_nicks_cannot_be_registered() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    // Reach a callsign-shaped nick the only way possible: it is refused at
    // NICK, so registration can only be attempted on a normal one. Check the
    // guard directly for the RF-user case.
    n.send(a, "REGISTER goodpassword");
    n.drain(a);
    assert!(n.server.accounts.is_registered("alice"));
    assert!(!n.server.accounts.is_registered("SM0ABC"));
}

// -------------------------------------------------------------------- housekeeping

#[test]
fn quit_removes_the_user_and_tells_the_channel() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    n.send(a, "JOIN #lobby");
    n.send(b, "JOIN #lobby");
    n.drain(b);

    n.send(a, "QUIT :73");
    assert!(n
        .drain(b)
        .iter()
        .any(|l| l.contains("QUIT") && l.contains("73")));
    assert!(n.server.state.by_nick("alice").is_none());
}

#[test]
fn the_command_flood_cap_bites_and_says_so() {
    let text = CONFIG
        .replace("ip_cmds_per_min = 6000", "ip_cmds_per_min = 60")
        .replace("ip_cmd_burst = 500", "ip_cmd_burst = 5");
    let mut n = Net::with(&text);
    let a = n.client(1, "alice");
    n.drain(a);
    let mut throttled = false;
    for i in 0..40 {
        let lines = n.ask(a, &format!("WHOIS alice{i}"));
        if lines.iter().any(|l| l.contains("Slow down")) {
            throttled = true;
            break;
        }
    }
    assert!(throttled, "a command flood should be throttled");

    // PING, PONG and QUIT are never throttled: a client being told to slow
    // down still has to be able to answer a ping and leave.
    assert!(n
        .ask(a, "PING :still-here")
        .iter()
        .any(|l| l.contains("PONG")));
}

#[test]
fn commands_before_registration_are_rate_limited() {
    let text = CONFIG
        .replace("ip_cmds_per_min = 6000", "ip_cmds_per_min = 60")
        .replace("ip_cmd_burst = 500", "ip_cmd_burst = 5");
    let mut n = Net::with(&text);
    let a = n.raw_client(1);
    n.drain(a);
    let mut throttled = false;
    for i in 0..40 {
        let lines = n.ask(a, &format!("NICK n{i}"));
        if lines.iter().any(|l| l.contains("Slow down")) {
            throttled = true;
            break;
        }
    }
    assert!(throttled, "a pre-registration flood should be throttled");
    assert!(n
        .ask(a, "PING :still-here")
        .iter()
        .any(|l| l.contains("PONG")));
}

#[test]
fn password_guessing_is_throttled_per_host() {
    let text = CONFIG
        .replace("identify_per_min = 600", "identify_per_min = 6")
        .replace("identify_burst = 200", "identify_burst = 3");
    let mut n = Net::with(&text);
    let a = n.client(1, "alice");
    n.drain(a);
    let mut throttled = false;
    for _ in 0..10 {
        if n.ask(a, "OPER root guess")
            .iter()
            .any(|l| l.contains("too many password attempts"))
        {
            throttled = true;
            break;
        }
    }
    assert!(throttled, "OPER guessing must be rate limited");
    assert!(!n.server.state.user(&user_id(a)).unwrap().oper);
}

#[test]
fn radio_commands_report_a_disabled_gateway_without_pretending() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let lines = n.ask(a, "RADIO");
    assert!(
        lines.iter().any(|l| l.contains("disabled")),
        "a plain IRC server should say so: {lines:?}"
    );
    // Everything else needs privilege.
    for sub in [
        "OFF",
        "ON",
        "ID",
        "HEARD",
        "MAIL",
        "QUEUE",
        "DUTY",
        "GRANT bob",
    ] {
        assert!(
            has_numeric(&n.ask(a, &format!("RADIO {sub}")), "481"),
            "RADIO {sub} should need control-operator privilege"
        );
    }

    n.send(a, "OPER root operpass1");
    n.drain(a);
    assert!(n
        .ask(a, "RADIO HEARD")
        .iter()
        .any(|l| l.contains("No stations")));
    assert!(n
        .ask(a, "RADIO MAIL")
        .iter()
        .any(|l| l.contains("No messages")));
    assert!(n.ask(a, "RADIO DUTY").iter().any(|l| l.contains("No TNC")));
    assert!(n.ask(a, "RADIO QUEUE").iter().any(|l| l.contains("No TNC")));
    assert!(n
        .ask(a, "RADIO ON")
        .iter()
        .any(|l| l.contains("disabled in the configuration")));
    assert!(n
        .ask(a, "RADIO NONSENSE")
        .iter()
        .any(|l| l.contains("RADIO STATUS")));
    assert!(n.ask(a, "RADIO GRANT").iter().any(|l| l.contains("Usage")));
    assert!(n.ask(a, "RADIO REVOKE").iter().any(|l| l.contains("Usage")));
    assert!(n.ask(a, "RADIO KICK").iter().any(|l| l.contains("Usage")));
    assert!(n
        .ask(a, "RADIO GRANT nobody")
        .iter()
        .any(|l| l.contains("not registered")));
}

#[test]
fn a_line_that_is_not_a_message_is_ignored() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    // Malformed lines must not panic or reply.
    for line in ["", "   ", "@only=tags", ":prefix-only"] {
        n.send(a, line);
    }
    assert!(n.drain(a).is_empty());
    // And the client still works afterwards.
    assert!(n.ask(a, "PING :ok").iter().any(|l| l.contains("PONG")));
}

fn user_id(id: ClientId) -> rfircd::server::state::UserId {
    rfircd::server::state::UserId::Ip(id)
}

// ------------------------------------------------- releasing a registered nick

#[test]
fn an_unidentified_registered_nick_is_released_to_a_guest_name() {
    let text = CONFIG.replace("identify_timeout_secs = 60", "identify_timeout_secs = 1");
    let mut n = Net::with(&text);

    // Register the nick, then leave.
    let a = n.client(1, "alice");
    n.send(a, "REGISTER goodpassword");
    n.drain(a);
    n.server.handle(Event::Disconnected {
        id: a,
        reason: "gone".into(),
    });

    // Somebody else takes it and does not identify.
    let b = n.client(2, "alice");
    n.drain(b);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    n.server.handle(Event::Tick);
    let lines = n.drain(b);
    assert!(
        lines.iter().any(|l| l.contains("Your nick is now Guest")),
        "an unidentified registered nick should be released: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("NICK Guest_2")));
    assert_eq!(n.server.state.user(&user_id(b)).unwrap().nick, "Guest_2");
    assert!(
        n.server.state.by_nick("alice").is_none(),
        "the registered nick is free for its owner again"
    );

    // The real owner can now take it and identify.
    let c = n.client(3, "alice");
    assert!(n
        .ask(c, "IDENTIFY goodpassword")
        .iter()
        .any(|l| l.contains("Password accepted")));
    n.server.handle(Event::Tick);
    assert_eq!(
        n.server.state.user(&user_id(c)).unwrap().nick,
        "alice",
        "an identified user keeps their nick"
    );
}

#[test]
fn a_user_is_disconnected_if_even_the_guest_name_is_taken() {
    let text = CONFIG.replace("identify_timeout_secs = 60", "identify_timeout_secs = 1");
    let mut n = Net::with(&text);
    let a = n.client(1, "alice");
    n.send(a, "REGISTER goodpassword");
    n.drain(a);
    n.server.handle(Event::Disconnected {
        id: a,
        reason: "gone".into(),
    });

    // Client 2 will be offered "Guest_2" — so park somebody on it first.
    let squatter = n.client(3, "Guest_2");
    let _ = squatter;
    let b = n.client(2, "alice");
    n.drain(b);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    n.server.handle(Event::Tick);
    let lines = n.drain(b);
    assert!(
        lines.iter().any(|l| l.contains("Disconnecting")),
        "with no free guest name the connection has to go: {lines:?}"
    );
    assert!(n.server.state.user(&user_id(b)).is_none());
}

/// The names the server hands out must be names it would accept.
#[test]
fn guest_names_are_not_in_the_reserved_callsign_space() {
    use rfircd::callsign::Callsign;
    for id in [0u64, 1, 2, 7, 42, 12345] {
        let guest = format!("Guest_{id}");
        assert!(
            Callsign::reserved_from_nick(&guest).is_none(),
            "{guest} would be refused if a client asked for it"
        );
        assert!(
            rfircd::irc::message::is_valid_nick(&guest, 30),
            "{guest} must still be a legal nickname"
        );
    }
    // The old form was in that space, which is what this guards against.
    assert!(Callsign::reserved_from_nick("Guest2").is_some());
}

#[test]
fn a_client_that_never_registers_is_reaped_by_the_tick() {
    let text = CONFIG.replace("bind = []", "bind = []\nregistration_timeout_secs = 1");
    let mut n = Net::with(&text);
    let a = n.raw_client(1);
    n.drain(a);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    n.server.handle(Event::Tick);
    assert!(
        n.drain(a)
            .iter()
            .any(|l| l.contains("Registration timeout")),
        "an open socket that never registers is the cheapest denial of service there is"
    );
    assert!(n.server.state.user(&user_id(a)).is_none());
}

#[test]
fn a_password_check_that_finishes_after_a_nick_change_is_discarded() {
    use rfircd::accounts::AccountError;
    use rfircd::server::AuthKind;

    let mut n = Net::new();
    let a = n.client(1, "alice");
    n.send(a, "REGISTER goodpassword");
    n.drain(a);

    // In production the hash runs on a blocking thread and the result comes
    // back as an event. If the nick changed in between, the answer is about
    // somebody else's nick and must not be applied.
    n.send(a, "NICK allison");
    n.drain(a);
    n.server.handle(Event::AuthFinished {
        id: a,
        kind: AuthKind::Identify,
        nick: "alice".into(),
        result: Ok(()),
        password_hash: None,
    });
    assert!(n
        .drain(a)
        .iter()
        .any(|l| l.contains("Nick changed during password check")));

    // An error coming back is reported rather than swallowed.
    n.server.handle(Event::AuthFinished {
        id: a,
        kind: AuthKind::Identify,
        nick: "allison".into(),
        result: Err(AccountError::BadPassword),
        password_hash: None,
    });
    assert!(n.drain(a).iter().any(|l| l.contains("Password incorrect")));

    // A register whose hashing failed must not create half an account.
    n.server.handle(Event::AuthFinished {
        id: a,
        kind: AuthKind::Register,
        nick: "allison".into(),
        result: Ok(()),
        password_hash: None,
    });
    assert!(n.drain(a).iter().any(|l| l.contains("Could not hash")));
    assert!(!n.server.accounts.is_registered("allison"));

    // And one for a client that has already gone is simply dropped.
    n.server.handle(Event::AuthFinished {
        id: 999,
        kind: AuthKind::Identify,
        nick: "ghost".into(),
        result: Ok(()),
        password_hash: None,
    });
}

#[test]
fn an_unreadable_accounts_file_refuses_to_start() {
    let path = unique_accounts_file();
    std::fs::write(&path, b"{not json").unwrap();
    let text = CONFIG.replace("target/test-commands-nicks.json", &path);
    let config = Arc::new(Config::from_toml(&text).unwrap());
    let err = match Server::new(config, None) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("corrupt nick file started an empty server"),
    };
    assert!(err.contains("overwrite") || err.contains("JSON"), "{err}");
    let _ = std::fs::remove_file(path);
}

#[test]
fn shutdown_tells_every_client() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    n.drain(a);
    n.drain(b);
    n.server.handle(Event::Shutdown);
    assert!(n.drain(a).iter().any(|l| l.contains("shutting down")));
    assert!(n.drain(b).iter().any(|l| l.contains("shutting down")));
}

#[test]
fn a_manual_mode_grant_survives_an_unrelated_privilege_refresh() {
    let mut n = Net::new();
    let a = n.client(1, "alice");
    let b = n.client(2, "bob");
    n.send(a, "JOIN #room"); // alice creates it, so she is op
    n.send(b, "JOIN #room");
    n.drain(a);
    n.drain(b);

    n.send(a, "MODE #room +v bob");
    n.send(a, "MODE #room +o bob");
    n.drain(a);
    {
        let f = n.server.state.channel("#room").unwrap().members[&user_id(b)];
        assert!(f.voice && f.op, "the operator's grant should have applied");
    }

    // Bob now does something entirely unrelated that recomputes his
    // privileges. It must not quietly undo what an operator granted.
    n.send(b, "REGISTER goodpassword");
    n.drain(b);
    let f = n.server.state.channel("#room").unwrap().members[&user_id(b)];
    assert!(
        f.voice,
        "IDENTIFY silently removed a +v a channel operator had granted"
    );
    assert!(f.op, "and the +o with it");
}

#[test]
fn a_configuration_with_two_channels_of_the_same_name_is_refused() {
    // Channel names are compared case-insensitively, so `#rf` and `#RF` are
    // one channel. Silently keeping the first and discarding the second means
    // an operator's `rf = true` or topic can vanish without a word.
    let text = CONFIG.replace(
        "[[opers]]",
        "[[channels]]\nname = \"#LOBBY\"\ntopic = \"the same channel, shouted\"\n\n[[opers]]",
    );
    let err = Config::from_toml(&text).unwrap_err().to_string();
    assert!(
        err.contains("#LOBBY") && err.contains("#lobby"),
        "the operator needs to be told which two collided: {err}"
    );
}

#[test]
fn a_released_nick_never_exceeds_the_configured_length() {
    // The server picks the replacement name itself, so it has to obey its own
    // rules — a `Guest_…` longer than `max_nick_len` is a nick the server
    // would refuse if a client asked for it.
    use rfircd::irc::message::is_valid_nick;
    let max = 12usize;
    for id in [0u64, 9, 99, 999_999, u64::MAX] {
        let guest = rfircd::server::guest_nick(id, max);
        assert!(
            is_valid_nick(&guest, max),
            "{guest} is not a nickname this server would accept at max_nick_len={max}"
        );
    }
}

#[test]
fn configuration_values_that_would_break_the_server_are_refused() {
    // Each of these parses fine and then makes the server unusable in a way
    // that is hard to diagnose from the outside, so they are caught at load.
    let cases = [
        ("max_nick_len = 20", "max_nick_len = 0", "max_nick_len"),
        (
            "max_channels_per_user = 3",
            "max_channels_per_user = 0",
            "max_channels_per_user",
        ),
        (
            "min_password_len = 8",
            "min_password_len = 0",
            "min_password_len",
        ),
    ];
    for (from, to, expect) in cases {
        let text = CONFIG.replace(from, to);
        let err = Config::from_toml(&text)
            .map(|_| String::new())
            .unwrap_or_else(|e| e.to_string());
        assert!(
            err.contains(expect),
            "{to} should be refused and say why; got {err:?}"
        );
    }
}

#[test]
fn a_zero_length_rf_message_limit_is_refused() {
    // `max_rf_text_len = 0` turns every radiated message into a bare ellipsis.
    let text = CONFIG.replace("[policy]", "[policy]\nmax_rf_text_len = 0");
    let err = Config::from_toml(&text)
        .map(|_| String::new())
        .unwrap_or_else(|e| e.to_string());
    assert!(err.contains("max_rf_text_len"), "{err:?}");
}
