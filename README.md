# rfircd

[![CI](https://github.com/mashu/rfircd/actions/workflows/ci.yml/badge.svg)](https://github.com/mashu/rfircd/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/mashu/rfircd/graph/badge.svg)](https://codecov.io/gh/mashu/rfircd)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![docs (stable)](https://github.com/mashu/rfircd/actions/workflows/docs.yml/badge.svg)](https://mashu.github.io/rfircd/stable/)
[![docs (dev)](https://img.shields.io/badge/docs-dev-c4a35a)](https://mashu.github.io/rfircd/dev/)

An IRC server with an RF gateway over KISS (Direwolf or any KISS TNC). Speaks
AIRC on the air, and interops with stock APRS radios.

```
   irssi ──TCP/TLS──►  rfircd  ──KISS──►  Direwolf ──RF──► stations
```

Docs: [stable](https://mashu.github.io/rfircd/stable/) · [dev](https://mashu.github.io/rfircd/dev/)

```sh
cargo build --release
cp rfircd.example.toml rfircd.toml   # edit callsign, TNC, channels
./target/release/rfircd -c rfircd.toml
```
