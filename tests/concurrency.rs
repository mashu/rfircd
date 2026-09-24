//! Does the server actually handle many clients at once, and does it stay up
//! under the kind of load that is cheap for an attacker to produce?
//!
//! These drive real TCP sockets against a real listener rather than poking the
//! `Server` struct, because the questions they answer — concurrency, accept
//! backlog, connection caps — are about the parts a unit test skips.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rfircd::config::Config;
use rfircd::irc::client::{listen, ListenerOptions};
use rfircd::server::{self, Event, Server};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const CONFIG: &str = r##"
[server]
name = "load.test"
motd = ["hello"]

[listen]
bind = []
ping_interval_secs = 60
registration_timeout_secs = 60
max_conns_per_host = 0
max_clients = 0

[accounts]
file = "target/test-concurrency-nicks.json"

[[channels]]
name = "#lobby"
"##;

/// Start a server on an ephemeral port and return its address.
async fn start(config_text: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = format!(
        "target/test-concurrency-nicks-{}-{}.json",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    let config_text = config_text.replace("target/test-concurrency-nicks.json", &path);
    let config = Arc::new(Config::from_toml(&config_text).unwrap());
    let (events_tx, events_rx) = mpsc::channel::<Event>(1024);
    let mut srv = Server::new(config, None).unwrap();
    srv.attach_events(events_tx.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();

    let opts = ListenerOptions {
        ping_interval: Duration::from_secs(60),
        tls: None,
        admission: Some(srv.admission.clone()),
    };
    tokio::spawn(async move {
        let _ = listen(listener, events_tx, Arc::new(AtomicU64::new(1)), opts).await;
    });
    tokio::spawn(server::run(srv, events_rx));
    // Give the listener a moment to bind.
    tokio::time::sleep(Duration::from_millis(100)).await;
    addr
}

/// Register one client and read until it has been welcomed.
async fn register(addr: &str, nick: &str) -> std::io::Result<TcpStream> {
    let stream = TcpStream::connect(addr).await?;
    let (r, mut w) = stream.into_split();
    w.write_all(format!("NICK {nick}\r\nUSER {nick} 0 * :{nick}\r\n").as_bytes())
        .await?;
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        // 001 RPL_WELCOME
        if line.contains(" 001 ") {
            let r = lines.into_inner().into_inner();
            return Ok(r.reunite(w).expect("halves of the same socket"));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "never welcomed",
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hundred_clients_connect_and_talk_at_once() {
    let addr = start(CONFIG).await;

    // All hundred handshake concurrently, not one after another.
    let mut joins = Vec::new();
    for i in 0..100u32 {
        let addr = addr.clone();
        joins.push(tokio::spawn(async move {
            let mut s = tokio::time::timeout(
                Duration::from_secs(20),
                register(&addr, &format!("load_{i:03}")),
            )
            .await
            .expect("registration timed out")
            .expect("registration failed");
            s.write_all(b"JOIN #lobby\r\nPRIVMSG #lobby :hello\r\n")
                .await
                .unwrap();
            s
        }));
    }
    let mut held = Vec::new();
    for j in joins {
        held.push(j.await.expect("client task panicked"));
    }
    assert_eq!(held.len(), 100);

    // The server is still responsive after all that.
    let mut probe = register(&addr, "probe")
        .await
        .expect("server stopped accepting");
    probe.write_all(b"PING :alive\r\n").await.unwrap();
    let (r, _w) = probe.into_split();
    let mut lines = BufReader::new(r).lines();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let line = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .expect("server went unresponsive under load")
            .unwrap()
            .expect("connection closed");
        if line.contains("PONG") {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_never_registers_is_reaped() {
    let text = CONFIG.replace(
        "registration_timeout_secs = 60",
        "registration_timeout_secs = 1",
    );
    let addr = start(&text).await;

    // Open sockets and say nothing. This is the cheapest denial of service
    // there is, so it has to cost the attacker something.
    let mut silent = Vec::new();
    for _ in 0..20 {
        silent.push(TcpStream::connect(&addr).await.unwrap());
    }
    // The reaper runs on the 2 s housekeeping tick.
    tokio::time::sleep(Duration::from_secs(6)).await;

    let mut dropped = 0;
    for s in &silent {
        let mut buf = [0u8; 64];
        match s.try_read(&mut buf) {
            // Either an ERROR line or a closed socket means it was reaped.
            Ok(0) => dropped += 1,
            Ok(n) => {
                if String::from_utf8_lossy(&buf[..n]).contains("Registration timeout") {
                    dropped += 1;
                }
            }
            Err(_) => {}
        }
    }
    assert!(
        dropped > 0,
        "sockets that never register must not be held forever"
    );

    // And the server still takes real clients.
    register(&addr, "still_here")
        .await
        .expect("the reaper should not have taken the server with it");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_flood_from_one_host_is_capped() {
    let text = CONFIG.replace("max_conns_per_host = 0", "max_conns_per_host = 5");
    let addr = start(&text).await;

    let mut refused = 0;
    let mut held = Vec::new();
    for i in 0..25u32 {
        match tokio::time::timeout(
            Duration::from_secs(5),
            register(&addr, &format!("flood_{i:03}")),
        )
        .await
        {
            Ok(Ok(s)) => held.push(s),
            _ => refused += 1,
        }
    }
    assert!(
        refused > 0 && held.len() <= 6,
        "the per-host cap did not bite: {} accepted, {refused} refused",
        held.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unregistered_sockets_count_toward_max_clients() {
    let text = CONFIG.replace("max_clients = 0", "max_clients = 3");
    let addr = start(&text).await;
    let mut silent = Vec::new();
    for _ in 0..3 {
        silent.push(TcpStream::connect(&addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let extra = TcpStream::connect(&addr).await.unwrap();
    let (r, mut w) = extra.into_split();
    w.write_all(b"NICK late\r\nUSER late 0 * :late\r\n")
        .await
        .unwrap();
    let mut lines = BufReader::new(r).lines();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut saw_full = false;
    let mut welcomed = false;
    loop {
        match tokio::time::timeout_at(deadline, lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                if line.contains("Server is full") {
                    saw_full = true;
                    break;
                }
                if line.contains(" 001 ") {
                    welcomed = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        saw_full && !welcomed,
        "sockets that have not registered must still occupy max_clients"
    );
    drop(silent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_shouting_without_reading_does_not_grow_the_server() {
    let addr = start(CONFIG).await;
    let mut victim = register(&addr, "quiet").await.unwrap();
    victim.write_all(b"JOIN #lobby\r\n").await.unwrap();

    // A second client floods the channel. The first never reads a byte, so
    // its output queue fills; the server must drop it rather than buffer
    // without limit.
    let mut loud = register(&addr, "loud").await.unwrap();
    loud.write_all(b"JOIN #lobby\r\n").await.unwrap();
    for i in 0..5000 {
        if loud
            .write_all(format!("PRIVMSG #lobby :flood {i}\r\n").as_bytes())
            .await
            .is_err()
        {
            break;
        }
    }

    // The server is still serving other clients, which is the point.
    tokio::time::sleep(Duration::from_millis(500)).await;
    register(&addr, "bystander")
        .await
        .expect("a slow reader took the server down with it");
    drop(victim);
}
