use std::env;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Instant;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug)]
enum Operation {
    Checkpoint,
    Remount,
}

#[derive(Debug)]
struct Config {
    member_devices: Vec<String>,
    pool_devices: Vec<String>,
    log_path: PathBuf,
    mountpoint: PathBuf,
}

#[derive(Debug)]
struct Model {
    member_devices: Vec<String>,
    pool_devices: Vec<String>,
    mountpoint: PathBuf,
    mounted: bool,
}

impl Model {
    fn new(config: &Config) -> Result<Self> {
        let mounted = is_mountpoint_active(&config.mountpoint)?;

        Ok(Self {
            member_devices: config.member_devices.clone(),
            pool_devices: config.pool_devices.clone(),
            mountpoint: config.mountpoint.clone(),
            mounted,
        })
    }

    fn assert_matches_reality(&self) -> Result<()> {
        let mounted = is_mountpoint_active(&self.mountpoint)?;

        if mounted != self.mounted {
            return Err(io::Error::other(format!(
                "model says mounted={}, but mountpoint {} is mounted={}",
                self.mounted,
                self.mountpoint.display(),
                mounted,
            ))
            .into());
        }

        Ok(())
    }
}

#[derive(Debug)]
struct Runner {
    model: Model,
    log: std::fs::File,
}

impl Runner {
    fn new(config: &Config) -> Result<Self> {
        let log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&config.log_path)?;
        let model = Model::new(config)?;

        Ok(Self { model, log })
    }

    fn run(&mut self, ops: &[Operation]) -> Result<()> {
        self.log_message(format!(
            "INFO phase=startup mounted_model={} member_devices={} pool_devices={}",
            self.model.mounted,
            self.model.member_devices.join(","),
            self.model.pool_devices.join(","),
        ))?;
        self.model.assert_matches_reality()?;

        for (index, op) in ops.iter().enumerate() {
            let start = Instant::now();

            self.log_message(format!("INFO op_start index={} op={}", index, op.name()))?;
            match op {
                Operation::Checkpoint => self.checkpoint()?,
                Operation::Remount => self.remount()?,
            }
            self.model.assert_matches_reality()?;
            self.log_message(format!(
                "INFO op_ok index={} op={} duration_ms={} mounted_model={}",
                index,
                op.name(),
                start.elapsed().as_millis(),
                self.model.mounted,
            ))?;
        }

        self.log_message(format!(
            "INFO phase=done mounted_model={}",
            self.model.mounted
        ))?;
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<()> {
        if !self.model.mounted {
            return Err(io::Error::other("checkpoint requires a mounted filesystem").into());
        }

        let mountpoint = self.mountpoint_str().to_owned();

        run_command(&mut self.log, "sync", &[])?;
        run_command(
            &mut self.log,
            "bcachefs",
            &["fs", "usage", "-h", "--all", mountpoint.as_str()],
        )?;
        Ok(())
    }

    fn remount(&mut self) -> Result<()> {
        if !self.model.mounted {
            return Err(io::Error::other("remount requires a mounted filesystem").into());
        }

        let mountpoint = self.mountpoint_str().to_owned();

        run_command(&mut self.log, "sync", &[])?;
        run_command(&mut self.log, "umount", &[mountpoint.as_str()])?;
        self.model.mounted = false;

        let mut fsck_args: Vec<&str> = vec!["fsck", "-n"];
        fsck_args.extend(self.model.member_devices.iter().map(String::as_str));
        run_command(&mut self.log, "bcachefs", &fsck_args)?;

        let joined = self.model.member_devices.join(":");
        run_command(
            &mut self.log,
            "mount",
            &["-t", "bcachefs", joined.as_str(), mountpoint.as_str()],
        )?;
        self.model.mounted = true;

        Ok(())
    }

    fn mountpoint_str(&self) -> &str {
        self.model
            .mountpoint
            .to_str()
            .expect("mountpoint must be utf-8")
    }

    fn log_message(&mut self, message: String) -> Result<()> {
        writeln!(self.log, "{message}")?;
        Ok(())
    }
}

impl Operation {
    fn name(&self) -> &'static str {
        match self {
            Operation::Checkpoint => "checkpoint",
            Operation::Remount => "remount",
        }
    }
}

fn main() -> Result<()> {
    let config = parse_args(env::args().skip(1))?;
    let mut runner = Runner::new(&config)?;

    // Start with the smallest possible state machine: a mounted filesystem and
    // two safe operations that exercise the harness plumbing without yet
    // changing topology or resize state.
    let ops = [
        Operation::Checkpoint,
        Operation::Remount,
        Operation::Checkpoint,
    ];
    runner.run(&ops)
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Config> {
    let mut member_devices = Vec::new();
    let mut pool_devices = Vec::new();
    let mut log_path = None;
    let mut mountpoint = None;

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--member-device" => member_devices.push(next_arg(&mut args, "--member-device")?),
            "--pool-device" => pool_devices.push(next_arg(&mut args, "--pool-device")?),
            "--log" => log_path = Some(PathBuf::from(next_arg(&mut args, "--log")?)),
            "--mountpoint" => {
                mountpoint = Some(PathBuf::from(next_arg(&mut args, "--mountpoint")?))
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => {
                return Err(io::Error::other(format!("unknown argument: {arg}")).into());
            }
        }
    }

    if member_devices.is_empty() {
        return Err(io::Error::other("at least one --member-device is required").into());
    }

    ensure_distinct_devices(&member_devices, &pool_devices)?;

    Ok(Config {
        member_devices,
        pool_devices,
        log_path: log_path.ok_or_else(|| io::Error::other("--log is required"))?,
        mountpoint: mountpoint.ok_or_else(|| io::Error::other("--mountpoint is required"))?,
    })
}

fn next_arg(
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    flag: &str,
) -> Result<String> {
    args.next()
        .ok_or_else(|| io::Error::other(format!("missing value for {flag}")).into())
}

fn print_help() {
    println!(
        "Usage: continuous-bcachefs-test --member-device <dev>... [--pool-device <dev>...] --mountpoint <path> --log <path>"
    );
}

fn ensure_distinct_devices(member_devices: &[String], pool_devices: &[String]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();

    for dev in member_devices.iter().chain(pool_devices.iter()) {
        if !seen.insert(dev) {
            return Err(io::Error::other(format!("device listed more than once: {dev}")).into());
        }
    }

    Ok(())
}

fn is_mountpoint_active(path: &Path) -> Result<bool> {
    let output = Command::new("mountpoint").arg("-q").arg(path).status()?;

    Ok(match output.code() {
        Some(0) => true,
        Some(32) => false,
        _ => {
            return Err(io::Error::other(format!(
                "mountpoint -q {} failed with status {}",
                path.display(),
                render_status(output),
            ))
            .into())
        }
    })
}

fn run_command(log: &mut std::fs::File, program: &str, args: &[&str]) -> Result<()> {
    writeln!(log, "CMD program={} args={}", program, args.join(" "))?;

    let output = Command::new(program).args(args).output()?;
    write_output(log, "stdout", &output.stdout)?;
    write_output(log, "stderr", &output.stderr)?;

    if !output.status.success() {
        writeln!(
            log,
            "ERROR command_failed program={} status={}",
            program,
            render_status(output.status),
        )?;
        return Err(
            io::Error::other(format!("command failed: {} {}", program, args.join(" "))).into(),
        );
    }

    Ok(())
}

fn write_output(log: &mut std::fs::File, stream: &str, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }

    for line in String::from_utf8_lossy(bytes).lines() {
        writeln!(log, "CMD_{} {}", stream, line)?;
    }

    Ok(())
}

fn render_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => code.to_string(),
        None => "signal".to_string(),
    }
}
