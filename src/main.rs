mod beaconapi;
mod cli;
mod http;
mod logfmt;
mod logs;
mod metrics;
mod output;
mod protocol;
mod term;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
