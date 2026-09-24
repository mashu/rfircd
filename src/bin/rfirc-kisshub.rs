//! `rfirc-kisshub` — a virtual radio channel.
//!
//! Every TCP client that connects is treated as a station on the same
//! frequency. The implementation is in [`rfircd::kisshub`]; this is the
//! command line around it.
//!
//! ```sh
//! rfirc-kisshub --bind 127.0.0.1:8001 &
//! rfircd -c rfircd.toml                     # radio.tnc points at 8001
//! rfirc-station --call SM0ABC-7 --gateway SK0MT-1 --channel '#rf'
//! ```

use rfircd::kisshub::{self, Invocation};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match kisshub::parse_args(std::env::args().skip(1)) {
        Invocation::Run(opts) => Ok(kisshub::run(opts).await?),
        Invocation::Help(text) => {
            println!("{text}");
            Ok(())
        }
        Invocation::Usage(message) => {
            eprintln!("{message}");
            eprintln!("{}", kisshub::USAGE);
            std::process::exit(2);
        }
    }
}
