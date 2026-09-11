mod beaconapi;
mod cli;
mod completions;
mod http;
mod logfmt;
mod logs;
mod metrics;
mod output;
mod protocol;
mod term;
mod update;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
