use std::{
    collections::HashMap,
    path::PathBuf,
    process::{Command, Stdio},
};

use clap::Parser;
use rand::{rngs::StdRng, thread_rng, Rng, SeedableRng};

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

    let device_infos = make_fs(&cli);
    mount_fs(&cli);
    run_operations(&cli, device_infos);
    fsck(&cli);
    unmount_fs(&cli);
}

#[derive(Debug)]
struct DeviceInfo {
    /// Size of the device in bytes.
    size: usize,
    /// Bucket size of the device in bytes.
    bucket_size: usize,
}

fn make_fs(cli: &Cli) -> HashMap<String, DeviceInfo> {
    let mut command = Command::new("bcachefs");
    command.arg("format").args(&cli.devices).arg("--force");

    let output = command.output().unwrap();
    assert!(output.status.success());

    String::from_utf8(output.stdout)
        .unwrap()
        .split("\nDevice ")
        .skip(2)
        .map(|str| {
            let (mut device, mut size, mut bucket_size) = ("", 0, 0);
            str.lines().for_each(|line| {
                let (field, value) = line.split_once(':').unwrap();
                match field.trim() {
                    "Size" => size = human_size_to_bytes(value),
                    "Bucket size" => bucket_size = human_size_to_bytes(value),
                    other if other.parse::<usize>().is_ok() => {
                        device = value.split_once('\t').unwrap().0.trim()
                    }
                    _ => {}
                }
            });
            (device.to_string(), DeviceInfo { size, bucket_size })
        })
        .collect()
}

fn human_size_to_bytes(size: &str) -> usize {
    let size = size.trim();
    let (factor, trim) = match size.chars().last().unwrap() {
        'k' => (1024., true),
        'M' => (1024. * 1024., true),
        'G' => (1024. * 1024. * 1024., true),
        'T' => (1024. * 1024. * 1024. * 1024., true),
        _ => (1., false),
    };
    (size[..size.len() - if trim { 1 } else { 0 }]
        .parse::<f64>()
        .unwrap()
        * factor) as usize
}

fn mount_fs(cli: &Cli) {
    let mut command = Command::new("bcachefs");
    command
        .arg("mount")
        .arg(&cli.devices[0])
        .arg(&cli.mountpoint);

    assert!(command.spawn().unwrap().wait().unwrap().success());
}

fn run_operations(cli: &Cli, device_infos: HashMap<String, DeviceInfo>) {
    let mut rng = StdRng::seed_from_u64(cli.seed);
    for _ in 0..cli.operations {
        let device = &cli.devices[rng.gen_range(0..cli.devices.len())];
        let device_size = device_infos[device].size;

        let target_size = rng.gen_range(0..((1.08 * device_size as f64) as usize));
        let expected_outcome =
            target_size >= 512 * device_infos[device].bucket_size && target_size <= device_size;

        println!("{device} -> {target_size} ({expected_outcome})");

        let mut command = Command::new("bcachefs");
        command
            .args(["device", "resize", device])
            .arg(format!("{target_size}B"));

        let result = command.output().unwrap();
        if result.status.success() != expected_outcome {
            panic!("Unexpected outcome: {result:?}");
        }
    }
}

fn fsck(cli: &Cli) {
    let mut command = Command::new("bcachefs");
    command.arg("fsck").arg(&cli.mountpoint);

    assert!(command.spawn().unwrap().wait().unwrap().success());
}

fn unmount_fs(cli: &Cli) {
    let mut command = Command::new("umount");
    command.arg(&cli.mountpoint);

    assert!(command.spawn().unwrap().wait().unwrap().success());
}

// fn run_cmd(parts: &[&str]) {
//     let mut command = Command::new(parts[0]);
//     command.args(&parts[1..parts.len()]);

//     assert!(command.spawn().unwrap().wait().unwrap().success());
// }
