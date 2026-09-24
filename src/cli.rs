//! Command line and startup wiring for the `rfircd` binary.
//!
//! Separated from `main` so it can be tested: argument handling and the
//! configuration-to-TNC-link step are exactly the parts where a mistake means
//! the gateway comes up wrong, and they are the parts a `main` function hides.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::audit::Audit;
use crate::ax25::tnc::{self, TncConfig, TncLink};
use crate::config::{Config, TncSection};
use crate::irc::admission::Admission;
use crate::irc::client::{listen, ListenerOptions};
use crate::server::{self, Event, Server};

/// What `main` should do once the command line is understood.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// Run the server with this configuration file.
    Run {
        path: String,
    },
    /// Validate the configuration and exit.
    Check {
        path: String,
    },
    /// Ask the questions that cannot be guessed and write a configuration.
    Init {
        path: String,
    },
    HashPassword,
    Version(String),
    Help(String),
    /// Bad usage; the message to print before exiting non-zero.
    Usage(String),
}

pub const DEFAULT_CONFIG: &str = "rfircd.toml";

pub fn version() -> String {
    format!("rfircd {}", env!("CARGO_PKG_VERSION"))
}

pub fn help() -> String {
    format!(
        "\
rfircd {} — IRC server with an RF gateway (KISS/Direwolf; AIRC and APRS)

Usage: rfircd [--config path] [--check] [--init] [--hash-password]

  -c, --config <path>   configuration file (default: {DEFAULT_CONFIG})
      --check           validate the configuration and exit
      --init            ask a few questions and write a starter configuration,
                        generating a TLS certificate if you want one. Refuses
                        to replace a file that already exists.
      --hash-password   read a password from stdin and print an Argon2id hash
                        (for [[opers]] when the listener is not loopback)
  -V, --version         print version
  -h, --help            this help

QMX on Debian: https://mashu.github.io/rfircd/
",
        env!("CARGO_PKG_VERSION")
    )
}

pub fn parse_args<I: IntoIterator<Item = String>>(argv: I) -> Invocation {
    let mut path = DEFAULT_CONFIG.to_string();
    let mut check_only = false;
    let mut init = false;
    let mut args = argv.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => match args.next() {
                Some(p) => path = p,
                None => return Invocation::Usage("--config needs a path".into()),
            },
            "--check" => check_only = true,
            "--init" => init = true,
            "--hash-password" => return Invocation::HashPassword,
            "--version" | "-V" => return Invocation::Version(version()),
            "--help" | "-h" => return Invocation::Help(help()),
            other => return Invocation::Usage(format!("unknown argument: {other}")),
        }
    }
    match (init, check_only) {
        // `--init --check` is a contradiction: there is nothing to check yet.
        // Treat it as the setup the operator clearly meant to run.
        (true, _) => Invocation::Init { path },
        (false, true) => Invocation::Check { path },
        (false, false) => Invocation::Run { path },
    }
}

/// Turn `[radio.tnc] kind` into a link the TNC task can open.
///
/// Returns the loopback's far end alongside the link when `kind = "loopback"`,
/// because something has to hold it: dropping it would close the fake radio
/// the moment this function returned.
pub fn resolve_link(
    section: &TncSection,
) -> anyhow::Result<(TncLink, Option<tokio::io::DuplexStream>)> {
    match section.kind.as_str() {
        "tcp" => Ok((
            TncLink::Tcp {
                host: section.host.clone(),
                port: section.port,
            },
            None,
        )),
        #[cfg(feature = "serial")]
        "serial" => Ok((
            TncLink::Serial {
                path: section.device.clone(),
                baud: section.baud,
            },
            None,
        )),
        #[cfg(not(feature = "serial"))]
        "serial" => {
            anyhow::bail!("this build has no serial support; rebuild with --features serial")
        }
        "loopback" => {
            let (link, far) = TncConfig::loopback_link();
            Ok((link, Some(far)))
        }
        other => anyhow::bail!(
            "unknown radio.tnc.kind: {other} (expected \"tcp\", \"serial\" or \"loopback\")"
        ),
    }
}

/// A gateway that has been assembled but not yet started.
///
/// Startup is separated from the event loop so it can be inspected: whether
/// the radio came up, whether the TNC task is attached, what the server thinks
/// its channels are. `main` builds one and runs it; a test builds one and
/// looks at it.
pub struct Gateway {
    pub server: Server,
    pub events: mpsc::Sender<Event>,
    events_rx: mpsc::Receiver<Event>,
    /// The far end of a loopback TNC, if that is the configured kind. It has
    /// to be held for the life of the gateway: dropping it closes the fake
    /// radio, and the server then decides it has no transmitter.
    pub loopback: Option<tokio::io::DuplexStream>,
}

/// Assemble the gateway: radio link, interlock, server, and the task that
/// feeds received frames into the event loop.
///
/// Listeners and the signal handler are started separately by [`serve`], so a
/// test can build a gateway without binding a port.
pub fn build(config: Arc<Config>) -> anyhow::Result<Gateway> {
    let mut loopback = None;
    let audit = Audit::open(config.logging.audit_file.as_deref());
    let (tnc_handle, rf_rx) = if config.radio.enabled {
        let (link, far) = resolve_link(&config.radio.tnc)?;
        if far.is_some() {
            warn!("TNC kind is 'loopback': nothing will be transmitted");
            loopback = far;
        }
        let (handle, rx) =
            tnc::spawn_with_audit(TncConfig::from_config(&config, link), audit.clone());
        if let Some(interlock) = config.radio.interlock.clone() {
            crate::interlock::spawn(interlock, handle.airtime().clone());
        }
        info!(
            callsign = %config.radio.callsign,
            "radio gateway enabled; identifying every {} s",
            config.radio.id_interval_secs
        );
        (Some(handle), Some(rx))
    } else {
        info!("radio gateway disabled; running as a plain IRC server");
        (None, None)
    };

    let (events, events_rx) = mpsc::channel::<Event>(1024);
    let mut server = Server::with_audit(config.clone(), tnc_handle, audit)?;
    server.attach_events(events.clone());

    // Frames heard on the air become events like anything else.
    if let Some(mut rf_rx) = rf_rx {
        let tx = events.clone();
        tokio::spawn(async move {
            while let Some(frame) = rf_rx.recv().await {
                if tx.send(Event::Rf(frame)).await.is_err() {
                    break;
                }
            }
        });
    }

    Ok(Gateway {
        server,
        events,
        events_rx,
        loopback,
    })
}

/// Bind the configured listeners. Returns the addresses actually bound, which
/// is what a test needs when the configuration asks for port 0.
pub async fn spawn_listeners(
    config: &Config,
    events: mpsc::Sender<Event>,
    admission: Arc<Admission>,
) -> std::io::Result<Vec<String>> {
    let ids = Arc::new(AtomicU64::new(1));
    let opts_plain = ListenerOptions {
        ping_interval: Duration::from_secs(config.listen.ping_interval_secs),
        tls: None,
        admission: Some(admission.clone()),
    };
    let mut bound = Vec::new();
    for addr in &config.listen.bind {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        bound.push(listener.local_addr()?.to_string());
        let tx = events.clone();
        let ids = ids.clone();
        let opts = opts_plain.clone();
        let name = addr.clone();
        tokio::spawn(async move {
            if let Err(e) = listen(listener, tx, ids, opts).await {
                error!(addr = %name, "listener failed: {e}");
            }
        });
    }
    if let Some(tls) = &config.listen.tls {
        let acceptor = crate::irc::tls::acceptor(&tls.cert, &tls.key)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        for addr in &tls.bind {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            bound.push(format!("tls:{}", listener.local_addr()?));
            let tx = events.clone();
            let ids = ids.clone();
            let opts = ListenerOptions {
                ping_interval: Duration::from_secs(config.listen.ping_interval_secs),
                tls: Some(acceptor.clone()),
                admission: Some(admission.clone()),
            };
            let name = addr.clone();
            tokio::spawn(async move {
                if let Err(e) = listen(listener, tx, ids, opts).await {
                    error!(addr = %name, "TLS listener failed: {e}");
                }
            });
        }
    }
    Ok(bound)
}

/// Run a built gateway until it shuts down.
pub async fn serve(gateway: Gateway) {
    let Gateway {
        server,
        events_rx,
        loopback,
        ..
    } = gateway;
    // Keep the fake radio open for as long as the server runs.
    let _loopback = loopback;
    server::run(server, events_rx).await;
}

/// Ask the server to shut down when the operator interrupts it.
pub fn spawn_shutdown(events: mpsc::Sender<Event>) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("shutting down");
            let _ = events.send(Event::Shutdown).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_the_command_line() {
        assert_eq!(
            parse_args(args(&[])),
            Invocation::Run {
                path: DEFAULT_CONFIG.into()
            }
        );
        assert_eq!(
            parse_args(args(&["-c", "/etc/rfircd.toml"])),
            Invocation::Run {
                path: "/etc/rfircd.toml".into()
            }
        );
        assert_eq!(
            parse_args(args(&["--config", "a.toml", "--check"])),
            Invocation::Check {
                path: "a.toml".into()
            }
        );
        assert!(matches!(parse_args(args(&["-V"])), Invocation::Version(_)));
        assert!(matches!(
            parse_args(args(&["--hash-password"])),
            Invocation::HashPassword
        ));
        assert!(matches!(parse_args(args(&["--wat"])), Invocation::Usage(_)));
        assert!(
            matches!(parse_args(args(&["--config"])), Invocation::Usage(_)),
            "a missing path must be an error, not a silent fall back to the default"
        );
    }

    #[test]
    fn init_takes_the_config_path_and_outranks_check() {
        assert_eq!(
            parse_args(args(&["--init"])),
            Invocation::Init {
                path: DEFAULT_CONFIG.into()
            }
        );
        assert_eq!(
            parse_args(args(&["--init", "-c", "/etc/rfircd/rfircd.toml"])),
            Invocation::Init {
                path: "/etc/rfircd/rfircd.toml".into()
            }
        );
        // There is nothing to check before the file exists, so the pair is
        // the setup run rather than an error or a silent no-op.
        assert_eq!(
            parse_args(args(&["--check", "--init", "-c", "a.toml"])),
            Invocation::Init {
                path: "a.toml".into()
            }
        );
    }

    #[test]
    fn resolves_every_tnc_kind() {
        let tcp = TncSection {
            kind: "tcp".into(),
            host: "10.0.0.1".into(),
            port: 8002,
            ..Default::default()
        };
        let (link, far) = resolve_link(&tcp).unwrap();
        assert!(far.is_none());
        assert!(
            matches!(link, TncLink::Tcp { ref host, port } if host == "10.0.0.1" && port == 8002)
        );

        let loop_back = TncSection {
            kind: "loopback".into(),
            ..Default::default()
        };
        let (link, far) = resolve_link(&loop_back).unwrap();
        assert!(matches!(link, TncLink::Loopback(_)));
        assert!(
            far.is_some(),
            "the far end has to be handed back, or the fake radio closes immediately"
        );

        let bogus = TncSection {
            kind: "carrier-pigeon".into(),
            ..Default::default()
        };
        let err = resolve_link(&bogus).unwrap_err().to_string();
        assert!(
            err.contains("carrier-pigeon") && err.contains("loopback"),
            "{err}"
        );
    }

    #[cfg(not(feature = "serial"))]
    #[test]
    fn serial_without_the_feature_says_how_to_get_it() {
        let serial = TncSection {
            kind: "serial".into(),
            device: "/dev/ttyUSB0".into(),
            ..Default::default()
        };
        let err = resolve_link(&serial).unwrap_err().to_string();
        assert!(err.contains("--features serial"), "{err}");
    }

    const GATEWAY: &str = r##"
[server]
name = "startup.test"

[listen]
bind = ["127.0.0.1:0"]

[radio]
enabled = true
callsign = "SK0MT-1"
id_interval_secs = 60

[radio.tnc]
kind = "loopback"

[accounts]
file = "target/test-cli-nicks.json"

[[channels]]
name = "#rf"
rf = true
"##;

    #[tokio::test]
    async fn a_gateway_starts_with_its_radio_attached() {
        let config = Arc::new(Config::from_toml(GATEWAY).unwrap());
        let gw = build(config.clone()).unwrap();
        assert!(
            gw.loopback.is_some(),
            "a loopback TNC hands back its far end, or the fake radio closes"
        );
        assert!(gw.server.radio.available(), "the radio should be usable");
        assert!(
            gw.server.radio.airtime().is_some(),
            "airtime counters are published"
        );
        assert!(gw.server.state.channel("#rf").is_some());
        assert!(gw.server.radio.status_line().contains("transmitter ON"));
    }

    #[tokio::test]
    async fn a_gateway_with_no_radio_is_a_plain_irc_server() {
        let text = GATEWAY.replace("enabled = true", "enabled = false");
        let config = Arc::new(Config::from_toml(&text).unwrap());
        let gw = build(config).unwrap();
        assert!(gw.loopback.is_none());
        assert!(!gw.server.radio.available());
        assert!(gw.server.radio.airtime().is_none());
        assert!(gw.server.radio.status_line().contains("disabled"));
    }

    #[tokio::test]
    async fn listeners_bind_and_report_their_addresses() {
        let config = Arc::new(Config::from_toml(GATEWAY).unwrap());
        let gw = build(config.clone()).unwrap();
        let bound = spawn_listeners(&config, gw.events.clone(), gw.server.admission.clone())
            .await
            .unwrap();
        assert_eq!(bound.len(), 1);
        assert_ne!(
            bound[0], "127.0.0.1:0",
            "port 0 should be resolved to the port actually bound"
        );
        // And it accepts a connection.
        let _ = tokio::net::TcpStream::connect(&bound[0]).await.unwrap();
    }

    #[tokio::test]
    async fn a_listener_that_cannot_bind_fails_at_startup() {
        // Port 1 without privileges, or an address that is not ours.
        let text = GATEWAY.replace(
            r#"bind = ["127.0.0.1:0"]"#,
            r#"bind = ["203.0.113.1:6667"]"#,
        );
        let config = Arc::new(Config::from_toml(&text).unwrap());
        let gw = build(config.clone()).unwrap();
        assert!(
            spawn_listeners(&config, gw.events.clone(), gw.server.admission.clone())
                .await
                .is_err(),
            "a bind failure must surface at startup, not only in the log"
        );
    }

    #[tokio::test]
    async fn a_started_gateway_serves_and_shuts_down() {
        let config = Arc::new(Config::from_toml(GATEWAY).unwrap());
        let gw = build(config.clone()).unwrap();
        let bound = spawn_listeners(&config, gw.events.clone(), gw.server.admission.clone())
            .await
            .unwrap();
        let events = gw.events.clone();
        let handle = tokio::spawn(serve(gw));

        // A real client can register against it.
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let stream = tokio::net::TcpStream::connect(&bound[0]).await.unwrap();
        let (r, mut w) = stream.into_split();
        w.write_all(b"NICK alice\r\nUSER alice 0 * :Alice\r\n")
            .await
            .unwrap();
        let mut lines = BufReader::new(r).lines();
        let mut welcomed = false;
        for _ in 0..40 {
            match tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await {
                Ok(Ok(Some(line))) if line.contains(" 001 ") => {
                    welcomed = true;
                    break;
                }
                Ok(Ok(Some(_))) => continue,
                _ => break,
            }
        }
        assert!(welcomed, "the gateway should welcome a client");

        events.send(Event::Shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the server should stop when told to")
            .unwrap();
    }

    #[test]
    fn help_and_version_say_what_they_should() {
        assert!(version().starts_with("rfircd "));
        let h = help();
        assert!(
            h.contains("--check") && h.contains("--hash-password") && h.contains(DEFAULT_CONFIG)
        );
    }
}
