//! `rfircd --init`: the questions that turn an empty directory into a
//! working configuration.
//!
//! The installer copies `rfircd.example.toml` and tells you to edit it. That
//! is fine if you already know what `paclen` and `txdelay` are, and a wall of
//! commented TOML if you do not. This asks the six things that cannot be
//! guessed, writes a small config, and refuses to write one that does not
//! pass [`Config::validate`].
//!
//! # Two rules it follows
//!
//! **It never guesses the baud rate.** A configured rate higher than the
//! modem's makes [`crate::ax25::airtime`] under-count key-down time, which is
//! how a QRP transceiver's finals die. The question is a closed choice and
//! the default is the *lowest* offered rate, because over-estimating airtime
//! costs throughput and under-estimating costs hardware.
//!
//! **It does not enable the transmitter behind your back.** `radio.enabled`
//! is only set when the operator has been shown what they are taking
//! responsibility for and has typed `yes` — a bare Enter leaves the station
//! receive-only with everything else configured, which is what the install
//! guide has always told people to do.
//!
//! The interview is driven by an explicit reader and writer rather than
//! stdin/stdout, so the whole flow can be tested by scripting the answers —
//! the same reason [`crate::station`] collects its output instead of printing
//! it.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::callsign::Callsign;
use crate::config::Config;

/// Symbol rates offered for the on-air link, lowest first.
///
/// Lowest first is deliberate: the first entry is the default, and a
/// too-low setting is the safe direction to be wrong in.
const BAUDS: &[(u32, &str)] = &[
    (300, "HF, the usual QMX/QRP case"),
    (1200, "VHF/UHF FM packet"),
    (9600, "9k6 FM, needs a capable radio"),
];

/// What the interview collected. Rendering and file writing take this, so a
/// test can build one directly and never touch a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answers {
    pub server_name: String,
    pub network: String,
    pub plain_bind: String,
    pub radio: Option<RadioAnswers>,
    pub tls: Option<TlsAnswers>,
    pub oper: Option<OperAnswers>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RadioAnswers {
    pub callsign: Callsign,
    /// False leaves a fully configured but receive-only station.
    pub enabled: bool,
    pub tnc: TncAnswer,
    pub baud: u32,
    pub channel: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TncAnswer {
    Tcp { host: String, port: u16 },
    Serial { path: String, baud: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsAnswers {
    pub bind: String,
    pub cert: String,
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperAnswers {
    pub name: String,
    /// Already an Argon2id PHC string. The plaintext never reaches this
    /// struct, so it cannot be written to the config by accident.
    pub password_hash: String,
}

/// Values the interview offers as defaults, so tests are not at the mercy of
/// the machine's hostname or home directory.
#[derive(Clone, Debug)]
pub struct Defaults {
    pub server_name: String,
    /// Where generated certificates go, normally beside the config file.
    pub conf_dir: PathBuf,
    /// The reader really is this process's terminal, so turning echo off for
    /// the password is both possible and ours to do.
    ///
    /// It has to be told rather than detected: `stty` acts on the process's
    /// stdin whatever the interview is actually reading from, so a caller
    /// reading a scripted `Cursor` would otherwise disable echo on whatever
    /// terminal happened to be attached — under `cargo test`, the developer's
    /// shell.
    pub terminal: bool,
}

impl Defaults {
    pub fn for_config_path(path: &Path) -> Self {
        let conf_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            server_name: default_server_name(),
            conf_dir,
            terminal: std::io::stdin().is_terminal(),
        }
    }
}

/// `hostname`, or something obviously a placeholder. Never fails: a bad
/// default is a question the operator answers, not a reason to stop.
fn default_server_name() -> String {
    let raw = std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    tidy_server_name(&raw)
}

/// Turn whatever `hostname` said into something usable as an IRC server name.
///
/// Split out from the command so the three cases are testable without
/// arranging for `hostname` to be absent, misbehaving, or to return a name
/// with a `_` in it.
fn tidy_server_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
        .collect();
    if cleaned.is_empty() {
        "rfirc.local".into()
    } else if cleaned.contains('.') {
        cleaned
    } else {
        format!("{cleaned}.local")
    }
}

// ------------------------------------------------------------------ prompts

/// Ask, echo the default in brackets, and take a bare Enter as the default.
fn ask<R: BufRead, W: Write>(
    inp: &mut R,
    out: &mut W,
    prompt: &str,
    default: &str,
) -> anyhow::Result<String> {
    loop {
        if default.is_empty() {
            write!(out, "{prompt}: ")?;
        } else {
            write!(out, "{prompt} [{default}]: ")?;
        }
        out.flush()?;
        let mut line = String::new();
        if inp.read_line(&mut line)? == 0 {
            // EOF. Taking the default is right for a piped run that ended
            // early; a required question with no default says so instead.
            if default.is_empty() {
                anyhow::bail!("input ended before {prompt:?} was answered");
            }
            writeln!(out)?;
            return Ok(default.to_string());
        }
        let answer = line.trim();
        if answer.is_empty() {
            if !default.is_empty() {
                return Ok(default.to_string());
            }
            writeln!(out, "  (needed)")?;
            continue;
        }
        return Ok(answer.to_string());
    }
}

fn ask_yes_no<R: BufRead, W: Write>(
    inp: &mut R,
    out: &mut W,
    prompt: &str,
    default_yes: bool,
) -> anyhow::Result<bool> {
    let d = if default_yes { "Y/n" } else { "y/N" };
    loop {
        let answer = ask(
            inp,
            out,
            &format!("{prompt} ({d})"),
            if default_yes { "y" } else { "n" },
        )?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(out, "  please answer y or n")?,
        }
    }
}

/// A numbered menu. Returns the chosen index.
fn ask_choice<R: BufRead, W: Write>(
    inp: &mut R,
    out: &mut W,
    prompt: &str,
    options: &[String],
    default_index: usize,
) -> anyhow::Result<usize> {
    writeln!(out, "{prompt}")?;
    for (i, o) in options.iter().enumerate() {
        writeln!(out, "  {}) {o}", i + 1)?;
    }
    loop {
        let answer = ask(inp, out, "  choice", &(default_index + 1).to_string())?;
        match answer.trim().parse::<usize>() {
            Ok(n) if n >= 1 && n <= options.len() => return Ok(n - 1),
            _ => writeln!(out, "  enter a number between 1 and {}", options.len())?,
        }
    }
}

/// Read a password without echoing it, when we can manage that.
///
/// `unsafe_code = "forbid"` rules out calling `tcsetattr` directly, so this
/// asks `stty` — which is present wherever a terminal is. When it is not
/// (a pipe, a stripped container), the password is echoed and the operator is
/// told so, because silently showing a password is worse than saying you are
/// about to.
fn read_password<R: BufRead, W: Write>(
    inp: &mut R,
    out: &mut W,
    prompt: &str,
    terminal: bool,
) -> anyhow::Result<String> {
    let guard = EchoOff::new(terminal);
    if !guard.active {
        writeln!(out, "  (cannot turn off echo here — this will be visible)")?;
    }
    write!(out, "{prompt}: ")?;
    out.flush()?;
    let mut line = String::new();
    let read = inp.read_line(&mut line);
    if guard.active {
        writeln!(out)?;
    }
    // `guard` restores echo when it drops, including on the `?` below and on
    // a panic. Leaving a terminal with echo off is the kind of mess that
    // outlives the program that made it.
    drop(guard);
    read?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Terminal echo, off for as long as this lives.
struct EchoOff {
    active: bool,
}

impl EchoOff {
    fn new(terminal: bool) -> Self {
        Self {
            active: terminal && set_echo(false),
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        if self.active {
            set_echo(true);
        }
    }
}

fn set_echo(on: bool) -> bool {
    std::process::Command::new("stty")
        .arg(if on { "echo" } else { "-echo" })
        // `stty` needs the terminal on stdin to act on it.
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------- interview

/// Ask everything. Pure with respect to the filesystem: the caller writes the
/// files, so a test can run the whole flow and inspect the answers.
pub fn interview<R: BufRead, W: Write>(
    inp: &mut R,
    out: &mut W,
    defaults: &Defaults,
) -> anyhow::Result<Answers> {
    writeln!(
        out,
        "rfircd setup. Enter accepts the value in brackets.\n\
         Nothing is written until the end, and an existing config is never replaced.\n"
    )?;

    let server_name = ask(
        inp,
        out,
        "Server name, as IRC clients will see it",
        &defaults.server_name,
    )?;
    let network = ask(inp, out, "Network name", "RFIRC")?;

    writeln!(
        out,
        "\nPlaintext IRC listens on loopback. A plaintext connection from\n\
         anywhere else is listen-only however you configure it — TLS is next."
    )?;
    let plain_bind = ask(inp, out, "Plaintext bind", "127.0.0.1:6667")?;

    // ----------------------------------------------------------- the radio
    writeln!(out)?;
    let radio = if ask_yes_no(inp, out, "Configure the radio side?", true)? {
        Some(ask_radio(inp, out)?)
    } else {
        writeln!(
            out,
            "  Skipping. The gateway will be an ordinary small IRC server."
        )?;
        None
    };

    // -------------------------------------------------------------- TLS
    writeln!(out)?;
    let tls = ask_tls(inp, out, defaults)?;

    // ------------------------------------------------------------- oper
    writeln!(out)?;
    let oper = if ask_yes_no(inp, out, "Create a control-operator account (OPER)?", true)? {
        Some(ask_oper(inp, out, &tls, &plain_bind, defaults.terminal)?)
    } else {
        None
    };

    Ok(Answers {
        server_name,
        network,
        plain_bind,
        radio,
        tls,
        oper,
    })
}

fn ask_radio<R: BufRead, W: Write>(inp: &mut R, out: &mut W) -> anyhow::Result<RadioAnswers> {
    let callsign = loop {
        let raw = ask(inp, out, "  Your callsign (with SSID, e.g. SK0MT-1)", "")?;
        match raw.parse::<Callsign>() {
            Ok(c) if c.looks_like_amateur_call() => break c,
            Ok(c) => writeln!(
                out,
                "  {c} does not look like an amateur callsign (it needs a digit and a letter)."
            )?,
            Err(e) => writeln!(out, "  {e}")?,
        }
    };

    let tnc_kinds = if cfg!(feature = "serial") {
        vec![
            "KISS over TCP — Direwolf, a networked TNC (usual)".to_string(),
            "KISS over a serial port".to_string(),
        ]
    } else {
        vec!["KISS over TCP — Direwolf, a networked TNC".to_string()]
    };
    let tnc = if ask_choice(inp, out, "  How is the TNC reached?", &tnc_kinds, 0)? == 0 {
        let host = ask(inp, out, "  TNC host", "127.0.0.1")?;
        let port = loop {
            let raw = ask(inp, out, "  TNC KISS port", "8001")?;
            match raw.parse::<u16>() {
                Ok(p) if p > 0 => break p,
                _ => writeln!(out, "  not a TCP port")?,
            }
        };
        TncAnswer::Tcp { host, port }
    } else {
        let path = ask(inp, out, "  Serial device", "/dev/ttyUSB0")?;
        let baud = loop {
            let raw = ask(inp, out, "  Serial line speed", "9600")?;
            match raw.parse::<u32>() {
                Ok(b) if b > 0 => break b,
                _ => writeln!(out, "  not a line speed")?,
            }
        };
        TncAnswer::Serial { path, baud }
    };

    writeln!(
        out,
        "\n  The on-air symbol rate must match what the modem actually sends.\n\
         \x20 Setting it too high makes the duty-cycle governor under-count key-down\n\
         \x20 time, which is how a QRP transmitter's finals die. If you are unsure,\n\
         \x20 the lower number is the safe way to be wrong."
    )?;
    let labels: Vec<String> = BAUDS
        .iter()
        .map(|(b, why)| format!("{b} baud — {why}"))
        .collect();
    let baud = BAUDS[ask_choice(inp, out, "  On-air symbol rate", &labels, 0)?].0;

    let channel = loop {
        let raw = ask(inp, out, "  Channel bridged to RF", "#rf")?;
        if crate::irc::message::is_channel_name(&raw) {
            break raw;
        }
        writeln!(out, "  a channel name starts with # or & and has no spaces")?;
    };

    writeln!(
        out,
        "\n  Enabling the transmitter means this station keys up automatically,\n\
         \x20 under your licence, carrying other people's traffic. You are the\n\
         \x20 control operator for everything it sends. Read the regulatory notes\n\
         \x20 first: https://mashu.github.io/rfircd/ (Regulatory).\n\
         \x20 Answering no configures everything and leaves it receive-only; you\n\
         \x20 turn it on later by setting radio.enabled = true."
    )?;
    let enabled = ask_yes_no(inp, out, "  Enable the transmitter now?", false)?;

    Ok(RadioAnswers {
        callsign,
        enabled,
        tnc,
        baud,
        channel,
    })
}

fn ask_tls<R: BufRead, W: Write>(
    inp: &mut R,
    out: &mut W,
    defaults: &Defaults,
) -> anyhow::Result<Option<TlsAnswers>> {
    writeln!(
        out,
        "TLS is required for anyone off this machine who will speak, IDENTIFY,\n\
         OPER or key the transmitter. Without it they can watch and nothing else."
    )?;
    let choice = ask_choice(
        inp,
        out,
        "TLS:",
        &[
            "generate a self-signed certificate now".to_string(),
            "use certificate files I already have".to_string(),
            "no TLS — loopback only".to_string(),
        ],
        0,
    )?;
    if choice == 2 {
        return Ok(None);
    }
    let bind = ask(inp, out, "  TLS bind", "0.0.0.0:6697")?;
    if choice == 1 {
        let cert = ask(inp, out, "  Certificate chain (PEM)", "")?;
        let key = ask(inp, out, "  Private key (PEM)", "")?;
        return Ok(Some(TlsAnswers { bind, cert, key }));
    }
    let cert = defaults.conf_dir.join("tls-cert.pem");
    let key = defaults.conf_dir.join("tls-key.pem");
    Ok(Some(TlsAnswers {
        bind,
        cert: cert.to_string_lossy().into_owned(),
        key: key.to_string_lossy().into_owned(),
    }))
}

fn ask_oper<R: BufRead, W: Write>(
    inp: &mut R,
    out: &mut W,
    tls: &Option<TlsAnswers>,
    plain_bind: &str,
    terminal: bool,
) -> anyhow::Result<OperAnswers> {
    let name = ask(inp, out, "  Operator name", "root")?;
    // A plaintext OPER password is only accepted by `Config::validate` when
    // the listener is loopback-only, and hashing is better anyway — so the
    // wizard always hashes and never offers the alternative.
    let hash = loop {
        let password = read_password(inp, out, "  Operator password", terminal)?;
        if password.chars().count() < 8 {
            writeln!(out, "  at least 8 characters")?;
            continue;
        }
        let again = read_password(inp, out, "  Again", terminal)?;
        if password != again {
            writeln!(out, "  they did not match")?;
            continue;
        }
        writeln!(out, "  hashing (Argon2id, this takes a moment)...")?;
        match crate::accounts::hash_password(&password) {
            Ok(h) => break h,
            Err(_) => anyhow::bail!("could not hash the operator password"),
        }
    };
    if tls.is_none() && !is_loopback_bind(plain_bind) {
        writeln!(
            out,
            "  Note: with no TLS and a non-loopback bind, OPER is only reachable\n\
             \x20 from this machine."
        )?;
    }
    Ok(OperAnswers {
        name,
        password_hash: hash,
    })
}

fn is_loopback_bind(addr: &str) -> bool {
    addr.parse::<std::net::SocketAddr>()
        .map(|s| s.ip().is_loopback())
        .unwrap_or(false)
}

// ------------------------------------------------------------------ render

/// TOML basic-string escaping. Answers are typed by a human, but a stray
/// quote or backslash would produce a file that does not parse, and the
/// wizard's whole promise is that what it writes is valid.
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render the configuration file. Pure, so the golden output is a test.
pub fn render(a: &Answers) -> String {
    let mut s = String::new();
    s.push_str(
        "# Written by `rfircd --init`. Every setting not named here keeps its\n\
         # default; the full annotated list is in rfircd.example.toml.\n\n",
    );

    s.push_str("[server]\n");
    s.push_str(&format!("name = {}\n", toml_str(&a.server_name)));
    s.push_str(&format!("network = {}\n", toml_str(&a.network)));
    if let Some(r) = &a.radio {
        s.push_str(&format!(
            "motd = [{}]\n",
            toml_str(&format!("{} packet gateway", r.callsign))
        ));
    }

    s.push_str("\n[listen]\n");
    s.push_str(&format!("bind = [{}]\n", toml_str(&a.plain_bind)));
    if let Some(t) = &a.tls {
        s.push_str(
            "\n# Anyone off this machine who will speak, IDENTIFY, OPER or key the\n\
             # transmitter connects here. TLS protects the IP hop only: everything\n\
             # that reaches the antenna is in the clear, by law and by design.\n",
        );
        s.push_str("[listen.tls]\n");
        s.push_str(&format!("bind = [{}]\n", toml_str(&t.bind)));
        s.push_str(&format!("cert = {}\n", toml_str(&t.cert)));
        s.push_str(&format!("key = {}\n", toml_str(&t.key)));
    }

    if let Some(r) = &a.radio {
        s.push_str("\n[radio]\n");
        if r.enabled {
            s.push_str("enabled = true\n");
        } else {
            s.push_str(
                "# Receive-only until you set this true. Everything else below is\n\
                 # configured and ready; read the Regulatory notes first.\n\
                 enabled = false\n",
            );
        }
        s.push_str(&format!(
            "callsign = {}\n",
            toml_str(&r.callsign.to_string())
        ));

        s.push_str("\n[radio.tnc]\n");
        match &r.tnc {
            TncAnswer::Tcp { host, port } => {
                s.push_str("kind = \"tcp\"\n");
                s.push_str(&format!("host = {}\n", toml_str(host)));
                s.push_str(&format!("port = {port}\n"));
            }
            TncAnswer::Serial { path, baud } => {
                s.push_str("# Needs a build with --features serial.\n");
                s.push_str("kind = \"serial\"\n");
                s.push_str(&format!("device = {}\n", toml_str(path)));
                s.push_str(&format!("baud = {baud}\n"));
            }
        }

        s.push_str(
            "\n# baud is the on-air symbol rate and must match the modem. Too high\n\
             # under-counts key-down time, which is what protects the finals.\n",
        );
        s.push_str("[radio.duty]\n");
        s.push_str(&format!("baud = {}\n", r.baud));

        s.push_str("\n[[channels]]\n");
        s.push_str(&format!("name = {}\n", toml_str(&r.channel)));
        s.push_str("rf = true\n");
    }

    s.push_str("\n[[channels]]\n");
    s.push_str("name = \"#local\"\n");

    if let Some(o) = &a.oper {
        s.push_str(
            "\n# Argon2id hash — the password itself was never written anywhere.\n\
             # Replace it with `rfircd --hash-password` output to change it.\n",
        );
        s.push_str("[[opers]]\n");
        s.push_str(&format!("name = {}\n", toml_str(&o.name)));
        s.push_str(&format!("password = {}\n", toml_str(&o.password_hash)));
    }

    s
}

// ------------------------------------------------------------------- certs

/// Write a self-signed certificate and key, and return the SHA-256
/// fingerprint of the certificate.
///
/// The fingerprint is the point. A self-signed certificate makes every IRC
/// client warn or refuse, and the honest answer to that is not to hide it but
/// to give the operator the string their client will ask them to confirm.
pub fn write_self_signed(
    names: Vec<String>,
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<String> {
    let cert = rcgen::generate_simple_self_signed(names)
        .map_err(|e| anyhow::anyhow!("could not generate a certificate: {e}"))?;
    if let Some(dir) = cert_path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    std::fs::write(cert_path, cert.cert.pem())?;
    write_private(key_path, &cert.key_pair.serialize_pem())?;
    let digest = ring::digest::digest(&ring::digest::SHA256, cert.cert.der());
    Ok(digest
        .as_ref()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":"))
}

/// 0600 from the moment it exists, not after — a private key that is briefly
/// world-readable is a private key that was world-readable.
fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = f.metadata()?.permissions();
        perms.set_mode(0o600);
        f.set_permissions(perms)?;
    }
    Ok(())
}

// --------------------------------------------------------------- the driver

/// Run the interview against the real terminal and write the results.
///
/// Refuses rather than overwrites: a configuration file is the record of
/// decisions somebody made, and this command is most often reached by
/// accident from an installer that has already run once.
pub fn run(path: &str) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut inp = stdin.lock();
    let mut out = std::io::stdout();
    let defaults = Defaults::for_config_path(Path::new(path));
    run_with(&mut inp, &mut out, path, &defaults)
}

/// [`run`] against an explicit reader, writer and set of defaults.
///
/// Certificate generation, validation and writing are all in here rather than
/// in `run`, because the order they happen in is the part worth testing: the
/// certificate has to exist before the configuration naming it will validate
/// (`Config::validate` loads the pair), and nothing may reach the disk if the
/// interview is abandoned.
pub fn run_with<R: BufRead, W: Write>(
    inp: &mut R,
    out: &mut W,
    path: &str,
    defaults: &Defaults,
) -> anyhow::Result<()> {
    let target = Path::new(path);
    if target.exists() {
        anyhow::bail!(
            "{} already exists — not replacing it. Check it with `rfircd --check -c {}`, \
             or move it aside and run --init again.",
            target.display(),
            target.display()
        );
    }

    let answers = interview(inp, out, defaults)?;

    // Generate the certificate only once the interview has succeeded, so an
    // abandoned run leaves nothing behind.
    let mut fingerprint = None;
    if let Some(t) = &answers.tls {
        let cert = Path::new(&t.cert);
        let key = Path::new(&t.key);
        if !cert.exists() && !key.exists() {
            let mut names = vec![answers.server_name.clone()];
            if !names.iter().any(|n| n == "localhost") {
                names.push("localhost".into());
            }
            fingerprint = Some(write_self_signed(names, cert, key)?);
        }
    }

    let text = render(&answers);
    // The promise of the wizard is that what it writes comes up. Parse it the
    // same way the server will, before anything reaches the disk.
    Config::from_toml(&text)
        .map_err(|e| anyhow::anyhow!("the generated configuration did not validate: {e}"))?;

    if let Some(dir) = target.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    // The file carries an operator password hash, so it is not world-readable.
    write_private(target, &text)?;

    writeln!(out, "\nwrote {}", target.display())?;
    if let (Some(t), Some(fp)) = (&answers.tls, &fingerprint) {
        writeln!(out, "wrote {} and {}", t.cert, t.key)?;
        writeln!(
            out,
            "\nThe certificate is self-signed, so IRC clients will ask about it.\n\
             Its SHA-256 fingerprint is:\n\n  {fp}\n\n\
             Compare that with what your client shows and accept it once. If this\n\
             host has a real name, a certificate from Let's Encrypt is better:\n\
             point listen.tls.cert and .key at it and restart."
        )?;
    }
    writeln!(out, "\nnext:")?;
    writeln!(out, "  rfircd --check -c {}", target.display())?;
    writeln!(out, "  rfircd -c {}", target.display())?;
    if let Some(r) = &answers.radio {
        if !r.enabled {
            writeln!(
                out,
                "\nThe transmitter is off. When you have read the regulatory notes,\n\
                 set radio.enabled = true in {}.",
                target.display()
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers_from(script: &str) -> anyhow::Result<Answers> {
        let mut inp = std::io::Cursor::new(script.as_bytes().to_vec());
        let mut out: Vec<u8> = Vec::new();
        let defaults = Defaults {
            server_name: "test.local".into(),
            conf_dir: PathBuf::from("/etc/rfircd"),
            // Never true in a test: `stty` would act on the real terminal.
            terminal: false,
        };
        interview(&mut inp, &mut out, &defaults)
    }

    /// Bare Enter throughout: the answers are the defaults, and the one thing
    /// that is not a default is the transmitter, which stays off.
    #[test]
    fn accepting_every_default_produces_a_valid_receive_only_station() {
        // server, network, bind, radio? y, callsign, tnc kind, host, port,
        // baud, channel, enable? (default n), tls -> 3 (none), oper? n
        let script = "\n\n\ny\nSK0MT-1\n\n\n\n\n\n\n3\nn\n";
        let a = answers_from(script).expect("interview");
        assert_eq!(a.server_name, "test.local");
        let radio = a.radio.as_ref().expect("radio configured");
        assert_eq!(radio.callsign.to_string(), "SK0MT-1");
        assert_eq!(radio.baud, 300, "the default must be the safest rate");
        assert!(!radio.enabled, "a bare Enter must not key a transmitter");
        assert_eq!(radio.channel, "#rf");
        assert!(a.tls.is_none());

        let text = render(&a);
        let config = Config::from_toml(&text).expect("generated config must validate");
        assert_eq!(config.server.name, "test.local");
        assert!(!config.radio.enabled);
    }

    /// Enabling the transmitter takes an explicit yes, and the config that
    /// comes out has to pass the same validation the server applies —
    /// including "radio.enabled with no bridged channel is an error".
    #[test]
    fn an_enabled_transmitter_validates_end_to_end() {
        let script = "gw.example\nCLUB\n\ny\nSK0MT-1\n\n\n\n2\n#rf\ny\n3\nn\n";
        let a = answers_from(script).expect("interview");
        let radio = a.radio.as_ref().expect("radio");
        assert!(radio.enabled);
        assert_eq!(radio.baud, 1200, "choice 2 is 1200 baud");
        let config = Config::from_toml(&render(&a)).expect("must validate");
        assert!(config.radio.enabled);
        assert_eq!(config.radio.callsign, "SK0MT-1");
    }

    #[test]
    fn a_bad_callsign_is_asked_again_rather_than_accepted() {
        // "ID" parses but is not an amateur call; "!!" does not parse at all.
        let script = "\n\n\ny\nID\n!!\nSK0MT-1\n\n\n\n\n\n\n3\nn\n";
        let a = answers_from(script).expect("interview");
        assert_eq!(
            a.radio.as_ref().expect("radio").callsign.to_string(),
            "SK0MT-1"
        );
    }

    #[test]
    fn declining_the_radio_still_produces_a_working_server() {
        let script = "\n\n\nn\n3\nn\n";
        let a = answers_from(script).expect("interview");
        assert!(a.radio.is_none());
        let config = Config::from_toml(&render(&a)).expect("must validate");
        assert!(!config.radio.enabled);
        // `#local` is always written, so the server is useful immediately.
        assert!(config.channels.iter().any(|c| c.name == "#local"));
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rfircd-wizard-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The end-to-end path, and the one place the ordering matters:
    /// `Config::validate` *loads* the certificate pair, so a configuration
    /// naming files that do not exist yet is invalid. Generating the pair
    /// before validating is what makes the wizard's promise — that what it
    /// writes comes up — true for the TLS case.
    #[test]
    fn generating_tls_produces_a_config_that_validates() {
        let dir = scratch("tls");
        let path = dir.join("rfircd.toml");
        let defaults = Defaults {
            server_name: "gw.example".into(),
            conf_dir: dir.clone(),
            terminal: false,
        };
        // server, network, bind, radio? n, tls -> 1 (generate), tls bind, oper? n
        let script = "\n\n\nn\n1\n0.0.0.0:6697\nn\n";
        let mut inp = std::io::Cursor::new(script.as_bytes().to_vec());
        let mut out: Vec<u8> = Vec::new();
        run_with(&mut inp, &mut out, &path.to_string_lossy(), &defaults).expect("run");

        assert!(dir.join("tls-cert.pem").exists(), "certificate not written");
        assert!(dir.join("tls-key.pem").exists(), "key not written");

        // The written file is what the server would load, cert pair included.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[listen.tls]"), "{text}");
        let config = Config::from_toml(&text).expect("the written config must validate");
        let tls = config.listen.tls.as_ref().expect("tls section");
        assert_eq!(tls.bind, vec!["0.0.0.0:6697".to_string()]);

        // The fingerprint is printed, because a self-signed certificate is
        // only usable if the operator can confirm it in their client.
        let shown = String::from_utf8_lossy(&out);
        assert!(shown.contains("SHA-256"), "{shown}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "config carries an oper hash; was {mode:o}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The operator password is retried until it is long enough and typed
    /// twice the same, never lands in `Answers` as plaintext, and reaches the
    /// file only as a PHC string that `is_phc_hash` recognises — which is
    /// what `Config::validate` uses to decide a password is not plaintext.
    #[test]
    fn the_operator_password_is_hashed_and_confirmed() {
        // radio n, tls none, oper y, name, short password (rejected),
        // mismatched pair (rejected), then a matching pair.
        let script = "\n\n\nn\n3\ny\nsysop\nshort\nshort\nlongenough1\ndifferent2\nlongenough1\nlongenough1\n";
        let a = answers_from(script).expect("interview");
        let oper = a.oper.as_ref().expect("oper created");
        assert_eq!(oper.name, "sysop");
        assert!(
            crate::accounts::is_phc_hash(&oper.password_hash),
            "not a PHC string: {}",
            oper.password_hash
        );
        assert!(
            !oper.password_hash.contains("longenough1"),
            "the plaintext survived into the hash field"
        );

        let text = render(&a);
        assert!(!text.contains("longenough1"), "plaintext reached the file");
        let config = Config::from_toml(&text).expect("must validate");
        assert_eq!(config.opers.len(), 1);
        assert_eq!(config.opers[0].name, "sysop");
        // Hashed, so it is accepted on a non-loopback listener too — the
        // check the wizard exists to make unnecessary to think about.
        assert!(crate::accounts::is_phc_hash(&config.opers[0].password));
    }

    /// A `Write` that fails once it has taken `budget` bytes, so the `?` on
    /// every prompt has something to propagate.
    struct FailingWriter {
        budget: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.budget == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "output closed",
                ));
            }
            let n = buf.len().min(self.budget);
            self.budget -= n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A terminal that goes away mid-interview is an error, not a panic and
    /// not a half-answered `Answers`. Every prompt writes before it reads, so
    /// this walks the `?` on all of them.
    #[test]
    fn output_that_fails_partway_is_reported() {
        const SCRIPT: &[u8] = b"\n\n\nn\n3\nn\n";
        let defaults = Defaults {
            server_name: "test.local".into(),
            conf_dir: PathBuf::from("/nonexistent"),
            terminal: false,
        };

        // How much this interview writes when nothing goes wrong. Anything
        // less than that must fail; anything more would not be a failing
        // writer at all, which is why the sweep stops here rather than at an
        // arbitrary number.
        let total = {
            let mut inp = std::io::Cursor::new(SCRIPT.to_vec());
            let mut out: Vec<u8> = Vec::new();
            interview(&mut inp, &mut out, &defaults).expect("baseline run");
            out.len()
        };
        assert!(total > 0);

        // Every prompt writes before it reads, so cutting the output at each
        // of these points walks the `?` on every write in the flow.
        for budget in (0..total).step_by(7) {
            let mut inp = std::io::Cursor::new(SCRIPT.to_vec());
            let mut out = FailingWriter { budget };
            assert!(
                interview(&mut inp, &mut out, &defaults).is_err(),
                "a writer that failed after {budget} of {total} bytes was not reported"
            );
        }
    }

    /// Every prompt that can be answered wrongly asks again rather than
    /// taking the bad value or giving up.
    #[test]
    fn invalid_answers_are_asked_again() {
        // radio yes; a yes/no typo, a non-numeric menu choice, an
        // out-of-range one, a bad TCP port, and a channel name with a space.
        let script = concat!(
            "\n\n\n",         // server, network, bind
            "maybe\ny\n",     // yes/no typo, then yes
            "SK0MT-1\n",      // callsign
            "banana\n9\n1\n", // TNC menu: not a number, out of range, then 1
            "127.0.0.1\n",
            "notaport\n70000\n8001\n", // port: text, out of u16 range, then good
            "\n",                      // baud default
            "no spaces here\nrf\n#rf\n", // channel: spaces, no sigil, then good
            "\n",                      // transmitter stays off
            "3\n",                     // no TLS
            "n\n",                     // no oper
        );
        let a = answers_from(script).expect("interview");
        let radio = a.radio.as_ref().expect("radio");
        assert_eq!(radio.channel, "#rf");
        assert_eq!(
            radio.tnc,
            TncAnswer::Tcp {
                host: "127.0.0.1".into(),
                port: 8001
            }
        );
        Config::from_toml(&render(&a)).expect("must validate");
    }

    /// `ask` has two edges that are easy to get wrong: input ending on a
    /// question that has a default, and an empty answer to one that does not.
    #[test]
    fn ask_handles_eof_and_required_answers() {
        // EOF on a question with a default takes the default.
        let mut inp = std::io::Cursor::new(Vec::new());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(
            ask(&mut inp, &mut out, "Server name", "fallback.local").unwrap(),
            "fallback.local"
        );

        // EOF on a required question is an error, not an empty string.
        let mut inp = std::io::Cursor::new(Vec::new());
        let mut out: Vec<u8> = Vec::new();
        assert!(ask(&mut inp, &mut out, "Callsign", "").is_err());

        // A bare Enter on a required question asks again.
        let mut inp = std::io::Cursor::new(b"\n\n  \nSK0MT-1\n".to_vec());
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(ask(&mut inp, &mut out, "Callsign", "").unwrap(), "SK0MT-1");
        assert!(
            String::from_utf8_lossy(&out).contains("(needed)"),
            "the operator was not told the answer was required"
        );
    }

    /// Choosing existing certificate files rather than generating a pair.
    /// Nothing is generated, and the paths given are what the config names.
    #[test]
    fn certificate_files_can_be_supplied_instead_of_generated() {
        let dir = scratch("byo");
        // A real pair, so the written config validates — `Config::validate`
        // loads whatever the paths point at.
        let cert = dir.join("mine-cert.pem");
        let key = dir.join("mine-key.pem");
        write_self_signed(vec!["gw.example".into()], &cert, &key).expect("pair");

        let path = dir.join("rfircd.toml");
        let defaults = Defaults {
            server_name: "gw.example".into(),
            conf_dir: dir.clone(),
            terminal: false,
        };
        let script = format!(
            "\n\n\nn\n2\n127.0.0.1:6697\n{}\n{}\nn\n",
            cert.display(),
            key.display()
        );
        let mut inp = std::io::Cursor::new(script.into_bytes());
        let mut out: Vec<u8> = Vec::new();
        run_with(&mut inp, &mut out, &path.to_string_lossy(), &defaults).expect("run");

        assert!(
            !dir.join("tls-cert.pem").exists(),
            "a certificate was generated when the operator supplied one"
        );
        let shown = String::from_utf8_lossy(&out);
        assert!(
            !shown.contains("SHA-256"),
            "a fingerprint was printed for a certificate we did not make"
        );
        let config = Config::from_toml(&std::fs::read_to_string(&path).unwrap()).expect("validate");
        assert_eq!(
            config.listen.tls.as_ref().unwrap().cert,
            cert.to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A radio station with no TLS: the closing advice has to say the
    /// transmitter is off and how to turn it on, because that is the one
    /// thing the wizard deliberately did not do.
    #[test]
    fn the_closing_advice_says_the_transmitter_is_off() {
        let dir = scratch("advice");
        let path = dir.join("rfircd.toml");
        let defaults = Defaults {
            server_name: "gw.example".into(),
            conf_dir: dir.clone(),
            terminal: false,
        };
        // Non-loopback plaintext bind and no TLS, so the OPER note fires too.
        let script =
            "\n\n0.0.0.0:6667\ny\nSK0MT-1\n\n\n\n\n\n\n3\ny\nsysop\nlongenough1\nlongenough1\n";
        let mut inp = std::io::Cursor::new(script.as_bytes().to_vec());
        let mut out: Vec<u8> = Vec::new();
        run_with(&mut inp, &mut out, &path.to_string_lossy(), &defaults).expect("run");

        let shown = String::from_utf8_lossy(&out);
        assert!(shown.contains("transmitter is off"), "{shown}");
        assert!(shown.contains("radio.enabled = true"), "{shown}");
        assert!(
            shown.contains("only reachable"),
            "no TLS and a public bind should warn about OPER: {shown}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The serial arm of the renderer. Asking for it needs a build with
    /// `--features serial`; writing it out does not, and a config that names
    /// a serial TNC has to come out right either way.
    #[test]
    fn a_serial_tnc_renders() {
        let a = Answers {
            server_name: "gw.example".into(),
            network: "RFIRC".into(),
            plain_bind: "127.0.0.1:6667".into(),
            radio: Some(RadioAnswers {
                callsign: "SK0MT-1".parse().unwrap(),
                enabled: false,
                tnc: TncAnswer::Serial {
                    path: "/dev/ttyUSB0".into(),
                    baud: 9600,
                },
                baud: 1200,
                channel: "#rf".into(),
            }),
            tls: None,
            oper: None,
        };
        let text = render(&a);
        assert!(text.contains("kind = \"serial\""), "{text}");
        assert!(text.contains("device = \"/dev/ttyUSB0\""), "{text}");
        // Two different rates, and they must not be confused: the line speed
        // to the TNC is not the symbol rate on the air.
        assert!(text.contains("baud = 9600"), "{text}");
        assert!(text.contains("baud = 1200"), "{text}");
        Config::from_toml(&text).expect("must validate");
    }

    /// The serial menu entry only exists in a build that can open a serial
    /// port, so the question is only reachable — and only worth asking —
    /// under that feature.
    #[cfg(feature = "serial")]
    #[test]
    fn a_serial_tnc_can_be_chosen() {
        let script = concat!(
            "\n\n\ny\n",                   // server, network, bind, radio yes
            "SK0MT-1\n",                   // callsign
            "2\n",                         // TNC menu: serial
            "/dev/ttyUSB1\nnope\n19200\n", // device, bad line speed, good one
            "\n\n\n",                      // baud, channel, transmitter off
            "3\nn\n",                      // no TLS, no oper
        );
        let a = answers_from(script).expect("interview");
        assert_eq!(
            a.radio.as_ref().unwrap().tnc,
            TncAnswer::Serial {
                path: "/dev/ttyUSB1".into(),
                baud: 19200
            }
        );
        Config::from_toml(&render(&a)).expect("must validate");
    }

    #[test]
    fn control_characters_in_an_answer_cannot_break_the_file() {
        assert_eq!(toml_str("a\tb"), "\"a\\tb\"");
        assert_eq!(toml_str("a\nb"), "\"a\\nb\"");
        assert_eq!(toml_str("a\rb"), "\"a\\rb\"");
        assert_eq!(toml_str("a\u{1b}b"), "\"a\\u001Bb\"");
        let a = Answers {
            server_name: "we\u{1b}[2Jird\tname".into(),
            network: "net\nwork".into(),
            plain_bind: "127.0.0.1:6667".into(),
            radio: None,
            tls: None,
            oper: None,
        };
        Config::from_toml(&render(&a)).expect("must still parse");
    }

    #[test]
    fn a_hostname_becomes_a_plausible_server_name() {
        assert_eq!(tidy_server_name("gw.example.org"), "gw.example.org");
        assert_eq!(tidy_server_name("shack"), "shack.local");
        // `_` is not legal in a hostname and is filtered rather than kept.
        assert_eq!(tidy_server_name("my_box"), "mybox.local");
        // `hostname` missing or silent.
        assert_eq!(tidy_server_name(""), "rfirc.local");
        assert_eq!(tidy_server_name("   "), "rfirc.local");
    }

    /// Paths with no directory component. `write_self_signed` and the config
    /// writer both guard `path.parent()` before creating anything, and the
    /// "there is no parent" side of that guard is what a bare filename takes.
    #[test]
    fn a_bare_filename_needs_no_directory_created() {
        let dir = scratch("bare");
        let cwd = std::env::current_dir().expect("cwd");
        // `write_self_signed` takes the paths as given, so chdir is how a
        // bare relative name is exercised without writing into the repo.
        std::env::set_current_dir(&dir).expect("chdir");
        let result = write_self_signed(
            vec!["localhost".into()],
            Path::new("bare-cert.pem"),
            Path::new("bare-key.pem"),
        );
        std::env::set_current_dir(&cwd).expect("chdir back");

        let fp = result.expect("a bare filename must work");
        assert_eq!(fp.split(':').count(), 32, "{fp}");
        assert!(dir.join("bare-cert.pem").exists());
        assert!(dir.join("bare-key.pem").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn certificates_go_beside_the_config_file() {
        let d = Defaults::for_config_path(Path::new("/etc/rfircd/rfircd.toml"));
        assert_eq!(d.conf_dir, PathBuf::from("/etc/rfircd"));
        // A bare filename means the working directory, not the filesystem root.
        let d = Defaults::for_config_path(Path::new("rfircd.toml"));
        assert_eq!(d.conf_dir, PathBuf::from("."));
    }

    /// Echo is only ever taken when the caller says the reader is the
    /// terminal. This is the guard that keeps `cargo test` from turning off
    /// echo in the shell that started it.
    #[test]
    fn echo_is_left_alone_unless_the_caller_owns_the_terminal() {
        let guard = EchoOff::new(false);
        assert!(!guard.active, "echo must not be touched on a scripted run");
        drop(guard);
    }

    /// An interview that never finishes must not leave a certificate, a key
    /// or a half-written config behind.
    #[test]
    fn an_abandoned_run_writes_nothing() {
        let dir = scratch("abandoned");
        let path = dir.join("rfircd.toml");
        let defaults = Defaults {
            server_name: "gw.example".into(),
            conf_dir: dir.clone(),
            terminal: false,
        };
        // Input ends at the callsign, which has no default.
        let mut inp = std::io::Cursor::new(b"\n\n\ny\n".to_vec());
        let mut out: Vec<u8> = Vec::new();
        let err = run_with(&mut inp, &mut out, &path.to_string_lossy(), &defaults)
            .expect_err("must fail");
        assert!(err.to_string().contains("input ended"), "{err}");
        assert!(!path.exists(), "a config was written for an abandoned run");
        assert!(
            !dir.join("tls-cert.pem").exists(),
            "a certificate was left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A quote in an answer must not produce a file that fails to parse.
    #[test]
    fn quotes_and_backslashes_in_answers_are_escaped() {
        let a = Answers {
            server_name: "we\"ird\\name".into(),
            network: "net\"work".into(),
            plain_bind: "127.0.0.1:6667".into(),
            radio: None,
            tls: None,
            oper: None,
        };
        let text = render(&a);
        let config = Config::from_toml(&text).expect("must still parse");
        assert_eq!(config.server.name, "we\"ird\\name");
    }

    #[test]
    fn a_generated_certificate_loads_as_a_tls_config() {
        let dir = std::env::temp_dir().join(format!(
            "rfircd-wizard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cert = dir.join("tls-cert.pem");
        let key = dir.join("tls-key.pem");
        let fp = write_self_signed(vec!["gw.example".into(), "localhost".into()], &cert, &key)
            .expect("generate");
        // 32 bytes as colon-separated hex.
        assert_eq!(fp.split(':').count(), 32, "{fp}");

        // The real proof: rustls accepts the pair.
        crate::irc::tls::server_config(&cert.to_string_lossy(), &key.to_string_lossy())
            .expect("the generated pair must build a TLS config");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "private key was {mode:o}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_existing_config_is_never_replaced() {
        let path = std::env::temp_dir().join(format!(
            "rfircd-wizard-existing-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "[server]\nname = \"mine.local\"\n").unwrap();
        let err = run(&path.to_string_lossy()).expect_err("must refuse");
        assert!(err.to_string().contains("already exists"), "{err}");
        // Untouched.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("mine.local"), "{text}");
        let _ = std::fs::remove_file(&path);
    }
}
