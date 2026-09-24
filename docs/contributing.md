# Contributing

```sh
cargo test
```

The session layer is a pure state machine driven by an explicit `now`, so
retries, timeouts and reassembly are tested in microseconds without a radio.
`tests/gateway.rs` asserts on the actual bytes transmitted: that a station's
JOIN reaches IRC, that a message from the air is not re-transmitted, that an
unidentified user's message never reaches the antenna, that apparent ciphertext
is refused, and that an acknowledged private message is not retried.

Docs: `mkdocs serve` from the repo root (`requirements-docs.txt`).

## Layout

```
src/
  lib.rs             crate root: layering and public modules
  main.rs            rfircd — argument parsing, wiring, shutdown
  config.rs          TOML config and validation
  callsign.rs        callsign/SSID type, nickname mapping
  aprs.rs            APRS message parse/encode (stock-radio interop)
  policy.rs          rate limits, sanitation, plain-language screen
  bridge.rs          what happens when a frame arrives from the air
  ax25/              address, frame, kiss, tnc
  airc/              frame (codec), session (reliability)
  irc/               message (parser), numerics, client (TCP task)
  server/            event loop, state, commands, mailbox
  bin/
    rfirc-station.rs
    rfirc-kisshub.rs
docs/                this site
packaging/linux/     AppImage, .run, dist script
tests/gateway.rs
rfircd.example.toml
```

## Not doing

Server linking (IRC's S2S protocol assumes cheap reliable links; two gateways
on one frequency should share a channel over the air instead), DCC and CTCP
file transfer, and encryption of any kind on the RF path.
