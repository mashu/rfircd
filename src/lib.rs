//! `rfircd` — an IRC server with an RF gateway over KISS (Direwolf or any
//! KISS TNC). Speaks AIRC on the air and interops with stock APRS radios.
//!
//! Layering, from the antenna up:
//!
//! ```text
//!  radio ─ KISS TNC ─┬─ ax25::airtime   duty cycle, PA cooldown, airtime budget
//!                    ├─ ax25::tnc       the link: framing, pacing, reconnect
//!                    ├─ ax25::frame     AX.25 UI frames, callsign addressing
//!                    ├─ airc::frame     AIRC/1, the compact on-air protocol
//!                    ├─ airc::session   sequencing, ACKs, fragmentation
//!                    └─ aprs            stock APRS messages addressed to us
//!                              │
//!                    server::radio      may this be transmitted, and when?
//!                    server::bridge     what a frame heard on the air means
//!                              │
//!                    server             the actor: one state, one ordering
//!                              │
//!                    server::commands   what an IRC client may ask for
//!                    server::clients ─ irc::client ─ TLS/TCP ─ IRC client
//! ```
//!
//! # Two properties hold the design together
//!
//! **One task owns all mutable state.** Connections, the TNC and the timer all
//! send [`Event`]s into a single channel and [`server::run`] processes them in
//! order. There is no lock anywhere in the message path. That matters more
//! than usual here because the two sides differ by six orders of magnitude in
//! latency — microseconds on TCP, seconds on RF — and a lock-based design
//! would let a slow radio write block an IRC client. It is a correctness
//! decision, not a throughput compromise: everything that scales with the
//! number of clients (parsing, socket I/O, password hashing, log writing) is
//! per-client work on the thread pool.
//!
//! **Transmitting is not the same kind of act as receiving.** Anything heard
//! on the air reaches IRC immediately, because that costs nothing. Anything
//! going *to* the air is priced in seconds of key-down time, checked against a
//! budget, and may be refused — and every refusal is reported to whoever asked
//! for it. That asymmetry is why [`server::radio`] is a subsystem with one
//! entry point rather than a set of helpers: a rule about airtime is only
//! worth something if there is no second way round it.

pub mod accounts;
pub mod airc;
pub mod aprs;
pub mod audit;
pub mod ax25;
pub mod callsign;
pub mod cli;
pub mod config;
pub mod interlock;
pub mod irc;
pub mod kisshub;
pub mod policy;
pub mod server;
pub mod station;
pub mod wizard;

pub use callsign::Callsign;
pub use config::Config;
pub use server::{Event, Server};
