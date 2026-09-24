# Station client and virtual channel

## rfirc-station

Line-oriented, so it works over ssh and on a headless Pi.

```sh
rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --channel '#rf' \
                --tnc tcp://127.0.0.1:8001
rfirc-station --call SM0ABC-7 --gateway SK0MT-1 \
                --tnc serial:/dev/ttyUSB0@9600 --path SK0MT-2
```

Commands: `/join #chan`, `/part`, `/names`, `/msg <nick> <text>`, `/ping`,
`/quit`. Anything else goes to the current channel. Chat is an unreliable
broadcast to `AIRC`, so every station in range hears it once; joins, private
messages to IRC nicks, and requests are unicast to the gateway and ACKed.
`/msg SM0XYZ|1 …` is unicast to that callsign, not via the gateway.

Serial TNCs need a build with `--features serial`.

### Airtime

The station client runs the same airtime governor as the gateway, with the same
50 % duty ceiling and the same refusal to accept a run/cooldown pair that would
exceed it. You are a human typing rather than an automatic service, but it is
the same QRP radio at the same baud rate, and the finals do not know the
difference.

```sh
rfirc-station --call SM0ABC-7 --gateway SK0MT-1 \
                --baud 300 --txdelay 400 --txtail 300 \
                --duty 25 --max-continuous 30 --cooldown 60
```

Those are the defaults, and they assume HF: 300 baud, a QMX-class radio. For
1200 baud VHF FM with a heatsink, `--baud 1200 --duty 40`. `--txdelay` and
`--txtail` are pushed to the TNC as the KISS parameters as well as being used
to price each frame, so the modem and the model cannot disagree.

See [Airtime](airtime.md) for what the limits mean and why they are shaped the
way they are.

## rfirc-kisshub

A virtual channel: every TCP client that connects is a station on the same
frequency, and it prints every frame in `axlisten` monitor format. No radio, no
licence.

```sh
rfirc-kisshub --bind 127.0.0.1:8001 &
rfircd -c rfircd.toml &                 # radio.tnc.port = 8001
rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --channel '#rf'
```

Then connect irssi to **only** this server (`irssi -! -c 127.0.0.1 -p 6667 -n alice`),
`/quote CALLSIGN SM0XYZ`, join `#rf`. That lets you speak on IRC. Type in the
station client to see `SM0ABC|7` talk in the channel. To key the virtual
transmitter from IRC, `OPER` or `RADIO GRANT` a registered nick — see
[usage.md](usage.md). With RF-TX you can CQ even before the station client
joins; without it, messages stay on IRC until an RF nick is in the channel.

```
#rf <alice> hello over the air
*alice* direct to you
-- #rf members: SM0ABC|7,alice
```

while the channel monitor shows what it cost:

```
SK0MT-1>AIRC:A1......#rf.alice.hello over the air
SM0ABC-7>AIRC:A1......#rf.morning all, 5 watts from Kista
```
