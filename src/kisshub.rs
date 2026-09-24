//! A virtual radio channel: KISS over TCP, with every connected client on the
//! same frequency.
//!
//! A frame from one client is delivered to all the others, exactly as a shared
//! half-duplex channel would (minus the collisions, the noise and the fun). It
//! exists so the gateway and the station client can be developed, demonstrated
//! and tested end to end without a radio, a TNC or a licence.
//!
//! Frames are re-framed rather than forwarded as raw bytes, so a client that
//! writes half a frame, or garbage, cannot corrupt anyone else's stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};

use crate::ax25::kiss::{self, KissDecoder};
use crate::ax25::Ax25Frame;

/// Frames buffered for one client before it is dropped.
///
/// Bounded for the same reason the IRC side is: a client that stops reading
/// must cost itself, not the process. On a virtual channel this matters less
/// than on the server, but a test harness that wedges is still a test harness
/// that wedges.
const CLIENT_QUEUE: usize = 256;

/// Largest KISS frame accepted from a client. Generous — this is a test
/// channel, not the air — but not unbounded.
const MAX_FRAME: usize = 2048;

/// Stations allowed on the channel at once.
///
/// A real frequency has a practical limit and so should this: each client
/// costs a task and a `CLIENT_QUEUE`-deep buffer of frames, so an unbounded
/// number of them is an unbounded amount of memory. The default bind is
/// loopback, but `--bind` takes any address and somebody will eventually
/// point it at one.
const MAX_STATIONS: usize = 64;

type Clients = Arc<Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub bind: String,
    /// Print every frame in `axlisten` monitor format.
    pub monitor: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8001".into(),
            monitor: true,
        }
    }
}

/// What `main` should do after parsing the command line.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    Run(Options),
    /// `--help` was given; the text to print.
    Help(String),
    /// Bad usage; the message and the exit code.
    Usage(String),
}

pub const USAGE: &str = "usage: rfirc-kisshub [--bind 127.0.0.1:8001] [--quiet]";

pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Invocation {
    let mut opts = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" | "-b" => match args.next() {
                Some(v) => opts.bind = v,
                None => return Invocation::Usage("--bind needs an address".into()),
            },
            "--quiet" => opts.monitor = false,
            "--help" | "-h" => return Invocation::Help(USAGE.into()),
            other => return Invocation::Usage(format!("unknown argument: {other}")),
        }
    }
    Invocation::Run(opts)
}

/// Serve the virtual channel until the listener fails.
pub async fn run(opts: Options) -> std::io::Result<()> {
    let listener = TcpListener::bind(&opts.bind).await?;
    println!("virtual channel listening on {}", listener.local_addr()?);
    serve(listener, opts).await
}

/// As [`run`], on a listener the caller already bound. Tests use this to get
/// an ephemeral port.
pub async fn serve(listener: TcpListener, opts: Options) -> std::io::Result<()> {
    let clients: Clients = Arc::new(Mutex::new(HashMap::new()));
    let ids = AtomicU64::new(1);
    loop {
        // A failed accept is not a reason to take the channel down: the
        // listener outlives any one peer, and per-connection errors (a client
        // that vanished between the SYN and the accept, a momentary fd
        // shortage) are routine.
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("accept failed: {e}");
                continue;
            }
        };
        if clients.lock().await.len() >= MAX_STATIONS {
            // Refused rather than queued: there is nothing useful to say to a
            // KISS client, and holding the socket open would cost the memory
            // the cap exists to bound.
            eprintln!("channel full ({MAX_STATIONS} stations); refusing {peer}");
            drop(stream);
            continue;
        }
        let id = ids.fetch_add(1, Ordering::Relaxed);
        println!("station {id} on channel ({peer})");
        let clients = clients.clone();
        let monitor = opts.monitor;
        tokio::spawn(async move {
            station(stream, id, clients, monitor).await;
            println!("station {id} left the channel");
        });
    }
}

async fn station(stream: TcpStream, id: u64, clients: Clients, monitor: bool) {
    let (mut read_half, mut write_half) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CLIENT_QUEUE);
    clients.lock().await.insert(id, tx);

    let writer = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if write_half.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let mut decoder = KissDecoder::new(MAX_FRAME);
    let mut buf = vec![0u8; 4096];
    while let Ok(n) = read_half.read(&mut buf).await {
        if n == 0 {
            break;
        }
        for frame in decoder.push(&buf[..n]) {
            if frame.command != kiss::CMD_DATA {
                // TXDELAY, TXTAIL and friends are local to a TNC; a real
                // channel does not carry them either.
                continue;
            }
            if monitor {
                match Ax25Frame::decode(&frame.payload) {
                    Ok(ax) => println!("{}", ax.to_monitor_line()),
                    Err(_) => println!("[{} bytes, undecodable]", frame.payload.len()),
                }
            }
            let wire = kiss::encode(0, kiss::CMD_DATA, &frame.payload);
            let mut peers = clients.lock().await;
            // Everyone but the sender. A client whose queue is full has
            // stopped reading; drop it rather than block the channel.
            peers.retain(|other, tx| *other == id || tx.try_send(wire.clone()).is_ok());
        }
    }

    clients.lock().await.remove(&id);
    writer.abort();
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
            parse_args(args(&["--bind", "0.0.0.0:9001", "--quiet"])),
            Invocation::Run(Options {
                bind: "0.0.0.0:9001".into(),
                monitor: false,
            })
        );
        assert_eq!(parse_args(args(&[])), Invocation::Run(Options::default()));
        assert!(matches!(parse_args(args(&["-h"])), Invocation::Help(_)));
        assert!(matches!(
            parse_args(args(&["--nonsense"])),
            Invocation::Usage(_)
        ));
        assert!(
            matches!(parse_args(args(&["--bind"])), Invocation::Usage(_)),
            "a flag with a missing value must not silently keep the default"
        );
    }

    async fn hub() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(serve(
            listener,
            Options {
                bind: addr.clone(),
                monitor: false,
            },
        ));
        addr
    }

    /// A client that keeps its decoder across reads.
    ///
    /// A fresh decoder per call would throw away whatever was left in the
    /// buffer from the previous frame, so a test that reads two frames in a
    /// row would lose the second one and blame the code under test.
    struct Listener {
        stream: TcpStream,
        decoder: KissDecoder,
        pending: std::collections::VecDeque<Vec<u8>>,
    }

    impl Listener {
        async fn connect(addr: &str) -> Self {
            Self {
                stream: TcpStream::connect(addr).await.unwrap(),
                decoder: KissDecoder::new(MAX_FRAME),
                pending: std::collections::VecDeque::new(),
            }
        }

        /// Next frame payload, or `None` if the channel stays quiet.
        async fn next_frame(&mut self) -> Option<Vec<u8>> {
            let mut buf = [0u8; 1024];
            for _ in 0..10 {
                if let Some(f) = self.pending.pop_front() {
                    return Some(f);
                }
                let n = tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    self.stream.read(&mut buf),
                )
                .await
                .ok()?
                .ok()?;
                if n == 0 {
                    return None;
                }
                self.pending
                    .extend(self.decoder.push(&buf[..n]).into_iter().map(|f| f.payload));
            }
            None
        }

        async fn send(&mut self, command: u8, payload: &[u8]) {
            self.stream
                .write_all(&kiss::encode(0, command, payload))
                .await
                .unwrap();
        }

        async fn send_raw(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).await.unwrap();
        }
    }

    fn frame(from: &str) -> Vec<u8> {
        crate::ax25::Ax25Frame::ui(
            from.parse().unwrap(),
            "AIRC".parse().unwrap(),
            &[],
            b"hello".to_vec(),
        )
        .unwrap()
        .encode()
    }

    #[tokio::test]
    async fn a_frame_reaches_every_other_station_but_not_the_sender() {
        let addr = hub().await;
        let mut a = Listener::connect(&addr).await;
        let mut b = Listener::connect(&addr).await;
        let mut c = Listener::connect(&addr).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let payload = frame("SM0ABC-7");
        a.send(kiss::CMD_DATA, &payload).await;

        assert_eq!(b.next_frame().await.as_deref(), Some(&payload[..]));
        assert_eq!(c.next_frame().await.as_deref(), Some(&payload[..]));
        assert_eq!(
            a.next_frame().await,
            None,
            "a station does not hear its own transmission back"
        );
    }

    #[tokio::test]
    async fn tnc_parameter_frames_stay_local() {
        let addr = hub().await;
        let mut a = Listener::connect(&addr).await;
        let mut b = Listener::connect(&addr).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // TXDELAY is a setting for one TNC, not something a channel carries.
        a.send(kiss::CMD_TXDELAY, &[40]).await;
        let data = frame("SM0XYZ-1");
        a.send(kiss::CMD_DATA, &data).await;

        assert_eq!(
            b.next_frame().await.as_deref(),
            Some(&data[..]),
            "the parameter frame should have been swallowed, leaving the data frame first"
        );
    }

    #[tokio::test]
    async fn a_station_that_leaves_is_forgotten() {
        let addr = hub().await;
        let mut a = Listener::connect(&addr).await;
        let b = Listener::connect(&addr).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(b);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // The remaining station keeps working after a peer disappears.
        let mut c = Listener::connect(&addr).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let payload = frame("SM0ABC-7");
        a.send(kiss::CMD_DATA, &payload).await;
        assert_eq!(c.next_frame().await.as_deref(), Some(&payload[..]));
    }

    #[tokio::test]
    async fn garbage_from_one_station_cannot_corrupt_another() {
        let addr = hub().await;
        let mut a = Listener::connect(&addr).await;
        let mut b = Listener::connect(&addr).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Half a frame, then noise, then a good frame. The noise is relayed
        // as its own (undecodable) frame — a real channel carries noise too —
        // but it must not be spliced onto the good one.
        a.send_raw(&[kiss::FEND, 0x00, 0x41, 0x42]).await;
        a.send_raw(&[0xFF; 64]).await;
        let payload = frame("SM0ABC-7");
        a.send(kiss::CMD_DATA, &payload).await;

        let mut seen = Vec::new();
        for _ in 0..4 {
            match b.next_frame().await {
                Some(f) => {
                    if f == payload {
                        return;
                    }
                    seen.push(f.len());
                }
                None => break,
            }
        }
        panic!("the good frame did not survive a neighbour writing garbage; saw frames of {seen:?} bytes");
    }

    #[tokio::test]
    async fn an_oversized_frame_is_dropped_not_relayed() {
        let addr = hub().await;
        let mut a = Listener::connect(&addr).await;
        let mut b = Listener::connect(&addr).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        a.send(kiss::CMD_DATA, &vec![0x41; MAX_FRAME * 2]).await;
        let payload = frame("SM0ABC-7");
        a.send(kiss::CMD_DATA, &payload).await;

        assert_eq!(
            b.next_frame().await.as_deref(),
            Some(&payload[..]),
            "the oversized frame should have been dropped by the decoder"
        );
    }
}
