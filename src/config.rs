//! Configuration. Everything an operator can get legally wrong (station ID
//! interval, gateway callsign, what may cross to RF) lives here and is
//! validated at startup rather than discovered on the air.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::callsign::Callsign;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub listen: ListenConfig,
    #[serde(default)]
    pub radio: RadioConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub opers: Vec<OperConfig>,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub accounts: AccountsConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Server name announced to clients, e.g. `rfirc.sk0mt.example`.
    pub name: String,
    #[serde(default = "default_network")]
    pub network: String,
    #[serde(default)]
    pub motd: Vec<String>,
    #[serde(default = "default_max_nick_len")]
    pub max_nick_len: usize,
    #[serde(default = "default_max_channels")]
    pub max_channels_per_user: usize,
    /// Optional connection password for IP clients (PASS).
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenConfig {
    #[serde(default = "default_bind")]
    pub bind: Vec<String>,
    /// Implicit TLS listeners (usually port 6697). Required for anyone who
    /// will speak, identify, OPER, or control the transmitter from off-box.
    #[serde(default)]
    pub tls: Option<TlsListenConfig>,
    #[serde(default = "default_ping_interval")]
    pub ping_interval_secs: u64,
    #[serde(default = "default_registration_timeout")]
    pub registration_timeout_secs: u64,
    /// Simultaneous IP connections from one host. 0 disables the cap.
    #[serde(default = "default_max_conns_per_host")]
    pub max_conns_per_host: u32,
    /// Simultaneous IP connections in total. 0 disables the cap.
    ///
    /// `max_conns_per_host` alone does not bound anything: an attacker with a
    /// hundred source addresses is under the per-host limit on every one of
    /// them. This is the limit that actually caps memory.
    #[serde(default = "default_max_clients")]
    pub max_clients: usize,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            tls: None,
            ping_interval_secs: default_ping_interval(),
            registration_timeout_secs: default_registration_timeout(),
            max_conns_per_host: default_max_conns_per_host(),
            max_clients: default_max_clients(),
        }
    }
}

/// Implicit TLS for internet IRC clients. Plaintext sockets from off-box are
/// listen-only; this is how they speak.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsListenConfig {
    pub bind: Vec<String>,
    /// PEM certificate chain (leaf first).
    pub cert: String,
    /// PEM private key.
    pub key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadioConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The gateway's own station callsign. This is the station that is
    /// transmitting, so this is the callsign that must be identified.
    #[serde(default)]
    pub callsign: String,
    /// AX.25 destination address for AIRC frames. Acts as a protocol marker
    /// so other users of the channel can filter us out.
    #[serde(default = "default_destination")]
    pub destination: String,
    /// Digipeater path, at most two hops by convention.
    #[serde(default)]
    pub path: Vec<String>,
    #[serde(default)]
    pub tnc: TncSection,
    /// Station identification interval. Must be <= 600 s.
    #[serde(default = "default_id_interval")]
    pub id_interval_secs: u64,
    #[serde(default = "default_id_text")]
    pub id_text: String,
    /// AX.25 information field limit.
    #[serde(default = "default_paclen")]
    pub paclen: usize,
    #[serde(default = "default_ack_timeout")]
    pub ack_timeout_secs: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_idle_timeout")]
    pub peer_idle_timeout_secs: u64,
    /// Stations remembered at once. A flood of unique sources cannot grow
    /// forever; the quietest peer is evicted when the table is full.
    #[serde(default = "default_max_peers")]
    pub max_peers: usize,
    /// Relay channel join/part notices to RF stations. Cheap on a quiet
    /// channel, expensive on a busy one.
    #[serde(default)]
    pub presence_notices: bool,
    /// Hold private messages for stations that are out of range and deliver
    /// them when the station is next heard.
    #[serde(default = "default_true")]
    pub mailbox_enabled: bool,
    /// Messages held per station.
    #[serde(default = "default_mailbox_per_station")]
    pub mailbox_per_station: usize,
    /// Messages held across all stations. A gateway is not a mail server.
    #[serde(default = "default_mailbox_total")]
    pub mailbox_total: usize,
    /// Held messages older than this are dropped.
    #[serde(default = "default_mailbox_ttl")]
    pub mailbox_ttl_secs: u64,
    /// Held messages delivered per exchange with a station. The rest wait for
    /// the next thing the station sends, so its own activity paces delivery
    /// rather than one HELLO releasing a minute of transmitting.
    #[serde(default = "default_mailbox_flush_batch")]
    pub mailbox_flush_batch: usize,
    /// Re-transmit messages that arrived from RF back onto RF, so that
    /// stations hidden from each other but both audible to the gateway can
    /// hold a conversation. Doubles the airtime of every RF message; leave it
    /// off unless you actually have a hidden-terminal problem.
    #[serde(default)]
    pub repeat_rf_traffic: bool,
    /// NOTICE the sender when a channel message is actually put on the air.
    #[serde(default = "default_true")]
    pub notice_air_relay: bool,
    /// Airtime the transmit queue may hold before new traffic is refused.
    ///
    /// This is the backlog limit, and it is deliberately in *seconds of
    /// airtime* rather than a frame count: sixty short frames and six long
    /// ones are very different amounts of transmitting. When the backlog is
    /// full, senders are told so immediately instead of having their message
    /// accepted and silently dropped at the transmitter minutes later.
    #[serde(default = "default_max_queued_airtime")]
    pub max_queued_airtime_secs: u64,
    /// Most names sent in reply to an explicit `NAMES` from a station.
    /// Member lists are never sent unasked.
    #[serde(default = "default_rf_names_max")]
    pub rf_names_max: usize,
    /// Answer APRS messages addressed to this gateway so a stock APRS radio
    /// can put a line into a bridged channel. On by default: a radio that
    /// gets no ACK retries, which is more airtime than the ACK. Set false
    /// to ignore APRS the way other foreign traffic is ignored.
    #[serde(default = "default_true")]
    pub aprs: bool,
    /// Channel used when the APRS message text has no `#channel` prefix.
    /// Empty (the default) means the prefix is required. Must name a
    /// configured `+r` channel.
    #[serde(default)]
    pub aprs_channel: String,
    /// How the gateway encodes traffic it transmits. Inbound always accepts
    /// both APRS and AIRC; only outbound encoding follows this setting.
    ///
    /// * `airc` (default) — one AIRC broadcast per channel line; APRS-only
    ///   peers still get addressed APRS copies so stock HTs hear the QSO.
    /// * `aprs` — channel and direct chat go out as APRS messages only
    ///   (one addressed frame per RF peer in the channel). No AIRC CQ.
    #[serde(default)]
    pub rf_mode: RfMode,
    /// External transmit interlock: a command that decides whether it is safe
    /// to key up at all. See [`InterlockConfig`].
    #[serde(default)]
    pub interlock: Option<InterlockConfig>,
    /// Airtime and duty-cycle limits. These protect the transmitter's finals
    /// and the shared channel; they are not the same thing as the per-user
    /// rate limits in `[policy]`.
    #[serde(default)]
    pub duty: DutyConfig,
}

/// A command that says whether it is safe to transmit.
///
/// rfircd cannot see the radio — it speaks KISS to a modem, and KISS carries
/// frames, not SWR readings — so the check is the operator's to supply. While
/// it fails, nothing is transmitted, station identification included.
///
/// The check fails closed: a command that cannot be run, times out, or has
/// not run yet counts as a failure.
///
/// ```toml
/// [radio.interlock]
/// command = "/usr/local/bin/check-swr"
/// args = ["--max", "2.5"]
/// interval_secs = 30
/// timeout_secs = 5
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterlockConfig {
    /// Executable to run. Not a shell line: use `sh -c "..."` in `args` if
    /// you want shell syntax.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_interlock_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_interlock_timeout")]
    pub timeout_secs: u64,
}

/// Transmitter airtime limits. Defaults are sized for a QRP HF station
/// (a QMX-class radio at 300 baud) because that is the configuration most
/// likely to be damaged by getting this wrong.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DutyConfig {
    /// Turn the governor off entirely. Only reasonable into a dummy load.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// On-air symbol rate. 300 for HF SSB packet, 1200 for VHF FM.
    #[serde(default = "default_duty_baud")]
    pub baud: u32,
    /// Keyed-but-idle time either side of the data, in milliseconds.
    ///
    /// This is the single source of truth for the station's key-up timing: the
    /// governor prices every frame with it, *and* it is pushed to the TNC as
    /// the KISS TXDELAY and TXTAIL parameters at connect, so the model and the
    /// hardware cannot disagree. At 300 baud these are a significant fraction
    /// of a short frame. KISS carries them in 10 ms units, so the ceiling is
    /// 2550 ms.
    #[serde(default = "default_txdelay_ms")]
    pub txdelay_ms: u64,
    #[serde(default = "default_txtail_ms")]
    pub txtail_ms: u64,
    /// Sliding window the duty cycle is measured over.
    #[serde(default = "default_duty_window")]
    pub window_secs: u64,
    /// Percentage of that window the station may be keyed for.
    #[serde(default = "default_max_duty_percent")]
    pub max_duty_percent: u32,
    /// Longest unbroken transmit run before the transmitter is forced off to
    /// let the power amplifier cool.
    #[serde(default = "default_max_continuous")]
    pub max_continuous_secs: u64,
    /// How long that enforced cooldown lasts.
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    /// Hard airtime ceiling per rolling hour. 0 disables it.
    #[serde(default = "default_hourly_airtime")]
    pub hourly_airtime_secs: u64,
    /// A frame the governor has held longer than this is dropped instead of
    /// being transmitted late. Stale traffic costs airtime and says nothing.
    #[serde(default = "default_max_hold")]
    pub max_hold_secs: u64,
    /// Permit a baud rate other than 300, 1200 or 9600. Without this, a
    /// mistyped rate (the usual QMX mistake is 1200 against a 300-baud modem)
    /// is refused at startup.
    #[serde(default)]
    pub allow_nonstandard_baud: bool,
}

impl Default for DutyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            baud: default_duty_baud(),
            txdelay_ms: default_txdelay_ms(),
            txtail_ms: default_txtail_ms(),
            window_secs: default_duty_window(),
            max_duty_percent: default_max_duty_percent(),
            max_continuous_secs: default_max_continuous(),
            cooldown_secs: default_cooldown(),
            hourly_airtime_secs: default_hourly_airtime(),
            max_hold_secs: default_max_hold(),
            allow_nonstandard_baud: false,
        }
    }
}

impl DutyConfig {
    pub fn to_airtime(&self) -> crate::ax25::AirtimeConfig {
        crate::ax25::AirtimeConfig {
            enabled: self.enabled,
            baud: self.baud.max(1),
            txdelay: Duration::from_millis(self.txdelay_ms),
            txtail: Duration::from_millis(self.txtail_ms),
            window: Duration::from_secs(self.window_secs.max(1)),
            max_duty: (self.max_duty_percent.min(100) as f64) / 100.0,
            max_continuous: Duration::from_secs(self.max_continuous_secs.max(1)),
            cooldown: Duration::from_secs(self.cooldown_secs),
            hourly_budget: Duration::from_secs(self.hourly_airtime_secs),
            max_hold: Duration::from_secs(self.max_hold_secs.max(1)),
            stuffing: 1.05,
        }
    }
}

impl Default for RadioConfig {
    fn default() -> Self {
        toml::from_str("").expect("RadioConfig defaults are self-consistent")
    }
}

/// On-air encoding for traffic this gateway transmits.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RfMode {
    /// Compact AIRC/1 (default). Mixed nets still fan out APRS to APRS peers.
    #[default]
    Airc,
    /// Stock APRS messages only. No AIRC chat or CQ.
    Aprs,
}

impl RfMode {
    pub fn is_airc(self) -> bool {
        matches!(self, RfMode::Airc)
    }

    pub fn is_aprs(self) -> bool {
        matches!(self, RfMode::Aprs)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TncSection {
    /// "tcp", "serial" or "loopback".
    #[serde(default = "default_tnc_kind")]
    pub kind: String,
    #[serde(default = "default_tnc_host")]
    pub host: String,
    #[serde(default = "default_tnc_port")]
    pub port: u16,
    #[serde(default)]
    pub device: String,
    #[serde(default = "default_baud")]
    pub baud: u32,
    #[serde(default)]
    pub kiss_port: u8,
    #[serde(default = "default_tx_pacing")]
    pub tx_pacing_ms: u64,
    /// KISS channel-access parameters, pushed to the TNC at connect.
    ///
    /// TXDELAY and TXTAIL are deliberately *not* here: they are key-down time,
    /// the governor has to know them to price a frame, and two places to write
    /// the same physical quantity is two places to get it wrong. They live in
    /// `[radio.duty]` and are pushed from there.
    #[serde(default)]
    pub persistence: Option<u8>,
    #[serde(default)]
    pub slottime: Option<u8>,
    /// Optional Direwolf config to cross-check against `[radio.duty]`.
    /// `--check` compares MODEM / TXDELAY / TXTAIL so the governor's cost
    /// model cannot silently disagree with the modem.
    #[serde(default)]
    pub direwolf_conf: Option<String>,
    /// Accepted only so that a configuration written before these moved gets
    /// an explanation instead of serde's "unknown field `txdelay`". Setting
    /// either is an error; see [`Config::validate`].
    #[serde(default)]
    pub txdelay: Option<u8>,
    #[serde(default)]
    pub txtail: Option<u8>,
}

impl Default for TncSection {
    fn default() -> Self {
        Self {
            kind: default_tnc_kind(),
            host: default_tnc_host(),
            port: default_tnc_port(),
            device: String::new(),
            baud: default_baud(),
            kiss_port: 0,
            tx_pacing_ms: default_tx_pacing(),
            persistence: None,
            slottime: None,
            direwolf_conf: None,
            txdelay: None,
            txtail: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    /// Longest message body relayed to RF. Longer messages are truncated and
    /// flagged, because a 400 character rant is 3 s of airtime.
    #[serde(default = "default_max_rf_text")]
    pub max_rf_text_len: usize,
    /// Token bucket per RF station.
    #[serde(default = "default_rf_msgs_per_min")]
    pub rf_msgs_per_min: u32,
    #[serde(default = "default_rf_burst")]
    pub rf_burst: u32,
    /// Same, for IP users sending into an RF-bridged channel. Keyed on the
    /// user's host, not their nickname — `/nick` is free.
    #[serde(default = "default_ip_msgs_per_min")]
    pub ip_to_rf_msgs_per_min: u32,
    /// Burst allowance for the limit above. It used to share `rf_burst` with
    /// the per-station limiter, which meant raising one silently raised the
    /// other and neither doc comment said so.
    #[serde(default = "default_ip_to_rf_burst")]
    pub ip_to_rf_burst: u32,
    /// Refuse to transmit text that looks like ciphertext or base64. Amateur
    /// rules in most countries forbid obscuring the meaning of a message.
    #[serde(default = "default_true")]
    pub block_apparent_ciphertext: bool,
    /// Callsigns that may not use the gateway at all.
    #[serde(default)]
    pub deny_callsigns: Vec<String>,
    /// If non-empty, only these callsigns may use the gateway.
    #[serde(default)]
    pub allow_callsigns: Vec<String>,
    /// IRC-side flood cap on +r channels (messages dropped, not just kept off the air).
    #[serde(default = "default_rf_channel_msgs")]
    pub rf_channel_msgs_per_min: u32,
    #[serde(default = "default_rf_channel_burst")]
    pub rf_channel_burst: u32,
    /// Commands per minute per IP nick (JOIN/PRIVMSG/MODE/...).
    #[serde(default = "default_ip_cmds_per_min")]
    pub ip_cmds_per_min: u32,
    #[serde(default = "default_ip_cmd_burst")]
    pub ip_cmd_burst: u32,
    /// Most AX.25 frames one radiated message may be split into.
    ///
    /// The real limit on message length, and stricter than
    /// `max_rf_text_len`: fragmentation multiplies both the airtime and the
    /// chance of loss, and a lost fragment costs the whole message. Two frames
    /// is a sentence, which is what packet is for.
    #[serde(default = "default_max_rf_fragments")]
    pub max_rf_fragments: usize,
    /// IDENTIFY / REGISTER / OPER guesses per minute per host.
    #[serde(default = "default_identify_per_min")]
    pub identify_per_min: u32,
    #[serde(default = "default_identify_burst")]
    pub identify_burst: u32,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            max_rf_text_len: default_max_rf_text(),
            rf_msgs_per_min: default_rf_msgs_per_min(),
            rf_burst: default_rf_burst(),
            ip_to_rf_msgs_per_min: default_ip_msgs_per_min(),
            ip_to_rf_burst: default_ip_to_rf_burst(),
            block_apparent_ciphertext: true,
            deny_callsigns: Vec::new(),
            allow_callsigns: Vec::new(),
            rf_channel_msgs_per_min: default_rf_channel_msgs(),
            rf_channel_burst: default_rf_channel_burst(),
            ip_cmds_per_min: default_ip_cmds_per_min(),
            ip_cmd_burst: default_ip_cmd_burst(),
            max_rf_fragments: default_max_rf_fragments(),
            identify_per_min: default_identify_per_min(),
            identify_burst: default_identify_burst(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub name: String,
    #[serde(default)]
    pub topic: String,
    /// Relay this channel over the air.
    #[serde(default)]
    pub rf: bool,
    /// Nicks that receive +o on join after IDENTIFY.
    #[serde(default)]
    pub operators: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperConfig {
    pub name: String,
    /// Plain password or Argon2id PHC string (`rfircd --hash-password`).
    /// Plaintext is only accepted when `listen.bind` is loopback-only.
    pub password: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    /// Extra copy of tracing output (journald/stderr still get it).
    #[serde(default)]
    pub file: Option<String>,
    /// Append-only audit trail: connections, callsigns, kicks, RF TX.
    #[serde(default)]
    pub audit_file: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountsConfig {
    /// JSON file of Argon2id nick hashes. Created on first REGISTER.
    #[serde(default = "default_nicks_file")]
    pub file: String,
    /// How long a user may sit on a registered nick without IDENTIFY.
    #[serde(default = "default_identify_timeout")]
    pub identify_timeout_secs: u64,
    #[serde(default = "default_min_password")]
    pub min_password_len: usize,
}

impl Default for AccountsConfig {
    fn default() -> Self {
        Self {
            file: default_nicks_file(),
            identify_timeout_secs: default_identify_timeout(),
            min_password_len: default_min_password(),
        }
    }
}

impl Config {
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path.as_ref())?;
        Self::from_toml(&text)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for (old, new) in [
            (self.radio.tnc.txdelay, "txdelay_ms"),
            (self.radio.tnc.txtail, "txtail_ms"),
        ] {
            if let Some(v) = old {
                anyhow::bail!(
                    "radio.tnc.{} has moved to radio.duty.{new}, in milliseconds rather than \
                     10 ms units: write `{new} = {}` under [radio.duty]. It lives there because \
                     the airtime governor prices every frame with it and then pushes it to the \
                     TNC, so the two cannot disagree.",
                    new.trim_end_matches("_ms"),
                    u32::from(v) * 10
                );
            }
        }
        if self.server.name.trim().is_empty() {
            anyhow::bail!("server.name must be set");
        }
        if self.listen.ping_interval_secs == 0 {
            anyhow::bail!(
                "listen.ping_interval_secs must be at least 1 (0 panics the connection task)"
            );
        }
        if self.listen.registration_timeout_secs == 0 {
            anyhow::bail!("listen.registration_timeout_secs must be at least 1");
        }
        if self.accounts.identify_timeout_secs == 0 {
            anyhow::bail!(
                "accounts.identify_timeout_secs must be at least 1 (0 releases the nick on the next tick)"
            );
        }
        check_oper_names(&self.opers)?;
        // Each of these parses happily and then makes the server unusable in a
        // way that is hard to diagnose from a client: no nickname is ever
        // valid, no channel can be joined, every radiated message is an
        // ellipsis, or any password at all is accepted for a registered nick.
        if self.server.max_nick_len == 0 {
            anyhow::bail!("server.max_nick_len must be at least 1, or no nickname is valid");
        }
        if self.server.max_channels_per_user == 0 {
            anyhow::bail!(
                "server.max_channels_per_user must be at least 1, or nobody can join anything"
            );
        }
        if self.accounts.min_password_len == 0 {
            anyhow::bail!(
                "accounts.min_password_len must be at least 1: an empty password on a nick \
                 that a control operator can grant RF-TX to is not a password"
            );
        }
        if self.policy.max_rf_text_len == 0 {
            anyhow::bail!(
                "policy.max_rf_text_len must be at least 1, or every radiated message is \
                 truncated to nothing"
            );
        }
        check_oper_passwords(&self.opers, &self.listen_addresses())?;
        if let Some(tls) = &self.listen.tls {
            if tls.bind.is_empty() {
                anyhow::bail!("listen.tls.bind must list at least one address");
            }
            if tls.cert.trim().is_empty() || tls.key.trim().is_empty() {
                anyhow::bail!("listen.tls needs cert and key PEM paths");
            }
            crate::irc::tls::server_config(&tls.cert, &tls.key)?;
        }
        let mut seen: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
        for ch in &self.channels {
            if !crate::irc::message::is_channel_name(&ch.name) {
                anyhow::bail!("invalid channel name: {}", ch.name);
            }
            // Channel names are compared case-insensitively, so two entries
            // that differ only in case are one channel — and the second one's
            // settings would be silently discarded, `rf = true` included.
            if let Some(first) = seen.insert(crate::irc::message::lower(&ch.name), &ch.name) {
                anyhow::bail!(
                    "channels {first} and {} are the same channel: names are compared \
                     case-insensitively, so only one of them would exist",
                    ch.name
                );
            }
        }
        if !self.radio.enabled {
            return Ok(());
        }

        let call: Callsign = self
            .radio
            .callsign
            .parse()
            .map_err(|e| anyhow::anyhow!("radio.callsign: {e}"))?;
        call.require_amateur()
            .map_err(|e| anyhow::anyhow!("radio.callsign: {e}"))?;
        self.radio
            .destination
            .parse::<Callsign>()
            .map_err(|e| anyhow::anyhow!("radio.destination: {e}"))?;
        for d in &self.radio.path {
            d.parse::<Callsign>()
                .map_err(|e| anyhow::anyhow!("radio.path entry {d}: {e}"))?;
        }
        if self.radio.path.len() > 2 {
            anyhow::bail!("radio.path: more than two digipeater hops is antisocial");
        }
        if self.radio.id_interval_secs == 0 || self.radio.id_interval_secs > 600 {
            anyhow::bail!(
                "radio.id_interval_secs must be between 1 and 600; \
                 an automatically transmitting station has to identify at least every 10 minutes"
            );
        }
        if self.radio.paclen < 32 || self.radio.paclen > 256 {
            anyhow::bail!("radio.paclen must be between 32 and 256");
        }
        if self.radio.max_peers == 0 {
            anyhow::bail!("radio.max_peers must be at least 1");
        }
        if self.policy.max_rf_fragments == 0 {
            anyhow::bail!("policy.max_rf_fragments must be at least 1");
        }
        if self.radio.max_queued_airtime_secs == 0 {
            anyhow::bail!("radio.max_queued_airtime_secs must be at least 1");
        }
        if let Some(i) = &self.radio.interlock {
            if i.command.trim().is_empty() {
                anyhow::bail!("radio.interlock.command must be set");
            }
            if i.interval_secs == 0 || i.timeout_secs == 0 {
                anyhow::bail!("radio.interlock interval_secs and timeout_secs must be at least 1");
            }
            if i.timeout_secs >= i.interval_secs {
                anyhow::bail!(
                    "radio.interlock.timeout_secs ({}) must be less than interval_secs ({}), \
                     or checks would overlap",
                    i.timeout_secs,
                    i.interval_secs
                );
            }
        }
        if self.channels.iter().all(|c| !c.rf) {
            anyhow::bail!("radio.enabled is true but no channel has rf = true");
        }
        if !self.radio.aprs_channel.is_empty() {
            if !crate::irc::message::is_channel_name(&self.radio.aprs_channel) {
                anyhow::bail!(
                    "radio.aprs_channel ({}) is not a channel name",
                    self.radio.aprs_channel
                );
            }
            let want = crate::irc::message::lower(&self.radio.aprs_channel);
            let bridged = self
                .channels
                .iter()
                .any(|c| c.rf && crate::irc::message::lower(&c.name) == want);
            if !bridged {
                anyhow::bail!(
                    "radio.aprs_channel ({}) must be a configured channel with rf = true",
                    self.radio.aprs_channel
                );
            }
        }

        let duty = &self.radio.duty;
        if !duty.enabled && self.radio.tnc.kind != "loopback" {
            anyhow::bail!(
                "radio.duty.enabled = false with a real transmitter: there would be nothing \
                 limiting how long the finals are keyed. It is only allowed with \
                 radio.tnc.kind = \"loopback\"."
            );
        }
        if duty.enabled {
            let ceiling = (crate::ax25::airtime::HARD_MAX_DUTY * 100.0) as u32;
            if duty.max_duty_percent == 0 || duty.max_duty_percent > ceiling {
                anyhow::bail!(
                    "radio.duty.max_duty_percent must be between 1 and {ceiling}. \
                     A transceiver keyed for more than half the time cooks its finals, \
                     and an automatically controlled station has nobody watching it."
                );
            }
            if duty.window_secs == 0 {
                anyhow::bail!("radio.duty.window_secs must be at least 1");
            }
            if duty.max_continuous_secs == 0 {
                anyhow::bail!("radio.duty.max_continuous_secs must be at least 1");
            }
            if duty.baud == 0 {
                anyhow::bail!("radio.duty.baud must be at least 1");
            }
            if !duty.allow_nonstandard_baud
                && !crate::ax25::airtime::STANDARD_PACKET_BAUDS.contains(&duty.baud)
            {
                anyhow::bail!(
                    "radio.duty.baud is {}; the packet rates this station will price \
                     without an override are 300 (HF), 1200 (VHF FM) and 9600. A wrong \
                     baud rate makes the governor under-count key-down time and will \
                     cook a QMX. Set allow_nonstandard_baud = true only if you mean it.",
                    duty.baud
                );
            }
            if self.radio.tnc.tx_pacing_ms == 0 && self.radio.tnc.kind != "loopback" {
                anyhow::bail!(
                    "radio.tnc.tx_pacing_ms is 0 on a real transmitter: frames would be \
                     handed to the TNC back-to-back aside from the duty governor. Use \
                     loopback for tests, or set a gap (2500 ms is the QMX starting point)."
                );
            }
            // These are pushed to the TNC as KISS parameters, which carry them
            // in 10 ms units in a single octet.
            for (name, v) in [
                ("txdelay_ms", duty.txdelay_ms),
                ("txtail_ms", duty.txtail_ms),
            ] {
                if v > 2550 {
                    anyhow::bail!(
                        "radio.duty.{name} is {v} ms; KISS carries it in 10 ms units in one \
                         octet, so the maximum is 2550"
                    );
                }
            }
            let air = duty.to_airtime();
            // The window average is only half the story: the run and cooldown
            // settings are an independent way to hold the transmitter keyed.
            air.check_hardware_safe()
                .map_err(|e| anyhow::anyhow!("radio.duty: {e}"))?;
            // A frame the governor can never fit inside its own allowance
            // would be deferred until `max_hold` and then dropped, forever.
            let biggest =
                crate::ax25::Governor::new(air.clone()).airtime_for(self.radio.paclen + 64);
            let allowance = air.window.mul_f64(air.effective_duty());
            if biggest > allowance {
                anyhow::bail!(
                    "radio.duty: a full-length frame is {:.1}s of airtime but the duty allowance \
                     is only {:.1}s per {}s window; raise max_duty_percent or window_secs, or \
                     lower paclen — otherwise nothing would ever be transmitted",
                    biggest.as_secs_f64(),
                    allowance.as_secs_f64(),
                    air.window.as_secs()
                );
            }
            if biggest > air.max_continuous {
                anyhow::bail!(
                    "radio.duty: a full-length frame is {:.1}s of airtime but max_continuous_secs \
                     is {}; a single frame would exceed the continuous-run limit. Raise \
                     max_continuous_secs or lower paclen.",
                    biggest.as_secs_f64(),
                    duty.max_continuous_secs
                );
            }
            if !air.hourly_budget.is_zero() && biggest > air.hourly_budget {
                anyhow::bail!(
                    "radio.duty.hourly_airtime_secs is smaller than a single full-length frame"
                );
            }
            if let Some(path) = &self.radio.tnc.direwolf_conf {
                check_direwolf_conf(path, duty)?;
            }
        }
        for c in self
            .policy
            .deny_callsigns
            .iter()
            .chain(&self.policy.allow_callsigns)
        {
            c.parse::<Callsign>()
                .map_err(|e| anyhow::anyhow!("policy callsign {c}: {e}"))?;
        }
        Ok(())
    }

    /// Channel that receives APRS positions, status beacons, and messages
    /// that have no `#channel` prefix. `radio.aprs_channel` if set, otherwise
    /// the first configured `+r` channel.
    pub fn aprs_listen_channel(&self) -> Option<&str> {
        if !self.radio.aprs_channel.is_empty() {
            return Some(self.radio.aprs_channel.as_str());
        }
        self.channels.iter().find(|c| c.rf).map(|c| c.name.as_str())
    }

    pub fn gateway_callsign(&self) -> Option<Callsign> {
        self.radio.callsign.parse().ok()
    }

    pub fn rf_path(&self) -> Vec<Callsign> {
        self.radio
            .path
            .iter()
            .filter_map(|d| d.parse().ok())
            .collect()
    }

    pub fn id_interval(&self) -> Duration {
        Duration::from_secs(self.radio.id_interval_secs)
    }

    /// Every IRC bind, plaintext and TLS, used for OPER-password rules.
    pub fn listen_addresses(&self) -> Vec<String> {
        let mut addrs = self.listen.bind.clone();
        if let Some(tls) = &self.listen.tls {
            addrs.extend(tls.bind.iter().cloned());
        }
        addrs
    }
}

fn check_oper_names(opers: &[OperConfig]) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    for o in opers {
        if o.name.trim().is_empty() {
            anyhow::bail!("[[opers]] name must not be empty");
        }
        let key = crate::irc::message::lower(&o.name);
        if !seen.insert(key) {
            anyhow::bail!(
                "duplicate [[opers]] name `{}` (IRC casemapping; the last entry would silently win)",
                o.name
            );
        }
    }
    Ok(())
}

fn check_oper_passwords(opers: &[OperConfig], bind: &[String]) -> anyhow::Result<()> {
    let plaintext = opers
        .iter()
        .filter(|o| !crate::accounts::is_phc_hash(&o.password))
        .count();
    if plaintext == 0 {
        return Ok(());
    }
    if bind.iter().all(|a| bind_is_loopback(a)) {
        return Ok(());
    }
    anyhow::bail!(
        "{plaintext} [[opers]] password(s) are plaintext and listen.bind is not loopback-only. \
         Hash them (`rfircd --hash-password`) or bind only to 127.0.0.1 / ::1. \
         OPER with a password in the clear on a public address is control of the transmitter."
    );
}

fn bind_is_loopback(addr: &str) -> bool {
    if let Ok(sa) = addr.parse::<std::net::SocketAddr>() {
        return sa.ip().is_loopback();
    }
    let host = addr
        .rsplit_once(':')
        .map(|(h, _)| h.trim_matches(['[', ']']))
        .unwrap_or(addr);
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn check_direwolf_conf(path: &str, duty: &DutyConfig) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("radio.tnc.direwolf_conf ({path}): {e}"))?;
    let mut modem: Option<u32> = None;
    let mut txdelay_units: Option<u32> = None;
    let mut txtail_units: Option<u32> = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        match key.to_ascii_uppercase().as_str() {
            "MODEM" => {
                if let Some(v) = parts.next().and_then(|s| s.parse().ok()) {
                    modem = Some(v);
                }
            }
            "TXDELAY" => txdelay_units = parts.next().and_then(|s| s.parse().ok()),
            "TXTAIL" => txtail_units = parts.next().and_then(|s| s.parse().ok()),
            _ => {}
        }
    }
    let Some(baud) = modem else {
        anyhow::bail!("radio.tnc.direwolf_conf ({path}) has no MODEM line");
    };
    if baud != duty.baud {
        anyhow::bail!(
            "radio.duty.baud is {} but {path} has MODEM {baud}; the governor would \
             under-count key-down time. They must match.",
            duty.baud
        );
    }
    if let Some(u) = txdelay_units {
        let ms = u64::from(u) * 10;
        if ms != duty.txdelay_ms {
            anyhow::bail!(
                "radio.duty.txdelay_ms is {} but {path} has TXDELAY {u} ({ms} ms). \
                 They must match so the cost model and the modem agree.",
                duty.txdelay_ms
            );
        }
    }
    if let Some(u) = txtail_units {
        let ms = u64::from(u) * 10;
        if ms != duty.txtail_ms {
            anyhow::bail!(
                "radio.duty.txtail_ms is {} but {path} has TXTAIL {u} ({ms} ms). \
                 They must match so the cost model and the modem agree.",
                duty.txtail_ms
            );
        }
    }
    Ok(())
}

fn default_network() -> String {
    "RFIRC".into()
}
fn default_max_nick_len() -> usize {
    30
}
fn default_max_channels() -> usize {
    20
}
fn default_bind() -> Vec<String> {
    vec!["127.0.0.1:6667".into()]
}
fn default_ping_interval() -> u64 {
    120
}
fn default_registration_timeout() -> u64 {
    60
}
fn default_destination() -> String {
    "AIRC".into()
}
fn default_id_interval() -> u64 {
    540
}
fn default_id_text() -> String {
    "RFIRC gateway".into()
}
fn default_paclen() -> usize {
    128
}
fn default_ack_timeout() -> u64 {
    12
}
fn default_max_retries() -> u32 {
    3
}
fn default_idle_timeout() -> u64 {
    1800
}
fn default_max_peers() -> usize {
    256
}
fn default_tnc_kind() -> String {
    "tcp".into()
}
fn default_tnc_host() -> String {
    "127.0.0.1".into()
}
fn default_tnc_port() -> u16 {
    8001
}
fn default_baud() -> u32 {
    9600
}
fn default_tx_pacing() -> u64 {
    1500
}
fn default_mailbox_per_station() -> usize {
    10
}
fn default_mailbox_total() -> usize {
    200
}
fn default_mailbox_ttl() -> u64 {
    24 * 3600
}
fn default_max_rf_text() -> usize {
    160
}
fn default_rf_msgs_per_min() -> u32 {
    6
}
fn default_rf_burst() -> u32 {
    4
}
fn default_ip_msgs_per_min() -> u32 {
    10
}
fn default_ip_to_rf_burst() -> u32 {
    4
}
fn default_true() -> bool {
    true
}
fn default_max_conns_per_host() -> u32 {
    8
}
fn default_max_clients() -> usize {
    256
}
fn default_rf_channel_msgs() -> u32 {
    10
}
fn default_rf_channel_burst() -> u32 {
    4
}
fn default_ip_cmds_per_min() -> u32 {
    90
}
fn default_ip_cmd_burst() -> u32 {
    30
}
fn default_nicks_file() -> String {
    "nicks.json".into()
}
fn default_identify_timeout() -> u64 {
    60
}
fn default_min_password() -> usize {
    8
}
fn default_identify_per_min() -> u32 {
    6
}
fn default_identify_burst() -> u32 {
    3
}
fn default_duty_baud() -> u32 {
    300
}
fn default_txdelay_ms() -> u64 {
    400
}
fn default_txtail_ms() -> u64 {
    300
}
fn default_duty_window() -> u64 {
    600
}
fn default_max_duty_percent() -> u32 {
    25
}
fn default_max_continuous() -> u64 {
    30
}
fn default_cooldown() -> u64 {
    60
}
fn default_hourly_airtime() -> u64 {
    900
}
fn default_max_hold() -> u64 {
    120
}
fn default_max_queued_airtime() -> u64 {
    60
}
fn default_rf_names_max() -> usize {
    8
}
fn default_mailbox_flush_batch() -> usize {
    1
}
fn default_interlock_interval() -> u64 {
    30
}
fn default_interlock_timeout() -> u64 {
    5
}
fn default_max_rf_fragments() -> usize {
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r##"
[server]
name = "test.example"
"##;

    #[test]
    fn minimal_config_parses() {
        let cfg = Config::from_toml(MINIMAL).unwrap();
        assert_eq!(cfg.listen.bind, vec!["127.0.0.1:6667"]);
        assert!(!cfg.radio.enabled);
    }

    #[test]
    fn rejects_illegal_id_interval() {
        let text = r##"
[server]
name = "test.example"
[radio]
enabled = true
callsign = "SM0ABC-1"
id_interval_secs = 3600
[[channels]]
name = "#rf"
rf = true
"##;
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("id_interval_secs"), "{err}");
    }

    #[test]
    fn rejects_radio_without_rf_channel() {
        let text = r##"
[server]
name = "test.example"
[radio]
enabled = true
callsign = "SM0ABC-1"
[[channels]]
name = "#ip"
"##;
        assert!(Config::from_toml(text).is_err());
    }

    #[test]
    fn rejects_non_callsign_gateway_identity() {
        let text = r##"
[server]
name = "test.example"
[radio]
enabled = true
callsign = "GATEWAY"
[[channels]]
name = "#rf"
rf = true
"##;
        assert!(Config::from_toml(text).is_err());
    }

    #[test]
    fn a_frame_longer_than_max_continuous_is_refused() {
        let text = r##"
[server]
name = "test.example"
[radio]
enabled = true
callsign = "SM0ABC-1"
paclen = 128
[[channels]]
name = "#rf"
rf = true
[radio.duty]
enabled = true
baud = 300
max_continuous_secs = 3
max_duty_percent = 25
window_secs = 600
"##;
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("max_continuous_secs"), "{err}");
    }

    #[test]
    fn rejects_zero_ping_interval() {
        let text = r##"
[server]
name = "test.example"
[listen]
ping_interval_secs = 0
"##;
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("ping_interval_secs"), "{err}");
    }

    #[test]
    fn rejects_zero_identify_timeout() {
        let text = r##"
[server]
name = "test.example"
[accounts]
identify_timeout_secs = 0
"##;
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("identify_timeout_secs"), "{err}");
    }

    #[test]
    fn rejects_duplicate_oper_names() {
        let text = r##"
[server]
name = "test.example"
[listen]
bind = ["127.0.0.1:6667"]
[[opers]]
name = "root"
password = "operpass1"
[[opers]]
name = "ROOT"
password = "otherpass1"
"##;
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn plaintext_oper_is_refused_on_a_public_bind() {
        let text = r##"
[server]
name = "test.example"
[listen]
bind = ["0.0.0.0:6667"]
[[opers]]
name = "root"
password = "operpass1"
"##;
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("plaintext"), "{err}");
    }

    #[test]
    fn plaintext_oper_is_allowed_on_loopback() {
        let text = r##"
[server]
name = "test.example"
[listen]
bind = ["127.0.0.1:6667"]
[[opers]]
name = "root"
password = "operpass1"
"##;
        Config::from_toml(text).unwrap();
    }

    #[test]
    fn tls_without_cert_files_is_refused() {
        let text = r##"
[server]
name = "test.example"
[listen]
bind = ["127.0.0.1:6667"]
[listen.tls]
bind = ["0.0.0.0:6697"]
cert = "/no/such/fullchain.pem"
key = "/no/such/privkey.pem"
"##;
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("listen.tls"), "{err}");
    }

    #[test]
    fn plaintext_oper_is_refused_when_tls_is_public() {
        let text = r##"
[server]
name = "test.example"
[listen]
bind = ["127.0.0.1:6667"]
[listen.tls]
bind = ["0.0.0.0:6697"]
cert = "/no/such/fullchain.pem"
key = "/no/such/privkey.pem"
[[opers]]
name = "root"
password = "operpass1"
"##;
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(
            err.contains("plaintext") || err.contains("listen.tls"),
            "{err}"
        );
    }

    const RADIO: &str = r##"
[server]
name = "test.example"
[radio]
enabled = true
callsign = "SM0ABC-1"
[[channels]]
name = "#rf"
rf = true
[radio.tnc]
kind = "loopback"
"##;

    #[test]
    fn rejects_a_nonstandard_baud_unless_overridden() {
        let text = format!("{RADIO}\n[radio.duty]\nbaud = 4800\n");
        let err = Config::from_toml(&text).unwrap_err().to_string();
        assert!(err.contains("baud"), "{err}");
        let text = format!("{RADIO}\n[radio.duty]\nbaud = 4800\nallow_nonstandard_baud = true\n");
        Config::from_toml(&text).unwrap();
    }

    #[test]
    fn rejects_zero_pacing_on_a_real_tnc() {
        let text = r##"
[server]
name = "test.example"
[radio]
enabled = true
callsign = "SM0ABC-1"
[[channels]]
name = "#rf"
rf = true
[radio.tnc]
kind = "tcp"
tx_pacing_ms = 0
"##;
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("tx_pacing_ms"), "{err}");
    }

    #[test]
    fn direwolf_conf_must_agree_with_the_governor() {
        let dir = std::env::temp_dir().join(format!(
            "rfircd-dw-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("direwolf.conf");
        let conf = path.to_str().unwrap();
        std::fs::write(&path, "MODEM 1200\nTXDELAY 40\nTXTAIL 30\n").unwrap();
        let with_path = RADIO.replace(
            "kind = \"loopback\"",
            &format!("kind = \"loopback\"\ndirewolf_conf = \"{conf}\""),
        );
        let text =
            format!("{with_path}\n[radio.duty]\nbaud = 300\ntxdelay_ms = 400\ntxtail_ms = 300\n");
        let err = Config::from_toml(&text).unwrap_err().to_string();
        assert!(err.contains("MODEM"), "{err}");
        std::fs::write(&path, "MODEM 300\nTXDELAY 40\nTXTAIL 30\n").unwrap();
        Config::from_toml(&text).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aprs_channel_must_be_a_bridged_channel() {
        let text = r##"
[server]
name = "test.example"
[radio]
enabled = true
callsign = "SM0ABC-1"
aprs_channel = "#nope"
[[channels]]
name = "#rf"
rf = true
"##;
        let err = Config::from_toml(text).unwrap_err().to_string();
        assert!(err.contains("aprs_channel"), "{err}");
    }
}
