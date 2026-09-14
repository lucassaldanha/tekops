mod beaconapi;
mod cli;
mod completions;
mod docker;
mod host;
mod http;
mod logfmt;
mod logs;
mod metrics;
mod output;
mod protocol;
mod stack;
mod term;
mod update;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
