//! `rfirc-station` — the client side of the gateway, for an operator with a
//! radio and a TNC.
//!
//! It speaks AIRC/1 (see `docs/protocol.md`) over KISS and presents a plain
//! line-oriented interface, so it works over ssh, on a Pi with no screen, or
//! piped into anything else. The implementation is in [`rfircd::station`];
//! this is the command line and the terminal around it.
//!
//! ```sh
//! rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --channel '#rf'
//! rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --tnc tcp://192.168.1.10:8001
//! rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --tnc serial:/dev/ttyUSB0@9600
//! ```

use std::time::{Duration, Instant};

use rfircd::airc::{encode_fields, Kind, SessionConfig, Sessions};
use rfircd::ax25::tnc::{self, TncConfig};
use rfircd::station::{self, Invocation, Station};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = match station::parse_args(std::env::args().skip(1)) {
        Invocation::Run(args) => *args,
        Invocation::Help(text) => {
            println!("{text}");
            return Ok(());
        }
        Invocation::Usage(message) => {
            eprintln!("{message}");
            eprintln!("{}", station::usage());
            std::process::exit(2);
        }
    };

    let cfg = TncConfig {
        link: args.link.clone(),
        kiss_port: 0,
        max_frame: args.paclen + 64,
        tx_pacing: Duration::from_millis(800),
        tx_queue_depth: 32,
        persistence: None,
        slottime: None,
        airtime: args.airtime.clone(),
    };
    let (tnc, mut rf_rx) = tnc::spawn(cfg);
    let sessions = Sessions::new(SessionConfig {
        paclen: args.paclen,
        ..Default::default()
    });

    println!(
        "-- {} calling gateway {} ; /help for commands",
        args.call, args.gateway
    );
    let channel = args.channel.clone();
    let mut station = Station::new(args, tnc, sessions);

    station.send_hello();
    if let Some(chan) = channel {
        station.send(Kind::Join, encode_fields(&[&chan]), true);
        station.set_channel(Some(chan));
    }
    flush(&mut station);

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            frame = rf_rx.recv() => match frame {
                Some(ax) => station.handle_rf(ax),
                None => break,
            },
            line = stdin.next_line() => match line {
                Ok(Some(line)) => {
                    if !station.handle_input(&line) {
                        flush(&mut station);
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            },
            _ = ticker.tick() => {
                if station.tick(Instant::now()) {
                    println!("!! the gateway is not answering");
                }
            }
        }
        flush(&mut station);
    }

    println!("-- 73");
    Ok(())
}

/// The station collects what it wants to say rather than printing it, so the
/// protocol logic is testable. Putting it on the terminal is this loop's job.
fn flush(station: &mut Station) {
    for line in station.drain_output() {
        println!("{line}");
    }
}
