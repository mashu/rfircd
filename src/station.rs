//! The client side of the gateway, for an operator with a radio and a TNC.
//!
//! It speaks AIRC/1 (see `docs/protocol.md`) over KISS and presents a plain
//! line-oriented interface, so it works over ssh, on a Pi with no screen, or
//! piped into anything else.
//!
//! ```sh
//! rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --channel '#rf'
//! rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --tnc tcp://192.168.1.10:8001
//! rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --tnc serial:/dev/ttyUSB0@9600
//! ```
//!
//! Commands: `/join #chan`, `/part #chan`, `/names [#chan]`, `/msg <nick> …`,
//! `/who`, `/ping`, `/quit`. Anything else is sent to the current channel.

use std::time::{Duration, Instant};

use crate::airc::frame::flags;
use crate::airc::{encode_fields, AircFrame, Kind, Sessions};
use crate::ax25::tnc::{self, TncLink};
use crate::ax25::AirtimeConfig;
use crate::ax25::Ax25Frame;
use crate::ax25::Keyed;
use crate::callsign::Callsign;

pub struct Args {
    pub call: Callsign,
    pub gateway: Callsign,
    pub channel: Option<String>,
    pub link: TncLink,
    pub paclen: usize,
    pub path: Vec<Callsign>,
    pub quiet: bool,
    /// Same airtime limits the gateway applies to itself. A station is a
    /// human typing rather than an automatic service, but the finals do not
    /// know the difference, and on HF the operator is running the same QRP
    /// radio at the same 300 baud.
    pub airtime: AirtimeConfig,
}

/// What `main` should do after parsing the command line.
pub enum Invocation {
    Run(Box<Args>),
    Help(String),
    Usage(String),
}

pub fn usage() -> String {
    String::from(
        "usage: rfirc-station --call <CALL-SSID> --gateway <CALL-SSID> [options]

options:
  --tnc <spec>        tcp://host:port (default tcp://127.0.0.1:8001)
                      serial:/dev/ttyUSB0@9600   (needs --features serial)
  --channel <#chan>   join this channel at startup
  --path <A,B>        digipeater path, at most two hops
  --paclen <n>        AX.25 information field limit (default 128)
  --quiet             do not print protocol chatter
  --help

airtime (protects your finals and the channel; see docs/airtime.md):
  --baud <n>          on-air symbol rate, 300 for HF, 1200 for VHF (default 300)
  --txdelay <ms>      key-up before data, pushed to the TNC (default 400)
  --txtail <ms>       key-down after data, pushed to the TNC (default 300)
  --duty <percent>    duty cycle ceiling, 1-50 (default 25)
  --max-continuous <s>  longest unbroken transmit run (default 30)
  --cooldown <s>      forced key-up after that run (default 60)",
    )
}

/// Parse a command line. Returns the error text rather than printing and
/// exiting, so the parsing rules can be tested.
pub fn parse_args<I: IntoIterator<Item = String>>(argv: I) -> Invocation {
    match parse_inner(argv) {
        Ok(Some(args)) => Invocation::Run(Box::new(args)),
        Ok(None) => Invocation::Help(usage()),
        Err(e) => Invocation::Usage(e.to_string()),
    }
}

/// `Ok(None)` means `--help` was asked for.
fn parse_inner<I: IntoIterator<Item = String>>(argv: I) -> anyhow::Result<Option<Args>> {
    let mut call = None;
    let mut gateway = None;
    let mut channel = None;
    let mut tnc_spec = "tcp://127.0.0.1:8001".to_string();
    let mut paclen = 128usize;
    let mut path = Vec::new();
    let mut quiet = false;
    let mut airtime = AirtimeConfig::default();

    let mut args = argv.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--call" => call = args.next(),
            "--gateway" => gateway = args.next(),
            "--channel" => channel = args.next(),
            "--tnc" => tnc_spec = args.next().unwrap_or(tnc_spec),
            "--paclen" => paclen = args.next().and_then(|v| v.parse().ok()).unwrap_or(paclen),
            "--path" => {
                if let Some(v) = args.next() {
                    for hop in v.split(',').filter(|s| !s.is_empty()) {
                        path.push(hop.parse::<Callsign>()?);
                    }
                }
            }
            "--quiet" => quiet = true,
            "--baud" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    airtime.baud = v;
                }
            }
            "--txdelay" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    airtime.txdelay = Duration::from_millis(v);
                }
            }
            "--txtail" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    airtime.txtail = Duration::from_millis(v);
                }
            }
            "--duty" => {
                if let Some(v) = args.next().and_then(|v| v.parse::<u32>().ok()) {
                    if v == 0 || f64::from(v) / 100.0 > crate::ax25::airtime::HARD_MAX_DUTY {
                        anyhow::bail!(
                            "--duty must be between 1 and {} (a QMX will not survive more)",
                            (crate::ax25::airtime::HARD_MAX_DUTY * 100.0) as u32
                        );
                    }
                    airtime.max_duty = f64::from(v) / 100.0;
                }
            }
            "--max-continuous" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    airtime.max_continuous = Duration::from_secs(v);
                }
            }
            "--cooldown" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    airtime.cooldown = Duration::from_secs(v);
                }
            }
            "--help" | "-h" => return Ok(None),
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }

    let (Some(call), Some(gateway)) = (call, gateway) else {
        anyhow::bail!("--call and --gateway are both required");
    };
    if path.len() > 2 {
        anyhow::bail!("more than two digipeater hops is antisocial");
    }
    // The same check the gateway makes on itself: the duty ceiling is clamped
    // in the governor, but the run/cooldown pair is a second, independent way
    // to hold the transmitter keyed.
    airtime
        .check_hardware_safe()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if !crate::ax25::airtime::STANDARD_PACKET_BAUDS.contains(&airtime.baud) {
        anyhow::bail!(
            "--baud {} is not 300, 1200 or 9600; a mismatch with the modem \
             under-counts key-down time. Those three are the packet rates.",
            airtime.baud
        );
    }
    for (name, ms) in [
        ("--txdelay", airtime.txdelay.as_millis()),
        ("--txtail", airtime.txtail.as_millis()),
    ] {
        if ms > 2550 {
            anyhow::bail!("{name} is {ms} ms; KISS carries it in 10 ms units, so 2550 is the max");
        }
    }

    let link = if let Some(rest) = tnc_spec.strip_prefix("tcp://") {
        let (host, port) = rest.rsplit_once(':').unwrap_or((rest, "8001"));
        TncLink::Tcp {
            host: host.to_string(),
            port: port.parse()?,
        }
    } else if let Some(rest) = tnc_spec.strip_prefix("serial:") {
        let (device, baud) = rest.split_once('@').unwrap_or((rest, "9600"));
        let _ = (device, baud);
        #[cfg(feature = "serial")]
        {
            TncLink::Serial {
                path: device.to_string(),
                baud: baud.parse()?,
            }
        }
        #[cfg(not(feature = "serial"))]
        {
            anyhow::bail!("this build has no serial support; rebuild with --features serial")
        }
    } else {
        anyhow::bail!("unrecognised --tnc spec: {tnc_spec}");
    };

    Ok(Some(Args {
        call: call.parse()?,
        gateway: gateway.parse()?,
        channel,
        link,
        paclen,
        path,
        quiet,
        airtime,
    }))
}

fn random_epoch() -> u16 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(1);
    (nanos as u16) ^ ((nanos >> 16) as u16)
}

pub struct Station {
    pub args: Args,
    tnc: tnc::TncHandle,
    sessions: Sessions,
    current: Option<String>,
    /// Lines for the operator's terminal.
    ///
    /// Collected rather than printed, so the protocol logic can be tested by
    /// asserting on what the station would have said. `main` drains it.
    output: Vec<String>,
    epoch: u16,
    last_rx: Option<Instant>,
    last_hello: Instant,
    keyed_rx: Option<tokio::sync::mpsc::Receiver<Keyed>>,
}

impl Station {
    pub fn new(args: Args, tnc: tnc::TncHandle, sessions: Sessions) -> Self {
        let keyed_rx = tnc.take_keyed();
        Self {
            args,
            tnc,
            sessions,
            current: None,
            output: Vec::new(),
            epoch: random_epoch(),
            last_rx: None,
            last_hello: Instant::now(),
            keyed_rx,
        }
    }

    /// Everything the station prints goes through here, and everything it
    /// prints is filtered — including the lines it composed itself.
    ///
    /// An AIRC field is `String::from_utf8_lossy` of whatever was on the air.
    /// `encode_fields` strips the separator and CR/LF/NUL, but that is the
    /// *sender's* courtesy and a hostile sender simply does not call it, so
    /// `ESC ] 0 ; … BEL` from an unidentified station used to reach the
    /// operator's terminal verbatim. Filtering at the one place output leaves
    /// the station — rather than at each of the twenty `say` call sites —
    /// is what makes that true of paths added later as well.
    fn say(&mut self, line: String) {
        self.output
            .push(crate::policy::strip_terminal_controls(&line));
    }

    /// Take everything the station has to say since the last call.
    pub fn drain_output(&mut self) -> Vec<String> {
        std::mem::take(&mut self.output)
    }

    pub fn current_channel(&self) -> Option<&str> {
        self.current.as_deref()
    }

    pub fn set_channel(&mut self, channel: Option<String>) {
        self.current = channel;
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        let keyed: Vec<Keyed> = match &mut self.keyed_rx {
            Some(rx) => {
                let mut out = Vec::new();
                while let Ok(k) = rx.try_recv() {
                    out.push(k);
                }
                out
            }
            None => Vec::new(),
        };
        for k in keyed {
            self.sessions.on_keyed(&k.dest, k.seq, now);
        }
        // Resend HELLO after we have been in a QSO and then gone quiet — a
        // HELLO lost in a collision is the one frame later traffic cannot
        // recover. Do not fire on a station that has never heard anyone:
        // the initial HELLO is already in flight and has its own ACK timer.
        if let Some(heard) = self.last_rx {
            if now.saturating_duration_since(heard) > Duration::from_secs(5 * 60)
                && now.saturating_duration_since(self.last_hello) > Duration::from_secs(60)
            {
                self.send_hello();
                self.last_hello = now;
            }
        }
        let outcome = self.sessions.tick(now);
        for (call, f) in outcome.transmit {
            self.transmit_to(&call, f);
        }
        for (call, f) in self.sessions.drain_acks() {
            self.transmit_to(&call, f);
        }
        !outcome.lost.is_empty()
    }

    pub fn send_hello(&mut self) {
        let epoch = format!("{:04X}", self.epoch);
        self.send(
            Kind::Hello,
            encode_fields(&["rfirc-station/1", &epoch]),
            true,
        );
        self.last_hello = Instant::now();
    }
}

impl Station {
    /// Queue a unicast for `dest`. Control traffic goes to the gateway;
    /// a private message to another RF nick goes to that callsign.
    pub fn send_to(&mut self, dest: &Callsign, kind: Kind, payload: Vec<u8>, reliable: bool) {
        let now = Instant::now();
        let frames = self.sessions.send(dest, kind, payload, reliable, now);
        for f in frames {
            self.transmit_to(dest, f);
        }
        for (call, f) in self.sessions.drain_acks() {
            self.transmit_to(&call, f);
        }
    }

    /// Queue a message for the gateway.
    pub fn send(&mut self, kind: Kind, payload: Vec<u8>, reliable: bool) {
        let gateway = self.args.gateway.clone();
        self.send_to(&gateway, kind, payload, reliable);
    }

    /// Channel chat: one broadcast to `AIRC`, not a unicast to the gateway.
    /// Every station in range hears it; the gateway ingests it as uplink.
    fn send_chat(&mut self, payload: Vec<u8>) {
        let now = Instant::now();
        let dest = protocol_dest();
        // Sequence numbers are per sender, not per destination. Borrowing the
        // gateway peer for seq allocation is a lie about the AX.25 dest, but
        // it keeps one space for unicast and broadcast.
        let gateway = self.args.gateway.clone();
        let frames = self.sessions.send(&gateway, Kind::Msg, payload, false, now);
        for f in frames {
            self.transmit_to(&dest, f);
        }
    }

    fn transmit_to(&mut self, dest: &Callsign, frame: AircFrame) {
        match Ax25Frame::ui(
            self.args.call.clone(),
            dest.clone(),
            &self.args.path,
            frame.encode(),
        ) {
            Ok(ax) => {
                if !self.tnc.try_send(ax) {
                    self.say("!! transmit queue full, message dropped".into());
                }
            }
            Err(e) => self.say(format!("!! cannot build frame: {e}")),
        }
    }

    pub fn handle_input(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() {
            return true;
        }
        let (cmd, rest) = match line.split_once(' ') {
            Some((c, r)) => (c, r.trim()),
            None => (line, ""),
        };
        match cmd {
            "/quit" | "/q" => {
                self.send(Kind::Quit, encode_fields(&[rest]), false);
                return false;
            }
            "/join" | "/j" => {
                if rest.is_empty() {
                    self.say("usage: /join #channel".into());
                } else {
                    self.send(Kind::Join, encode_fields(&[rest]), true);
                    self.current = Some(rest.to_string());
                }
            }
            "/part" => {
                let chan = if rest.is_empty() {
                    self.current.clone().unwrap_or_default()
                } else {
                    rest.to_string()
                };
                if !chan.is_empty() {
                    self.send(Kind::Part, encode_fields(&[&chan]), true);
                    if self.current.as_deref() == Some(chan.as_str()) {
                        self.current = None;
                    }
                }
            }
            "/names" | "/who" => {
                let chan = if rest.is_empty() {
                    self.current.clone().unwrap_or_default()
                } else {
                    rest.to_string()
                };
                if chan.is_empty() {
                    self.say("join a channel first".into());
                } else {
                    self.send(Kind::Names, encode_fields(&[&chan]), true);
                }
            }
            "/msg" | "/m" => match rest.split_once(' ') {
                Some((who, text)) if !text.is_empty() => {
                    if let Some(dest) = rf_dest(who) {
                        self.send_to(&dest, Kind::Msg, encode_fields(&[who, text]), true);
                    } else {
                        self.send(Kind::Msg, encode_fields(&[who, text]), true);
                    }
                    self.say(format!("-> {who}: {text}"));
                }
                _ => self.say("usage: /msg <nick> <text>".into()),
            },
            "/ping" => self.send(Kind::Ping, encode_fields(&["s"]), false),
            "/help" => {
                self.say("/join #chan  /part  /names  /msg <nick> <text>  /ping  /quit".into());
            }
            _ if cmd.starts_with('/') => self.say(format!("unknown command: {cmd}")),
            _ => {
                let Some(chan) = self.current.clone() else {
                    self.say("join a channel first: /join #rf".into());
                    return true;
                };
                // Chat is a broadcast so every station in range hears it,
                // not only the gateway. Unreliable: a retransmission arriving
                // thirty seconds late is noise.
                self.send_chat(encode_fields(&[&chan, line]));
            }
        }
        true
    }

    pub fn handle_rf(&mut self, ax: Ax25Frame) {
        if ax.source.call == self.args.call {
            return;
        }
        // Unicast traffic for somebody else on frequency is not ours to read
        // or acknowledge. A protocol address such as AIRC is a broadcast and
        // never looks like an amateur callsign.
        let dest = &ax.destination.call;
        if dest.looks_like_amateur_call() && *dest != self.args.call {
            return;
        }
        let Ok(frame) = AircFrame::decode(&ax.info) else {
            return;
        };
        let now = Instant::now();
        self.last_rx = Some(now);
        let src = ax.source.call.clone();
        let addressed_to_us = *dest == self.args.call;
        let outcome = self.sessions.on_receive(&src, frame, now, addressed_to_us);
        for f in outcome.transmit {
            self.transmit_to(&src, f);
        }
        let Some(msg) = outcome.deliver else {
            return;
        };
        let f = msg.fields();
        let stale = if msg.flags & flags::RETRY != 0 {
            " (repeat)"
        } else {
            ""
        };
        match msg.kind {
            Kind::Msg | Kind::Notice => {
                let from_call = src.to_nick();
                let (target, from, text) = if f.len() >= 3 {
                    (
                        f.first().cloned().unwrap_or_default(),
                        f.get(1).cloned().unwrap_or_default(),
                        f.get(2).cloned().unwrap_or_default(),
                    )
                } else {
                    (
                        f.first().cloned().unwrap_or_default(),
                        from_call,
                        f.get(1).cloned().unwrap_or_default(),
                    )
                };
                if target.starts_with('#') || target.starts_with('&') {
                    self.say(format!("{target} <{from}> {text}{stale}"));
                } else {
                    self.say(format!("*{from}* {text}{stale}"));
                }
            }
            Kind::Stored => {
                let from = f.get(1).cloned().unwrap_or_default();
                let text = f.get(2).cloned().unwrap_or_default();
                let age = f
                    .get(3)
                    .and_then(|a| a.parse::<u64>().ok())
                    .map(human_age)
                    .unwrap_or_else(|| "earlier".into());
                self.say(format!("*{from}* [held {age}] {text}"));
            }
            Kind::Welcome => {
                self.say(format!(
                    "-- connected to {} : {}",
                    f.first().cloned().unwrap_or_default(),
                    f.get(1).cloned().unwrap_or_default()
                ));
            }
            Kind::NamesReply => {
                // Two shapes share this kind: a join confirmation, which
                // carries a member *count* ("12 here"), and the answer to an
                // explicit /names, which carries the (capped) list. Joining
                // does not read out the roll — that is airtime nobody asked
                // for — so use /names when you actually want to know.
                let chan = f.first().cloned().unwrap_or_default();
                let body = f.get(1).cloned().unwrap_or_default();
                if body.ends_with("here") {
                    self.say(format!("-- joined {chan} ({body}; /names to list them)"));
                } else {
                    self.say(format!("-- {chan} members: {body}"));
                }
                if let Some(topic) = f.get(2).filter(|t| !t.is_empty()) {
                    self.say(format!("-- {chan} topic: {topic}"));
                }
            }
            Kind::Presence => {
                let sign = f.get(2).cloned().unwrap_or_default();
                let verb = if sign == "+" { "joined" } else { "left" };
                self.say(format!(
                    "-- {} {verb} {}",
                    f.get(1).cloned().unwrap_or_default(),
                    f.first().cloned().unwrap_or_default()
                ));
            }
            Kind::Error => self.say(format!("!! {}", f.join(" "))),
            Kind::Pong if !self.args.quiet => self.say("-- pong".into()),
            Kind::Id if !self.args.quiet => self.say(format!("-- {}", f.join(" "))),
            _ => {}
        }
    }
}

fn protocol_dest() -> Callsign {
    "AIRC".parse().unwrap()
}

/// Direct RF when the nick is a callsign (`SM0XYZ|1` or `SM0XYZ-1`). IRC
/// nicks go via the gateway.
fn rf_dest(who: &str) -> Option<Callsign> {
    who.parse::<Callsign>()
        .ok()
        .or_else(|| Callsign::from_nick(who).ok())
        .filter(|c| c.looks_like_amateur_call())
}

pub fn human_age(secs: u64) -> String {
    match secs {
        0..=90 => format!("{secs}s"),
        91..=5400 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airc::frame::flags;
    use crate::airc::SessionConfig;
    use crate::ax25::TncConfig;

    fn args(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|s| s.to_string()).collect()
    }

    fn parsed(argv: &[&str]) -> Args {
        match parse_args(args(argv)) {
            Invocation::Run(a) => *a,
            Invocation::Help(_) => panic!("unexpected --help"),
            Invocation::Usage(e) => panic!("unexpected usage error: {e}"),
        }
    }

    fn usage_error(argv: &[&str]) -> String {
        match parse_args(args(argv)) {
            Invocation::Usage(e) => e,
            Invocation::Help(_) => panic!("expected an error, got help"),
            Invocation::Run(_) => panic!("expected an error, got a valid parse"),
        }
    }

    const MIN: &[&str] = &["--call", "SM0ABC-7", "--gateway", "SK0MT-1"];

    #[test]
    fn parses_a_minimal_command_line() {
        let a = parsed(MIN);
        assert_eq!(a.call.to_string(), "SM0ABC-7");
        assert_eq!(a.gateway.to_string(), "SK0MT-1");
        assert_eq!(a.paclen, 128);
        assert!(a.path.is_empty());
        assert!(!a.quiet);
        assert!(
            matches!(a.link, TncLink::Tcp { ref host, port } if host == "127.0.0.1" && port == 8001)
        );
    }

    #[test]
    fn parses_every_option() {
        let a = parsed(&[
            "--call",
            "SM0ABC-7",
            "--gateway",
            "SK0MT-1",
            "--channel",
            "#rf",
            "--tnc",
            "tcp://10.0.0.5:9001",
            "--paclen",
            "200",
            "--path",
            "SK0MT-2,SK0AA-1",
            "--quiet",
            "--baud",
            "1200",
            "--txdelay",
            "250",
            "--txtail",
            "120",
            "--duty",
            "40",
            "--max-continuous",
            "45",
            "--cooldown",
            "90",
        ]);
        assert_eq!(a.channel.as_deref(), Some("#rf"));
        assert_eq!(a.paclen, 200);
        assert_eq!(a.path.len(), 2);
        assert!(a.quiet);
        assert!(
            matches!(a.link, TncLink::Tcp { ref host, port } if host == "10.0.0.5" && port == 9001)
        );
        assert_eq!(a.airtime.baud, 1200);
        assert_eq!(a.airtime.txdelay, Duration::from_millis(250));
        assert_eq!(a.airtime.txtail, Duration::from_millis(120));
        assert_eq!(a.airtime.max_duty, 0.40);
        assert_eq!(a.airtime.max_continuous, Duration::from_secs(45));
        assert_eq!(a.airtime.cooldown, Duration::from_secs(90));
    }

    #[test]
    fn rejects_what_it_should() {
        assert!(usage_error(&["--call", "SM0ABC-7"]).contains("required"));
        assert!(usage_error(&["--nonsense"]).contains("unknown argument"));
        assert!(usage_error(&[
            "--call",
            "SM0ABC-7",
            "--gateway",
            "SK0MT-1",
            "--path",
            "A,B,C"
        ])
        .contains("two digipeater hops"));
        assert!(usage_error(&[
            "--call",
            "NOTACALL",
            "--gateway",
            "SK0MT-1",
            "--tnc",
            "carrier-pigeon://x"
        ])
        .contains("unrecognised --tnc"));
        // The airtime limits are enforced here, not only in the gateway.
        assert!(usage_error(&[
            "--call",
            "SM0ABC-7",
            "--gateway",
            "SK0MT-1",
            "--max-continuous",
            "60",
            "--cooldown",
            "5"
        ])
        .contains("burst duty cycle"));
        assert!(usage_error(&[
            "--call",
            "SM0ABC-7",
            "--gateway",
            "SK0MT-1",
            "--txdelay",
            "9000"
        ])
        .contains("2550"));
        assert!(
            usage_error(&["--call", "SM0ABC-7", "--gateway", "SK0MT-1", "--duty", "90"])
                .contains("1 and 50")
        );
        assert!(usage_error(&[
            "--call",
            "SM0ABC-7",
            "--gateway",
            "SK0MT-1",
            "--baud",
            "2400"
        ])
        .contains("300"));
        assert!(matches!(parse_args(args(&["--help"])), Invocation::Help(_)));
    }

    /// A station wired to a loopback TNC, with the far end for assertions.
    fn station() -> (Station, tokio::io::DuplexStream) {
        let a = parsed(MIN);
        let (link, far) = TncConfig::loopback_link();
        let cfg = TncConfig {
            link,
            tx_pacing: Duration::from_millis(0),
            airtime: AirtimeConfig {
                baud: 9600,
                txdelay: Duration::from_millis(10),
                txtail: Duration::from_millis(10),
                ..AirtimeConfig::default()
            },
            ..TncConfig::default()
        };
        let (tnc, _rx) = tnc::spawn(cfg);
        let sessions = Sessions::new(SessionConfig {
            paclen: a.paclen,
            ..Default::default()
        });
        (Station::new(a, tnc, sessions), far)
    }

    /// A frame as the gateway would transmit it.
    fn from_gateway(kind: Kind, seq: u16, fields: &[&str]) -> Ax25Frame {
        Ax25Frame::ui(
            "SK0MT-1".parse().unwrap(),
            "AIRC".parse().unwrap(),
            &[],
            AircFrame::new(kind, seq, encode_fields(fields)).encode(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn commands_produce_the_right_frames() {
        let (mut s, _far) = station();

        assert!(s.handle_input("/join #rf"));
        assert_eq!(s.current_channel(), Some("#rf"));

        assert!(s.handle_input("hello everyone"));
        assert!(s.handle_input("/msg SM0XYZ|1 direct message"));
        assert!(s.drain_output().iter().any(|l| l.contains("-> SM0XYZ|1")));

        assert!(s.handle_input("/names"));
        assert!(s.handle_input("/ping"));
        assert!(s.handle_input("/part"));
        assert_eq!(s.current_channel(), None, "/part leaves the channel");

        // /quit is the only input that ends the session.
        assert!(!s.handle_input("/quit 73"));
    }

    #[tokio::test]
    async fn unhelpful_input_is_answered_not_transmitted() {
        let (mut s, _far) = station();
        assert!(s.handle_input(""));
        assert!(s.drain_output().is_empty(), "blank lines say nothing");

        assert!(s.handle_input("/join"));
        assert!(s.drain_output().iter().any(|l| l.contains("usage: /join")));

        assert!(s.handle_input("/nonsense"));
        assert!(s
            .drain_output()
            .iter()
            .any(|l| l.contains("unknown command")));

        assert!(s.handle_input("talking before joining"));
        assert!(s
            .drain_output()
            .iter()
            .any(|l| l.contains("join a channel first")));

        assert!(s.handle_input("/msg onlyanick"));
        assert!(s.drain_output().iter().any(|l| l.contains("usage: /msg")));

        assert!(s.handle_input("/help"));
        assert!(s.drain_output().iter().any(|l| l.contains("/join")));
    }

    #[tokio::test]
    async fn traffic_from_the_gateway_is_rendered() {
        let (mut s, _far) = station();

        s.handle_rf(from_gateway(Kind::Welcome, 1, &["test.gateway", "welcome"]));
        assert!(s
            .drain_output()
            .iter()
            .any(|l| l.contains("connected to test.gateway")));

        s.handle_rf(from_gateway(Kind::Msg, 2, &["#rf", "alice", "hello there"]));
        let out = s.drain_output();
        assert!(
            out.iter().any(|l| l == "#rf <alice> hello there"),
            "channel traffic should read like a channel: {out:?}"
        );

        s.handle_rf(from_gateway(Kind::Msg, 3, &["SM0ABC|7", "bob", "just you"]));
        assert!(s.drain_output().iter().any(|l| l == "*bob* just you"));

        // A join confirmation carries a count; an explicit NAMES carries names.
        s.handle_rf(from_gateway(
            Kind::NamesReply,
            4,
            &["#rf", "12 here", "the topic"],
        ));
        let out = s.drain_output();
        assert!(
            out.iter().any(|l| l.contains("joined #rf (12 here")),
            "{out:?}"
        );
        assert!(out.iter().any(|l| l.contains("topic: the topic")));

        s.handle_rf(from_gateway(
            Kind::NamesReply,
            5,
            &["#rf", "@alice,+SM0XYZ|1", ""],
        ));
        assert!(s
            .drain_output()
            .iter()
            .any(|l| l.contains("members: @alice,+SM0XYZ|1")));

        s.handle_rf(from_gateway(
            Kind::Stored,
            6,
            &["SM0ABC|7", "carol", "held for you", "7200"],
        ));
        let out = s.drain_output();
        assert!(out.iter().any(|l| l.contains("[held 2h]")), "{out:?}");

        s.handle_rf(from_gateway(Kind::Presence, 7, &["#rf", "dave", "+"]));
        assert!(s
            .drain_output()
            .iter()
            .any(|l| l.contains("dave joined #rf")));
        s.handle_rf(from_gateway(Kind::Presence, 8, &["#rf", "dave", "-"]));
        assert!(s.drain_output().iter().any(|l| l.contains("dave left #rf")));

        s.handle_rf(from_gateway(Kind::Error, 9, &["403", "no such channel"]));
        assert!(s.drain_output().iter().any(|l| l.contains("!! 403")));
    }

    #[tokio::test]
    async fn a_retransmission_is_marked_as_one() {
        let (mut s, _far) = station();
        let mut ax = from_gateway(Kind::Msg, 20, &["#rf", "alice", "did you get this"]);
        let mut airc = AircFrame::decode(&ax.info).unwrap();
        airc.flags |= flags::RETRY;
        ax.info = airc.encode();
        s.handle_rf(ax);
        assert!(
            s.drain_output().iter().any(|l| l.ends_with("(repeat)")),
            "a repeat should be visible, so nobody answers the same message twice"
        );
    }

    #[tokio::test]
    async fn frames_that_are_not_ours_are_ignored() {
        let (mut s, _far) = station();

        // From another station entirely.
        let other = Ax25Frame::ui(
            "SM0XYZ-9".parse().unwrap(),
            "SK0MT-1".parse().unwrap(),
            &[],
            AircFrame::new(Kind::Msg, 30, encode_fields(&["#rf", "eve", "spoofed"])).encode(),
        )
        .unwrap();
        s.handle_rf(other);
        assert!(
            s.drain_output().is_empty(),
            "another station's unicast to the gateway is not ours to read"
        );

        // From the gateway, but unicast to somebody else. PROTOCOL.md §3.1:
        // reading it would poison our own duplicate-suppression window.
        let elsewhere = Ax25Frame::ui(
            "SK0MT-1".parse().unwrap(),
            "SM0QQQ-3".parse().unwrap(),
            &[],
            AircFrame::new(
                Kind::Msg,
                31,
                encode_fields(&["SM0QQQ|3", "bob", "private"]),
            )
            .encode(),
        )
        .unwrap();
        s.handle_rf(elsewhere);
        assert!(
            s.drain_output().is_empty(),
            "another station's unicast traffic is not ours to read"
        );

        // Not AIRC at all — an APRS beacon sharing the frequency.
        let aprs = Ax25Frame::ui(
            "SK0MT-1".parse().unwrap(),
            "AIRC".parse().unwrap(),
            &[],
            b"!5930.00N/01803.00E-".to_vec(),
        )
        .unwrap();
        s.handle_rf(aprs);
        assert!(s.drain_output().is_empty());
    }

    #[tokio::test]
    async fn a_quiet_station_says_less() {
        let (mut s, _far) = station();
        s.args.quiet = true;
        s.handle_rf(from_gateway(Kind::Pong, 40, &["x"]));
        s.handle_rf(from_gateway(Kind::Id, 41, &["SK0MT-1 gateway"]));
        assert!(
            s.drain_output().is_empty(),
            "--quiet suppresses protocol chatter"
        );

        s.args.quiet = false;
        s.handle_rf(from_gateway(Kind::Pong, 42, &["x"]));
        assert!(s.drain_output().iter().any(|l| l.contains("pong")));
    }

    fn from_peer(kind: Kind, seq: u16, fields: &[&str]) -> Ax25Frame {
        Ax25Frame::ui(
            "SM0XYZ-9".parse().unwrap(),
            "AIRC".parse().unwrap(),
            &[],
            AircFrame::new(kind, seq, encode_fields(fields)).encode(),
        )
        .unwrap()
    }

    async fn take_tx(far: &mut tokio::io::DuplexStream) -> Vec<Ax25Frame> {
        use crate::ax25::kiss::KissDecoder;
        use tokio::io::AsyncReadExt;
        let mut decoder = KissDecoder::new(4096);
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(Duration::from_millis(150), far.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    for kf in decoder.push(&buf[..n]) {
                        if kf.command != crate::ax25::kiss::CMD_DATA {
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

    #[tokio::test]
    async fn channel_chat_is_a_broadcast_not_a_unicast_to_the_gateway() {
        let (mut s, mut far) = station();
        s.handle_input("/join #rf");
        let _ = take_tx(&mut far).await;
        s.handle_input("hello everyone");
        let tx = take_tx(&mut far).await;
        assert!(
            tx.iter().any(|ax| ax.destination.call.to_string() == "AIRC"
                && AircFrame::decode(&ax.info)
                    .map(|f| f.kind == Kind::Msg
                        && f.fields().last() == Some(&"hello everyone".into()))
                    .unwrap_or(false)),
            "channel chat must be addressed to AIRC so every station hears it: {:?}",
            tx.iter()
                .map(|ax| ax.destination.call.to_string())
                .collect::<Vec<_>>()
        );
        assert!(
            !tx.iter()
                .any(|ax| ax.destination.call.to_string() == "SK0MT-1"
                    && AircFrame::decode(&ax.info)
                        .map(|f| f.kind == Kind::Msg)
                        .unwrap_or(false)),
            "unicasting chat to the gateway hides it from every other station"
        );
    }

    #[tokio::test]
    async fn a_peer_broadcast_is_shown() {
        let (mut s, _far) = station();
        s.handle_rf(from_peer(Kind::Msg, 4, &["#rf", "from the hillside"]));
        let out = s.drain_output();
        assert!(
            out.iter().any(|l| l == "#rf <SM0XYZ|9> from the hillside"),
            "stations in range must hear each other without the gateway repeating: {out:?}"
        );
    }

    #[tokio::test]
    async fn a_direct_message_to_a_callsign_is_unicast_to_them() {
        let (mut s, mut far) = station();
        s.handle_input("/msg SM0XYZ|9 just you");
        let tx = take_tx(&mut far).await;
        assert!(
            tx.iter()
                .any(|ax| ax.destination.call.to_string() == "SM0XYZ-9"),
            "a callsign nick is on the air, not an IRC hop through the gateway: {:?}",
            tx.iter()
                .map(|ax| ax.destination.call.to_string())
                .collect::<Vec<_>>()
        );
    }

    /// `encode_fields` is what a well-behaved sender uses. A hostile one
    /// writes the payload itself, and `fields()` is `from_utf8_lossy`, so
    /// anything at all can arrive — including the sequences that make a
    /// terminal act rather than draw.
    #[tokio::test]
    async fn escape_sequences_from_the_air_never_reach_the_terminal() {
        let (mut s, _far) = station();
        // [target, text] uplink shape, built by hand: OSC 0 (set window
        // title, which some terminals will report back onto the shell's input
        // line), CSI 2J (clear screen), and a right-to-left override.
        let mut payload = b"#rf".to_vec();
        payload.push(0x1F);
        payload.extend_from_slice("hi\u{1b}]0;pwned\u{7}\u{1b}[2J\u{202e}bye".as_bytes());
        let ax = Ax25Frame::ui(
            "SM0XYZ-9".parse().unwrap(),
            "AIRC".parse().unwrap(),
            &[],
            AircFrame::new(Kind::Msg, 42, payload).encode(),
        )
        .unwrap();
        s.handle_rf(ax);

        let out = s.drain_output();
        assert!(!out.is_empty(), "the message should still be shown");
        for line in &out {
            assert!(
                !line.chars().any(|c| c.is_control()),
                "a control character from the air reached the terminal: {line:?}"
            );
            assert!(
                !line.contains('\u{202e}'),
                "a bidi override from the air reached the terminal: {line:?}"
            );
        }
        // Filtered, not swallowed: the readable part still gets through.
        assert!(
            out.iter().any(|l| l.contains("hi") && l.contains("bye")),
            "{out:?}"
        );
    }

    /// The filter is on output, so it must not eat the client's own
    /// formatting — the help line is column-aligned with double spaces.
    #[tokio::test]
    async fn local_output_keeps_its_spacing() {
        let (mut s, _far) = station();
        s.handle_input("/help");
        let out = s.drain_output();
        assert!(
            out.iter().any(|l| l.contains("/join #chan  /part")),
            "the help line lost its alignment: {out:?}"
        );
    }

    #[test]
    fn ages_are_readable() {
        assert_eq!(human_age(0), "0s");
        assert_eq!(human_age(90), "90s");
        assert_eq!(human_age(91), "1m");
        assert_eq!(human_age(5400), "90m");
        assert_eq!(human_age(5401), "1h");
        assert_eq!(human_age(86_400), "24h");
    }

    #[tokio::test]
    async fn a_gateway_that_stops_answering_is_reported() {
        let (mut s, _far) = station();
        s.handle_input("/join #rf"); // reliable, so it waits for an ACK
                                     // The ACK clock starts at key-down. Give the loopback TNC a moment
                                     // to actually transmit so `on_keyed` can start it.
        tokio::time::sleep(Duration::from_millis(80)).await;
        s.tick(Instant::now());
        let mut now = Instant::now();
        let mut lost = false;
        for _ in 0..10 {
            now += Duration::from_secs(30);
            if s.tick(now) {
                lost = true;
                break;
            }
        }
        assert!(lost, "the station should notice a gateway that never ACKs");
    }
}
