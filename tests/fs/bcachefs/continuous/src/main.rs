use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use proptest::{
    prelude::{BoxedStrategy, Just},
    prop_oneof,
    strategy::Strategy,
    test_runner::{Config as ProptestConfig, TestCaseError, TestError, TestRunner},
};
use proptest_state_machine::strategy::Sequential;
use std::{
    cell::Cell,
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

    /// Number of generated test cases to execute.
    #[arg(long, default_value_t = 32)]
    cases: u32,
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
    cases: u32,
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
        ensure!(cli.cases > 0, "--cases must be at least 1");
        ensure_distinct_devices(&cli.available_devices)?;

        Ok(Self {
            available_devices: cli.available_devices,
            log_path: cli.log,
            mountpoint: cli.mountpoint,
            operations: cli.operations,
            cases: cli.cases,
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
            .append(true)
            .open(&config.log_path)
            .with_context(|| format!("failed to open log file {}", config.log_path.display()))?;
        let model = Model::from_config(config)?;

        Ok(Self { model, log })
    }

    fn run_case(&mut self, case_index: u32, ops: &[Operation]) -> Result<()> {
        self.log_message(format!(
            "INFO case_start index={} transitions={:?}",
            case_index, ops
        ))?;
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
        self.log_message(format!("INFO case_done index={}", case_index))?;
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
    initialize_log_file(&config.log_path)?;
    run_proptest_cases(&config)
}

fn run_proptest_cases(config: &Config) -> Result<()> {
    let initial_model = Model::from_config(config)?;
    let strategy = operation_sequence_strategy(initial_model.clone(), 1..=config.operations);
    let mut runner = TestRunner::new(proptest_config(config.cases));
    let case_index = Cell::new(0_u32);

    let result = runner.run(
        &strategy,
        |(generated_initial_model, transitions, _seen_counter)| {
            let current_case = case_index.get();
            case_index.set(current_case + 1);

            match run_generated_case(config, &generated_initial_model, &transitions, current_case) {
                Ok(()) => Ok(()),
                Err(err) => Err(TestCaseError::fail(format!("{err:#}"))),
            }
        },
    );

    match result {
        Ok(()) => Ok(()),
        Err(TestError::Fail(reason, value)) => {
            append_failure_summary(&config.log_path, &reason.to_string(), &value)?;
            bail!("proptest found a minimal failing case: {reason}; transitions={value:?}");
        }
        Err(TestError::Abort(reason)) => {
            append_abort_summary(&config.log_path, &reason.to_string())?;
            bail!("proptest aborted: {reason}");
        }
    }
}

fn run_generated_case(
    config: &Config,
    generated_initial_model: &Model,
    transitions: &[Operation],
    case_index: u32,
) -> Result<()> {
    let mut harness = Harness::new(config)?;
    ensure!(
        &harness.model == generated_initial_model,
        "generated initial state diverged from the observed runtime model",
    );
    harness.run_case(case_index, transitions)
}

fn operation_sequence_strategy(
    initial_model: Model,
    size: impl Into<proptest::collection::SizeRange>,
) -> Sequential<Model, Operation, BoxedStrategy<Model>, BoxedStrategy<Operation>> {
    Sequential::new(
        size.into(),
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

fn proptest_config(cases: u32) -> ProptestConfig {
    let mut config = ProptestConfig::default();
    config.cases = cases;
    config.failure_persistence = None;
    config
}

fn initialize_log_file(path: &Path) -> Result<()> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to initialize log file {}", path.display()))?;
    Ok(())
}

fn append_failure_summary(
    path: &Path,
    reason: &str,
    value: &(
        Model,
        Vec<Operation>,
        Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    ),
) -> Result<()> {
    let mut log = OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("failed to append failure summary to {}", path.display()))?;
    writeln!(log, "ERROR proptest_failure reason={reason}")
        .context("failed to write proptest failure reason")?;
    writeln!(log, "ERROR proptest_failure_value {value:?}")
        .context("failed to write proptest failure value")?;
    Ok(())
}

fn append_abort_summary(path: &Path, reason: &str) -> Result<()> {
    let mut log = OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("failed to append abort summary to {}", path.display()))?;
    writeln!(log, "ERROR proptest_abort reason={reason}")
        .context("failed to write proptest abort reason")?;
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
