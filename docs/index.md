# rfircd

An IRC server with an RF gateway over KISS (Direwolf or any KISS TNC). People
with an ordinary IRC client, people with a radio speaking AIRC, and people
with a stock APRS radio talk in the same channels.

![QMX to Direwolf to rfircd to IRC](assets/chain.png)

Three binaries: **`rfircd`** (gateway), **`rfirc-station`** (radio-side
client), **`rfirc-kisshub`** (virtual channel, no licence).

!!! warning "Read this before you transmit"
    Enabling `radio.enabled` makes your station transmit automatically, under
    your callsign, carrying other people's traffic. See [Regulatory](regulatory.md).

[Quick start](quickstart.md){ .md-button .md-button--primary }
[QMX on Debian](qmx.md){ .md-button }
[Install binaries](install.md){ .md-button }
