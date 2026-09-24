# Airtime, duty cycle and the transmit queue

A packet gateway is an automatically controlled station. Nobody is watching it
when the IRC side gets busy at two in the morning, and the two things it can
damage — the transmitter's finals and the shared channel — are both slow
enough that you find out afterwards.

rfircd has two separate mechanisms for this, and they are easy to confuse.

| | `[policy]` | `[radio.duty]` |
|---|---|---|
| Counts | messages | seconds of key-down time |
| Scope | per user, per station, per channel | the whole transmitter |
| Protects | fairness between users | the finals and the band |
| Bypassed by | nothing | nothing |

`[policy]` stops one loud user drowning out the others. It does not stop
*fifty* polite users between them keying your radio continuously, and it has
no idea how long a message actually takes to transmit.

`[radio.duty]` is the one that keeps the smoke inside the transistors.

## Why this matters more than it sounds

At 300 baud — HF packet, which is what a QMX runs — a 128-octet AX.25 frame is
about **four seconds of unbroken carrier**, plus TXDELAY and TXTAIL. So:

* A single long IRC line, fragmented into three frames, is ~15 s of transmit.
* Ten messages held in a station's mailbox, flushed the moment that station is
  heard, is over a minute of near-continuous key-down.
* A retry cycle triples it.

A QMX has no heatsink worth the name and no thermal fold-back. Sustained
transmit at that duty is the documented way to kill its finals. Almost any HF
transceiver derates hard above 30-50 % duty; a QRP rig has less margin, not
more.

None of this is malicious traffic. It is one busy evening on `#rf`.

## The asymmetry that makes this work

Receiving is free. Transmitting is not. Everything in the design follows from
that one fact:

* **Anything heard on the air reaches IRC immediately.** There is no queue on
  that side and there never should be — displaying a message costs nothing.
* **Anything going to the air is queued, priced, and may be refused.** The
  sender is told which, straight away.

So a QSO through the gateway is not symmetric, and pretending otherwise is
what gets transmitters cooked. An IRC user types a line and sees it in the
channel instantly; the RF side of that same line may be thirty seconds behind,
or may never go out at all. The server says so:

```
-!- Queued for RF (SK0MT-1), about 24s of queue ahead of it. 2 station(s) on frequency.
-!- Not put on the air: the transmit queue is 58s deep and the duty-cycle limit
    will not clear it in time. Your message was delivered on IRC. Try again
    shortly — or say it shorter.
```

That second message is the important one. The alternative — accept it, queue
it, drop it silently at the transmitter two minutes later — is the worst
possible outcome: the sender believes it went out, so they do not repeat
themselves, and the airtime was reserved for nothing. **Refuse early and say
so** is the rule everywhere in this code.

## Admission control, not just throttling

The backlog is bounded in **seconds of airtime**, not frames
(`radio.max_queued_airtime_secs`, default 60). Sixty short frames and six long
ones are very different amounts of transmitting, and only one of those units
means anything to a power amplifier.

Every outbound frame has a class, and each class may fill only part of that
budget:

| Class | May fill | Why |
|---|---|---|
| `ack` | 100 % | One short frame that stops a long retransmission. Never rationed. |
| `control` | 85 % | WELCOME, NAMES replies, errors, PONG. |
| `direct` | 70 % | Private messages and held mail. |
| `chat` | 50 % | Channel conversation: the bulk of the traffic and the most tolerant of being dropped. |

A single FIFO would be the wrong shape. A burst of channel chat would fill it,
and the ACK that would have ended a retry cycle waits behind ten seconds of
gossip — costing more airtime than the chat did. Reserving headroom means
conversation is what gets squeezed, which is the correct answer.

Admission is checked **before** the session layer accepts a message, because
once it does, an ACK timer is running: a message admitted when there is no
airtime for it does not cost one transmission, it costs four.

## Nothing is transmitted that nobody asked for

The IRC feature set is much larger than a 300 baud channel can carry, so most
of it is deliberately not bridged:

| IRC event | On the air |
|---|---|
What the gateway will transmit is an **allowlist** (see `Delivery::rf_emission`).
Anything not named there stays on IRC.

| On the list | On the air? |
|---|---|
| Channel PRIVMSG (including `/me`) | **Yes** — queued, class `chat` |
| Private PRIVMSG to a station | **Yes** — queued, class `direct`, acknowledged |
| Held mail | **Yes** — `mailbox_flush_batch` per exchange, default one |
| TOPIC | Only if it actually changed, and through the full chat gate |
| JOIN/PART presence | Only if `presence_notices = true`. Off by default |
| JOIN by a station | Confirmation with a member **count**, never the list |
| `NAMES` (from a station) | Only when explicitly asked; capped at `radio.rf_names_max` and 160 octets |
| Errors to a station | One frame, **not** acknowledged or retried |
| APRS ACK/REJ to a radio that messaged us | One short frame, class `ack` (stops the radio retrying) |
| APRS position / status heard | **Not on the list** — IRC NOTICE only |
| Server notices to a station | One frame, capped at 80 characters, not retried |
| NOTICE, other CTCP, QUIT, NICK, MODE, KICK, INVITE, numerics, RADIO | **Not on the list** |

Two of those are worth spelling out.

**Joining does not read out the roll.** A station that joins wants to know it
is in. It did not ask who else is there, and forty nicknames is a kilobyte —
which fragments into a long acknowledged exchange with retries, minutes of
airtime, for a question nobody asked. The join confirmation carries the member
count; `/names` gets the (capped) list when someone actually wants it.

**Errors are sent unreliably on purpose.** A reliable error is ACK-requested
and retried up to `max_retries` times, so "no such channel" would cost four
transmissions — more airtime than the message that provoked it. If it is lost,
the station sees no reply, which conveys the same thing.

## Messages are short, and that is enforced twice

`policy.max_rf_text_len` is an upper bound, but the limit that matters is
`policy.max_rf_fragments` (default 2). At startup the server computes what
actually fits in that many frames at the configured paclen and lowers the text
limit to match, logging the number it settled on.

Fragmentation is worse than it looks: it multiplies the airtime, and a message
only arrives if *every* fragment does, so the loss rate compounds — and a retry
resends all of them. Two frames is a sentence, which is what packet is for.

## How the governor works

Every frame is costed before it is keyed:

```text
airtime = txdelay + (octets_on_wire × 8 / baud) + txtail
```

and three independent limits are checked:

1. **Duty cycle.** Airtime in a sliding window (default 25 % of ten minutes).
2. **Continuous run.** After `max_continuous_secs` of unbroken transmitting,
   the transmitter is forced off for `cooldown_secs`. Transmissions less than
   three seconds apart count as one run, because as far as the power amplifier
   is concerned they are.
3. **Rolling-hour budget.** A hard ceiling on airtime per hour, so a
   pathological backlog cannot drip-feed the channel indefinitely.

### The 50 % ceiling

**The station will not exceed a 50 % duty cycle, whatever the configuration
says.** This is not a default, it is a bound:

* `max_duty_percent` is clamped to 50 in the governor and rejected above 50 by
  `--check`.
* The run and cooldown settings are an *independent* way to hold the
  transmitter keyed, so they are checked separately: `max_continuous_secs /
  (max_continuous_secs + cooldown_secs)` must also be at most 50 %. A 60 s run
  with a 10 s cooldown is an 86 % burst duty cycle and is refused at startup,
  even though the window average would have looked fine.
* `radio.duty.enabled = false` is only accepted with
  `radio.tnc.kind = "loopback"`. You cannot turn the governor off in front of
  a real transmitter.

The invariant is tested rather than asserted: the test suite drives the
governor with a sender that never stops trying for two simulated hours, then
slides a ten-minute window across every second of the result and checks the
measured duty at every position.

A frame that cannot go yet is **deferred**, not dropped, and is retried exactly
when enough old airtime falls out of the window. A frame held longer than
`max_hold_secs` **is** dropped and counted: on a 1 kbit/s channel a two-minute-
old chat line costs the same airtime as a fresh one and carries less
information.

Station identification **jumps the data queue** so `RADIO OFF` can sign off
without waiting for chat to drain, but it uses the **same airtime clock** as
every other frame: it waits for the previous transmission to finish, asks the
governor, and counts toward the continuous run and cooldown. `RADIO ID` is
limited to once per `id_interval_secs` unless the station currently owes an
identification. The safety interlock still holds it, including at sign-off.

## Configuration

```toml
[radio.duty]
enabled = true
baud = 300               # 300 = HF SSB packet, 1200 = VHF FM
txdelay_ms = 400         # match the TNC's TXDELAY
txtail_ms = 300          # match the TNC's TXTAIL
window_secs = 600
max_duty_percent = 25
max_continuous_secs = 30
cooldown_secs = 60
hourly_airtime_secs = 900   # 0 disables
max_hold_secs = 120
```

Two starting points:

=== "QRP HF (QMX, 300 baud)"

    The defaults above. Conservative on purpose: this is the configuration
    most likely to be damaged by getting it wrong.

=== "VHF FM (1200 baud, a radio with a heatsink)"

    ```toml
    [radio.duty]
    baud = 1200
    txdelay_ms = 300
    txtail_ms = 100
    max_duty_percent = 40
    max_continuous_secs = 60
    cooldown_secs = 30
    hourly_airtime_secs = 1800
    ```

`rfircd --check` refuses a configuration where a full-length frame is longer
than the whole duty allowance — otherwise nothing would ever be transmitted and
you would be left guessing why. It also refuses a `baud` that is not 300, 1200
or 9600 unless `allow_nonstandard_baud = true`: a 1200-baud governor in front
of Direwolf `MODEM 300` under-counts key-down by a factor of four. Set
`radio.tnc.direwolf_conf` so `--check` compares MODEM, TXDELAY and TXTAIL to
this block.

## The order things are refused in

Four independent mechanisms can stop a message reaching the air, and they fire
in this order. When something does not go out, this is the list to walk:

1. **IRC-side flood cap** (`[policy]`: `rf_channel_msgs_per_min`,
   `ip_to_rf_msgs_per_min`) — counts *messages*, per host. Cheap, and it stops
   a runaway client before anything else has to think about it.
2. **Privilege and content** — RF-TX granted, a callsign claimed, the text
   screened and length-capped.
3. **Airtime admission** (`radio.max_queued_airtime_secs`, per class) — is
   there room in the backlog for the seconds this will cost? This is the last
   point at which the sender can be told, so it is where the refusal happens.
4. **The governor** — duty cycle, continuous run, hourly budget. Defers what
   was already admitted, and drops it after `max_hold_secs` rather than
   transmitting it stale.

They are not redundant: the first counts messages and knows nothing about
length, the third counts seconds and knows nothing about who is talking, and
the fourth is the only one that knows what the transmitter has actually been
doing.

The station client (`rfirc-station`) runs the same governor with the same
ceiling — see [Station](station.md).

## Watching it

```
/quote RADIO DUTY
```

```
airtime: 12.4% duty over the last 600s, 74s keyed in the last hour (budget 900s);
8.2s queued (0.0s until the next slot); total keyed 512s; 3 frames deferred,
1 refused as backlog, 0 dropped stale, 0 dropped while inhibited
limits: 25% of 600s, max 30s continuous then 60s cooldown, 900s per rolling hour,
frames dropped after 120s held (baud 300, txdelay 400ms, txtail 300ms)
```

`queued` is the honest answer to "when will my message go out": it is the
airtime already committed, and the ETA senders are quoted is that plus the
governor's next free slot.

`RADIO STATUS` shows the duty percentage and any active cooldown to everyone;
the full breakdown is control-operator only.

Each frame that actually keys is also written to the audit log (and to
`rfircd::audit` at info) at the moment it is handed to the TNC:

```
rf_tx dest=SA0KAM kind=Welcome bytes=88 class=control keyed=3.3s duty=1.2%
```

`keyed` is modeled key-down time — TXDELAY + on-wire bits (with a stuffing
allowance) + TXTAIL — which is what the PA sees. KISS does not report PTT, so
this is not a measurement from the radio. `duty` is the sliding-window duty
cycle *after* that transmission was counted. The timestamp is when the frame
keyed, not when it was queued; a message sitting behind the governor will
show up here when it goes out, or not at all if it is dropped stale.

## What the operator can do while it is running

`RADIO QUEUE` answers "what has been accepted but not yet transmitted?", in
the three places something can be waiting:

```
-!- transmit queue: 4 frame(s), 18.2s of airtime, next slot in 0.0s (budget 60s)
-!- Per-station (reliable, awaiting ACK):
-!-   SM0ABC-7: 2 message(s) awaiting acknowledgement, 0 dropped
-!- Held for stations out of range: 3 message(s). Refused for backlog since
    start: 11. Dropped at the transmitter: 0.
```

`RADIO LIMIT` retunes the station without a restart, because the situations
that call for it are live ones — the band opened, the finals are hot, somebody
else needs the frequency — and restarting the gateway drops every station on
it:

```
/quote RADIO LIMIT DUTY 10          # down to 10% until further notice
/quote RADIO LIMIT DUTY off         # back to the configured limit
/quote RADIO LIMIT PACING 4000      # at least 4s between transmissions
/quote RADIO LIMIT PACING off
```

Both are audited, both take effect on the next frame, and neither is written
back to the configuration file — a restart returns to what is on disk. The
duty override is clamped to the same 50 % ceiling as everything else: asking
for 90 gets you 50 and a notice saying so. Pacing can only ever *slow* the
station: a `RADIO LIMIT PACING` below `tx_pacing_ms` is raised to that floor
and the operator is told. It cannot buy airtime the duty limit has not
released.

## When it is not safe to transmit at all

rfircd cannot see your radio. It speaks KISS to a modem, and KISS carries
frames, not telemetry — there is no SWR reading, no PA temperature and no power
meter anywhere in that path. On a QMX the one port that could answer (CAT) is
already held by Direwolf for PTT, and two processes cannot share a serial port.

So the check is yours to supply:

```toml
[radio.interlock]
command = "/usr/local/bin/check-swr"
args = ["--max", "2.5"]
interval_secs = 30
timeout_secs = 5
```

The command runs on a timer. While it fails, **nothing is transmitted**.
Traffic already in the transmit queue is *held* until the check passes again,
or dropped as stale if it waits longer than `max_hold`. It is not discarded
the way `RADIO OFF` discards it — a momentary SWR failure must not destroy
messages already admitted, and must not make the session layer retry into a
void until it gives a station up for lost.

Two properties make this a safety feature rather than a status light:

* **It fails closed.** A command that cannot be run, times out, or has not run
  yet counts as a failure. The failure mode of an unreadable SWR meter is not
  "assume it is fine".
* **It blocks station identification too.** Identification jumps the data
  queue, not the airtime clock — but if it is not safe to key up, it is not
  safe to key up for an ID either. A licence requires you to identify the
  transmissions you make, not to make one.

`RADIO STATUS` says so plainly when it is the interlock holding the station
down, so it is never confused with `RADIO OFF`.

## Stopping it

```
/quote RADIO OFF
```

This is the kill switch, and it is a real one:

1. The station identifies, because it has transmitted and owes the band a
   sign-off.
2. Everything already queued in the TNC task is **discarded**, not drained.
   An operator who says "off" does not mean "off once the backlog clears".
3. Nothing further is queued, and the TNC task refuses to key up even if
   something tries.

`RADIO ON` re-enables it. The airtime already spent is still counted — the
window does not reset because you toggled the switch.

The operator's switch and the safety interlock are deliberately separate flags
and cannot undo each other: an interlock recovering does not cancel a
`RADIO OFF`, and `RADIO ON` does not override a failing interlock. The one
difference between them is the sign-off ID. `RADIO OFF` lets it through —
that is the point of it — and a failing interlock does not.

If you need something more forceful than that, stop the process: with
`panic = "abort"` and no PTT of its own, rfircd cannot leave the radio keyed.
PTT belongs to Direwolf, and Direwolf drops it when its client goes away.

!!! warning "`enabled = false`"
    Turning the governor off is only reasonable into a dummy load. `RADIO DUTY`
    says so in as many words, because the setting is easy to copy from
    somebody's example config and forget.
