// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! The one spawn seam: run a processor, supervise it, read what it produced.
//!
//! Every external processor is started from this module and nowhere else.
//! That is a deliberate constraint rather than tidiness: the supervision
//! rules below (process-tree kill, reaping, environment hygiene, timeout)
//! are what make a crashed or runaway script a bounded event instead of a
//! leaked process, and a second spawn site would be a second place to get
//! them wrong. A future resident mode replaces the transport INSIDE this
//! module and changes no caller.
//!
//! # The success rule
//!
//! Success is **exit 0 AND a schema-valid `result.json` AND geometry that
//! matches the input**. Never the exit code alone:
//!
//! - a non-zero exit is a failure even when a valid `result.json` is on
//!   disk - the script declared failure, and honouring its output anyway
//!   would persist a result the author said was wrong;
//! - a timeout or a kill is a failure even when `result.json` appeared -
//!   the file may be half written, and the resident mode contemplated for
//!   later needs the same rule so a "the answer arrived" message can never
//!   be treated as the answer itself.
//!
//! # Blocking I/O, no async runtime
//!
//! A child process boundary is blocking by nature. This crate has no async
//! runtime and will not grow one for this: the child is a
//! `std::process::Command` with piped stdio, its stderr is drained by a
//! plain thread, and the timeout is a `try_wait` poll loop because std has
//! no `wait_timeout`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use super::schema;

/// How a run ended, in the terms the caller needs to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 0 and a `result.json` this build could read.
    Success,
    /// The process exited with a status other than zero, whether or not it
    /// left a `result.json` behind.
    ExitFailure(i32),
    /// The process was still running when its budget ran out and had to be
    /// killed.
    TimedOut,
    /// The process could not be started at all: a missing interpreter or a
    /// missing script.
    NotStarted(String),
    /// The process exited zero but left no readable `result.json`.
    NoResult(String),
}

/// What one processor run produced, for the caller to persist.
#[derive(Debug)]
pub struct RunReport {
    pub outcome: Outcome,
    /// The parsed result, present only when `outcome` is
    /// [`Outcome::Success`].
    pub result: Option<schema::ResultFile>,
    /// Wall-clock seconds the child was alive.
    pub seconds: f64,
    /// Whether the process tree had to be killed (timeout or shutdown).
    pub killed: bool,
}

impl RunReport {
    /// The phrase that goes into (0074,1238) Reason For Cancellation.
    ///
    /// Fixed clinical-safe wording, never the script's stderr: stderr is
    /// arbitrary text and would land in a DICOM attribute. The full stderr
    /// stays in the log, where an operator reads it.
    ///
    /// [Self::diagnosis] carries the detail for that log. The two are
    /// separate on purpose: this phrase is what a DICOM reader sees, so it
    /// stays SHORT AND STABLE, while the log has to say which of the
    /// several failures actually happened. Collapsing them was a real
    /// defect - a live run cancelled a study whose processor had exited 0
    /// and written a complete 28 MB structure set, because the result
    /// exceeded the model's `max_result_bytes`; the log said only "produced no
    /// result",
    /// which reads as a missing file and sent the operator looking for the
    /// wrong thing.
    #[must_use]
    pub fn cancel_reason(&self) -> Option<String> {
        match &self.outcome {
            Outcome::Success => None,
            Outcome::ExitFailure(code) => Some(format!("external processor failed (exit {code})")),
            Outcome::TimedOut => Some("external processor timed out".to_string()),
            Outcome::NotStarted(_) => Some("external processor could not be started".to_string()),
            Outcome::NoResult(_) => Some("external processor produced no result".to_string()),
        }
    }

    /// Why the run ended as it did, in full, for the LOG.
    ///
    /// `None` on success. Every other outcome carries its detail where one
    /// exists: the exit code, or - for [`Outcome::NoResult`] - whether the
    /// file was absent, unreadable, too large, or written but not parseable.
    /// Those four are different faults with different remedies, and an
    /// operator can only act on the one that happened.
    #[must_use]
    pub fn diagnosis(&self) -> Option<String> {
        match &self.outcome {
            Outcome::Success => None,
            Outcome::ExitFailure(code) => Some(format!("external processor failed (exit {code})")),
            Outcome::TimedOut => Some(format!(
                "external processor timed out after {:.1}s",
                self.seconds
            )),
            Outcome::NotStarted(detail) => {
                Some(format!("external processor could not be started: {detail}"))
            }
            Outcome::NoResult(detail) => Some(format!(
                "external processor left no usable result: {detail}"
            )),
        }
    }
}

/// Everything about one configured processor that the seam needs.
#[derive(Debug, Clone)]
pub struct Processor {
    /// The configuration key, used as the counters' label and in log lines.
    pub id: String,
    /// The interpreter, already defaulted to `python3` by the parser.
    pub interpreter: PathBuf,
    /// The program itself.
    pub script: PathBuf,
    /// Budget of one run, end to end.
    pub timeout_secs: u32,
    /// Ceiling on the `result.json` this processor may write, in bytes.
    ///
    /// Carried here rather than read from a constant, because the document's
    /// size is a property of what the model produces and the models in one
    /// deployment differ: a detector writes kilobytes while an outliner
    /// writes megabytes. A refusal therefore names the budget that actually
    /// applied instead of a build-wide number the operator never set.
    pub max_result_bytes: u64,
}

/// The most stderr clinfer keeps from one run, in bytes.
///
/// A chatty script must not be able to flood the journal, and a silent one
/// must not cost anything. What a site sees is the first bytes and a note
/// that the rest was dropped, which is enough to find the failure without
/// turning the log into the script's own output channel.
pub const MAX_STDERR_BYTES: usize = 64 * 1024;

/// The most stderr clinfer keeps on ONE line, in bytes.
///
/// A progress bar written with carriage returns is a single enormous line.
/// Truncating per line keeps the log readable and bounds one write.
pub const MAX_STDERR_LINE_BYTES: usize = 2 * 1024;

/// How often the watchdog looks at the child.
///
/// The poll interval, not the timeout: std has no `wait_timeout`, so the
/// watchdog loop samples. 50 ms is far below any processor's runtime
/// (Python startup is ~50-200 ms on its own) and costs nothing measurable.
const WATCHDOG_POLL: Duration = Duration::from_millis(50);

/// How long the seam waits for the stderr reader thread after the child is
/// gone, before detaching it.
///
/// Normally the reader finishes the instant the pipe closes, so this is
/// never reached. It exists for the case where a grandchild inherited the
/// pipe's write end and outlived what the tree-kill reached: the lines
/// already captured are complete, and waiting on a handle nobody will
/// close is not. Two seconds is far longer than any pipe drain and far
/// shorter than a study's budget.
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// A running processor and the kill obligation attached to it.
///
/// The guard exists so that EVERY early return - a panic in the caller, a
/// `?` down another path, a shutdown - still kills and reaps the child.
/// Without it a killed clinfer leaves an orphaned Python holding a GPU.
struct KillOnDrop {
    child: Child,
    /// The workdir the child was given, for the tree-kill message.
    workdir: PathBuf,
    /// Set once the child is known to be gone, so `drop` does not try to
    /// kill a reaped process (which on Windows can target a recycled pid).
    finished: bool,
}

impl KillOnDrop {
    /// Kill the child AND reap it.
    ///
    /// The reap is not optional: a killed child that is never waited for
    /// stays a zombie for the life of the sidecar, one per study.
    fn kill_and_reap(&mut self) {
        if self.finished {
            return;
        }
        kill_tree(&mut self.child, &self.workdir);
        // Reaping can only fail if the child was already reaped, which is
        // the state `finished` tracks; either way there is nothing further
        // to do about it here.
        let _ = self.child.wait();
        self.finished = true;
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

/// Kill the child and every process it started.
///
/// A Python script using multiprocessing, a dataloader or a worker pool has
/// children of its own. Killing only the direct child orphans them: they
/// keep the CPU and the GPU, the study is cancelled, and nothing in the log
/// says where the load went.
///
/// - Unix: the child was made a process-group leader at spawn
///   (`process_group(0)`), so its process group id equals its pid and a
///   negative pid addresses the whole group. `kill -9 -PGID` is the
///   standard tool for that, and using it keeps this crate free of `libc`
///   and of `unsafe`.
/// - Windows: `taskkill /T /F` walks the process tree. Job Objects would be
///   the other answer and are refused, because they need FFI and this crate
///   is `deny(unsafe_code)`.
///
/// Both paths then fall back to `Child::kill`, so the direct child dies even
/// if the tree-kill tool is missing from the image.
fn kill_tree(child: &mut Child, _workdir: &Path) {
    #[cfg(unix)]
    {
        // "-PGID": the leading minus makes the pid a process-group id. The
        // group was created at spawn, so the group id is the child's pid.
        let group = format!("-{}", child.id());
        let _ = Command::new("kill")
            .args(["-9", &group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        // `taskkill` is the documented Windows way to address a process
        // tree from a program without FFI. `/T` includes children, `/F`
        // forces termination.
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    // The direct child is killed through std as well: the tree-kill above
    // can fail (a missing `kill`/`taskkill`, a permission surprise), and
    // the child this function is responsible for must die regardless.
    let _ = child.kill();
}

/// Anything that stopped a run before its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalError {
    /// The workdir could not be prepared or read.
    Workdir(String),
    /// The script exited with something other than a plain status code
    /// (killed by a signal on Unix, for instance).
    NoStatus,
}
impl std::fmt::Display for ExternalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Workdir(message) => write!(f, "workdir: {message}"),
            Self::NoStatus => f.write_str("the processor ended without an exit status"),
        }
    }
}

impl std::error::Error for ExternalError {}

/// This run's handles on the per-processor gauges.
///
/// A guard, not a bag of fields: whatever ends the run - a clean exit, a
/// timeout, a kill, or a panic unwinding through the model thread - the
/// gauges go back to "no child running" and the peak survives as
/// `external_last_peak_rss_bytes`. That is the same discipline
/// [`KillOnDrop`] applies to the process itself, applied to the numbers
/// about it.
///
/// Every write is an ABSOLUTE value, so `finish` is idempotent: calling it
/// twice, or once from `Drop` after an explicit call, publishes the same
/// state.
pub struct ChildMetrics<'a> {
    counters: &'a super::super::receiver::Counters,
    id: String,
    /// The last peak this run observed, kept because the reap writes it
    /// after the child is gone.
    peak: u64,
    finished: bool,
}

impl<'a> ChildMetrics<'a> {
    /// A handle for a run that is about to be started.
    #[must_use]
    pub fn new(counters: &'a super::super::receiver::Counters, id: &str) -> Self {
        Self {
            counters,
            id: id.to_string(),
            peak: 0,
            finished: false,
        }
    }

    /// The child exists: publish its pid and mark it running.
    ///
    /// The pid is known exactly here and nowhere else, which is why the
    /// gauge is set at spawn rather than derived later.
    pub fn spawned(&mut self, pid: u32) {
        self.counters.external_pid.set(&self.id, u64::from(pid));
        self.counters.external_running.set(&self.id, 1);
    }

    /// Read the child and the sidecar once, on the watchdog's idle tick.
    ///
    /// A failed read publishes NOTHING: the previous values stay. That is
    /// the difference between "no reading this tick" and "no memory in
    /// use", and only the reap path is allowed to say the latter.
    pub fn sample(&mut self, pid: u32) {
        if let Some(stat) = crate::procstat::read(pid, None) {
            self.peak = self.peak.max(stat.peak_rss_bytes);
            self.counters
                .external_rss_bytes
                .set(&self.id, stat.rss_bytes);
            self.counters
                .external_peak_rss_bytes
                .set(&self.id, stat.peak_rss_bytes);
            self.counters
                .external_cpu_seconds_millis
                .set(&self.id, stat.cpu_nanos / 1_000_000);
        }
        if let Some(peak) = crate::procstat::read_self(None) {
            self.counters
                .clinfer_peak_rss_bytes
                .fetch_max(peak, Ordering::Relaxed);
        }
    }

    /// The child is gone: publish the run's peak and clear the live gauges.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        // The peak is remembered by the gauges themselves, so a sample
        // that arrived after the last tick is not lost here: `self.peak`
        // and the gauge were updated together.
        let peak = self
            .counters
            .external_peak_rss_bytes
            .get(&self.id)
            .max(self.peak);
        self.counters
            .external_last_peak_rss_bytes
            .set(&self.id, peak);
        self.counters.external_rss_bytes.set(&self.id, 0);
        self.counters.external_peak_rss_bytes.set(&self.id, 0);
        self.counters.external_cpu_seconds_millis.set(&self.id, 0);
        self.counters.external_pid.set(&self.id, 0);
        self.counters.external_running.set(&self.id, 0);
    }
}

impl Drop for ChildMetrics<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The workdir as the child receives it: guaranteed absolute.
///
/// The ABI promises an absolute path in argv[1] and as the cwd, but a
/// RELATIVE `cache_dir` in the configuration builds a relative workdir. A
/// script whose cwd IS the workdir would resolve `argv[1]/manifest.json`
/// against itself and miss - the failure the pack E2E caught.
fn absolute_workdir(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::{absolute_workdir, read_result};
    use crate::abi::external::Outcome;
    use std::path::Path;

    /// A relative workdir is anchored at the current directory before it
    /// reaches a child; an absolute one is handed through untouched.
    #[test]
    fn a_relative_workdir_is_anchored_at_the_current_directory() {
        let relative = Path::new("state/run-1");
        let anchored = absolute_workdir(relative);
        assert!(anchored.is_absolute(), "{anchored:?} is not absolute");
        assert!(anchored.ends_with(relative), "{anchored:?} lost the tail");

        // Absolute on every platform by construction (a Unix-style
        // `/state/run-1` is NOT absolute on Windows: no drive prefix).
        let fixed = std::env::current_dir().expect("cwd").join("state/run-1");
        assert_eq!(
            absolute_workdir(&fixed),
            fixed,
            "an absolute path is handed through untouched"
        );
    }

    /// An over-budget result is refused on its LENGTH, before it is read.
    ///
    /// The file is a hole: `set_len` gives it the length without giving it
    /// the content, so a read would return megabytes of zeros and the
    /// refusal would have to come from the parser. Only the length gate can
    /// produce a diagnosis that names the budget, which is what the
    /// assertion reads.
    #[test]
    fn an_oversized_result_is_refused_without_being_read() {
        let dir = std::env::temp_dir().join(format!(
            "clinfer-read-result-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("workdir");
        {
            let file = std::fs::File::create(dir.join(crate::abi::manifest::RESULT_FILE))
                .expect("result.json");
            file.set_len(64 * 1024 * 1024).expect("set_len");
        }

        let report = read_result(&dir, 1.0, false, 1024).expect("read_result");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            matches!(report.outcome, Outcome::NoResult(_)),
            "64 MiB is not a result under a 1 KiB budget: {:?}",
            report.outcome
        );
        let diagnosis = report.diagnosis().expect("a diagnosis");
        assert!(
            diagnosis.contains("max_result_bytes") && diagnosis.contains("1024"),
            "the refusal names the budget it applied, which only the length \
             gate can produce: {diagnosis}"
        );
        // The two refusals word this differently, and the difference is the
        // whole point: the length gate quotes the PATH it stat'ed, while a
        // refusal from the parser quotes the bare file name because by then
        // the bytes are already in memory.
        assert!(
            diagnosis.contains("result.json") && diagnosis.contains("clinfer-read-result"),
            "the refusal reports the path it measured, before reading it: {diagnosis}"
        );
    }
}

/// Run one processor over one prepared workdir.
///
/// `on_stderr` receives each captured line already prefixed and bounded, so
/// the caller decides where it goes (the log) and this module stays free of
/// logging policy.
pub fn run<F>(
    processor: &Processor,
    workdir: &Path,
    counters: &super::super::receiver::Counters,
    metrics: &mut ChildMetrics<'_>,
    mut on_stderr: F,
) -> Result<RunReport, ExternalError>
where
    F: FnMut(&str),
{
    counters.external_spawns.record(&processor.id);
    let started = Instant::now();

    // The contract (abi/README.md) promises an ABSOLUTE workdir in argv[1]
    // and as the child's cwd. A RELATIVE cache_dir builds a relative
    // workdir, and handing that to a script whose cwd is the workdir
    // itself makes every `argv[1]/manifest.json` read miss - the path
    // resolves workdir-relative. Absolutize here, at the one boundary
    // that hands the path to a foreign program.
    let absolute_workdir = absolute_workdir(workdir);

    let mut command = Command::new(&processor.interpreter);
    command
        .arg(&processor.script)
        .arg(&absolute_workdir)
        // cwd = the workdir: a script may write its own temporaries, and
        // naming them relative to the study it is processing is what makes
        // a workdir self-contained.
        .current_dir(&absolute_workdir)
        // stdin = null. A script that reads stdin would otherwise block
        // until the timeout, and "reads stdin" is a plausible thing for a
        // site's first draft to do.
        .stdin(Stdio::null())
        // stdout is DISCARDED, not piped. Nothing reads it, so a pipe here
        // fills after one buffer's worth of output and the child blocks on
        // its own write - a deadlock the timeout then reports as a hang. A
        // processor's own progress output belongs on stderr, which IS
        // drained (see `spawn_reader`).
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    apply_environment(&mut command);
    #[cfg(unix)]
    {
        // A new process group, so `kill_tree` can address the whole tree.
        // This is the stable std API - no libc, no unsafe.
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ok(RunReport {
                outcome: Outcome::NotStarted(error.to_string()),
                result: None,
                seconds: started.elapsed().as_secs_f64(),
                killed: false,
            });
        }
    };

    let mut guard = KillOnDrop {
        child,
        workdir: absolute_workdir.clone(),
        finished: false,
    };
    // The child exists from here on, so the gauges describe it: a pid and
    // "running" now, a reset on every path out of this function.
    metrics.spawned(guard.child.id());

    // stderr is drained on its own thread: a child that fills the pipe
    // buffer while nobody reads it blocks forever, and the timeout would
    // then report a hang that is really clinfer's own fault. stdout needs no
    // reader because it is not a pipe at all.
    let stderr = guard.child.stderr.take();
    let (lines, reader) = spawn_reader(stderr);

    let mut watchdog = Watchdog {
        child: &mut guard.child,
        finished: false,
        status: None,
    };
    let outcome = watchdog.wait(
        Duration::from_secs(u64::from(processor.timeout_secs)),
        metrics,
    );
    let exit = watchdog.status;
    guard.finished = watchdog.finished;

    // Kill BEFORE joining the reader. The kill is what closes the child's
    // end of the stderr pipe; joining first would block on a pipe that a
    // killed-but-not-yet-reaped child still holds open, which turns a
    // timeout into an unbounded wait - the exact failure the timeout
    // exists to prevent. `kill_and_reap` is a no-op once `finished` is
    // set, so the ordinary exits pay nothing for this.
    if matches!(outcome, WaitOutcome::TimedOut) {
        guard.kill_and_reap();
    }
    // And bound the join: a grandchild that inherited the pipe's write end
    // can outlive what the tree-kill reached, and a reader thread that will
    // not end must not be allowed to wedge the seam. The lines that did
    // arrive are still in the sink and are still delivered below, because
    // the sink is shared rather than channelled.
    let reader_finished = join_reader(reader, READER_JOIN_TIMEOUT);
    for line in lines.iter() {
        on_stderr(&line);
    }
    if !reader_finished {
        eprintln!(
            "external: {}: stderr reader did not finish within {} ms; \
             the lines captured so far are shown, the rest are lost",
            processor.id,
            READER_JOIN_TIMEOUT.as_millis()
        );
    }

    let seconds = started.elapsed().as_secs_f64();
    match outcome {
        WaitOutcome::Exited(0) => read_result(workdir, seconds, false, processor.max_result_bytes),
        WaitOutcome::Exited(code) => {
            counters.external_exit_failures.record(&processor.id);
            Ok(RunReport {
                outcome: Outcome::ExitFailure(code),
                result: None,
                seconds,
                killed: false,
            })
        }
        WaitOutcome::TimedOut => {
            // Already killed above, before the join; the counters are
            // recorded here so the diagnosis stays next to the outcome.
            counters.external_timeouts.record(&processor.id);
            counters.external_kills.record(&processor.id);
            Ok(RunReport {
                outcome: Outcome::TimedOut,
                result: None,
                seconds,
                killed: true,
            })
        }
        WaitOutcome::Signal => Ok(RunReport {
            outcome: Outcome::ExitFailure(exit.unwrap_or(-1)),
            result: None,
            seconds,
            killed: false,
        }),
    }
}

/// Wait for the stderr reader, but not forever.
///
/// Returns whether the reader actually finished. The thread is detached
/// when the budget runs out: it holds no state the caller needs (the lines
/// live in the shared sink), so letting it end on its own is strictly
/// better than blocking a worker thread on a pipe some grandchild is
/// holding open.
fn join_reader(reader: thread::JoinHandle<()>, budget: Duration) -> bool {
    let started = Instant::now();
    while !reader.is_finished() {
        if started.elapsed() >= budget {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = reader.join();
    true
}

/// The environment a processor runs in.
///
/// `env_clear` first: inheriting clinfer's whole environment leaks whatever
/// else the deployment set and makes a run behave differently on two hosts
/// that differ only in env. `PATH` is what lets `python3` resolve; and
/// `PYTHONUNBUFFERED=1` is what makes the captured stderr live rather than
/// buffered until exit, which is the difference between a log that shows
/// progress and one that shows nothing until the timeout.
fn apply_environment(command: &mut Command) {
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    // Windows resolves its Winsock providers and system DLLs under
    // SystemRoot: a child without it cannot `import asyncio` (the
    // `_overlapped` module fails with WSAEPROVIDERFAILEDINIT, 10106).
    // USERPROFILE (with the HOMEDRIVE/HOMEPATH fallback pair) is where a
    // site puts its pre-downloaded model weights (~/.torchxrayvision);
    // without it a producer re-downloads them or finds nothing. These
    // are host identity, not deployment state - pass them through like
    // PATH.
    #[cfg(windows)]
    {
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        if let Some(root) = std::env::var_os("WINDIR") {
            command.env("WINDIR", root);
        }
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            command.env("USERPROFILE", profile);
        }
        if let Some(drive) = std::env::var_os("HOMEDRIVE") {
            command.env("HOMEDRIVE", drive);
        }
        if let Some(path) = std::env::var_os("HOMEPATH") {
            command.env("HOMEPATH", path);
        }
    }
    command.env("PYTHONUNBUFFERED", "1");
    // A producer's stdout must not depend on the console codepage: on a
    // cp1251 host, torch's download bar ("\u2588") would otherwise kill
    // the child with a UnicodeEncodeError. Python reads this variable;
    // other interpreters ignore it.
    command.env("PYTHONIOENCODING", "utf-8");
}

/// Read stderr on its own thread, in bounded lines.
///
/// Returns the shared line buffer and the thread handle. The buffer is
/// shared rather than channelled so the caller can read what arrived even
/// when the thread is still draining a killed child.
fn spawn_reader(stderr: Option<std::process::ChildStderr>) -> (LineSink, thread::JoinHandle<()>) {
    let sink = LineSink::new();
    let handle = match stderr {
        Some(stderr) => {
            let sink = sink.clone();
            thread::spawn(move || drain(stderr, sink))
        }
        None => thread::spawn(|| {}),
    };
    (sink, handle)
}

/// A shared, bounded collection of captured stderr lines.
#[derive(Clone, Default)]
struct LineSink {
    inner: std::sync::Arc<std::sync::Mutex<LineSinkInner>>,
}

#[derive(Default)]
struct LineSinkInner {
    lines: Vec<String>,
    bytes: usize,
    dropped: u64,
}

/// The note appended to a line that had to be cut, so an operator knows
/// the line is incomplete rather than merely short.
const TRUNCATION_NOTE: &str = "... [line truncated]";

impl LineSink {
    fn new() -> Self {
        Self::default()
    }

    /// Append one line, subject to both limits.
    ///
    /// The line DELIVERED is at most [`MAX_STDERR_LINE_BYTES`]: the
    /// truncation note is counted inside the cap, not added on top of it,
    /// because a cap the result can exceed is not a cap.
    fn push(&self, line: &str) {
        let mut inner = self.inner.lock().expect("line sink lock");
        if inner.bytes >= MAX_STDERR_BYTES {
            inner.dropped += 1;
            return;
        }
        let capped = if line.len() > MAX_STDERR_LINE_BYTES {
            // The content is cut short enough that the note still fits, and
            // on a char boundary, so the log stays valid UTF-8 even when
            // the script wrote a multi-byte character across the cut.
            let room = MAX_STDERR_LINE_BYTES.saturating_sub(TRUNCATION_NOTE.len());
            let mut end = room.min(line.len());
            while end > 0 && !line.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}{TRUNCATION_NOTE}", &line[..end])
        } else {
            line.to_string()
        };
        inner.bytes += capped.len();
        inner.lines.push(capped);
    }

    fn iter(&self) -> Vec<String> {
        let inner = self.inner.lock().expect("line sink lock");
        let mut lines = inner.lines.clone();
        if inner.dropped > 0 {
            lines.push(format!(
                "[{} further line(s) dropped: the {}-byte stderr budget for this run is spent]",
                inner.dropped, MAX_STDERR_BYTES
            ));
        }
        lines
    }
}

/// Read a child's stderr into the sink, one bounded line at a time.
fn drain(mut stderr: std::process::ChildStderr, sink: LineSink) {
    // A fixed buffer reused across reads: a processor that logs a line per
    // frame must not cost an allocation per frame on clinfer's side.
    let mut buffer = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                for byte in &buffer[..count] {
                    if *byte == b'\n' {
                        // A partial line at the end of a killed child is
                        // still worth showing: it is often the line that
                        // says what it was doing when it died.
                        let line = String::from_utf8_lossy(&pending);
                        sink.push(line.trim_end_matches('\r').trim_end());
                        pending.clear();
                    } else if pending.len() < MAX_STDERR_LINE_BYTES * 2 {
                        // The cap here bounds the BUFFER, not the reported
                        // line: a runaway single line must not grow memory
                        // without limit while it arrives.
                        pending.push(*byte);
                    }
                }
            }
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        let line = String::from_utf8_lossy(&pending);
        sink.push(line.trim_end_matches('\r').trim_end());
    }
}

/// The watchdog's verdict on how the child ended.
enum WaitOutcome {
    /// A real exit status, as the OS reported it.
    Exited(i32),
    /// The budget ran out; the child must be killed.
    TimedOut,
    /// No status was available - killed by a signal, or already reaped.
    Signal,
}

/// The `try_wait` poll loop that std makes necessary.
struct Watchdog<'a> {
    child: &'a mut Child,
    finished: bool,
    status: Option<i32>,
}

impl<'a> Watchdog<'a> {
    /// Wait for the child, up to `budget`.
    ///
    /// Each idle iteration is also the metrics tick: the same 50 ms poll
    /// that keeps the watchdog cheap is the cheapest place there is to read
    /// the child's memory and CPU, and it costs nothing when no run is in
    /// flight because the loop does not exist then.
    fn wait(&mut self, budget: Duration, metrics: &mut ChildMetrics<'_>) -> WaitOutcome {
        let deadline = Instant::now() + budget;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.finished = true;
                    self.status = status.code();
                    return match status.code() {
                        Some(code) => WaitOutcome::Exited(code),
                        None => WaitOutcome::Signal,
                    };
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        return WaitOutcome::TimedOut;
                    }
                    metrics.sample(self.child.id());
                    thread::sleep(WATCHDOG_POLL);
                }
                // `try_wait` failing means the child is gone from the OS's
                // point of view without a status we can read: treat it as
                // a signal death rather than as a success.
                Err(_) => {
                    self.finished = true;
                    return WaitOutcome::Signal;
                }
            }
        }
    }
}

/// Read and validate the result file after a clean exit.
///
/// `max_bytes` is the model's own budget, passed through to the schema so a
/// size refusal quotes the number this processor was configured with.
fn read_result(
    workdir: &Path,
    seconds: f64,
    killed: bool,
    max_bytes: u64,
) -> Result<RunReport, ExternalError> {
    let path = workdir.join(super::manifest::RESULT_FILE);
    // Refuse on the FILE LENGTH before reading it. The child writes this
    // file, so its size is the child's decision: reading first and checking
    // afterwards made the budget a limit on what the sidecar would ACCEPT
    // while the sidecar itself allocated the whole document - a processor
    // that wrote a multi-gigabyte file was refused only after the memory was
    // spent, and the OOM kill that followed was not the refusal the budget
    // describes.
    //
    // A length that cannot be read is not a refusal: the metadata error is
    // reported through the open that follows, which names the path the same
    // way.
    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > max_bytes
    {
        return Ok(RunReport {
            outcome: Outcome::NoResult(format!(
                "{}: {} bytes, over the model's max_result_bytes limit of {max_bytes}",
                path.display(),
                meta.len()
            )),
            result: None,
            seconds,
            killed,
        });
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(RunReport {
                outcome: Outcome::NoResult(format!("{}: {error}", path.display())),
                result: None,
                seconds,
                killed,
            });
        }
    };
    match schema::parse(&bytes, max_bytes) {
        Ok(result) => Ok(RunReport {
            outcome: Outcome::Success,
            result: Some(result),
            seconds,
            killed,
        }),
        Err(error) => Ok(RunReport {
            outcome: Outcome::NoResult(format!("result.json: {error}")),
            result: None,
            seconds,
            killed,
        }),
    }
}

/// A receiver the caller can use when it has no use for stderr lines.
///
/// Kept so the tests and the forensics reader can call [`run`] without
/// inventing a closure, while the worker passes its own logger.
#[must_use]
pub fn discard_sink() -> Receiver<String> {
    let (_sender, receiver) = mpsc::channel();
    receiver
}
