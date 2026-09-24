# IRC

The client in these examples is **irssi**. Any RFC 1459 client works; the
commands after connect are the same (`/quote` sends a raw IRC command).

TLS protects the hop to the gateway only. Everything that reaches the air is
in the clear.

## Connect

* **On this machine** — plaintext `127.0.0.1:6667`. Full access (speak,
  `REGISTER` / `IDENTIFY`, `OPER`, radio control). This is the operator sitting
  next to the radio.
* **From the internet** — implicit TLS on `[listen.tls]` (usually port 6697).
  Required to speak, register, identify, `OPER`, or control the transmitter.
* **Plaintext from off-box** is listen-only: join and watch. No
  `IDENTIFY`/`REGISTER`/`OPER`, no `PRIVMSG`, no `RADIO GRANT`. A connection
  `PASS` is still accepted so a passworded server remains watchable.

### irssi, on the radio machine

```
/connect 127.0.0.1 6667
```

### irssi, over TLS

```
/connect -tls irc.example.net 6697
```

Older irssi uses `-ssl` instead of `-tls`. Accept or pin the certificate the
same way you would for any other TLS IRC network. There is no STARTTLS: the
TLS port is TLS from the first byte.

If the server has a connection password (`server.password`, or an oper has
set one with `PASSWD`):

```
/connect -tls irc.example.net 6697
/quote PASS the-server-password
```

(Or put the password in irssi's `/connect` password field.) Then `NICK` /
`USER` as usual — irssi does those for you when you `/connect`.

## irssi: this server only

irssi's default config autoconnects to public networks. Start it so it does
**not** do that, and point it only at this gateway:

```
irssi -! -c 127.0.0.1 -p 6667 -n alice
```

`-!` skips autoconnect. `-c` / `-p` / `-n` are this process, this port, this
nick.

Your **nick cannot be your callsign.** `SM0XYZ`, `SM0XYZ-7` and `SM0XYZ|7` are
reserved for stations heard on the air. Use a normal nick (`alice`) and claim
the callsign after connect.

A dedicated config (nothing else in it) lives next to the gateway config:

```
# ~/.config/rfircd/irssi.conf  —  irssi --config=this-file
servers = (
  {
    address = "127.0.0.1";
    chatnet = "rfirc";
    port = "6667";
    use_tls = "no";
    autoconnect = "yes";
  }
);
chatnets = {
  rfirc = { type = "IRC"; nick = "alice"; };
};
settings = {
  core = {
    real_name = "alice";
    user_name = "alice";
    nick = "alice";
  };
};
channels = (
  { name = "#rf"; chatnet = "rfirc"; autojoin = "Yes"; }
);
ignores = ( );
```

```
mkdir -p ~/.config/rfircd
# save the file above, then:
irssi --config=~/.config/rfircd/irssi.conf
```

Once connected:

```
/quote CALLSIGN SM0XYZ
/quote REGISTER a-secret-password
```

On later sessions, after the nick is registered:

```
/quote IDENTIFY a-secret-password
```

irssi can send those itself (`/set autosendcmd` on that network, or a
`autosendcmd` in the chatnet block). Do not put the OPER password in
autosendcmd on a shared machine.

TLS from another host: `use_tls = "yes"; port = "6697"; address = "irc.example.net";`

## Nick, callsign, and registration

Two things can be reserved so nobody else can use them: the **nick** and the
**callsign**. Both are bound by `REGISTER` on that nick.

```
/nick alice
/quote CALLSIGN SM0XYZ
/quote REGISTER your-secret-password
```

`REGISTER` writes an Argon2id hash of the password to `accounts.file`
(`nicks.json` by default). If you already claimed a callsign this session, that
callsign is bound to this nick too. After that:

* Nobody else can keep the nick `alice` without `IDENTIFY` (an unidentified
  taker is renamed to a guest after `identify_timeout_secs`).
* Nobody else can `CALLSIGN SM0XYZ`. A control operator can release either
  with `DROPNICK` / `UNCLAIM`.

`CALLSIGN` without `REGISTER` lasts for this session only. It is a logged
claim, not proof of licence.

On the next connect:

```
/nick alice
/quote IDENTIFY your-secret-password
```

That restores RF-TX (if an oper granted it). A `CALLSIGN` claimed this
session is bound to the nick; otherwise the stored callsign is restored.

`UNREGISTER <password>` deletes the account, including the grant and the
stored callsign.

Changing nick drops `IDENTIFY` and the session `CALLSIGN`. Identify again on
the nick that owns the grant.

Callsign-shaped nicks (`SM0ABC|7`, `SM0ABC-7`, `SM0ABC\7`) are reserved for
stations heard on the air. An internet user cannot take them.

## What is transmitted

Radiation is an **allowlist**. Only these IRC events can key the transmitter,
and only after RF-TX, CALLSIGN, a `+r` channel, and the usual airtime gate.
Chat may CQ into an empty channel; TOPIC still waits for an RF member.

| On the list | |
|---|---|
| `PRIVMSG` chat | including `/me` (CTCP ACTION) |
| `TOPIC` | if it actually changed; same RF-TX gate as chat |
| JOIN/PART presence | only if `presence_notices = true` (off by default) |

Everything else is not on the list, so it stays on IRC: NOTICE, other CTCP,
MODE, KICK, INVITE, QUIT, NICK, numerics, RADIO replies, REGISTER, OPER.
A new event type does not go on the air until someone adds it to the list.

## Watch a simulated station

No radio and no licence: a virtual frequency plus the station client. This is
the way to see RF nicks and messages in irssi.

In `rfircd.toml`, `[radio] enabled = true` and `[radio.tnc]` pointing at
kisshub (`host = "127.0.0.1"`, `port = 8001`). Then:

```sh
./target/release/rfirc-kisshub --bind 127.0.0.1:8001
./target/release/rfircd -c rfircd.toml
./target/release/rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --channel '#rf'
```

In irssi (localhost, as above): `/join #rf`. The station appears as
`SM0ABC|7`. Type in the station client; the line shows up in `#rf`. Type in
irssi; it stays on IRC until a control operator `RADIO GRANT`s your registered
nick (or you `OPER`). The hub prints frames in `axlisten` format.

More detail: [station.md](station.md), [quickstart.md](quickstart.md).

## Two populations

They are not the same account.

* **Internet user** — ordinary nick (`alice`). Registers, may be granted RF-TX,
  claims a callsign with `CALLSIGN`. The nick itself is not a callsign.
* **RF station** — `rfirc-station` or any AIRC client. Already on the air.
  Appears as a reserved nick derived from the AX.25 source (`SM0ABC-7` →
  `SM0ABC|7`). No `REGISTER`, no `RADIO GRANT`. Restrict with
  `policy.allow_callsigns` / `deny_callsigns`.
* **APRS radio** — a stock HT or app, no AIRC. Messages the gateway callsign
  to put a line into a channel (below). Same RF nick as an AIRC station.

Speaking on IRC and radiating are separate. Without RF-TX your messages stay
on the internet even in `#rf`.

## Letting an internet user key the transmitter

Three things. The nick does **not** have to look like a callsign.

1. **Register the nick** (and, if you want it kept, the callsign) as above.
2. **A control operator grants RF-TX.** They `OPER`, then
   `/quote RADIO GRANT alice`. Refused if the nick is not registered — there
   would be nothing to persist. The flag is stored on that account and
   applied immediately if they are online.
3. **Claim a callsign** if it was not restored by `IDENTIFY`.
   `/quote CALLSIGN SM0XYZ`.

Then join `#rf`. If the transmitter is on and you have RF-TX plus a CALLSIGN,
`PRIVMSG #rf` is radiated — including as CQ when no RF station has joined
yet. You will get a NOTICE when it actually goes out. Chat from users
without RF-TX stays on IRC until a station is in the channel.

A control operator (`OPER`) has RF-TX for that session without a grant. They
still need `CALLSIGN` before their text is radiated. On a public
`listen.bind`, `[[opers]]` passwords must be Argon2id hashes from
`rfircd --hash-password`; plaintext is accepted only on loopback.

```
/oper root the-oper-password
```

## Letting an RF station on the air

The station transmits with its own radio. The gateway does not grant it RF-TX.

```sh
rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --channel '#rf'
```

If `allow_callsigns` is empty, any plausible amateur callsign may use the RF
side. A non-empty list is a closed system. `deny_callsigns` bans a station
(SSID 0 bans every SSID of that call).

## From a stock APRS radio

AIRC is not required. Message the gateway callsign (`SK0MT-1` in the example
config) from any APRS HT or app:

```
#rf hello from the trail
```

The gateway ACKs so the radio's retry cycle stops, the station appears in
the channel as `SM0ABC|7`, and the line shows up on IRC. AIRC stations
already in `#rf` hear a translated copy; they did not decode the APRS
frame. A line `alice hello` when `alice` is online is a private query to
that nick instead.

Replies: `/msg SM0ABC|7 …` or channel chat in `#rf` while that station is
present goes back as an addressed APRS message (`nick: text`). Set
`radio.rf_mode = "aprs"` if you want *all* outbound RF chat as APRS (no AIRC
CQ). IRC JOIN/PART/MODE/numerics are never put on the air as APRS.

Position reports and status beacons heard on frequency are shown in the
channel as a NOTICE (`-SM0ABC|7- 59°30.00N 018°03.00E - QTH`). They are
never put back on the air. They do not join the channel.

`?` or `HELP` is answered with `send #chan text`. With `radio.aprs_channel = "#rf"`, a line that has no `#channel` prefix goes there, and that is also where beacons are shown (otherwise the first `+r` channel). `radio.aprs = false` turns the whole path off.

The same `allow_callsigns` / `deny_callsigns` lists apply. A kicked station
is refused (`rej`), not injected.

## Control operator: accounts, bans, server password

After `/oper`:

```
ACCOUNTS                 list registered nicks, bound callsigns, RF-TX
DROPNICK <nick>          unregister a nick (no password); online user is told
UNCLAIM <callsign>       release a bound callsign so someone else may claim it
KLINE <host> [reason]    ban that IP; drop matching clients; stored in nicks.json
UNKLINE <host>
KLINES                   list IP bans
PASSWD <password|off>    set or clear the IRC connection password for this process
```

In irssi: `/quote ACCOUNTS`, `/quote KLINE 203.0.113.9 :abuse`,
`/quote PASSWD secret`, `/quote PASSWD off`.

`KLINE` survives restart (it is written with the nick file). `PASSWD` does
not: a restart reloads `server.password` from the config file.

## After a restart

`Accounts::load` reads `nicks.json` at startup. There is no other privilege
store.

| | Survives | Lost |
|---|---|---|
| Registered nicks, password hashes, RF-TX grants, last CALLSIGN, IP bans | `nicks.json` | |
| Channels, `+r`, channel operator lists, OPER passwords, `server.password` | config file | runtime `MODE` / `PASSWD` |
| Who is connected, mailbox, RF sessions, `OPER` this session | | yes — in memory |

On connect, `IDENTIFY` reloads RF-TX. A `CALLSIGN` claimed this session is
bound; otherwise the stored callsign is restored. `OPER` is entered
again each session. The mailbox is a cache, not a mail server.

## Channel modes

Set in `[[channels]]` in the config. Reloaded on start. Only `OPER` can change
`+r` at runtime, and that change is not written back to the file.

| Mode | Meaning |
|---|---|
| `+r` | Bridged to RF. Config: `rf = true`. JOIN-created channels are never `+r`. |
| `+m` | Moderated. Default on `+r` channels: only `+v` / `+o` may `PRIVMSG`. |
| `+t` | Topic locked (default). |
| `+k` / `+l` | Key / limit, as usual. |
| `+n` | Always on. No external messages. |

A `+r` message is radiated if the transmitter is on, the sender has RF-TX and
a CALLSIGN, and **either** an RF station is already in the channel **or** the
sender is calling CQ into an empty one (so a station on frequency can hear
the gateway and join). Chat from users without RF-TX stays on IRC. TOPIC
still waits for an RF member — do not CQ a topic into silence.

## User and member flags

| Flag | Where | Meaning |
|---|---|---|
| `+v` | channel | May speak on a `+m` channel. On `+r`, a CALLSIGN (or OPER) grants this. Does **not** mean the text is radiated. |
| `+o` | channel | Channel operator. On `+r`, only a server OPER may grant it. Config `operators = ["alice"]` applies after IDENTIFY. |
| `+o` | user (`MODE` yourself) | Control operator (`OPER` this session). |
| `+R` | user | RF-TX. `WHOIS` says so too. |

## Commands

RFC 1459 / 2812 client commands this server implements:

```
PASS NICK USER QUIT PING PONG
JOIN PART PRIVMSG NOTICE TOPIC NAMES LIST WHO WHOIS WHOWAS MODE
MOTD LUSERS AWAY KICK INVITE ISON USERHOST
VERSION TIME ADMIN INFO HELP STATS LINKS CAP
OPER KILL
```

Not implemented (deliberately): server linking (`SERVER`, `SQUIT`, `CONNECT`),
services, SASL, DCC, STARTTLS, IRCv3 capabilities (`sasl`, `server-time`,
`echo-message`, …). `CAP LS` advertises an empty list; the client sends
`CAP END` and continues. Local extensions:

```
OPER <name> <pass>   control operator this session ([[opers]] in the config)
CALLSIGN <call>      claim a callsign; +v on +r. Required before radiation.
CALLSIGN             show the current claim
REGISTER <password>  bind this nick (and current CALLSIGN) so nobody else can use them
IDENTIFY <password>  prove you own it; bind this session's CALLSIGN, or restore the stored one
UNREGISTER <password>
RADIO                transmitter status (anyone)
KICK <chan> <nick>   channel op or OPER
KILL <nick>          OPER only
```

After `OPER`:

```
RADIO STATUS              transmitter, frames, stations heard, duty cycle
RADIO DUTY                airtime spent, backlog, limits in force, PA cooldown
RADIO QUEUE               everything accepted but not yet transmitted
RADIO LIMIT DUTY <1-50|off>    lower the duty cycle now, no restart
RADIO LIMIT PACING <ms|off>    widen the gap between transmissions
RADIO OFF / ON            kill switch; purges the transmit queue
RADIO ID                  identify now (rate-limited to id_interval)
RADIO HEARD               stations, last heard, queue depth
RADIO MAIL                held private messages (in memory, expires)
RADIO KICK <callsign>     drop an RF station's presence
RADIO GRANT <nick>        persist RF-TX on a registered nick
RADIO REVOKE <nick>       take it away (also persisted)
ACCOUNTS / DROPNICK / UNCLAIM / KLINE / UNKLINE / KLINES / PASSWD
```

## Held messages

A private message to a callsign that is not on frequency is held and delivered
when the station is next heard, carrying its age. Bounded and expiring; gone
on restart. See `mailbox_*` in the config.

```
-> PRIVMSG SM0ABC|7 :meet on 145.500 at 1900
<- NOTICE: SM0ABC-7 is not on frequency. Held for delivery when the station
   is next heard (1 waiting, dropped after 24h).
```

## Configuration

Every option is documented inline in `rfircd.example.toml`. Unknown keys are
refused. The validator also refuses an identification interval over ten
minutes, a gateway "callsign" that is not a plausible callsign, more than two
digipeater hops, and `radio.enabled` with no bridged channel.
