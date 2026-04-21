use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
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

    /// Number of operations to launch per case.
    #[arg(long, default_value_t = 3)]
    operations: usize,

    /// Number of fresh filesystem cases to execute.
    #[arg(long, default_value_t = 32)]
    cases: u32,

    /// Deterministic seed for operation scheduling.
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Maximum number of concurrent in-flight operations.
    #[arg(long, default_value_t = 3)]
    max_inflight: usize,

    /// Manager polling/spawn interval in milliseconds.
    #[arg(long, default_value_t = 200)]
    spawn_interval_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Operation {
    AddDevice(String),
    RemoveDevice(String),
    ResizeDevice { device: String, target_bytes: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Config {
    available_devices: Vec<String>,
    device_physical_bytes: BTreeMap<String, u64>,
    device_resize_targets: BTreeMap<String, Vec<u64>>,
    log_path: PathBuf,
    mountpoint: PathBuf,
    operations: usize,
    cases: u32,
    seed: u64,
    max_inflight: usize,
    spawn_interval_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Observation {
    mounted: bool,
    active_member_devices: Vec<String>,
    device_indices: BTreeMap<String, u32>,
    device_sizes: BTreeMap<String, u64>,
    used_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpectedObservation {
    mounted: bool,
    active_member_devices: Option<Vec<String>>,
    required_device_sizes: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpectedOutcome {
    require_success: Option<bool>,
    on_success: ExpectedObservation,
    on_failure: ExpectedObservation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LatestResizeResult {
    target_bytes: u64,
    completed_success: bool,
}

#[derive(Debug)]
struct InflightOperation {
    id: u64,
    op: Operation,
    before: Observation,
    concurrent_at_spawn: Vec<Operation>,
    expected: ExpectedOutcome,
    child: Child,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    superseded: bool,
    started_at: Instant,
}

#[derive(Debug)]
struct Harness {
    config: Config,
    log: File,
    latest_resize_results: BTreeMap<String, LatestResizeResult>,
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
        ensure!(cli.max_inflight > 0, "--max-inflight must be at least 1");
        ensure_distinct_devices(&cli.available_devices)?;

        let mut device_physical_bytes = BTreeMap::new();
        let mut device_resize_targets = BTreeMap::new();

        for device in &cli.available_devices {
            let physical_bytes = query_device_size_bytes(device)?;
            let resize_targets = resize_candidate_target_bytes(physical_bytes)?;
            device_physical_bytes.insert(device.clone(), physical_bytes);
            device_resize_targets.insert(device.clone(), resize_targets);
        }

        Ok(Self {
            available_devices: cli.available_devices,
            device_physical_bytes,
            device_resize_targets,
            log_path: cli.log,
            mountpoint: cli.mountpoint,
            operations: cli.operations,
            cases: cli.cases,
            seed: cli.seed,
            max_inflight: cli.max_inflight,
            spawn_interval_ms: cli.spawn_interval_ms,
        })
    }
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
            log,
            latest_resize_results: BTreeMap::new(),
        })
    }

    fn prepare_fresh_filesystem(&mut self) -> Result<()> {
        self.latest_resize_results.clear();
        self.cleanup_mountpoint()?;

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
        ensure!(
            observed.mounted,
            "expected the test filesystem to be mounted after prepare",
        );
        ensure!(
            observed.active_member_devices == vec![primary_device.clone()],
            "unexpected initial topology after prepare: {:?}",
            observed,
        );
        ensure!(
            observed.device_sizes.get(&primary_device)
                == self.config.device_physical_bytes.get(&primary_device),
            "unexpected initial device size after prepare: {:?}",
            observed,
        );
        Ok(())
    }

    fn cleanup_mountpoint(&mut self) -> Result<()> {
        if is_mountpoint_active(&self.config.mountpoint)? {
            let mountpoint = self.mountpoint_str()?.to_owned();
            run_command(&mut self.log, "umount", &[mountpoint.as_str()])?;
        }

        Ok(())
    }

    fn run_case(&mut self, case_index: u32, seed: u64) -> Result<()> {
        self.log_message(format!(
            "INFO case_start index={} seed={} operations={} max_inflight={} spawn_interval_ms={}",
            case_index,
            seed,
            self.config.operations,
            self.config.max_inflight,
            self.config.spawn_interval_ms,
        ))?;

        let mut rng = StdRng::seed_from_u64(seed);
        let mut inflight = Vec::new();
        let mut launched = 0_usize;
        let mut next_op_id = 0_u64;

        let loop_result = (|| -> Result<()> {
            while launched < self.config.operations || !inflight.is_empty() {
                self.reap_completed(&mut inflight)?;

                if launched < self.config.operations && inflight.len() < self.config.max_inflight {
                    let observation = self.snapshot_state("manager_tick")?;
                    let candidates = self.operation_candidates(&observation, &inflight);

                    if candidates.is_empty() && inflight.is_empty() {
                        bail!(
                            "manager ran out of legal operations with {} launches remaining; observation={:?}",
                            self.config.operations - launched,
                            observation,
                        );
                    }

                    if !candidates.is_empty() && (inflight.is_empty() || rng.gen_bool(0.5)) {
                        let op = candidates[rng.gen_range(0..candidates.len())].clone();
                        let expected = self.expected_outcome(&observation, &op)?;
                        self.mark_superseded(&mut inflight, &op)?;
                        let spawned = self.spawn_operation(
                            next_op_id,
                            &observation,
                            &inflight,
                            &op,
                            expected,
                        )?;
                        inflight.push(spawned);
                        launched += 1;
                        next_op_id += 1;
                    }
                }

                if launched < self.config.operations || !inflight.is_empty() {
                    thread::sleep(Duration::from_millis(self.config.spawn_interval_ms));
                }
            }

            Ok(())
        })();

        let abort_result = self.abort_inflight(&mut inflight);

        match (loop_result, abort_result) {
            (Ok(()), Ok(())) => {
                self.log_message(format!("INFO case_done index={}", case_index))?;
                Ok(())
            }
            (Err(case_err), Ok(())) => Err(case_err),
            (Ok(()), Err(abort_err)) => Err(abort_err),
            (Err(case_err), Err(abort_err)) => Err(case_err.context(format!(
                "aborting inflight operations also failed: {abort_err:#}"
            ))),
        }
    }

    fn reap_completed(&mut self, inflight: &mut Vec<InflightOperation>) -> Result<()> {
        loop {
            let mut completed_index = None;

            for (index, op) in inflight.iter_mut().enumerate() {
                if let Some(status) = op
                    .child
                    .try_wait()
                    .with_context(|| format!("failed to poll in-flight operation {}", op.id))?
                {
                    completed_index = Some((index, status));
                    break;
                }
            }

            let Some((index, status)) = completed_index else {
                return Ok(());
            };

            let op = inflight.remove(index);
            let quiescent = inflight.is_empty();
            self.finish_operation(op, status, quiescent)?;
        }
    }

    fn finish_operation(
        &mut self,
        op: InflightOperation,
        status: ExitStatus,
        quiescent: bool,
    ) -> Result<()> {
        self.append_operation_output(&op)?;
        let after = self.snapshot_state("after_op")?;
        let success = status.success();

        if op.superseded {
            ensure!(
                after.mounted,
                "superseded operation {:?} left the filesystem unmounted",
                op.op,
            );
        } else {
            self.assert_expected_outcome(&op.op, &op.expected, success, &after)?;
            self.record_latest_resize_result(&op.op, success);
        }

        self.assert_live_properties(&after)?;
        if quiescent {
            self.assert_quiescent_properties(&after)?;
        }

        self.log_message(format!(
            "INFO op_done id={} op={:?} superseded={} success={} duration_ms={} before_used_bytes={} after_used_bytes={} concurrent_at_spawn={} active_member_devices={} device_sizes={}",
            op.id,
            op.op,
            op.superseded,
            success,
            op.started_at.elapsed().as_millis(),
            op.before.used_bytes,
            after.used_bytes,
            format_operations(&op.concurrent_at_spawn),
            after.active_member_devices.join(","),
            format_device_sizes(&after.active_member_devices, &after.device_sizes),
        ))?;

        Ok(())
    }

    fn operation_candidates(
        &self,
        observation: &Observation,
        inflight: &[InflightOperation],
    ) -> Vec<Operation> {
        if !observation.mounted {
            return Vec::new();
        }

        let topology_locked = inflight
            .iter()
            .any(|op| !matches!(op.op, Operation::ResizeDevice { .. }));
        let mut operations = Vec::new();

        /*
         * Add/remove change the member set directly, so the current harness only
         * issues them from a quiescent topology state. Resizes stay async and
         * may overlap each other so the manager can exercise superseding
         * requests on one device without also having to reason about concurrent
         * membership churn yet.
         */
        if !topology_locked && inflight.is_empty() {
            for device in &self.config.available_devices {
                if !observation.active_member_devices.contains(device) {
                    operations.push(Operation::AddDevice(device.clone()));
                }
            }

            if observation.active_member_devices.len() > 1 {
                for device in &observation.active_member_devices {
                    operations.push(Operation::RemoveDevice(device.clone()));
                }
            }
        }

        if !topology_locked {
            for device in &observation.active_member_devices {
                let Some(&current_bytes) = observation.device_sizes.get(device) else {
                    continue;
                };
                let Some(targets) = self.config.device_resize_targets.get(device) else {
                    continue;
                };

                for &target_bytes in targets {
                    if target_bytes == current_bytes {
                        continue;
                    }
                    if inflight.iter().any(|op| {
                        matches!(
                            op.op,
                            Operation::ResizeDevice {
                                device: ref inflight_device,
                                target_bytes: inflight_target,
                            } if inflight_device == device && inflight_target == target_bytes
                        )
                    }) {
                        continue;
                    }
                    operations.push(Operation::ResizeDevice {
                        device: device.clone(),
                        target_bytes,
                    });
                }
            }
        }

        operations
    }

    fn expected_outcome(
        &self,
        before: &Observation,
        operation: &Operation,
    ) -> Result<ExpectedOutcome> {
        ensure!(before.mounted, "operations require a mounted filesystem");

        let on_failure = ExpectedObservation {
            mounted: true,
            active_member_devices: Some(before.active_member_devices.clone()),
            required_device_sizes: before.device_sizes.clone(),
        };

        let on_success = match operation {
            Operation::AddDevice(device) => {
                let mut active = before.active_member_devices.clone();
                active.push(device.clone());
                sort_devices(&mut active);

                let mut required_device_sizes = before.device_sizes.clone();
                let physical_bytes = self
                    .config
                    .device_physical_bytes
                    .get(device)
                    .copied()
                    .with_context(|| format!("missing physical size for {device}"))?;
                required_device_sizes.insert(device.clone(), physical_bytes);

                ExpectedObservation {
                    mounted: true,
                    active_member_devices: Some(active),
                    required_device_sizes,
                }
            }
            Operation::RemoveDevice(device) => {
                let mut active = before.active_member_devices.clone();
                active.retain(|d| d != device);

                let mut required_device_sizes = before.device_sizes.clone();
                required_device_sizes.remove(device);

                ExpectedObservation {
                    mounted: true,
                    active_member_devices: Some(active),
                    required_device_sizes,
                }
            }
            /*
             * Concurrent resizes may complete in either order, so immediate
             * per-op checks only pin the member set. The final size convergence
             * check runs once the manager reaches a quiescent point.
             */
            Operation::ResizeDevice { .. } => ExpectedObservation {
                mounted: true,
                active_member_devices: Some(before.active_member_devices.clone()),
                required_device_sizes: BTreeMap::new(),
            },
        };

        let require_success = match operation {
            Operation::AddDevice(_) | Operation::RemoveDevice(_) => Some(true),
            Operation::ResizeDevice { target_bytes, .. } => {
                classify_resize_outcome(before.used_bytes, *target_bytes)
            }
        };

        Ok(ExpectedOutcome {
            require_success,
            on_success,
            on_failure,
        })
    }

    fn mark_superseded(
        &mut self,
        inflight: &mut [InflightOperation],
        new_op: &Operation,
    ) -> Result<()> {
        let Operation::ResizeDevice { device, .. } = new_op else {
            return Ok(());
        };

        for old in inflight.iter_mut() {
            if matches!(&old.op, Operation::ResizeDevice { device: old_device, .. } if old_device == device)
            {
                old.superseded = true;
                self.log_message(format!(
                    "INFO op_superseded old_id={} old_op={:?} new_op={new_op:?}",
                    old.id, old.op,
                ))?;
            }
        }

        Ok(())
    }

    fn spawn_operation(
        &mut self,
        id: u64,
        before: &Observation,
        inflight: &[InflightOperation],
        op: &Operation,
        expected: ExpectedOutcome,
    ) -> Result<InflightOperation> {
        let mountpoint = self.mountpoint_str()?.to_owned();
        let output_dir = self
            .config
            .log_path
            .parent()
            .unwrap_or_else(|| Path::new("/tmp"));
        let stdout_path = output_dir.join(format!("continuous-op-{id}.stdout"));
        let stderr_path = output_dir.join(format!("continuous-op-{id}.stderr"));
        let stdout = File::create(&stdout_path)
            .with_context(|| format!("failed to create {}", stdout_path.display()))?;
        let stderr = File::create(&stderr_path)
            .with_context(|| format!("failed to create {}", stderr_path.display()))?;

        let concurrent_ops = inflight.iter().map(|op| op.op.clone()).collect::<Vec<_>>();
        self.log_message(format!(
            "INFO op_spawn id={} op={op:?} before_used_bytes={} before_active_member_devices={} before_device_sizes={} concurrent={} expected={expected:?}",
            id,
            before.used_bytes,
            before.active_member_devices.join(","),
            format_device_sizes(&before.active_member_devices, &before.device_sizes),
            format_operations(&concurrent_ops),
        ))?;

        let mut command = match op {
            Operation::AddDevice(device) => {
                let mut cmd = Command::new("bcachefs");
                cmd.args(["device", "add", "-f", mountpoint.as_str(), device.as_str()]);
                cmd
            }
            Operation::RemoveDevice(device) => {
                let dev_idx = before
                    .device_indices
                    .get(device)
                    .copied()
                    .with_context(|| {
                        format!("missing device index for removable member {device}")
                    })?;
                let script = format!(
                    "bcachefs device evacuate {} && bcachefs device remove {} {}",
                    shell_quote(device),
                    dev_idx,
                    shell_quote(&mountpoint),
                );
                let mut cmd = Command::new("bash");
                cmd.args(["-lc", script.as_str()]);
                cmd
            }
            Operation::ResizeDevice {
                device,
                target_bytes,
            } => {
                let target = target_bytes.to_string();
                let mut cmd = Command::new("bcachefs");
                cmd.args(["device", "resize", device.as_str(), target.as_str()]);
                cmd
            }
        };

        let child = command
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .with_context(|| format!("failed to spawn operation {op:?}"))?;

        Ok(InflightOperation {
            id,
            op: op.clone(),
            before: before.clone(),
            concurrent_at_spawn: concurrent_ops,
            expected,
            child,
            stdout_path,
            stderr_path,
            superseded: false,
            started_at: Instant::now(),
        })
    }

    fn abort_inflight(&mut self, inflight: &mut Vec<InflightOperation>) -> Result<()> {
        while let Some(mut op) = inflight.pop() {
            self.log_message(format!("INFO op_abort id={} op={:?}", op.id, op.op))?;
            let _ = op.child.kill();
            let status = op
                .child
                .wait()
                .with_context(|| format!("failed to wait for aborted operation {}", op.id))?;
            self.append_operation_output(&op)?;
            self.log_message(format!(
                "INFO op_aborted id={} op={:?} status={}",
                op.id,
                op.op,
                render_status(status),
            ))?;
        }
        Ok(())
    }

    fn append_operation_output(&mut self, op: &InflightOperation) -> Result<()> {
        append_output_file(&mut self.log, op.id, "stdout", &op.stdout_path)?;
        append_output_file(&mut self.log, op.id, "stderr", &op.stderr_path)?;
        let _ = fs::remove_file(&op.stdout_path);
        let _ = fs::remove_file(&op.stderr_path);
        Ok(())
    }

    fn assert_expected_outcome(
        &mut self,
        operation: &Operation,
        expected: &ExpectedOutcome,
        success: bool,
        observed: &Observation,
    ) -> Result<()> {
        if let Some(required) = expected.require_success {
            ensure!(
                success == required,
                "unexpected command status for {operation:?}: required success={required}, got success={success}",
            );
        }

        let wanted = if success {
            &expected.on_success
        } else {
            &expected.on_failure
        };

        self.assert_expected_observation(operation, wanted, observed)
    }

    fn assert_expected_observation(
        &self,
        operation: &Operation,
        expected: &ExpectedObservation,
        observed: &Observation,
    ) -> Result<()> {
        ensure!(
            observed.mounted == expected.mounted,
            "unexpected mount state after {operation:?}: expected {}, got {}",
            expected.mounted,
            observed.mounted,
        );

        if let Some(active_member_devices) = &expected.active_member_devices {
            ensure!(
                &observed.active_member_devices == active_member_devices,
                "unexpected active member set after {operation:?}: expected {:?}, got {:?}",
                active_member_devices,
                observed.active_member_devices,
            );
        }

        for (device, expected_bytes) in &expected.required_device_sizes {
            let observed_bytes = observed
                .device_sizes
                .get(device)
                .copied()
                .with_context(|| format!("missing device size for {device} after {operation:?}"))?;
            ensure!(
                observed_bytes == *expected_bytes,
                "unexpected size for {device} after {operation:?}: expected {}, got {}",
                expected_bytes,
                observed_bytes,
            );
        }

        Ok(())
    }

    fn record_latest_resize_result(&mut self, operation: &Operation, success: bool) {
        match operation {
            Operation::AddDevice(device) | Operation::RemoveDevice(device) => {
                self.latest_resize_results.remove(device);
            }
            Operation::ResizeDevice {
                device,
                target_bytes,
            } => {
                self.latest_resize_results.insert(
                    device.clone(),
                    LatestResizeResult {
                        target_bytes: *target_bytes,
                        completed_success: success,
                    },
                );
            }
        }
    }

    fn assert_live_properties(&self, observed: &Observation) -> Result<()> {
        ensure!(
            observed.mounted,
            "continuous operations must leave the filesystem mounted",
        );
        ensure!(
            !observed.active_member_devices.is_empty(),
            "continuous operations must leave at least one active member device",
        );
        Ok(())
    }

    fn assert_quiescent_properties(&mut self, expected_live: &Observation) -> Result<()> {
        ensure!(
            expected_live.mounted,
            "quiescent assertions require a mounted filesystem",
        );

        /*
         * Offline fsck and remount are only safe once the manager has drained
         * all in-flight async operations. Doing this while a resize is still
         * running would turn the assertion itself into interference.
         */
        let mountpoint = self.mountpoint_str()?.to_owned();
        run_command(&mut self.log, "sync", &[])?;
        run_command(&mut self.log, "umount", &[mountpoint.as_str()])?;

        let mut fsck_args: Vec<&str> = vec!["fsck", "-n"];
        fsck_args.extend(
            expected_live
                .active_member_devices
                .iter()
                .map(String::as_str),
        );
        run_command(&mut self.log, "bcachefs", &fsck_args)?;

        let joined = expected_live.active_member_devices.join(":");
        run_command(
            &mut self.log,
            "mount",
            &["-t", "bcachefs", joined.as_str(), mountpoint.as_str()],
        )?;

        let remounted = self.snapshot_state("after_remount")?;
        ensure!(
            remounted.active_member_devices == expected_live.active_member_devices,
            "state changed across fsck/remount: expected active members {:?}, got {:?}",
            expected_live.active_member_devices,
            remounted.active_member_devices,
        );
        ensure!(
            remounted.device_sizes == expected_live.device_sizes,
            "state changed across fsck/remount: expected device sizes {:?}, got {:?}",
            expected_live.device_sizes,
            remounted.device_sizes,
        );

        for (device, latest) in &self.latest_resize_results {
            let Some(&observed_bytes) = remounted.device_sizes.get(device) else {
                continue;
            };
            if latest.completed_success {
                ensure!(
                    observed_bytes == latest.target_bytes,
                    "latest successful resize for {} did not converge: expected {}, got {}",
                    device,
                    latest.target_bytes,
                    observed_bytes,
                );
            } else {
                ensure!(
                    observed_bytes != latest.target_bytes,
                    "latest failed resize for {} still converged to failed target {}",
                    device,
                    latest.target_bytes,
                );
            }
        }

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
                device_sizes: BTreeMap::new(),
                used_bytes: 0,
            });
        }

        let mountpoint = self.mountpoint_str()?.to_owned();
        let usage = run_command_capture(
            &mut self.log,
            "bcachefs",
            &["fs", "usage", "--all", mountpoint.as_str()],
        )?;
        let usage_stdout = String::from_utf8_lossy(&usage.stdout);
        let (active_member_devices, device_indices, device_sizes, used_bytes) =
            parse_active_member_devices(&usage_stdout, &self.config.available_devices)?;

        self.log_message(format!(
            "INFO snapshot phase={} mounted=true used_bytes={} active_member_devices={} device_sizes={}",
            phase,
            used_bytes,
            active_member_devices.join(","),
            format_device_sizes(&active_member_devices, &device_sizes),
        ))?;

        Ok(Observation {
            mounted: true,
            active_member_devices,
            device_indices,
            device_sizes,
            used_bytes,
        })
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
    run_cases(&config)
}

fn run_cases(config: &Config) -> Result<()> {
    for case_index in 0..config.cases {
        let mut harness = Harness::new(config)?;
        let seed = config.seed.wrapping_add(case_index as u64);

        let prepare_result = harness.prepare_fresh_filesystem();
        let case_result = prepare_result.and_then(|()| harness.run_case(case_index, seed));
        let cleanup_result = harness.cleanup_mountpoint();

        match (case_result, cleanup_result) {
            (Ok(()), Ok(())) => {}
            (Err(case_err), Ok(())) => {
                harness.log_message(format!(
                    "ERROR case_failed index={} error={:#}",
                    case_index, case_err
                ))?;
                return Err(case_err);
            }
            (Ok(()), Err(cleanup_err)) => return Err(cleanup_err),
            (Err(case_err), Err(cleanup_err)) => {
                return Err(case_err.context(format!("case cleanup also failed: {cleanup_err:#}")));
            }
        }
    }

    Ok(())
}

fn parse_active_member_devices(
    usage_text: &str,
    available_devices: &[String],
) -> Result<(
    Vec<String>,
    BTreeMap<String, u32>,
    BTreeMap<String, u64>,
    u64,
)> {
    let mut active_member_devices = Vec::new();
    let mut device_indices = BTreeMap::new();
    let mut device_sizes = BTreeMap::new();
    let mut used_bytes = None;
    let mut current_device = None;

    for line in usage_text.lines() {
        if let Some(rest) = line.strip_prefix("Used:") {
            used_bytes = Some(rest.trim().parse::<u64>().with_context(|| {
                format!("failed to parse used bytes from fs usage output: {line}")
            })?);
            continue;
        }

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
            current_device = Some(device.clone());
            active_member_devices.push(device);
            continue;
        }

        if let Some(rest) = line.trim_start().strip_prefix("capacity:") {
            let device = current_device
                .as_ref()
                .with_context(|| format!("saw capacity line before device header: {line}"))?;
            let size_bytes = rest
                .split_whitespace()
                .next()
                .with_context(|| {
                    format!("failed to parse device capacity from fs usage output: {line}")
                })?
                .parse::<u64>()
                .with_context(|| {
                    format!("failed to parse device capacity bytes from fs usage output: {line}")
                })?;
            device_sizes.insert(device.clone(), size_bytes);
        }
    }

    ensure!(
        !active_member_devices.is_empty(),
        "fs usage did not report any active member devices",
    );
    let used_bytes =
        used_bytes.with_context(|| "fs usage did not report total Used bytes".to_string())?;
    ensure_distinct_devices(&active_member_devices)?;
    ensure!(
        device_sizes.len() == active_member_devices.len(),
        "fs usage did not report capacities for every active device: devices={active_member_devices:?} sizes={device_sizes:?}",
    );
    sort_devices(&mut active_member_devices);
    Ok((
        active_member_devices,
        device_indices,
        device_sizes,
        used_bytes,
    ))
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

fn query_device_size_bytes(device: &str) -> Result<u64> {
    let output = Command::new("blockdev")
        .args(["--getsize64", device])
        .output()
        .with_context(|| format!("failed to query size for device {device}"))?;

    if !output.status.success() {
        bail!(
            "blockdev --getsize64 {} failed with status {}",
            device,
            render_status(output.status),
        );
    }

    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("device size output for {device} was not utf-8"))?;
    stdout
        .trim()
        .parse::<u64>()
        .with_context(|| format!("failed to parse device size for {device}: {stdout:?}"))
}

fn resize_candidate_target_bytes(physical_bytes: u64) -> Result<Vec<u64>> {
    const MIN_TARGET_BYTES: u64 = 256 * 1024 * 1024;
    const STEP_BYTES: u64 = 64 * 1024 * 1024;

    ensure!(
        physical_bytes >= MIN_TARGET_BYTES + STEP_BYTES,
        "device size {physical_bytes} is too small for resize target generation",
    );

    let mut targets = Vec::new();
    let mut target = MIN_TARGET_BYTES;
    while target < physical_bytes {
        targets.push(target);
        target += STEP_BYTES;
    }
    targets.push(physical_bytes);
    Ok(targets)
}

fn classify_resize_outcome(used_bytes: u64, target_bytes: u64) -> Option<bool> {
    const AMBIGUOUS_MARGIN_BYTES: u64 = 50 * 1024 * 1024;

    if target_bytes.saturating_add(AMBIGUOUS_MARGIN_BYTES) < used_bytes {
        Some(false)
    } else if target_bytes > used_bytes.saturating_add(AMBIGUOUS_MARGIN_BYTES) {
        Some(true)
    } else {
        None
    }
}

fn format_device_sizes(
    active_devices: &[String],
    current_device_bytes: &BTreeMap<String, u64>,
) -> String {
    active_devices
        .iter()
        .filter_map(|device| {
            current_device_bytes
                .get(device)
                .map(|bytes| format!("{device}={bytes}"))
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn format_operations(operations: &[Operation]) -> String {
    if operations.is_empty() {
        return "-".to_string();
    }

    operations
        .iter()
        .map(|op| format!("{op:?}"))
        .collect::<Vec<_>>()
        .join("|")
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
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

fn run_command_capture(
    log: &mut File,
    program: &str,
    args: &[&str],
) -> Result<std::process::Output> {
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
) -> Result<std::process::Output> {
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

fn append_output_file(log: &mut File, op_id: u64, stream: &str, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let contents = fs::read(path)
        .with_context(|| format!("failed to read operation output {}", path.display()))?;

    for line in String::from_utf8_lossy(&contents).lines() {
        writeln!(log, "ASYNC_{}_{} {}", stream, op_id, line)
            .with_context(|| format!("failed to append {} output", stream))?;
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
