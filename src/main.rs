mod beaconapi;
mod cli;
mod logfmt;
mod logs;
mod output;
mod protocol;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
