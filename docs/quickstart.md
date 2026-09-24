# Quick start

Three ways to run this, in order of how much RF you are willing to emit.

Read [regulatory.md](regulatory.md) before any path that enables `radio.enabled`.

Prebuilt binaries: [Install](install.md). QMX on Debian GNU/Linux: [qmx.md](qmx.md).

## 0. Build

```sh
cargo build --release
cp rfircd.example.toml rfircd.toml
```

Edit `server.name` and, if the radio will transmit, `radio.callsign` (your
callsign). Check the file:

```sh
./target/release/rfircd --check -c rfircd.toml
```

## 1. IRC only (no radio)

Leave `radio.enabled = false`. Start the server and connect any IRC client to
`127.0.0.1:6667`. Join `#local`.

```sh
./target/release/rfircd -c rfircd.toml
```

`#rf` exists but nothing is radiated.

On `#rf`, `CALLSIGN` grants `+v` (permission to speak on IRC). Radiating
requires a registered nick that a control operator has `RADIO GRANT`ed, or
`OPER`. See [usage.md](usage.md).

```
/quote CALLSIGN SM0XYZ
/join #rf
/quote RADIO                 # transmitter status (no OPER needed)
/quote REGISTER secret12     # bind this nick; then ask an oper to GRANT it
```

## 2. Whole stack without a licence (virtual channel)

`rfirc-kisshub` is a fake shared frequency. Point the gateway TNC at it.

In `rfircd.toml`:

```toml
[radio]
enabled = true
callsign = "SK0MT-1"

[radio.tnc]
kind = "tcp"
host = "127.0.0.1"
port = 8001
```

```sh
./target/release/rfirc-kisshub --bind 127.0.0.1:8001
./target/release/rfircd -c rfircd.toml
./target/release/rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --channel '#rf'
```

Connect an IRC client to `127.0.0.1:6667` **only this server** (irssi:
`irssi -! -c 127.0.0.1 -p 6667 -n alice`), `/quote CALLSIGN SM0XYZ`, join
`#rf`. The station nick appears as `SM0ABC|7`. Type a line in the station
client; it shows in irssi as ordinary channel text. Your IRC chat stays on the
internet until a control operator `RADIO GRANT`s your registered nick (or
you `OPER`). The hub prints frames in `axlisten` format.

## 3. Real RF via Direwolf

rfircd never talks to a radio. It talks **KISS** to a TNC. Direwolf is the
usual TNC: it takes sound-card audio, modulates AX.25, and offers KISS on TCP
port 8001.

```
radio ── audio/PTT ──► Direwolf (KISSPORT 8001) ──► rfircd :6667
```

Minimal `direwolf.conf` for VHF FM 1200 baud (a 2 m FM rig, SignaLink, etc.):

```
ADEVICE  plughw:1,0
MYCALL   SK0MT-1
CHANNEL  0
MODEM    1200
KISSPORT 8001
TXDELAY  30
# Optional: AX.25 with FEC. The KISS payload is still AX.25; rfircd does
# not implement FX.25 itself. Enable this in Direwolf, not in the gateway.
# FX25TX 1
```

Then the same `[radio]` / `[radio.tnc]` block as in section 2. Start Direwolf
first, then rfircd.

A stock APRS radio can message the gateway callsign with `#rf hello` to put
a line into a bridged channel. See [usage.md](usage.md).

Serial hardware TNCs: `cargo build --release --features serial` and
`radio.tnc.kind = "serial"`.

## 4. QRP Labs QMX (or QMX+)

Debian packages, groups, hamlib, 300 baud Direwolf, and first QSO:
**[QMX on Debian](qmx.md)**. Do not use Digi mode.

## IRC client (irssi)

Do not use a default irssi config that autoconnects to Libera or similar.
This server only:

```
irssi -! -c 127.0.0.1 -p 6667 -n alice
/quote CALLSIGN YOURCALL
/join #rf
```

Nick and callsign are **not** the same string: a callsign-shaped nick is
reserved for RF stations. Dedicated config, TLS, REGISTER: [usage.md](usage.md).
