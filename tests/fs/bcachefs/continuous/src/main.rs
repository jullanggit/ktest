// TODO
// - Replicas (on it)
// - Erasure coding
// - Device add / remove
// - All other fs options
// - Concurrent load
// - Changing (changeable) fs options mid-run & mid-shrink
// - Mixing per-file options

use std::{
    collections::HashMap,
    fs::{self, remove_file, File},
    io::BufWriter,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use clap::Parser;
use rand::{
    rngs::{SmallRng, StdRng},
    seq::{index::sample, SliceRandom},
    thread_rng, Rng, RngCore, SeedableRng,
};

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
    #[arg(long)]
    seed: Option<u64>,
}

fn main() {
    let cli = Cli::parse();

    let seed = cli.seed.unwrap_or_else(|| thread_rng().gen());
    println!("Using seed: {seed}");
    let mut rng = SmallRng::seed_from_u64(seed);

    let fs_info = make_fs(&cli, &mut rng);
    mount_fs(&cli);
    run_operations(&cli, fs_info, &mut rng);
    fsck(&cli);
    unmount_fs(&cli);
}

#[derive(Debug)]
struct FsInfo {
    device_infos: HashMap<String, DeviceInfo>,
    ec: bool,
    replicas: usize,
}

#[derive(Debug)]
struct DeviceInfo {
    /// Size of the device in bytes.
    size: usize,
    /// Bucket size of the device in bytes.
    bucket_size: usize,
}

fn make_fs(cli: &Cli, rng: &mut SmallRng) -> FsInfo {
    let ec: bool = rng.gen();
    println!("ec = {ec}");

    let max_replicas = if ec {
        (cli.devices.len() - 1).min(3) // fault tolerance of ec replicas = normal replicas + 1
    } else {
        cli.devices.len()
    };
    let replicas = rng.gen_range(1..=max_replicas);
    println!("replicas = {replicas}");

    let mut command = Command::new("bcachefs");
    command
        .arg("format")
        .arg("--force")
        .arg(format!("--replicas={replicas}"));
    if ec {
        command.arg("--erasure_code");
        for dev in &cli.devices {
            // force equal bucket sizes to make stripes be able to allocate across all devices.
            // TODO: allow different bucket sizes - would require more complicated capacity calculations
            command.arg("--bucket_size=512k").arg(dev);
        }
    } else {
        command.args(&cli.devices);
    }

    let output = command.output().unwrap();
    if !output.status.success() {
        panic!("{output:?}");
    }

    FsInfo {
        ec,
        replicas,
        device_infos: String::from_utf8(output.stdout)
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
            .collect(),
    }
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

fn bytes_to_human_size(size: usize) -> String {
    let mut size = size as f64;
    let mut unit = 'B';
    for u in ['k', 'M', 'G', 'T'] {
        if size < 1024. {
            break;
        }
        size /= 1024.;
        unit = u;
    }
    if size < 10. {
        format!("{:.2}{}", size, unit)
    } else if size < 100. {
        format!("{:.1}{}", size, unit)
    } else {
        format!("{:.0}{}", size, unit)
    }
}

fn mount_fs(cli: &Cli) {
    let mut command = Command::new("bcachefs");
    command
        .arg("mount")
        .arg(&cli.devices[0])
        .arg(&cli.mountpoint);

    assert!(command.spawn().unwrap().wait().unwrap().success());
}

fn run_operations(cli: &Cli, fs_info: FsInfo, rng: &mut SmallRng) {
    let FsInfo {
        device_infos,
        replicas,
        ec,
    } = fs_info;

    let min_bucket_size = device_infos
        .values()
        .map(|info| info.bucket_size)
        .min()
        .unwrap();

    let mut device_fs_sizes = device_infos
        .iter()
        .map(|(device, info)| (device.clone(), info.size))
        .collect::<HashMap<_, _>>();

    let mut files = Vec::new();

    let mut num_files = 0; // will be overwritten in first iteration
    for round in 0..cli.operations {
        let device = &cli.devices[rng.gen_range(0..cli.devices.len())];
        let device_size = device_infos[device].size;

        let target_size = rng.gen_range(0..((1.08 * device_size as f64) as usize)); // TODO: actually use buckets here as that is the atomic space unit used
        let target_device_fs_sizes = device_fs_sizes
            .iter()
            .map(|(map_device, size)| {
                (
                    map_device.clone(),
                    if map_device == { device } {
                        target_size
                    } else {
                        *size
                    },
                )
            })
            .collect::<HashMap<_, _>>();

        let max_data = {
            let mut sorted_device_fs_sizes =
                target_device_fs_sizes.values().cloned().collect::<Vec<_>>();
            sorted_device_fs_sizes.sort();

            let (replicas, overhead) = if ec {
                (
                    sorted_device_fs_sizes.len(), // full-width stripes
                    // overhead relative to full redundancy
                    {
                        let n = sorted_device_fs_sizes.len() as f64;
                        let p = replicas as f64;
                        p * (n - p + 1.) / n
                    },
                )
            } else {
                (replicas, 1.)
            };

            ((0..replicas)
                .map(|excluded| {
                    (0..sorted_device_fs_sizes.len() - excluded)
                        .map(|i| sorted_device_fs_sizes[i])
                        .sum::<usize>()
                        / (replicas - excluded)
                })
                .min()
                .unwrap() as f64
                * overhead) as usize
        };

        // expensive operation
        if round % 10 == 0 {
            let max_num_files = (0.92 // don't over-fill fs so we don't run into any nondeterministic resizes
                * max_data as f64) as usize
                / (min_bucket_size * replicas);

            num_files = rng.gen_range(0..max_num_files);
            make_num_files(num_files, min_bucket_size, &cli.mountpoint, &mut files, rng);
        };

        let device_reserved_space = 512 * device_infos[device].bucket_size;

        let usage = || {
            let mut command = Command::new("bcachefs");
            command.args(["fs", "usage", "-h"]).arg(&cli.mountpoint);

            String::from_utf8(command.output().unwrap().stdout).unwrap()
        };

        let (expected_outcome, reason) = if target_size < device_reserved_space {
            (false, "less than reserved space")
        // maybe also add reserved space?
        } else if num_files * min_bucket_size > max_data {
            (false, "not enough space for data")
        } else if target_size > device_size {
            (false, "bigger than device size")
        } else {
            (true, "no reason to fail")
        };

        println!(
            "{device} -> {} ({})",
            bytes_to_human_size(target_size),
            if expected_outcome {
                "success"
            } else {
                "failure"
            }
        );

        let usage_before = usage();

        let mut command = Command::new("bcachefs");
        command
            .args(["device", "resize", "--shrink", device])
            .arg(format!("{target_size}B"));

        let result = command.output().unwrap();
        if result.status.success() != expected_outcome {
            let usage_after = usage();
            panic!(
                "Unexpected outcome. Expected to {} because {}, but {}.\nResult: {result:?}\nUsage before: {usage_before}\nUsage after: {usage_after}",
                if expected_outcome { "succeed" } else { "fail" },
                reason,
                if result.status.success() {
                    "succeeded"
                } else {
                    "failed"
                }
            );
        }

        if result.status.success() {
            *device_fs_sizes.get_mut(device).unwrap() = target_size;
        }

        wait_for_reconcile(&cli.mountpoint);
    }
}

fn make_num_files(
    num_files: usize,
    file_size: usize,
    mountpoint: &Path,
    files: &mut Vec<usize>,
    rng: &mut SmallRng,
) {
    use std::cmp::Ordering::*;

    let file_path = |num: usize| {
        let mut path = mountpoint.to_path_buf();
        path.push(num.to_string());
        path
    };
    match files.len().cmp(&num_files) {
        Less => {
            let diff = num_files - files.len();
            println!(
                "Add {} - total: {}",
                bytes_to_human_size(diff * file_size),
                bytes_to_human_size(num_files * file_size)
            );

            let next_num = files.iter().max().copied().unwrap_or(0) + 1;
            for num in next_num..(next_num + diff) {
                let file_path = file_path(num);

                let mut data = vec![0u8; file_size];
                rng.fill_bytes(&mut data);

                fs::write(file_path, data).unwrap();

                files.push(num);
            }
        }
        Equal => {}
        Greater => {
            let diff = files.len() - num_files;
            println!(
                "Remove {} - total: {}",
                bytes_to_human_size(diff * file_size),
                bytes_to_human_size(num_files * file_size)
            );

            files.shuffle(rng);
            for file in files.drain(0..diff) {
                remove_file(file_path(file)).unwrap();
            }
        }
    }
    wait_for_reconcile(mountpoint);
}

/// wait for reconcile to finish
fn wait_for_reconcile(mountpoint: &Path) {
    // TODO: automatically trigger reconcile by writing I/O, if it would otherwise be waiting for it
    assert!(Command::new("bcachefs")
        .args(["reconcile", "wait"])
        .arg(mountpoint)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
        .wait()
        .unwrap()
        .success());
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
