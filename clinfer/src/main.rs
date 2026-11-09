// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! clinfer - DICOMweb inference sidecar.
//!
//! Usage:
//!   clinfer --config <path>     Run with this configuration file.
//!   clinfer --inspect <study>   Print the origin's non-empty study tags.
//!   clinfer --version           Print version and exit.
//!   clinfer --help              Print this help.

#![forbid(unsafe_code)]

use clinfer::dicomweb::Dicomweb;
use clinfer::http::Client;
use clinfer::notify;
use clinfer::receiver::{self, Counters};
use clinfer::reload::Reloader;
use clinfer::worker::{self, Shared};
use std::env;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, SystemTime};

const HELP_TEXT: &str = "\
clinfer - DICOMweb inference sidecar.

USAGE:
    clinfer [--config <path>]
    clinfer --inspect <study-uid> [--config <path>]

OPTIONS:
    --config <path>      Configuration file (default: ./clinfer.conf).
    --inspect <study>    Print the non-empty tags the origin holds for one
                         study, its series and its instances.
    --version            Print version information and exit.
    --help               Print this help message and exit.

The sidecar claims UPS workitems from a DICOMweb origin, runs a model and
stores the result back as an ordinary DICOM object. It listens on the
address in [server] listen for pushed workitems, and serves /health,
/metrics and /version there.
";

/// How often the main loop wakes to look at the signal flags.
///
/// The reload cadence is NOT this: `Reloader::poll` enforces its own
/// `CHECK_INTERVAL` on the file's modification time, so looking often costs
/// one timestamp comparison. 200 ms is the shutdown-reaction granularity an
/// operator perceives; a supervisor that waits half a minute to acknowledge
/// SIGTERM looks wedged.
const SIGNAL_TICK: Duration = Duration::from_millis(200);

/// What the command line asked for.
enum Command {
    Run(PathBuf),
    Inspect { study: String, config: PathBuf },
    Version,
    Help,
}

fn main() {
    let command = match parse_args(env::args().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("clinfer: {message}");
            eprintln!("Try 'clinfer --help' for more information.");
            process::exit(2);
        }
    };

    let outcome = match command {
        Command::Version => {
            println!("clinfer {}", clinfer::VERSION);
            Ok(())
        }
        Command::Help => {
            print!("{HELP_TEXT}");
            Ok(())
        }
        Command::Inspect { study, config } => inspect(&config, &study),
        Command::Run(path) => run(&path),
    };

    if let Err(err) = outcome {
        eprintln!("clinfer: {err}");
        process::exit(1);
    }
}

/// Parse the argument list. An empty list means the default file.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut config: Option<PathBuf> = None;
    let mut inspect: Option<String> = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" | "-V" => return Ok(Command::Version),
            "--help" | "-h" => return Ok(Command::Help),
            "--config" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--config needs a path".to_string())?;
                if config.replace(PathBuf::from(value)).is_some() {
                    return Err("--config given more than once".to_string());
                }
            }
            "--inspect" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--inspect needs a study UID".to_string())?;
                if inspect.replace(value).is_some() {
                    return Err("--inspect given more than once".to_string());
                }
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option '{other}'"));
            }
            other => {
                // A bare path is accepted as the configuration file, which
                // is how the other sidecar binaries in this repository are
                // invoked.
                if config.replace(PathBuf::from(other)).is_some() {
                    return Err("more than one configuration file given".to_string());
                }
            }
        }
    }
    let config = config.unwrap_or_else(|| PathBuf::from(clinfer::conf::DEFAULT_CONFIG_FILE));
    Ok(match inspect {
        Some(study) => Command::Inspect { study, config },
        None => Command::Run(config),
    })
}

/// Build the origin client from the configured URL and identity.
fn origin(conf: &clinfer::conf::Conf) -> Result<Dicomweb, Box<dyn std::error::Error>> {
    let client = Client::new(&conf.server.base_url, &conf.server.identity_user)?;
    // The station AE title is the Requesting AE of the events this sidecar
    // originates; it goes onto every Change State request as the optional
    // `requester` parameter (PS3.18 11.7).
    Ok(Dicomweb::new(client).with_requester(conf.server.station_aet.clone()))
}

/// The two shutdown flags and the listener thread that owns them.
type ShutdownSignals = (
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    std::thread::JoinHandle<()>,
);

/// Register the two-phase shutdown flags and keep the listener alive.
///
/// A dedicated thread owns a signal_hook iterator: the first TERM or INT
/// sets `term` (graceful drain); any signal delivered after that sets
/// `force` (immediate exit). The previous version registered two
/// flag-handlers per signal, and signal_hook runs EVERY registered
/// handler on every delivery - so the first signal tripped `force` too
/// and the drain never ran.
#[cfg(not(windows))]
fn register_shutdown_flags() -> Result<ShutdownSignals, std::io::Error> {
    use signal_hook::iterator::Signals;
    let term = Arc::new(AtomicBool::new(false));
    let force = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let term_flag = Arc::clone(&term);
    let force_flag = Arc::clone(&force);
    let handle = std::thread::spawn(move || {
        let mut signals =
            match Signals::new([signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT]) {
                Ok(signals) => signals,
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
        if ready_tx.send(Ok(())).is_err() {
            return;
        }
        for _signal in signals.forever() {
            if term_flag.swap(true, Ordering::Relaxed) {
                force_flag.store(true, Ordering::Relaxed);
            }
        }
    });
    ready_rx
        .recv()
        .map_err(|_| std::io::Error::other("the signal thread died before installing"))??;
    Ok((term, force, handle))
}

/// Windows build of the same entry point: signal_hook's iterator is not
/// available there, and clinfer runs as a Linux service in production,
/// so dev builds get no two-phase signal shutdown (Ctrl+C just kills the
/// process). The tuple shape stays identical so `run` is untouched.
#[cfg(windows)]
fn register_shutdown_flags() -> Result<ShutdownSignals, std::io::Error> {
    let term = Arc::new(AtomicBool::new(false));
    let force = Arc::new(AtomicBool::new(false));
    let handle = std::thread::spawn(|| {});
    Ok((term, force, handle))
}

/// Load the configuration, report it, and serve until the worker stops.
fn run(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut reloader = Reloader::new(path)?;
    let conf = reloader.current().clone();
    let counters = Arc::new(Counters::default());

    // Claimed before anything is claimed FROM the origin: two sidecars
    // sharing a configuration would both try to take the same workitems,
    // and the second one would discover it one study at a time.
    //
    // Held as a guard so an EARLY error path (a failed bind, a failed
    // origin) releases the file; the forced-exit paths below release it
    // explicitly because `process::exit` does not run destructors.
    let pid_guard = clinfer::pidfile::PidfileGuard::new(path)?;

    // A signal handler can set a flag and nothing else, so the shutdown is
    // driven from the main loop: the FIRST signal starts the drain, any
    // signal after that is the operator saying they will not wait.
    let (term, force, _signal_thread) = register_shutdown_flags()?;

    // The receiver's address is still validated, but for a weaker reason
    // than it once had: nothing external is asked to dial it any more, so a
    // wildcard bind is no longer a correctness problem. It is refused
    // because an operational endpoint (health, metrics) that answers on
    // every interface of a clinical host is a larger surface than the
    // deployment needs, and the address is worth stating explicitly.
    let (host, _port) = clinfer::conf::parse_listen(&conf.server.listen)
        .ok_or_else(|| format!("server.listen '{}' is not host:port", conf.server.listen))?;
    if host == "0.0.0.0" || host == "::" {
        return Err(
            "server.listen must not be a wildcard: the operational endpoints should be bound to one address".into(),
        );
    }

    // Bind BEFORE anything is announced or subscribed. A port conflict
    // found later would leave a process reporting a listen address it does
    // not hold.
    let listener = std::net::TcpListener::bind(&conf.server.listen)
        .map_err(|err| format!("cannot listen on {}: {err}", conf.server.listen))?;

    report(&conf, path);

    let origin = Arc::new(origin(&conf)?);
    check_capabilities(&origin, &conf.server.base_url);

    let (policy, unparsed) = receiver::Policy::new(
        &conf.server.trusted_proxies,
        conf.server.require_proxy_identity,
    );
    for entry in &unparsed {
        eprintln!(
            "clinfer: WARNING: trusted_proxies entry '{entry}' is not an IP literal and is ignored"
        );
    }

    let (sender, queue) = sync_channel(receiver::QUEUE_DEPTH);
    // A second sender for the notification channel. Both feed the one queue
    // the worker drains, so a claim is triggered by exactly one mechanism no
    // matter how the work was announced. The clone is taken before the
    // original moves into the receiver, which keeps the listener's own
    // sender (if it ever needs one again) and the notifier's independent.
    let notify_sender = sender.clone();
    let receiver_shutdown = Arc::new(AtomicBool::new(false));
    // `GET /processors` answers from the SAME configuration the worker runs
    // from, read through the shared cell at request time. That is what keeps
    // the menu honest: a configuration reloaded under a running sidecar
    // changes both what the worker executes and what the menu lists, so the
    // two cannot drift apart.
    //
    // ONE cell, created here and handed to both. A second `Shared` built later
    // from a fresh read of the reloader looks identical at startup and is a
    // different `Arc` from then on: the worker would swap its copy on reload
    // while the receiver kept serving the menu from its own frozen one.
    let processors = Arc::new(Shared::new(conf.clone()));
    spawn_receiver(
        listener,
        policy,
        Arc::clone(&counters),
        Arc::clone(&processors),
        sender,
        Arc::clone(&receiver_shutdown),
    );

    // One thread per configured processor, plus the ceiling they run under.
    // Created BEFORE the startup catch-up, which dispatches through the same
    // channels: a thread set that appeared later would make the catch-up run
    // inline and the design's single-claimer property would hold only after
    // startup.
    let models = worker::ModelThreads::spawn(&conf, Arc::clone(&origin), Arc::clone(&counters));
    let channels = models.topology();

    // Startup catch-up runs before the coordinator takes pushes, so work
    // that accumulated while the sidecar was down is not waiting behind a
    // push that will never come.
    sweep_workdirs(&conf);
    let tallies = worker::catch_up_with(&origin, &conf, &counters, &channels);
    println!(
        "catch-up:  {} completed, {} canceled, {} conflict, {} failed, {} left alone, {} skipped",
        tallies.completed,
        tallies.canceled,
        tallies.conflicts,
        tallies.failed,
        tallies.left_alone,
        tallies.skipped
    );

    // The same cell the receiver answers from - see the note above.
    let shared = Arc::clone(&processors);
    let (worker_thread, worker_shutdown) = worker::spawn_with(
        Arc::clone(&origin),
        Arc::clone(&shared),
        queue,
        Arc::clone(&counters),
        channels.clone(),
    );

    // Subscribe WITHOUT a notification URI. The subscription is what puts
    // this performer's step label on the origin's mailing list; the delivery
    // path is the WebSocket Notification Connection opened below, so no
    // inbound route into clinfer is needed and none is advertised.
    //
    // Still a courtesy in the sense that a failure here is a warning rather
    // than a refusal to run: the startup catch-up search finds everything
    // that is already scheduled, and the connection is opened regardless.
    //
    // ORDER IS LOAD-BEARING: the server gates channel delivery on a
    // subscription existing for the requester, so connecting first would
    // yield an open, permanently silent channel.
    let subscriber = subscription_name(&conf);
    // The performer's OWN filter: one matching key, the coding scheme, in the
    // query form PS3.18 11.10.1.1/8.3.4.1 prescribes. A subscription cannot
    // repeat an attribute (8.3.4.1), so the scheme - which every code this
    // deployment mints shares - is the legal filter that removes foreign
    // workitems while the per-code routing stays in the rules.
    let filter = clinfer::dicomweb::WorklistFilter::own_scheme();
    match origin.subscribe(&subscriber, None, Some(&filter)) {
        Ok(()) => {
            println!(
                "subscribed: {subscriber} (scheme {}; no endpoint, notifications arrive on \
                 the channel)",
                clinfer::dicomweb::LOCAL_CODING_SCHEME
            )
        }
        Err(err) => {
            eprintln!("clinfer: WARNING: subscribe failed: {err}");
            eprintln!(
                "clinfer: the origin will not filter for this performer; only the startup search runs"
            );
        }
    }

    // The notification channel: its own thread, because the read loop blocks
    // on the socket and the main loop owns the reload cadence and the
    // shutdown. It shares the SAME queue the worker drains, so a claim is
    // triggered by exactly one mechanism regardless of how it was announced.
    let notify_shutdown = Arc::new(AtomicBool::new(false));
    let notify_thread = spawn_notifier(
        &origin,
        &conf,
        &subscriber,
        notify_sender,
        Arc::clone(&counters),
        Arc::clone(&notify_shutdown),
    );
    println!(
        "reloading {} every {}s",
        path.display(),
        clinfer::reload::CHECK_INTERVAL.as_secs()
    );

    // The main loop owns the reload cadence. A configuration that fails to
    // parse keeps the running one, and the worker adopts a replacement on
    // its next workitem rather than mid-workitem.
    //
    // It also owns the shutdown. A sidecar killed mid-claim leaves its UPS
    // workitem IN PROGRESS, and the restarted sidecar can only report a
    // claim conflict: the study stays unfinished until an operator resets
    // the workitem. Draining instead - stop taking pushes, stop claiming,
    // finish what is held - removes the window rather than explaining it.
    loop {
        // The tick bounds how fast a SIGNAL is noticed, and the reload
        // cadence is not set here: `Reloader::poll` throttles itself on
        // `CHECK_INTERVAL`, so polling it often costs a timestamp compare
        // and nothing else. Sleeping the reload interval instead meant a
        // SIGTERM waited up to 30 s for the loop to wake, and the operator's
        // second signal waited another 30 s behind it.
        std::thread::sleep(SIGNAL_TICK);

        if force.load(Ordering::Relaxed) {
            eprintln!("clinfer: second signal; exiting now");
            clinfer::pidfile::release(pid_guard.path());
            process::exit(1);
        }
        if term.load(Ordering::Relaxed) {
            // The bound the operator actually has: the largest per-run
            // budget any processor declared. A workitem holds at most one
            // such run, so this is how long the drain can take.
            let budget = shared
                .current()
                .models
                .iter()
                .map(|model| model.timeout_secs)
                .max()
                .unwrap_or(0);
            println!(
                "signal:    shutting down; refusing new work and finishing the current one \
                 (up to {budget}s)"
            );
            worker_shutdown.store(true, Ordering::Relaxed);
            receiver_shutdown.store(true, Ordering::Relaxed);
            notify_shutdown.store(true, Ordering::Relaxed);
            // Waiting for the notifier BEFORE the worker is deliberate: the
            // notifier is what feeds the queue, so a notifier still running
            // could enqueue a UID after the worker had been declared
            // finished. It exits within one read timeout plus one backoff
            // slice, both of which are sub-second, so this does not extend
            // the drain the operator was told to expect.
            let _ = notify_thread.join();
            while !worker_thread.is_finished() {
                if force.load(Ordering::Relaxed) {
                    eprintln!("clinfer: second signal; exiting with work in flight");
                    clinfer::pidfile::release(pid_guard.path());
                    process::exit(1);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // The coordinator is done, so its sender clone is gone. Dropping
            // the model threads releases the other one, which is what wakes
            // a thread parked in `recv_timeout`, and joining them bounds the
            // exit: their children already finished (the coordinator drained
            // its workitem), so this is a wait for thread teardown.
            let model_joins = models.join();
            for handle in model_joins {
                let _ = handle.join();
            }
            println!("signal:    stopped cleanly");
            return Ok(());
        }

        match reloader.poll(SystemTime::now()) {
            Ok(Some(generation)) => {
                println!("reloaded {} (generation {generation})", path.display());
                let reloaded = reloader.current().clone();
                // The ceiling follows the reload; the THREAD SET does not.
                // A raise admits waiting threads at once, a cut applies at
                // the next acquire, and a run already in flight is never
                // interrupted - the knob is a gate, not a kill switch.
                models.semaphore().set_capacity(reloaded.workers());
                shared.replace(reloaded);
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("clinfer: WARNING: reload failed, keeping the running config: {err}");
            }
        }
        if worker_thread.is_finished() {
            eprintln!("clinfer: worker stopped; shutting down");
            worker_shutdown.store(true, Ordering::Relaxed);
            receiver_shutdown.store(true, Ordering::Relaxed);
            notify_shutdown.store(true, Ordering::Relaxed);
            return Ok(());
        }
    }
}

/// Spawn the inbound listener on an already-bound socket.
fn spawn_receiver(
    listener: std::net::TcpListener,
    policy: receiver::Policy,
    counters: Arc<Counters>,
    processors: Arc<Shared>,
    sender: std::sync::mpsc::SyncSender<String>,
    shutdown: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        if let Err(err) = receiver::serve_bound(
            listener,
            policy,
            counters,
            processors,
            receiver::deliver_channel(sender),
            shutdown,
        ) {
            // The socket was bound, so reaching here means the accept loop
            // itself failed. Without it the catch-up search still finds
            // work, but pushes are lost, which is worth a loud line.
            eprintln!("clinfer: receiver stopped: {err}");
        }
    });
}

/// Remove work directories left behind by abandoned runs.
///
/// At startup, before any work is claimed: a sidecar that is up is busy,
/// and a sidecar that restarts is exactly when nobody is using the
/// directories its predecessor left. Only directories older than the
/// threshold go; a FAILED run's directory is the forensic artefact an
/// operator wants, so it is the AGE that decides, not the outcome - a
/// failure looked at within the day is still there.
fn sweep_workdirs(conf: &clinfer::conf::Conf) {
    let root = clinfer::abi::workdir::root(&conf.engine.cache_dir);
    let outcome = clinfer::abi::workdir::sweep(&root, SystemTime::now());
    let mut removed = 0usize;
    let mut kept = 0usize;
    for (path, verdict) in &outcome {
        match verdict {
            clinfer::abi::workdir::Swept::Removed => {
                removed += 1;
                println!("sweep:     removed {}", path.display());
            }
            clinfer::abi::workdir::Swept::Kept => kept += 1,
            clinfer::abi::workdir::Swept::Unknown => eprintln!(
                "clinfer: WARNING: workdir {} was kept: its age could not be established",
                path.display()
            ),
        }
    }
    if removed > 0 || kept > 0 {
        println!(
            "sweep:     {removed} workdir(s) removed, {kept} kept (threshold {}h)",
            clinfer::abi::workdir::SWEEP_AGE.as_secs() / 3600
        );
    }
}

/// Report the loaded configuration.
fn report(conf: &clinfer::conf::Conf, path: &Path) {
    println!("clinfer {} - {}", clinfer::VERSION, path.display());
    println!("origin:    {}", conf.server.base_url);
    println!(
        "listening: {} (identity '{}')",
        conf.server.listen, conf.server.identity_user
    );
    println!("cache:     {}", conf.engine.cache_dir.display());
    // BOTH values, so an operator who set the knob sees what it resolved to
    // and does not read a cap as a broken setting. The number that matters
    // is the second one: it is how many model processes may run at once.
    match conf.engine.workers {
        Some(configured) => println!(
            "workers:   {configured} configured, capped to {} (models with scripts)",
            conf.workers()
        ),
        None => println!(
            "workers:   {} (every model with a script; set engine.workers to lower it)",
            conf.workers()
        ),
    }
    println!("rules (file order; every match runs):");
    for rule in &conf.rules {
        println!(
            "  [{}] modality {} -> model '{}' -> {}{}",
            rule.index,
            rule.r#match.modality.join("/"),
            rule.model,
            rule.result.kind,
            if rule.result.profile_version == 1 {
                String::new()
            } else {
                format!(" (profile {})", rule.result.profile_version)
            }
        );
    }
    println!("on no match: {}", conf.default_rule.as_str());
}

/// Check the origin's capabilities document and warn about what is missing.
///
/// A warning rather than a refusal: an origin that predates the worklist
/// rows still accepts the calls, and a sidecar that will not start is
/// worse than one that says exactly what it noticed.
fn check_capabilities(origin: &Dicomweb, base_url: &str) {
    match origin.capabilities() {
        Ok(document) => {
            let (workitems, subscribers) =
                clinfer::dicomweb::capabilities_has_workitems_and_subscribers(&document);
            if !workitems {
                eprintln!(
                    "clinfer: WARNING: {base_url} does not announce a workitems resource; \
                     the startup search may find nothing"
                );
            }
            if !subscribers {
                eprintln!(
                    "clinfer: WARNING: {base_url} does not announce a subscribers resource; \
                     pushed workitems will not arrive, the startup search is the only path"
                );
            }
        }
        Err(err) => {
            eprintln!("clinfer: WARNING: capabilities check failed: {err}");
        }
    }
}

/// Spawn the notification-channel thread.
///
/// The thread owns the reconnect loop. `redeliver` re-runs the catch-up
/// search after a reconnect, because a channel that was down for a while
/// missed an unknown number of Event Reports and the search is the only
/// thing that can say what is still SCHEDULED.
///
/// It ENQUEUES what the search found rather than processing it, so the
/// coordinator stays the only claimer. The origin is passed by value as a
/// cloned `Arc` because the loop owns the channel for the process's
/// lifetime; the redeliver closure takes its own clone for the same reason.
fn spawn_notifier(
    origin: &Arc<Dicomweb>,
    conf: &clinfer::conf::Conf,
    subscriber: &str,
    queue: std::sync::mpsc::SyncSender<String>,
    counters: Arc<Counters>,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    // The channel opens a raw TCP connection, so it needs the same host,
    // port and base path the HTTP client uses - taken from the client
    // rather than re-parsed, so the two cannot disagree.
    let (host, port, base_path) = origin.client().target();
    let wiring = notify::Wiring {
        host,
        port,
        base_path,
        requester: subscriber.to_string(),
        identity_user: conf.server.identity_user.clone(),
        poll_interval_secs: conf.server.poll_interval_secs,
    };

    let redeliver =
        worker::redeliver_into(Arc::clone(origin), Arc::new(conf.clone()), queue.clone());

    // Re-subscribe before EVERY connect, not only at startup. The origin
    // keeps subscriptions in process memory, so an origin that restarted
    // under a running sidecar has forgotten this performer while the
    // channel reconnects without complaint - the channel would be open and
    // permanently silent. Idempotent on the origin, so repeating the
    // startup call is free.
    //
    // ORDER IS LOAD-BEARING, exactly as at startup: the server gates
    // channel delivery on a subscription existing for the requester.
    let subscriber = subscriber.to_string();
    let resubscribe_origin = Arc::clone(origin);
    let subscribe = move || {
        // The SAME filter as startup: a reconnect that registered unfiltered
        // would widen the push stream behind the operator's back.
        let filter = clinfer::dicomweb::WorklistFilter::own_scheme();
        resubscribe_origin
            .subscribe(&subscriber, None, Some(&filter))
            .map_err(|err| err.to_string())
    };

    std::thread::spawn(move || {
        notify::run_with(&wiring, queue, counters, shutdown, redeliver, subscribe);
    })
}

/// The subscriber name: an AE-Title-shaped token, because the origin's own
/// subscribe route validates it as one.
#[must_use]
fn subscription_name(conf: &clinfer::conf::Conf) -> String {
    let cleaned: String = conf
        .server
        .identity_user
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .collect();
    if cleaned.is_empty() {
        "CLINFER".to_string()
    } else {
        cleaned.to_ascii_uppercase()
    }
}

/// `--inspect`: print what the origin holds.
fn inspect(config: &Path, study: &str) -> Result<(), Box<dyn std::error::Error>> {
    let conf = clinfer::conf::load(config)?;
    let origin = origin(&conf)?;
    print!("{}", clinfer::inspect::inspect(&origin, study)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|arg| (*arg).to_string()))
    }

    fn example() -> clinfer::conf::Conf {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("clinfer.conf.example.en");
        clinfer::conf::load(&path).expect("parses")
    }

    /// Wait until a flag reaches a value; signal delivery is asynchronous
    /// (signal_hook dispatches handlers on its own thread).
    #[cfg(not(windows))]
    fn wait_until(flag: &AtomicBool, want: bool) {
        for _ in 0..10_000 {
            if flag.load(Ordering::Relaxed) == want {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("the shutdown flag did not reach {want}");
    }

    #[cfg(not(windows))]
    #[test]
    fn the_first_signal_starts_the_drain_and_only_a_second_forces_exit() {
        let (term, force, _thread) = register_shutdown_flags().expect("register");
        assert!(!term.load(Ordering::Relaxed));
        assert!(!force.load(Ordering::Relaxed));

        signal_hook::low_level::raise(signal_hook::consts::SIGTERM).expect("raise");
        wait_until(&term, true);
        assert!(
            !force.load(Ordering::Relaxed),
            "one signal must not trip force"
        );

        signal_hook::low_level::raise(signal_hook::consts::SIGINT).expect("raise");
        wait_until(&force, true);
    }

    #[test]
    fn the_default_configuration_is_the_working_directory() {
        match parse(&[]).expect("default") {
            Command::Run(path) => assert_eq!(path, PathBuf::from("clinfer.conf")),
            _ => panic!("expected a run command"),
        }
    }

    #[test]
    fn an_early_bind_failure_releases_the_pidfile() {
        // Occupy a real port, point a temp config at it, and let `run`
        // fail at the bind - the EARLIEST error after the pidfile claim.
        // The guard must remove the file; without it the next start has to
        // fall back on the stale-pid rule for a pid that was never live.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
        let occupied = probe.local_addr().expect("addr");
        let dir = std::env::temp_dir().join(format!("clinfer-main-bind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let conf_path = dir.join("clinfer.conf");
        let example_text = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("clinfer.conf.example.en"),
        )
        .expect("example");
        let rewritten = example_text.replace(&example().server.listen, &occupied.to_string());
        std::fs::write(&conf_path, rewritten).expect("write");

        let err = run(&conf_path).expect_err("the occupied port must fail the run");
        assert!(
            err.to_string().contains("cannot listen"),
            "unexpected error: {err}"
        );
        assert!(
            !clinfer::pidfile::path_for(&conf_path).exists(),
            "the pidfile must be released on the early bind failure"
        );

        drop(probe);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_takes_a_path() {
        match parse(&["--config", "/etc/clinfer.conf"]).expect("explicit") {
            Command::Run(path) => assert_eq!(path, PathBuf::from("/etc/clinfer.conf")),
            _ => panic!("expected a run command"),
        }
    }

    #[test]
    fn a_bare_path_is_the_configuration_file() {
        match parse(&["./site.conf"]).expect("bare") {
            Command::Run(path) => assert_eq!(path, PathBuf::from("./site.conf")),
            _ => panic!("expected a run command"),
        }
    }

    #[test]
    fn inspect_takes_a_study_and_uses_the_default_config() {
        match parse(&["--inspect", "1.2.3"]).expect("inspect") {
            Command::Inspect { study, config } => {
                assert_eq!(study, "1.2.3");
                assert_eq!(config, PathBuf::from("clinfer.conf"));
            }
            _ => panic!("expected an inspect command"),
        }
    }

    #[test]
    fn inspect_accepts_an_explicit_config_in_either_order() {
        for args in [
            ["--inspect", "1.2.3", "--config", "site.conf"],
            ["--config", "site.conf", "--inspect", "1.2.3"],
        ] {
            match parse(&args).expect("inspect") {
                Command::Inspect { study, config } => {
                    assert_eq!(study, "1.2.3");
                    assert_eq!(config, PathBuf::from("site.conf"));
                }
                _ => panic!("expected an inspect command"),
            }
        }
    }

    #[test]
    fn version_and_help_short_circuit_the_rest() {
        assert!(matches!(
            parse(&["--version"]).expect("version"),
            Command::Version
        ));
        assert!(matches!(
            parse(&["--help", "--config", "x"]).expect("help"),
            Command::Help
        ));
    }

    #[test]
    fn bad_arguments_are_reported_rather_than_ignored() {
        assert!(parse(&["--config"]).is_err());
        assert!(parse(&["--config", "a", "--config", "b"]).is_err());
        assert!(parse(&["--inspect"]).is_err());
        assert!(parse(&["--inspect", "a", "--inspect", "b"]).is_err());
        assert!(parse(&["a", "b"]).is_err());
        assert!(parse(&["--nope"]).is_err());
    }

    #[test]
    fn a_subscriber_name_is_an_ae_title_shaped_token() {
        // The origin's subscribe route validates the name as an AE title,
        // so it must survive a configuration value that is not one.
        assert_eq!(subscription_name(&example()), "CLINFER");

        let mut odd = example();
        odd.server.identity_user = "clin fer/1".to_string();
        assert_eq!(subscription_name(&odd), "CLINFER1");

        let mut empty = example();
        empty.server.identity_user = "///".to_string();
        assert_eq!(subscription_name(&empty), "CLINFER");
    }
}
