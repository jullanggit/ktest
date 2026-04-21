use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use proptest::{
    prelude::{BoxedStrategy, Just},
    prop_oneof,
    strategy::{Strategy, ValueTree},
    test_runner::TestRunner,
};
use proptest_state_machine::strategy::Sequential;
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    time::Instant,
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Scratch devices available to the continuous test harness.
    #[arg(long = "device", required = true)]
    available_devices: Vec<String>,

    /// File to write the harness log to.
    #[arg(long)]
    log: PathBuf,

    /// Filesystem mountpoint managed by the wrapper.
    #[arg(long)]
    mountpoint: PathBuf,

    /// Number of state-machine transitions to generate for this run.
    #[arg(long, default_value_t = 3)]
    operations: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Operation {
    Checkpoint,
    Remount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Config {
    available_devices: Vec<String>,
    log_path: PathBuf,
    mountpoint: PathBuf,
    operations: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Model {
    available_devices: Vec<String>,
    active_member_devices: Vec<String>,
    mountpoint: PathBuf,
    mounted: bool,
}

impl TryFrom<Cli> for Config {
    type Error = anyhow::Error;

    fn try_from(cli: Cli) -> Result<Self> {
        ensure!(
            !cli.available_devices.is_empty(),
            "at least one --device is required",
        );
        ensure!(cli.operations > 0, "--operations must be at least 1");
        ensure_distinct_devices(&cli.available_devices)?;

        Ok(Self {
            available_devices: cli.available_devices,
            log_path: cli.log,
            mountpoint: cli.mountpoint,
            operations: cli.operations,
        })
    }
}

impl Model {
    fn from_config(config: &Config) -> Result<Self> {
        let mounted = is_mountpoint_active(&config.mountpoint)?;

        Ok(Self {
            available_devices: config.available_devices.clone(),
            // The wrapper currently formats and mounts the first device before
            // invoking the Rust harness. Keep the membership boundary inside the
            // model by deriving the initially active set from the available
            // pool here rather than from the CLI shape.
            active_member_devices: vec![config.available_devices[0].clone()],
            mountpoint: config.mountpoint.clone(),
            mounted,
        })
    }

    fn supports(&self, operation: &Operation) -> bool {
        match operation {
            Operation::Checkpoint | Operation::Remount => self.mounted,
        }
    }

    fn apply_transition(&self, operation: &Operation) -> Self {
        match operation {
            Operation::Checkpoint => self.clone(),
            // This high-level transition models a completed unmount + fsck +
            // mount cycle. The system remains mounted before and after it even
            // though the concrete executor performs multiple commands.
            Operation::Remount => self.clone(),
        }
    }
}

#[derive(Debug)]
struct Harness {
    model: Model,
    log: File,
}

impl Harness {
    fn new(config: &Config) -> Result<Self> {
        let log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&config.log_path)
            .with_context(|| format!("failed to open log file {}", config.log_path.display()))?;
        let model = Model::from_config(config)?;

        Ok(Self { model, log })
    }

    fn run(&mut self, initial_model: &Model, ops: &[Operation]) -> Result<()> {
        ensure!(
            &self.model == initial_model,
            "generated initial model does not match observed runtime model",
        );

        self.log_message(format!(
            "INFO phase=startup mounted_model={} active_member_devices={} available_devices={}",
            self.model.mounted,
            self.model.active_member_devices.join(","),
            self.model.available_devices.join(","),
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
        ensure!(
            self.model.mounted,
            "checkpoint requires a mounted filesystem",
        );

        let mountpoint = self.mountpoint_str()?.to_owned();

        run_command(&mut self.log, "sync", &[])?;
        run_command(
            &mut self.log,
            "bcachefs",
            &["fs", "usage", "-h", "--all", mountpoint.as_str()],
        )?;
        self.model = self.model.apply_transition(&Operation::Checkpoint);
        Ok(())
    }

    fn remount(&mut self) -> Result<()> {
        ensure!(self.model.mounted, "remount requires a mounted filesystem");

        let mountpoint = self.mountpoint_str()?.to_owned();

        run_command(&mut self.log, "sync", &[])?;
        run_command(&mut self.log, "umount", &[mountpoint.as_str()])?;

        let mut fsck_args: Vec<&str> = vec!["fsck", "-n"];
        fsck_args.extend(self.model.active_member_devices.iter().map(String::as_str));
        run_command(&mut self.log, "bcachefs", &fsck_args)?;

        let joined = self.model.active_member_devices.join(":");
        run_command(
            &mut self.log,
            "mount",
            &["-t", "bcachefs", joined.as_str(), mountpoint.as_str()],
        )?;

        self.model = self.model.apply_transition(&Operation::Remount);
        Ok(())
    }

    fn mountpoint_str(&self) -> Result<&str> {
        self.model.mountpoint.to_str().with_context(|| {
            format!(
                "mountpoint path is not valid utf-8: {}",
                self.model.mountpoint.display()
            )
        })
    }

    fn log_message(&mut self, message: String) -> Result<()> {
        writeln!(self.log, "{message}").context("failed to write harness log")?;
        Ok(())
    }
}

impl Model {
    fn assert_matches_reality(&self) -> Result<()> {
        let mounted = is_mountpoint_active(&self.mountpoint)?;

        ensure!(
            mounted == self.mounted,
            "model says mounted={}, but mountpoint {} is mounted={}",
            self.mounted,
            self.mountpoint.display(),
            mounted,
        );

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
    let cli = Cli::parse();
    let config = Config::try_from(cli)?;
    let initial_model = Model::from_config(&config)?;
    let transitions = generate_operations(&initial_model, config.operations)?;
    let mut harness = Harness::new(&config)?;

    harness.run(&initial_model, &transitions)
}

fn generate_operations(initial_model: &Model, count: usize) -> Result<Vec<Operation>> {
    let strategy = operation_sequence_strategy(initial_model.clone(), count);
    let mut runner = TestRunner::deterministic();
    let tree = strategy.new_tree(&mut runner).map_err(|reason| {
        anyhow::anyhow!("failed to generate state-machine transitions: {reason}")
    })?;
    let (generated_initial_model, transitions, _seen_counter) = tree.current();

    ensure!(
        generated_initial_model == *initial_model,
        "generated initial state diverged from the requested runtime model",
    );

    Ok(transitions)
}

fn operation_sequence_strategy(
    initial_model: Model,
    count: usize,
) -> Sequential<Model, Operation, BoxedStrategy<Model>, BoxedStrategy<Operation>> {
    Sequential::new(
        (count..=count).into(),
        move || Just(initial_model.clone()).boxed(),
        |state, transition| state.supports(transition),
        operation_strategy,
        |state, transition| state.apply_transition(transition),
    )
}

fn operation_strategy(_state: &Model) -> BoxedStrategy<Operation> {
    prop_oneof![Just(Operation::Checkpoint), Just(Operation::Remount)].boxed()
}

fn ensure_distinct_devices(devices: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();

    for dev in devices {
        if !seen.insert(dev) {
            bail!("device listed more than once: {dev}");
        }
    }

    Ok(())
}

fn is_mountpoint_active(path: &Path) -> Result<bool> {
    let status = Command::new("mountpoint")
        .arg("-q")
        .arg(path)
        .status()
        .with_context(|| format!("failed to query mountpoint {}", path.display()))?;

    match status.code() {
        Some(0) => Ok(true),
        Some(32) => Ok(false),
        _ => bail!(
            "mountpoint -q {} failed with status {}",
            path.display(),
            render_status(status),
        ),
    }
}

fn run_command(log: &mut File, program: &str, args: &[&str]) -> Result<()> {
    writeln!(log, "CMD program={} args={}", program, args.join(" "))
        .context("failed to write command header to log")?;

    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run command: {} {}", program, args.join(" ")))?;
    write_output(log, "stdout", &output.stdout)?;
    write_output(log, "stderr", &output.stderr)?;

    if !output.status.success() {
        writeln!(
            log,
            "ERROR command_failed program={} status={}",
            program,
            render_status(output.status),
        )
        .context("failed to write command failure to log")?;
        bail!("command failed: {} {}", program, args.join(" "));
    }

    Ok(())
}

fn write_output(log: &mut File, stream: &str, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }

    for line in String::from_utf8_lossy(bytes).lines() {
        writeln!(log, "CMD_{} {}", stream, line)
            .with_context(|| format!("failed to write {} output to log", stream))?;
    }

    Ok(())
}

fn render_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => code.to_string(),
        None => "signal".to_string(),
    }
}
