#![feature(exit_status_error)]

use std::{path::PathBuf, process::Command};

use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Scratch devices available to the continuous test harness.
    #[arg(long = "device", required = true)]
    devices: Vec<String>,

    /// File to write the harness log to.
    #[arg(long)]
    log: PathBuf,

    /// Filesystem mountpoint managed by the wrapper.
    #[arg(long)]
    mountpoint: PathBuf,

    /// Number of operations to launch per case.
    #[arg(long, default_value_t = 20)]
    operations: usize,

    /// Deterministic seed for operation scheduling.
    #[arg(long, default_value_t = 42)]
    seed: u64,
}

fn main() {
    let cli = Cli::parse();

    make_fs(&cli);
    run_operations(&cli);
    offline_fsck(&cli);
}

fn make_fs(cli: &Cli) {
    let mut command = Command::new("bcachefs");
    command.arg("format").args(&cli.devices);

    command.spawn().unwrap().wait().unwrap().exit_ok().unwrap();
}

fn run_operations(cli: &Cli) {
    let mut command = Command::new("bcachefs");
    command.args(["device", "resize", &cli.devices[0], "1G"]);

    command.spawn().unwrap().wait().unwrap().exit_ok().unwrap();
}

fn offline_fsck(cli: &Cli) {
    let mut command = Command::new("bcachefs");
    command.arg("fsck").arg(&cli.mountpoint);

    command.spawn().unwrap().wait().unwrap().exit_ok().unwrap();
}
