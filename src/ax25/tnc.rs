//! The radio link: a task that owns the TNC connection, turns bytes into
//! [`Ax25Frame`]s and back, reconnects when the TNC goes away, and paces
//! transmissions so we do not bury the channel.
//!
//! Three link types are supported:
//!   * `tcp`      - KISS over TCP, e.g. Direwolf's port 8001. The normal case.
//!   * `serial`   - KISS over a serial port (feature `serial`).
//!   * `loopback` - an in-process fake radio for development and tests.

use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::airtime::{AirtimeConfig, AirtimeShared, Governor, TxDecision};
use super::frame::Ax25Frame;
use super::kiss::{self, KissDecoder};
use super::scheduler::{Class, Poll, Queued, Scheduler, SchedulerConfig};
use crate::audit::Audit;
use crate::callsign::Callsign;

pub trait ReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ReadWrite for T {}

/// How to reach the TNC.
#[derive(Clone, Debug)]
pub enum TncLink {
    Tcp {
        host: String,
        port: u16,
    },
    #[cfg(feature = "serial")]
    Serial {
        path: String,
        baud: u32,
    },
    /// In-process loopback. The far end of the duplex stream is handed to the
    /// test harness (see `TncConfig::loopback`).
    Loopback(Arc<Mutex<Option<DuplexStream>>>),
}

#[derive(Clone, Debug)]
pub struct TncConfig {
    pub link: TncLink,
    /// KISS port number on the TNC (0-15).
    pub kiss_port: u8,
    /// Largest AX.25 frame we will accept or emit.
    pub max_frame: usize,
    /// Minimum gap between transmissions, to leave the channel usable by
    /// others. At 1200 baud a 256 byte frame already occupies ~1.9 s.
    pub tx_pacing: Duration,
    /// Frames queued for transmission before we start dropping.
    pub tx_queue_depth: usize,
    /// KISS persistence and slot time, if we should set them.
    pub persistence: Option<u8>,
    pub slottime: Option<u8>,
    /// Duty-cycle and airtime limits. This is what keeps a QRP transmitter
    /// alive and the channel usable; see [`super::airtime`].
    pub airtime: AirtimeConfig,
}

impl Default for TncConfig {
    fn default() -> Self {
        Self {
            link: TncLink::Tcp {
                host: "127.0.0.1".into(),
                port: 8001,
            },
            kiss_port: 0,
            max_frame: 512,
            tx_pacing: Duration::from_millis(1500),
            tx_queue_depth: 64,
            persistence: None,
            slottime: None,
            airtime: AirtimeConfig::default(),
        }
    }
}

impl TncConfig {
    /// Everything the TNC task needs, derived from the parsed configuration.
    ///
    /// One constructor, used by the server and by the tests alike. When these
    /// were two pieces of code the test harness quietly stopped passing
    /// `[radio.duty]` through, so the airtime governor was disabled in every
    /// integration test while the unit tests said it worked.
    ///
    /// `link` is a parameter rather than derived here because choosing it is
    /// the one part that differs: the server resolves `radio.tnc.kind`, and a
    /// test substitutes a loopback whose far end it keeps.
    pub fn from_config(config: &crate::config::Config, link: TncLink) -> Self {
        let section = &config.radio.tnc;
        Self {
            link,
            kiss_port: section.kiss_port,
            // paclen is the *information* field. A full AX.25 header with
            // eight digipeaters is 58 octets on top of it; +32 silently
            // discarded long-path frames we were perfectly able to decode.
            max_frame: config.radio.paclen + 64,
            tx_pacing: Duration::from_millis(section.tx_pacing_ms),
            tx_queue_depth: 64,
            persistence: section.persistence,
            slottime: section.slottime,
            airtime: config.radio.duty.to_airtime(),
        }
    }

    /// Build a loopback link and return the far end, which behaves like a TNC:
    /// write KISS frames into it to simulate reception, read to see what the
    /// server transmitted.
    ///
    /// The link only. Pair it with [`TncConfig::from_config`] to get a TNC
    /// configured exactly as the server would configure it.
    pub fn loopback_link() -> (TncLink, DuplexStream) {
        let (near, far) = tokio::io::duplex(64 * 1024);
        (TncLink::Loopback(Arc::new(Mutex::new(Some(near)))), far)
    }
}

/// A frame that has just been written to the TNC. The session layer starts
/// its ACK clock here, not at enqueue: a message sitting behind the governor
/// must not be declared lost before it has keyed.
#[derive(Clone, Debug)]
pub struct Keyed {
    pub dest: Callsign,
    pub seq: u16,
}

/// Handle used by the rest of the server to transmit.
#[derive(Clone)]
pub struct TncHandle {
    scheduler: Arc<StdMutex<Scheduler<Ax25Frame>>>,
    keyed_rx: Arc<StdMutex<Option<mpsc::Receiver<Keyed>>>>,
    airtime: Arc<AirtimeShared>,
    /// A copy of the governor's cost model, so the sender can price a frame
    /// before committing to it. The governor itself lives in the TNC task.
    cost: AirtimeConfig,
}

impl TncHandle {
    /// Live airtime counters and the hard transmit inhibit. Shared with the
    /// TNC task; see [`AirtimeShared`].
    pub fn airtime(&self) -> &Arc<AirtimeShared> {
        &self.airtime
    }

    /// Take the key-down event stream. Once, at startup; later calls return
    /// `None`.
    pub fn take_keyed(&self) -> Option<mpsc::Receiver<Keyed>> {
        self.keyed_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Stop transmitting *now*. Frames already queued are discarded rather
    /// than radiated later: an operator who says "off" means off, not
    /// "off once the backlog has drained". Identification is kept so a
    /// sign-off can still go out.
    pub fn set_inhibit(&self, inhibit: bool) {
        self.airtime.inhibit.store(inhibit, Ordering::Release);
        self.airtime.wake.notify_waiters();
    }

    pub fn inhibited(&self) -> bool {
        self.airtime.inhibit.load(Ordering::Acquire)
    }

    /// Queue a station identification. Jumps the *data* queue so a sign-off
    /// is not stuck behind chat, but still waits for the airtime clock and
    /// the governor — an ID is key-down time like anything else.
    pub fn try_send_id(&self, frame: Ax25Frame) -> bool {
        self.enqueue(frame, Class::Id, "id", false)
    }

    /// Key-down time a frame of this size will cost.
    pub fn airtime_for(&self, octets: usize) -> Duration {
        Governor::new(self.cost.clone()).airtime_for(octets)
    }

    /// Airtime already queued and not yet radiated.
    pub fn queued(&self) -> Duration {
        self.airtime.queued()
    }

    /// Best estimate of how long a frame queued now would wait: the
    /// governor's next free slot plus the existing backlog.
    pub fn eta(&self) -> Duration {
        self.airtime.eta()
    }

    /// How many frames of this class `enqueue` will still accept.
    pub fn tx_room_in(&self, class: Class) -> usize {
        self.scheduler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .room_in(class)
    }

    /// How many ordinary (chat) frames `try_send` will still accept.
    pub fn tx_room(&self) -> usize {
        self.tx_room_in(Class::Chat)
    }

    /// Queue a frame as channel conversation. Tests and the station client
    /// use this; the gateway passes an explicit class via [`TncHandle::enqueue`].
    pub fn try_send(&self, frame: Ax25Frame) -> bool {
        let account = frame.destination.call.to_string();
        self.enqueue(frame, Class::Chat, &account, false)
    }

    /// Queue a frame in a specific class. Returns false if that class is full,
    /// which is a normal condition on a congested channel and must be handled
    /// by the caller (usually: drop, count, and tell the user).
    pub fn enqueue(
        &self,
        frame: Ax25Frame,
        class: Class,
        account: &str,
        expects_reply: bool,
    ) -> bool {
        let cost = self.airtime_for(frame.encode().len());
        let item = Queued {
            payload: frame,
            class,
            account: account.to_string(),
            cost,
            queued_at: Instant::now(),
            expects_reply,
        };
        let mut sched = self.scheduler.lock().unwrap_or_else(|e| e.into_inner());
        match sched.push(item) {
            Ok(()) => {
                publish_queue(&self.airtime, &sched);
                self.airtime.wake.notify_waiters();
                true
            }
            Err(_) => {
                warn!("TX queue full ({})", class.as_str());
                false
            }
        }
    }
}

fn publish_queue(shared: &AirtimeShared, sched: &Scheduler<Ax25Frame>) {
    shared
        .queued_ms
        .store(sched.queued_airtime().as_millis() as u64, Ordering::Relaxed);
    shared
        .queued_frames
        .store(sched.len() as u64, Ordering::Relaxed);
}

/// Sequence number of an AIRC payload, if the information field is one.
/// Kept here so the TNC layer does not have to depend on `airc`.
fn airc_seq(info: &[u8]) -> Option<u16> {
    if info.len() >= 8 && info[0] == b'A' && info[1] == b'1' {
        Some(u16::from_be_bytes([info[4], info[5]]))
    } else {
        None
    }
}

/// AIRC kind name for the audit line. Names match [`crate::airc::Kind`]'s
/// Debug form so a log that used to be written at enqueue still greps the
/// same way. Unknown or non-AIRC payloads are `-`.
fn airc_kind(info: &[u8]) -> &'static str {
    if info.len() < 3 || info[0] != b'A' || info[1] != b'1' {
        return "-";
    }
    match info[2] {
        0x01 => "Hello",
        0x02 => "Welcome",
        0x03 => "Join",
        0x04 => "Part",
        0x05 => "Msg",
        0x06 => "Notice",
        0x07 => "Names",
        0x08 => "NamesReply",
        0x09 => "Ping",
        0x0A => "Pong",
        0x0B => "Ack",
        0x0C => "Error",
        0x0D => "Id",
        0x0E => "Quit",
        0x0F => "Presence",
        0x10 => "Stored",
        _ => "-",
    }
}

/// A frame has just been written to the TNC. `keyed` is the modeled key-down
/// time (TXDELAY + on-wire bits + TXTAIL), which is what the PA sees; KISS
/// does not report actual PTT. `duty` is the sliding-window duty cycle after
/// this transmission was counted.
fn audit_keyed(audit: &Audit, item: &Queued<Ax25Frame>, bytes: usize, keyed: Duration, duty: f64) {
    let dest = item.payload.destination.call.to_string();
    let n = bytes.to_string();
    let keyed_s = format!("{:.1}s", keyed.as_secs_f64());
    let duty_s = format!("{:.1}%", duty);
    let kind = airc_kind(&item.payload.info);
    let event = if item.class == Class::Id {
        "rf_id"
    } else {
        "rf_tx"
    };
    audit.event(
        event,
        &[
            ("dest", &dest),
            ("kind", kind),
            ("bytes", &n),
            ("class", item.class.as_str()),
            ("keyed", &keyed_s),
            ("duty", &duty_s),
        ],
    );
}

/// Start the TNC task. Received frames are delivered on the returned channel.
///
/// Keyed frames still reach `tracing` (`rfircd::audit`); pass
/// [`spawn_with_audit`] when they should also hit the on-disk trail.
pub fn spawn(config: TncConfig) -> (TncHandle, mpsc::Receiver<Ax25Frame>) {
    spawn_with_audit(config, Audit::open(None))
}

/// Like [`spawn`], writing each keyed frame to `audit` with modeled key-down
/// time and the sliding-window duty cycle.
pub fn spawn_with_audit(config: TncConfig, audit: Audit) -> (TncHandle, mpsc::Receiver<Ax25Frame>) {
    let (tx_in, rx_in) = mpsc::channel::<Ax25Frame>(256);
    let (keyed_tx, keyed_rx) = mpsc::channel::<Keyed>(64);
    let airtime = Arc::new(AirtimeShared::default());
    let scheduler = Arc::new(StdMutex::new(Scheduler::new(scheduler_from_tnc(&config))));
    tokio::spawn(run(
        config.clone(),
        tx_in,
        keyed_tx,
        scheduler.clone(),
        airtime.clone(),
        audit,
    ));
    let cost = config.airtime.clone();
    (
        TncHandle {
            scheduler,
            keyed_rx: Arc::new(StdMutex::new(Some(keyed_rx))),
            airtime,
            cost,
        },
        rx_in,
    )
}

/// Scheduler timings derived from the same numbers the governor prices with.
/// A 3.5 s reply window is right at 300 baud and a test failure at 9600.
fn scheduler_from_tnc(config: &TncConfig) -> SchedulerConfig {
    let mut cfg = SchedulerConfig::default();
    cfg.max_hold = config.airtime.max_hold;
    cfg.min_gap = config.tx_pacing;
    let gov = Governor::new(config.airtime.clone());
    // Tests set tx_pacing to zero so a frame that was transmitted is not
    // mistaken for one that was not. Shrink the half-duplex guards to match;
    // production pacing (seconds) keeps the 300-baud-sized windows.
    if config.tx_pacing.is_zero() {
        cfg.reply_window = Duration::from_millis(20);
        cfg.rx_guard = Duration::from_millis(20);
    } else {
        cfg.reply_window = gov.airtime_for(40) + Duration::from_millis(200);
        cfg.rx_guard = config.airtime.txdelay + Duration::from_millis(80);
    }
    cfg.depth[Class::Chat as usize] = config.tx_queue_depth.max(1);
    cfg.depth[Class::Id as usize] = 4;
    cfg
}

async fn connect(link: &TncLink) -> io::Result<Box<dyn ReadWrite>> {
    match link {
        TncLink::Tcp { host, port } => {
            let stream = TcpStream::connect((host.as_str(), *port)).await?;
            stream.set_nodelay(true).ok();
            Ok(Box::new(stream))
        }
        #[cfg(feature = "serial")]
        TncLink::Serial { path, baud } => {
            use tokio_serial::SerialPortBuilderExt;
            let port = tokio_serial::new(path, *baud).open_native_async()?;
            Ok(Box::new(port))
        }
        TncLink::Loopback(slot) => slot
            .lock()
            .await
            .take()
            .map(|s| Box::new(s) as Box<dyn ReadWrite>)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "loopback already taken")),
    }
}

/// Where received frames go. The transmit scheduler outlives reconnects the
/// same way: a dropped TNC must not forget a sign-off ID or a held message.
struct Link {
    rx_sink: mpsc::Sender<Ax25Frame>,
}

async fn run(
    config: TncConfig,
    rx_sink: mpsc::Sender<Ax25Frame>,
    keyed_tx: mpsc::Sender<Keyed>,
    scheduler: Arc<StdMutex<Scheduler<Ax25Frame>>>,
    shared: Arc<AirtimeShared>,
    audit: Audit,
) {
    let mut state = Link { rx_sink };
    // The governor outlives individual TNC connections on purpose: airtime
    // already radiated does not stop counting because Direwolf restarted.
    let mut governor = Governor::new(config.airtime.clone());
    let mut backoff = Duration::from_secs(1);
    loop {
        match connect(&config.link).await {
            Ok(link) => {
                info!(?config.link, "TNC connected");
                backoff = Duration::from_secs(1);
                if let Err(e) = pump(
                    &config,
                    link,
                    &mut state,
                    &mut governor,
                    &shared,
                    &scheduler,
                    &keyed_tx,
                    &audit,
                )
                .await
                {
                    warn!("TNC link closed: {e}");
                }
            }
            Err(e) => warn!("TNC connect failed: {e}"),
        }
        if matches!(config.link, TncLink::Loopback(_)) {
            // Nothing to reconnect to; the harness is gone. Release anything
            // still queued so admission control cannot see a phantom backlog.
            let mut sched = scheduler.lock().unwrap_or_else(|e| e.into_inner());
            let leftover = sched.drain();
            publish_queue(&shared, &sched);
            drop(sched);
            let _ = leftover;
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn pump(
    config: &TncConfig,
    mut link: Box<dyn ReadWrite>,
    state: &mut Link,
    governor: &mut Governor,
    shared: &AirtimeShared,
    scheduler: &StdMutex<Scheduler<Ax25Frame>>,
    keyed_tx: &mpsc::Sender<Keyed>,
    audit: &Audit,
) -> io::Result<()> {
    let rx_sink = &state.rx_sink;
    // Push KISS parameters at connect.
    //
    // TXDELAY and TXTAIL come from the airtime config rather than a separate
    // TNC setting: the governor prices every frame with those numbers, so the
    // TNC had better be using the same ones. In 10 ms units, hence the /10.
    let ms_to_kiss = |ms: u128| -> u8 { (ms / 10).min(255) as u8 };
    let mut params: Vec<(u8, u8)> = vec![
        (
            kiss::CMD_TXDELAY,
            ms_to_kiss(config.airtime.txdelay.as_millis()),
        ),
        (
            kiss::CMD_TXTAIL,
            ms_to_kiss(config.airtime.txtail.as_millis()),
        ),
        // Half duplex, always. A TNC in full-duplex mode transmits without
        // listening first, which on a shared channel is precisely the
        // behaviour this whole module exists to prevent.
        (kiss::CMD_FULLDUPLEX, 0),
    ];
    for (cmd, value) in [
        (kiss::CMD_PERSISTENCE, config.persistence),
        (kiss::CMD_SLOTTIME, config.slottime),
    ] {
        if let Some(v) = value {
            params.push((cmd, v));
        }
    }
    for (cmd, v) in params {
        link.write_all(&kiss::encode(config.kiss_port, cmd, &[v]))
            .await?;
    }

    let mut decoder = KissDecoder::new(config.max_frame);
    let mut buf = vec![0u8; 4096];

    loop {
        if shared.inhibit.load(Ordering::Acquire) {
            let dropped = scheduler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain_except(Class::Id);
            if !dropped.is_empty() {
                shared
                    .dropped_inhibited
                    .fetch_add(dropped.len() as u64, Ordering::Relaxed);
                let sched = scheduler.lock().unwrap_or_else(|e| e.into_inner());
                publish_queue(shared, &sched);
            }
        }

        governor.set_duty(shared.duty_limit(config.airtime.max_duty));
        let pacing = shared.pacing(config.tx_pacing);
        scheduler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_min_gap(pacing);

        let now = Instant::now();
        let expired = scheduler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expire(now);
        if !expired.is_empty() {
            shared
                .dropped_stale
                .fetch_add(expired.len() as u64, Ordering::Relaxed);
            for item in &expired {
                warn!(
                    "dropping a frame held {:?} — stale traffic is worse than no traffic",
                    now.saturating_duration_since(item.queued_at)
                );
            }
            let sched = scheduler.lock().unwrap_or_else(|e| e.into_inner());
            publish_queue(shared, &sched);
        }

        let poll = scheduler
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .poll(now);
        let interlock = shared.interlock_failed();
        let now_tx = tokio::time::Instant::now();
        let wake = match poll {
            Poll::Idle => now_tx + Duration::from_secs(3600),
            Poll::Wait(d) => now_tx + d,
            // A held frame and a failing interlock re-polls while the
            // interlock stays down. `interlock::spawn` notifies `wake` on
            // recovery so this is normally not what notices, but it must
            // stay short enough to be a real backstop: anything else that
            // clears `interlock_ok` without notifying would otherwise leave
            // the transmitter held for the length of this sleep. Half a
            // second halves the idle wakeups the 200 ms poll cost without
            // making recovery depend on the notify arriving.
            Poll::Ready if interlock => now_tx + Duration::from_millis(500),
            Poll::Ready => now_tx,
        };

        tokio::select! {
            read = link.read(&mut buf) => {
                let n = read?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "TNC closed"));
                }
                for kf in decoder.push(&buf[..n]) {
                    if kf.command != kiss::CMD_DATA {
                        continue;
                    }
                    if kf.port != config.kiss_port {
                        continue;
                    }
                    match Ax25Frame::decode(&kf.payload) {
                        Ok(frame) => {
                            debug!(target: "rf::rx", "{}", frame.to_monitor_line());
                            scheduler
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .on_heard(Instant::now());
                            if rx_sink.send(frame).await.is_err() {
                                return Ok(());
                            }
                        }
                        Err(e) => debug!("undecodable AX.25 frame ({} bytes): {e}", kf.payload.len()),
                    }
                }
            }
            _ = shared.wake.notified() => {}
            _ = tokio::time::sleep_until(wake) => {
                if shared.interlock_failed() {
                    continue;
                }
                let now = Instant::now();
                let item = {
                    let mut sched = scheduler.lock().unwrap_or_else(|e| e.into_inner());
                    match sched.poll(now) {
                        Poll::Ready => sched.pop(now),
                        _ => None,
                    }
                };
                let Some(item) = item else {
                    continue;
                };
                if shared.inhibit.load(Ordering::Acquire) && item.class != Class::Id {
                    shared.dropped_inhibited.fetch_add(1, Ordering::Relaxed);
                    let sched = scheduler.lock().unwrap_or_else(|e| e.into_inner());
                    publish_queue(shared, &sched);
                    continue;
                }
                let bytes = item.payload.encode();
                if bytes.len() > config.max_frame {
                    warn!("refusing to transmit oversized frame ({} bytes)", bytes.len());
                    let sched = scheduler.lock().unwrap_or_else(|e| e.into_inner());
                    publish_queue(shared, &sched);
                    continue;
                }
                match governor.check(bytes.len(), now) {
                    TxDecision::Send => {
                        // Re-read the interlock immediately before keying.
                        // It was checked once before the item was popped,
                        // which leaves the governor decision and the encode
                        // between the check and the write; `inhibit` is
                        // already re-read above, and the interlock is the one
                        // that says the antenna may be disconnected or
                        // somebody may be up the tower. Requeued, not
                        // dropped: the frame is still wanted when it clears.
                        if shared.interlock_failed() {
                            scheduler
                                .lock()
                                .unwrap_or_else(|g| g.into_inner())
                                .requeue(item, now, Duration::from_millis(200));
                            continue;
                        }
                        if let Err(e) = write_kiss_bytes(&mut link, config, &item.payload, &bytes).await {
                            scheduler
                                .lock()
                                .unwrap_or_else(|g| g.into_inner())
                                .requeue(item, Instant::now(), Duration::ZERO);
                            return Err(e);
                        }
                        let recorded = governor.record(bytes.len(), now);
                        {
                            let mut sched = scheduler.lock().unwrap_or_else(|g| g.into_inner());
                            sched.on_keyed(now, &item.account, recorded, item.expects_reply);
                            publish_queue(shared, &sched);
                        }
                        governor.publish_with(shared, now, config.max_frame);
                        // `record` returns zero when the governor is off; the
                        // cost model still knows how long the PA is keyed.
                        let keyed = if recorded.is_zero() {
                            item.cost
                        } else {
                            recorded
                        };
                        audit_keyed(audit, &item, bytes.len(), keyed, shared.duty_percent());
                        if let Some(seq) = airc_seq(&item.payload.info) {
                            let dest = item.payload.destination.call.clone();
                            let _ = keyed_tx.try_send(Keyed { dest, seq });
                        }
                    }
                    TxDecision::Defer(delay, reason) => {
                        governor.publish_with(shared, now, config.max_frame);
                        let waited = now.saturating_duration_since(item.queued_at);
                        if item.class != Class::Id
                            && item.class != Class::Ack
                            && waited + delay > config.airtime.max_hold
                        {
                            shared.dropped_stale.fetch_add(1, Ordering::Relaxed);
                            warn!(
                                "dropping a frame held {:?} by {} — stale traffic is worse than no traffic",
                                waited,
                                reason.as_str()
                            );
                            let sched = scheduler.lock().unwrap_or_else(|g| g.into_inner());
                            publish_queue(shared, &sched);
                            continue;
                        }
                        shared.deferred.fetch_add(1, Ordering::Relaxed);
                        debug!("holding a frame for {:?} ({})", delay, reason.as_str());
                        scheduler
                            .lock()
                            .unwrap_or_else(|g| g.into_inner())
                            .requeue(item, now, delay);
                    }
                }
            }
        }
    }
}

async fn write_kiss_bytes(
    link: &mut Box<dyn ReadWrite>,
    config: &TncConfig,
    frame: &Ax25Frame,
    bytes: &[u8],
) -> io::Result<()> {
    debug!(target: "rf::tx", "{}", frame.to_monitor_line());
    link.write_all(&kiss::encode(config.kiss_port, kiss::CMD_DATA, bytes))
        .await?;
    link.flush().await
}
