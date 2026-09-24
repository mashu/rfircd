//! Implicit TLS on the IRC listener: a client that speaks must have a
//! certificate in front of it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rfircd::config::Config;
use rfircd::irc::client::{listen, ListenerOptions};
use rfircd::irc::tls;
use rfircd::server::{self, Event, Server};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

fn write_self_signed(dir: &str) -> (String, String) {
    let _ = std::fs::create_dir_all(dir);
    let cert = rcgen::generate_simple_self_signed(["localhost".into()]).unwrap();
    let cert_path = format!("{dir}/cert.pem");
    let key_path = format!("{dir}/key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
    (cert_path, key_path)
}

#[tokio::test]
async fn a_tls_client_can_register_and_speak() {
    tls::ensure_provider();
    let dir = format!("target/test-tls-{}-{}", std::process::id(), {
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    });
    let (cert_path, key_path) = write_self_signed(&dir);

    let toml = format!(
        r##"
[server]
name = "tls.test"
[listen]
bind = []
[listen.tls]
bind = ["127.0.0.1:0"]
cert = "{cert_path}"
key = "{key_path}"
[accounts]
file = "{dir}/nicks.json"
[[channels]]
name = "#lobby"
"##
    );
    let config = Arc::new(Config::from_toml(&toml).unwrap());
    let (events_tx, events_rx) = mpsc::channel::<Event>(256);
    let mut srv = Server::new(config.clone(), None).unwrap();
    srv.attach_events(events_tx.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = tls::acceptor(&cert_path, &key_path).unwrap();
    let admission = srv.admission.clone();
    tokio::spawn(async move {
        let _ = listen(
            listener,
            events_tx,
            Arc::new(AtomicU64::new(1)),
            ListenerOptions {
                ping_interval: Duration::from_secs(60),
                tls: Some(acceptor),
                admission: Some(admission),
            },
        )
        .await;
    });
    tokio::spawn(server::run(srv, events_rx));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let pem = std::fs::read(&cert_path).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_pemfile::certs(&mut pem.as_slice()) {
        roots.add(c.unwrap()).unwrap();
    }
    let client_cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_cfg));
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let mut tls_stream = connector.connect(name, tcp).await.unwrap();
    tls_stream
        .write_all(b"NICK alice\r\nUSER alice 0 * :Alice\r\n")
        .await
        .unwrap();
    let mut lines = BufReader::new(tls_stream).lines();
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
    assert!(welcomed, "a TLS client should be welcomed");
    let _ = std::fs::remove_dir_all(&dir);
}
