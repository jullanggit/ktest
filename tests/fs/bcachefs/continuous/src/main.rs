use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use proptest::{
    prelude::{BoxedStrategy, Just},
    sample::select,
    strategy::Strategy,
    test_runner::{Config as ProptestConfig, TestCaseError, TestError, TestRunner},
};
use proptest_state_machine::strategy::Sequential;
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output},
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
    AddDevice(String),
    RemoveDevice(String),
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Observation {
    mounted: bool,
    active_member_devices: Vec<String>,
    device_indices: BTreeMap<String, u32>,
}

impl TryFrom<Cli> for Config {
    type Error = anyhow::Error;

    fn try_from(cli: Cli) -> Result<Self> {
        ensure!(
            !cli.available_devices.is_empty(),
            "at least one --device is required",
        );
        ensure!(
            cli.available_devices.len() >= 2,
            "at least two --device entries are required for add/remove testing",
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
    fn initial(config: &Config) -> Self {
        let mut active_member_devices = vec![config.available_devices[0].clone()];
        sort_devices(&mut active_member_devices);

        Self {
            available_devices: config.available_devices.clone(),
            active_member_devices,
        }
    }

    fn supports(&self, operation: &Operation) -> bool {
        match operation {
            Operation::AddDevice(device) => {
                !self.active_member_devices.contains(device)
                    && self.available_devices.contains(device)
            }
            Operation::RemoveDevice(device) => {
                self.active_member_devices.len() > 1 && self.active_member_devices.contains(device)
            }
        }
    }

    fn apply_transition(&self, operation: &Operation) -> Self {
        let mut next = self.clone();

        match operation {
            Operation::AddDevice(device) => {
                if !next.active_member_devices.contains(device) {
                    next.active_member_devices.push(device.clone());
                    sort_devices(&mut next.active_member_devices);
                }
            }
            Operation::RemoveDevice(device) => {
                next.active_member_devices.retain(|d| d != device);
            }
        }

        next
    }
}

#[derive(Debug)]
struct Harness {
    config: Config,
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

        Ok(Self {
            config: config.clone(),
            model: Model::initial(config),
            log,
        })
    }

    fn prepare_fresh_filesystem(&mut self) -> Result<()> {
        self.cleanup_mountpoint()?;
        self.model = Model::initial(&self.config);

        let primary_device = self.config.available_devices[0].clone();
        let mountpoint = self.mountpoint_str()?.to_owned();

        self.log_message(format!(
            "INFO prepare_case primary_device={} mountpoint={mountpoint}",
            primary_device,
        ))?;

        run_command(
            &mut self.log,
            "bcachefs",
            &["format", "-f", primary_device.as_str()],
        )?;
        run_command(
            &mut self.log,
            "mount",
            &[
                "-t",
                "bcachefs",
                primary_device.as_str(),
                mountpoint.as_str(),
            ],
        )?;

        let observed = self.snapshot_state("after_prepare")?;
        self.assert_model_matches_observation(&observed)?;
        Ok(())
    }

    fn cleanup_mountpoint(&mut self) -> Result<()> {
        if is_mountpoint_active(&self.config.mountpoint)? {
            let mountpoint = self.mountpoint_str()?.to_owned();
            run_command(&mut self.log, "umount", &[mountpoint.as_str()])?;
        }

        Ok(())
    }

    fn run_case(&mut self, case_index: u32, ops: &[Operation]) -> Result<()> {
        self.log_message(format!(
            "INFO case_start index={} transitions={:?}",
            case_index, ops
        ))?;

        let initial = self.snapshot_state("case_start")?;
        self.assert_model_matches_observation(&initial)?;

        for (index, op) in ops.iter().enumerate() {
            let start = Instant::now();
            let before = self.snapshot_state("before_op")?;
            let expected = self.expected_observation_after(&before, op)?;

            self.log_message(format!(
                "INFO op_start index={} op={op:?} before_active_member_devices={} expected_active_member_devices={}",
                index,
                before.active_member_devices.join(","),
                expected.active_member_devices.join(","),
            ))?;

            self.execute_operation(op, &before)?;
            let after = self.snapshot_state("after_op")?;
            self.assert_expected_outcome(op, &expected, &after)?;
            self.assert_properties(&after)?;
            self.model = self.model.apply_transition(op);

            self.log_message(format!(
                "INFO op_ok index={} op={op:?} duration_ms={} active_member_devices={}",
                index,
                start.elapsed().as_millis(),
                after.active_member_devices.join(","),
            ))?;
        }

        self.log_message(format!("INFO case_done index={}", case_index))?;
        Ok(())
    }

    fn expected_observation_after(
        &self,
        before: &Observation,
        operation: &Operation,
    ) -> Result<Observation> {
        ensure!(before.mounted, "operations require a mounted filesystem");
        ensure!(
            self.model.active_member_devices == before.active_member_devices,
            "generator model diverged from observed active members before operation",
        );
        ensure!(
            self.model.supports(operation),
            "generator produced an operation unsupported by the current model: {operation:?}",
        );

        let next_model = self.model.apply_transition(operation);
        Ok(Observation {
            mounted: true,
            active_member_devices: next_model.active_member_devices,
            device_indices: BTreeMap::new(),
        })
    }

    fn execute_operation(&mut self, operation: &Operation, before: &Observation) -> Result<()> {
        match operation {
            Operation::AddDevice(device) => self.add_device(device),
            Operation::RemoveDevice(device) => self.remove_device(before, device),
        }
    }

    fn add_device(&mut self, device: &str) -> Result<()> {
        let mountpoint = self.mountpoint_str()?.to_owned();
        run_command(
            &mut self.log,
            "bcachefs",
            &["device", "add", "-f", mountpoint.as_str(), device],
        )
    }

    fn remove_device(&mut self, before: &Observation, device: &str) -> Result<()> {
        let mountpoint = self.mountpoint_str()?.to_owned();
        let dev_idx = before
            .device_indices
            .get(device)
            .copied()
            .with_context(|| format!("missing device index for removable member {device}"))?;
        let dev_idx = dev_idx.to_string();
        // `device remove` expects a fully evacuated member. Follow the documented
        // userspace workflow so remove only fails when reconcile cannot move the
        // remaining data/metadata elsewhere.
        run_command(&mut self.log, "bcachefs", &["device", "evacuate", device])?;
        run_command(
            &mut self.log,
            "bcachefs",
            &["device", "remove", dev_idx.as_str(), mountpoint.as_str()],
        )
    }

    fn assert_expected_outcome(
        &mut self,
        operation: &Operation,
        expected: &Observation,
        observed: &Observation,
    ) -> Result<()> {
        ensure!(
            observed.mounted == expected.mounted
                && observed.active_member_devices == expected.active_member_devices,
            "unexpected observed topology after {operation:?}: expected {expected:?}, got {observed:?}",
        );
        Ok(())
    }

    fn assert_properties(&mut self, expected_live_topology: &Observation) -> Result<()> {
        ensure!(
            expected_live_topology.mounted,
            "property assertions require a mounted filesystem",
        );

        let mountpoint = self.mountpoint_str()?.to_owned();
        run_command(&mut self.log, "sync", &[])?;
        run_command(&mut self.log, "umount", &[mountpoint.as_str()])?;

        let mut fsck_args: Vec<&str> = vec!["fsck", "-n"];
        fsck_args.extend(
            expected_live_topology
                .active_member_devices
                .iter()
                .map(String::as_str),
        );
        run_command(&mut self.log, "bcachefs", &fsck_args)?;

        let joined = expected_live_topology.active_member_devices.join(":");
        run_command(
            &mut self.log,
            "mount",
            &["-t", "bcachefs", joined.as_str(), mountpoint.as_str()],
        )?;

        let remounted = self.snapshot_state("after_remount")?;
        ensure!(
            remounted.mounted == expected_live_topology.mounted
                && remounted.active_member_devices == expected_live_topology.active_member_devices,
            "topology changed across fsck/remount: expected {expected_live_topology:?}, got {remounted:?}",
        );
        Ok(())
    }

    fn snapshot_state(&mut self, phase: &str) -> Result<Observation> {
        let mounted = is_mountpoint_active(&self.config.mountpoint)?;

        if !mounted {
            self.log_message(format!(
                "INFO snapshot phase={} mounted=false active_member_devices=",
                phase,
            ))?;
            return Ok(Observation {
                mounted: false,
                active_member_devices: Vec::new(),
                device_indices: BTreeMap::new(),
            });
        }

        let mountpoint = self.mountpoint_str()?.to_owned();
        let usage = run_command_capture(
            &mut self.log,
            "bcachefs",
            &["fs", "usage", "-h", "--all", mountpoint.as_str()],
        )?;
        let usage_stdout = String::from_utf8_lossy(&usage.stdout);
        let (active_member_devices, device_indices) =
            parse_active_member_devices(&usage_stdout, &self.config.available_devices)?;

        self.log_message(format!(
            "INFO snapshot phase={} mounted=true active_member_devices={}",
            phase,
            active_member_devices.join(","),
        ))?;

        Ok(Observation {
            mounted: true,
            active_member_devices,
            device_indices,
        })
    }

    fn assert_model_matches_observation(&mut self, observed: &Observation) -> Result<()> {
        ensure!(
            observed.mounted,
            "expected the test filesystem to be mounted, but it is not",
        );
        ensure!(
            self.model.active_member_devices == observed.active_member_devices,
            "generator model diverged from observed topology: model={:?}, observed={:?}",
            self.model.active_member_devices,
            observed.active_member_devices,
        );
        Ok(())
    }

    fn mountpoint_str(&self) -> Result<&str> {
        self.config.mountpoint.to_str().with_context(|| {
            format!(
                "mountpoint path is not valid utf-8: {}",
                self.config.mountpoint.display()
            )
        })
    }

    fn log_message(&mut self, message: String) -> Result<()> {
        writeln!(self.log, "{message}").context("failed to write harness log")?;
        Ok(())
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::try_from(cli)?;
    initialize_log_file(&config.log_path)?;
    run_proptest_cases(&config)
}

fn run_proptest_cases(config: &Config) -> Result<()> {
    let initial_model = Model::initial(config);
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
        "generated initial state diverged from the configured initial model",
    );

    let prepare_result = harness.prepare_fresh_filesystem();
    let case_result = prepare_result.and_then(|()| harness.run_case(case_index, transitions));
    let cleanup_result = harness.cleanup_mountpoint();

    match (case_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(case_err), Ok(())) => Err(case_err),
        (Ok(()), Err(cleanup_err)) => Err(cleanup_err),
        (Err(case_err), Err(cleanup_err)) => {
            Err(case_err.context(format!("case cleanup also failed: {cleanup_err:#}")))
        }
    }
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

fn operation_strategy(state: &Model) -> BoxedStrategy<Operation> {
    let mut operations = Vec::new();

    for device in &state.available_devices {
        if state.supports(&Operation::AddDevice(device.clone())) {
            operations.push(Operation::AddDevice(device.clone()));
        }
    }

    for device in &state.active_member_devices {
        if state.supports(&Operation::RemoveDevice(device.clone())) {
            operations.push(Operation::RemoveDevice(device.clone()));
        }
    }

    assert!(
        !operations.is_empty(),
        "state-machine reached a topology with no valid operations",
    );

    select(operations).boxed()
}

fn parse_active_member_devices(
    usage_text: &str,
    available_devices: &[String],
) -> Result<(Vec<String>, BTreeMap<String, u32>)> {
    let mut active_member_devices = Vec::new();
    let mut device_indices = BTreeMap::new();

    for line in usage_text.lines() {
        if line.contains("(device ") {
            let dev_idx = line
                .split_once("(device ")
                .and_then(|(_, rest)| rest.split_once(')'))
                .and_then(|(dev_idx, _)| dev_idx.parse::<u32>().ok())
                .with_context(|| {
                    format!("failed to parse device index from fs usage output: {line}")
                })?;
            let (_, rest) = line.split_once(':').with_context(|| {
                format!("failed to parse device line from fs usage output: {line}")
            })?;
            let dev_name = rest.split_whitespace().next().with_context(|| {
                format!("failed to parse device name from fs usage output: {line}")
            })?;

            let device = resolve_available_device(dev_name, available_devices);
            device_indices.insert(device.clone(), dev_idx);
            active_member_devices.push(device);
        }
    }

    ensure!(
        !active_member_devices.is_empty(),
        "fs usage did not report any active member devices",
    );
    ensure_distinct_devices(&active_member_devices)?;
    sort_devices(&mut active_member_devices);
    Ok((active_member_devices, device_indices))
}

fn resolve_available_device(device_name: &str, available_devices: &[String]) -> String {
    available_devices
        .iter()
        .find(|path| {
            Path::new(path).file_name().and_then(|name| name.to_str()) == Some(device_name)
        })
        .cloned()
        .unwrap_or_else(|| device_name.to_owned())
}

fn sort_devices(devices: &mut [String]) {
    devices.sort();
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
    run_command_capture(log, program, args)?;
    Ok(())
}

fn run_command_capture(log: &mut File, program: &str, args: &[&str]) -> Result<Output> {
    let output = run_command_capture_allow_failure(log, program, args)?;

    if !output.status.success() {
        bail!("command failed: {} {}", program, args.join(" "));
    }

    Ok(output)
}

fn run_command_capture_allow_failure(
    log: &mut File,
    program: &str,
    args: &[&str],
) -> Result<Output> {
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
    }

    Ok(output)
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
