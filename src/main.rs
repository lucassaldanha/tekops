mod beaconapi;
mod cli;
mod http;
mod logfmt;
mod logs;
mod metrics;
mod output;
mod protocol;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
