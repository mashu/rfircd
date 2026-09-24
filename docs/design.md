# rfircd design

## 1. What this is

An IRC server that is simultaneously an RF packet gateway over KISS. Two
populations share the same channels, and a third can write into them:

* **IP users** with an ordinary IRC client (irssi) over TCP or TLS.
* **RF stations** with a radio and a TNC, speaking a compact protocol (AIRC/1)
  carried in AX.25 UI frames.
* **APRS radios** that message the gateway callsign. No AIRC client; a line
  of the form `#rf hello` is injected into that channel, and channel/DM
  replies can be sent back as addressed APRS when needed.

A message sent in a bridged channel reaches both. Nothing else does: the design
is mostly about deciding what is *not* worth putting on the air.

## 2. The two constraints that shape everything

### 2.1 Physics

A typical VHF packet channel is 1200 baud AFSK, half duplex, shared, with no
collision detection. That is about 120 bytes per second of theoretical
throughput, and perhaps half of that once TXDELAY, preamble, collisions and
retries are accounted for. A single 256 byte frame occupies the channel for
roughly two seconds.

IRC's wire format is hostile to this. A single channel message costs:

```
:SM0ABC|7!rf@sk0mt.ax25 PRIVMSG #rf :hello                    58 bytes
```

of which 40 bytes are prefix and syntax. The same message in AIRC/1:

```
A1 05 00 00 07 00 01 | #rf US hello                           8 + 10 bytes
```

At 1200 baud that difference is roughly 0.3 seconds of channel time per
message, every message. Multiply by a channel with ten users and it is the
difference between a usable QSO and an unusable one.

Consequences that appear throughout the code:

* The RF side is an allowlist: PRIVMSG chat (`/me` included), TOPIC, and
  optionally join/part presence. Numerics, MODE, NOTICE, other CTCP, nick
  changes and (by default) JOIN/PART are not on that list, so they stay on IRC.
* Channel traffic is transmitted **once**, as a broadcast, not once per
  listening station. RF stations send channel chat to `AIRC` as well, so they
  hear each other without the gateway repeating.
* Messages that arrive from the air are **not** re-transmitted: every station
  in range already heard them. Repeating is opt-in
  (`radio.repeat_rf_traffic`), for hidden-terminal situations only.
* Rate limits and length caps are on by default and are not configurable to
  "unlimited".
* Transmissions are paced (`tx_pacing_ms`) so the gateway cannot monopolise a
  channel it shares with APRS, NET/ROM and other people's QSOs.

### 2.2 Law

Amateur licences almost everywhere impose three rules that a gateway must
respect. See [regulatory.md](regulatory.md) for the detail; the design consequences are:

* **No obscured meaning.** No encryption, no compression that is not publicly
  documented, no private codes. This kills the obvious "just run IRC over
  TLS over the radio" approach, and it means the on-air protocol has to be
  documented and readable. AIRC/1 is plain UTF-8 behind an 8 byte header.
* **Identification.** An automatically transmitting station must identify at
  intervals (10 minutes in most jurisdictions). The server refuses to start
  with `id_interval_secs > 600`, and transmits an ID frame whenever it has
  transmitted since the last one — and again at shutdown.
* **Control operator responsibility.** Everything the station radiates is the
  licensee's responsibility, including traffic that originated with an
  anonymous stranger on the Internet. Hence RF-TX grants, the CALLSIGN
  requirement, the ciphertext screen, the deny/allow lists, the `RADIO OFF`
  kill switch, and the monitor log that records every frame in the same
  format as `axlisten`.

## 3. Architecture

```
                    ┌──────────────────────────────────────────┐
   radio ── TNC ───►│ ax25::tnc      KISS framing, reconnect,   │
        (KISS/TCP   │                TX pacing, TX queue        │
         or serial) └───────────────┬──────────────────────────┘
                                    │ Ax25Frame
                    ┌───────────────▼──────────────────────────┐
                    │ ax25::frame    addresses, UI frames       │
                    │ airc::frame    AIRC/1 codec               │
                    │ airc::session  seq, ACK, fragments, dedup │
                    │ aprs           stock APRS messages to us  │
                    └───────────────┬──────────────────────────┘
                                    │ AircFrame
                    ┌───────────────▼──────────────────────────┐
                    │ bridge         RF ⇄ IRC translation       │
                    │ policy         airtime + legality gates   │
                    └───────────────┬──────────────────────────┘
                                    │ Delivery
                    ┌───────────────▼──────────────────────────┐
   IRC client ─TCP─►│ server         users, channels, event loop│
                    │ irc::client    RFC 1459 line protocol     │
                    └──────────────────────────────────────────┘
```

### 3.1 Concurrency

One task owns all mutable state. Everything else — each client connection, the
TNC link, the timer — sends `Event` values into a single `mpsc` channel and the
server task processes them in order. There is no lock anywhere in the message
path and no `Arc<Mutex<State>>`.

This matters more than usual here, because the two sides have wildly different
latencies (microseconds on TCP, seconds on RF) and a lock-based design would
let a slow radio write block an IRC client. Instead the radio has a bounded
transmit queue: when it fills, frames are refused at admission and the sender
is told, and the IRC side never waits.

**"One task owns the state" is not "the server is single threaded."** The
runtime is `rt-multi-thread`, and the work that scales with the number of
clients is per-client:

| Task | How many | What it does |
|---|---|---|
| Listener | one per `bind` address | accept, spawn |
| Connection reader | one per client | socket → `Event` |
| Connection writer | one per client | bounded queue → socket |
| Server actor | exactly one | all state mutation |
| TNC link | one | KISS framing, pacing, the airtime governor |
| Argon2 | one per `REGISTER`/`IDENTIFY` | `spawn_blocking` |
| Audit writer | one | `spawn_blocking`, batched appends |
| Interlock | one, if configured | polls the safety command |

So parsing, socket I/O (including TLS), line framing, password hashing and log writing
all run on the thread pool in parallel; only the state transitions are
serialised, and those are microseconds of `HashMap` work. The actor is a
correctness decision — one ordering of events, no interleaving between the
radio and the wire — not a throughput compromise.

The rule that keeps it true: **nothing that can block goes in the actor.**
Password hashing was moved to `spawn_blocking` and the audit log to its own
writer task for exactly this reason. The one blocking call left is the nick
database write on `REGISTER`, `RADIO GRANT` and `CALLSIGN` — a small file,
written only on account changes, and rate-limited to six attempts a minute per
host.

`tests/concurrency.rs` checks the claim against real sockets rather than
asserting it: a hundred clients registering and talking simultaneously, a
client that stops reading, silent sockets that never register, and a
connection flood from one address.

### 3.2 The `Delivery` type

The bridge does not translate IRC lines into AX.25 frames. Both are rendered
from a neutral `Delivery` value:

```rust
Delivery::Privmsg { from_nick, from_prefix, target, text, notice }
```

An IP user gets `:nick!user@host PRIVMSG #rf :text`. An RF station gets
`MSG ["#rf", "nick", "text"]` in 8 + n bytes. A `Delivery::Quit` renders for IP
and renders to *nothing* for RF, because a quit notice is not worth two seconds
of airtime. Adding a new event type means answering "what does this cost on the
air?" once, in one place.

## 4. Identity and trust

On the air, identity is a claim. Anyone can transmit any callsign; AX.25 has no
authentication, and adding cryptographic authentication is legally fraught in
several jurisdictions (and useless against a replay attack over a broadcast
medium unless you also add a nonce exchange, which costs airtime).

The RF side therefore does not pretend: an AX.25 source address is logged, a
station's IRC nick is derived from it (`SM0ABC-7` → `SM0ABC|7`), and
callsign-shaped nicknames are reserved so an IP user cannot sit on
`SM0ABC|7`, `SM0ABC\7` (RFC 1459 casemapping) or `SM0ABC-7`. Allow/deny lists
decide which callsigns the gateway will talk to. Treat the radio as a public
party line.

The IP side is where real authentication belongs, and it is also where the
licensee's risk is: an IRC `PRIVMSG` that the gateway radiates is third-party
traffic under the gateway callsign. Speaking on IRC and radiating are
separate:

* **Listen and local chat are open.** Anyone who can connect may join a `+r`
  channel, hear RF traffic, and talk to other IRC users. `CALLSIGN` grants
  `+v` (permission to speak on the moderated channel). Those messages stay
  on the internet until the next point.
* **RF-TX is a persisted grant on a registered nick.** The user `REGISTER`s,
  a control operator `RADIO GRANT`s that nick (stored in `nicks.json`), and
the user `IDENTIFY`s on later connects. `CALLSIGN` is still required. IDENTIFY
binds a callsign claimed this session, or restores the stored one. `OPER` has
RF-TX for that session without a grant. Internet clients that will do any of
this must connect with TLS (`[listen.tls]`). A plaintext connection from off
the machine is listen-only.
* RF stations are not IRC accounts. Their identity is the AX.25 source
  address; `allow_callsigns` / `deny_callsigns` are the gate.

### 4.1 What an unauthenticated callsign costs

"Identity is a claim" has a consequence worth stating outright, because it is
easy to build a limit that does not limit anything: **every per-station rate
limit on the RF side is keyed on a name the sender chooses.** A flood that
invents a new plausible callsign for each frame gets a brand-new token bucket
every time, so `rf_msgs_per_min` bounds one polite station and nothing else.

Three things used to follow from that, and each is now bounded:

* Each new callsign took a slot in the session table, which is capped at
  `radio.max_peers`. A few hundred forged names filled it, and because a full
  table refuses *new* peers — deliberately, so a flood cannot evict a station
  in the middle of a QSO — every real station was locked out until the entries
  aged out at `peer_idle_timeout_secs`.
* On the APRS path an unknown station that sends a message with a `{msgid}` is
  *answered*, because a stock radio retries until it hears an ack. A received
  frame therefore bought a transmission, made under the gateway licensee's
  callsign, addressed to a station that does not exist.
* `QUIT` and `PART` from the air went through no rate limit at all, and a
  forged `QUIT` removes a station's IRC presence for the cost of one
  unacknowledged broadcast.

The fix is a single unkeyed budget: the *first* frame from a callsign the
gateway has not heard before is rationed against one bucket shared by every
unknown station, so an unbounded supply of names no longer buys an unbounded
supply of resources. An established contact never touches it.

It authenticates nothing — nothing on this side can. A station that transmits
continuously can still crowd out new arrivals for as long as it transmits;
that is jamming, and no software limit answers it. What the budget changes is
that the damage is proportional to the flood and ends when the flood does,
instead of leaving a full table behind for half an hour.

The airtime governor is the backstop under all of this: whatever provokes a
transmission, the duty cycle, the continuous-run limit and the hourly budget
still decide whether it is keyed. See [airtime.md](airtime.md).

### 4.2 Text from the air is not text you can print

An AIRC field is `String::from_utf8_lossy` of whatever arrived. The encoder
strips the field separator and CR/LF/NUL, but that is the *sender's*
courtesy — a hostile sender writes the payload itself. Anything that renders
RF-sourced text on a terminal must filter it first
(`policy::strip_terminal_controls`), or a station within earshot can set the
window title, clear the screen, or on a terminal that answers back, put its
own text on the operator's shell prompt. The gateway sanitises before text
reaches IRC; `rfirc-station` filters everything it prints, including the
lines it composed itself, because filtering at the one exit is what keeps it
true of paths added later.

Commands, flags, and what survives a restart: [usage.md](usage.md).

A typical club setup: internet users join `#rf` to follow the QSO; only the
control operator and nicks they have granted can key the transmitter.
`rfirc-station` is not an IRC client — it speaks AIRC over KISS.

If you need people on the internet to speak or control the transmitter,
configure `[listen.tls]` (implicit TLS, typically port 6697). A plaintext
socket from off this machine is listen-only: they can watch `#rf`, they
cannot IDENTIFY, REGISTER, OPER, or transmit. A connection `PASS` is still
accepted so a passworded server remains watchable. TLS protects the hop to the
gateway and stops there. Everything that reaches the antenna is in the
clear, by law and by design.

## 5. Channel model

Channels carry one extra mode: `+r`, "bridged to RF".

* Only a control operator (`OPER`) can set or clear `+r`. Deciding what your
  station radiates is not a user-level decision.
* Channels created on the fly by `JOIN` are never `+r`.
* An RF station that joins a non-`+r` channel is told `404`.
* A message to a `+r` channel is radiated if the sender holds RF-TX and a
  CALLSIGN, and either an RF station is already in the channel or they are
  calling CQ into an empty one. The empty-channel lock remains for everyone
  else: ordinary IRC chat is not a reason to key the transmitter. TOPIC
  still requires an RF member.

Everything else (`+m`, `+t`, `+k`, `+l`, `+o`, `+v`) behaves as usual and
applies to both populations.

## 6. Reliability

UI frames are unacknowledged datagrams. AX.25 connected mode exists, but it
would give us one virtual circuit per station, no broadcast, head-of-line
blocking across independent conversations, and an implementation dependency on
whatever the far end runs. So AIRC/1 does its own:

| Traffic | Mode | Why |
|---|---|---|
| Channel messages, presence, ID | broadcast, unreliable, deduplicated by sequence number | one transmission serves every station in range |
| Private messages, welcome, error, NAMES replies | unicast, ACKed, retried | it matters that this one station got it |

Retransmission is stop-and-wait *per message* with **linear** backoff, one
message in flight per station and a bounded queue behind it. Fragments of that
message use selective repeat: a bitmap ACK names the holes, and only those
are resent. Exponential backoff is wrong here: the usual cause of loss is a
collision, not congestion at a router, and backing off to minutes turns a QSO
into a mailbox.

An ACK that can ride on the next unicast to that station does so (`PIGGYACK`):
one key-up instead of two. That is the cheaper path for QRP finals.

Fragmentation is at the AIRC layer, not AX.25's: all fragments share a sequence
number and carry `index`/`total`, reassembly is bounded by a timer, and a
sequence number reused with a different fragment count resets the buffer.

Duplicates are suppressed per station with a 64-entry window — but a duplicate
is still ACKed, because a repeat almost always means our previous ACK was the
frame that got lost.

## 6.1 Store and forward

A station is on a hilltop for twenty minutes and in a valley for two hours.
A private message to a station that is not currently in range is therefore
held rather than refused, and delivered as a `STORED` frame (carrying its age)
the moment the station is next heard.

The limits are the design. The mailbox is bounded per station
(`mailbox_per_station`), bounded across the gateway (`mailbox_total`) and
expires (`mailbox_ttl_secs`), because an unbounded queue on a shared amateur
channel eventually becomes somebody's free mail server. Held messages pass the
same policy screen as live traffic *at the time they are accepted*, so nothing
can be smuggled onto the air by waiting.

`RADIO MAIL` shows the control operator what is waiting for whom.

## 6.2 APRS interop

A stock APRS radio (Kenwood, Yaesu, APRSdroid) does not speak AIRC. It can
still join the public RF QSO by sending an APRS **message** whose addressee
is the gateway callsign:

```
:SK0MT-1  :#rf hello from the trail{01
```

The AX.25 destination is a TOCALL (`APRS`, `APK004`, …); the addressee lives
in the information field. The gateway ACKs (`ack01`) so the radio stops
retrying — silence would cost more airtime than the ACK — and injects the
text into `#rf`. AIRC stations already in that channel get a translated
broadcast; they did not decode the APRS frame. A line `nick text` when that
nick is online is delivered as an IRC query instead.

Outbound: IRC channel chat and private messages reach APRS peers as addressed
APRS (`nick: text{id}`), with msgid + ACK/retry. Under the default
`radio.rf_mode = "airc"`, AIRC broadcast stays one frame per line and only
APRS-dialect peers are fanned out. With `rf_mode = "aprs"`, the gateway
encodes outbound chat as APRS only (one frame per RF peer in the channel; no
AIRC CQ). JOIN/PART/numerics/MODE never go out as APRS.

Position reports (`! = / @`, compressed, Mic-E) and status beacons (`>`)
heard on frequency are shown on IRC as a channel NOTICE. That costs no
airtime and is never retransmitted: every station already heard the beacon.
They do not join the channel and they do not get an ACK. The listen
channel is `radio.aprs_channel` if set, otherwise the first `+r` channel.

`?` or `HELP` is answered with a one-line hint, not injected. A configured
`radio.aprs_channel` accepts a bare line with no `#channel` prefix. Off
with `radio.aprs = false`.

## 7. Failure modes

| Failure | Behaviour |
|---|---|
| TNC socket dies | reconnect with capped exponential backoff; IRC side unaffected |
| Transmit queue full | new traffic refused at admission and the sender told why; visible in `RADIO QUEUE` |
| Duty cycle or PA cooldown reached | frames deferred until airtime frees up, then dropped after `max_hold_secs` rather than transmitted stale |
| Safety interlock fails or cannot be run | everything inhibited, station identification included; fails closed |
| Audit writer falls behind | lines dropped and counted, never buffered without limit and never blocking the server |
| Station stops answering | 3 retries, then the station is declared lost and its IRC presence quits with "Signal lost" |
| Station goes quiet | removed after `peer_idle_timeout_secs` |
| Corrupt frame from the air | logged in monitor format, ignored; never fatal |
| Non-AIRC traffic on frequency (NET/ROM) | logged at debug, ignored |
| APRS position or status beacon | shown on IRC as NOTICE; never retransmitted |
| APRS message addressed to the gateway | ACKed so the radio stops retrying; `#chan text` is injected into that channel |
| Frame from an implausible callsign | ignored |
| Someone floods from RF | token bucket drops the traffic; no reply is transmitted, because answering a flood with transmissions is how you jam your own channel |
| Client never registers | dropped after `registration_timeout_secs` |
| Client stops reading its socket | bounded output queue; the connection is dropped rather than buffered without limit |
| Connection flood | capped per host and in total (`max_conns_per_host`, `max_clients`) |
| Channels created and abandoned | user-created channels are reaped when the last member leaves; configured ones persist |
| Message for a station that is out of range | held, bounded and expiring; delivered as `STORED` on next contact |
| Control operator needs the transmitter off *now* | `RADIO OFF` — signs off with an ID, purges the queue, IRC keeps running |
| Control operator needs it slower, not off | `RADIO LIMIT DUTY` / `RADIO LIMIT PACING`, effective on the next frame |

## 8. Operating it

Typical deployment with Direwolf (step-by-step, including QMX:
[quickstart.md](quickstart.md), [qmx.md](qmx.md)):

```
[direwolf]  ADEVICE plughw:1,0 / MODEM 1200 / KISSPORT 8001
     │  KISS over TCP :8001
[rfircd]  radio.tnc.kind = "tcp", port 8001
     │  TCP :6667 on localhost (operator console)
     │  TLS :6697 for internet clients
```

Internet clients that speak or OPER use implicit TLS (`[listen.tls]`). A
plaintext connection from off the machine is listen-only. TLS protects the
hop between the user and the gateway, and stops there. Everything that
reaches the antenna is in the clear, by law and by design. The server says this
to every user at registration.

Control operator console (requires `OPER`):

```
RADIO STATUS            transmitter state, frames, bytes, stations heard
RADIO OFF | ON          kill switch
RADIO ID                identify now
RADIO HEARD             stations, last heard, queue depth, drops
RADIO MAIL              held private messages
RADIO KICK <callsign>   remove a station's presence
RADIO GRANT <nick>      persist RF-TX on a registered nick
RADIO REVOKE <nick>     take it away
```

## 8.1 Development without a radio

`rfirc-kisshub` is a virtual channel: every TCP client that connects is a
station on the same frequency, and a KISS frame from one is delivered to all
the others. With it, the whole system runs on a laptop:

```sh
rfirc-kisshub --bind 127.0.0.1:8001 &
rfircd -c rfircd.toml &
rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --channel '#rf'
```

The hub prints every frame in `axlisten` monitor format, which is how the
sequence-space bug described in [protocol.md](protocol.md) §3.1 was found: the gateway
had separate counters for unicast and broadcast, so a station discarded its
first broadcast as a duplicate of the welcome it had already received.

## 9. Testing

The awkward parts of this system are timeouts, retries, reassembly and
fragmentation, so `airc::session` is a pure state machine: it takes an explicit
`now` and returns the frames to transmit. Its tests run in microseconds and
cover retry-then-give-up, ACK releasing a queued message, duplicate
suppression, bounded queues and fragment reassembly.

Above that, `TncConfig::loopback()` provides an in-process fake TNC. The
integration tests in `tests/gateway.rs` drive a real `Server` with a real KISS
codec on one side and a fake IRC client on the other, and assert on actual
transmitted bytes: that a station's JOIN appears on IRC, that a message heard
on the air is *not* re-transmitted, that an unidentified IP user's message
never reaches the antenna, that ciphertext is refused, that an ACKed
private message is not retried, and that an APRS message to the gateway
callsign is ACKed and injected while a beacon on the same frequency is not.

## 10. Deliberate non-goals

* **Server linking.** IRC's server-to-server protocol assumes cheap, reliable
  links. Two gateways on the same frequency should share a channel over the
  air, not netsplit at 1200 baud.
* **DCC, CTCP, file transfer.** Not on this medium.
* **Encryption of any kind on the RF path.**
* **Pretending RF identity is authenticated.**

## 11. Possible future work

* **Multiple RF ports** (2 m + 70 cm, or 1200 + 9600 baud) with per-channel
  port mapping. The TNC layer already carries a KISS port number.
* **FX.25 / IL2P** in the TNC (Direwolf `FX25TX 1`). The gateway still sends
  AX.25 UI frames over KISS; FEC is the modem's job.
* **Digest mode**: a station on a handheld subscribes to a channel and receives
  a periodic summary instead of every message.
* **IRCv3 `server-time`, `echo-message`, `chghost`** on the IP side, where they
  cost nothing.
