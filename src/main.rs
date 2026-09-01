mod beaconapi;
mod cli;
mod logfmt;
mod logs;
mod output;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
