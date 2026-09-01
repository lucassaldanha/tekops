mod beaconapi;
mod cli;
mod logfmt;
mod logs;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
