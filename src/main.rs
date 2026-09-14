mod beaconapi;
mod cli;
mod completions;
mod config;
mod curl;
mod docker;
mod doctor;
mod dump;
mod gist;
mod host;
mod http;
mod logfmt;
mod loglevel;
mod logs;
mod metrics;
mod output;
mod protocol;
mod redact;
mod stack;
mod term;
mod update;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
