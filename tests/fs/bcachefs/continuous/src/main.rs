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
    // plan:
    // - add data
    //  - for now only before, not during shrinking
    //  - random amount of data should be on fs before shrinking
    //      - data stored as min(bucket-size)-sized files in a '/data' dir
    //      - add / delete random files to achieve target fullness
    let min_bucket_size = device_infos
        .values()
        .map(|info| info.bucket_size)
        .min()
        .unwrap();
    let reserved_space = device_infos
        .values()
        .map(|info| info.bucket_size)
        .sum::<usize>()
        * 512;
    let max_num_files = (device_infos.values().map(|info| info.size).sum::<usize>()
        - reserved_space)
        / min_bucket_size;

    let mut files = Vec::new();

    let mut rng = SmallRng::seed_from_u64(cli.seed);

    let mut num_files = 0; // will be overwritten in first iteration
    for round in 0..cli.operations {
        let device = &cli.devices[rng.gen_range(0..cli.devices.len())];
        let device_size = device_infos[device].size;

        let target_size = rng.gen_range(0..((1.08 * device_size as f64) as usize));

        // expensive operation
        if round % 10 == 0 {
            num_files = rng.gen_range(0..max_num_files);
            make_num_files(
                num_files,
                min_bucket_size,
                &cli.mountpoint,
                &mut files,
                &mut rng,
            );
        };

        let device_reserved_space = 512 * device_infos[device].bucket_size;
        let target_fs_size = target_size
            + device_infos
                .iter()
                .filter(|(map_device, _)| *map_device != device)
                .map(|(_, info)| info.size)
                .sum::<usize>();
        let fs_free_space = target_fs_size - num_files * min_bucket_size; // maybe also add reserved space?
        let (expected_outcome, reason) = if target_size < device_reserved_space {
            (false, "less than reserved space")
        } else if target_fs_size < fs_free_space {
            (false, "less than fs free space")
        } else if target_size > device_size {
            (false, "bigger than device size")
        } else {
            (true, "no reason to fail")
        };

        println!(
            "{device} -> {target_size} ({})",
            if expected_outcome {
                "success"
            } else {
                "failure"
            }
        );

        let usage = || {
            let mut command = Command::new("bcachefs");
            command.args(["fs", "usage"]).arg(&cli.mountpoint);

            String::from_utf8(command.output().unwrap().stdout).unwrap()
        };

        let usage_before = usage();

        let mut command = Command::new("bcachefs");
        command
            .args(["device", "resize", device])
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
                diff * file_size,
                num_files * file_size
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
                diff * file_size,
                num_files * file_size
            );

            files.shuffle(rng);
            for file in files.drain(0..diff) {
                remove_file(file_path(file)).unwrap();
            }
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
