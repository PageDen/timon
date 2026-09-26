use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use std::ffi::OsString;
use std::io::Read;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use timon::worker::slots::{SlotError, SlotMode, SlotPool};
use timon::worker::{WorkerLimits, WorkerSpec, run_worker};

/// Exit code when no worker slot is free (EX_TEMPFAIL).
const EXIT_NO_SLOT: u8 = 75;
const EXIT_WORKER_FAILED: u8 = 1;
const EXIT_TIMED_OUT: u8 = 124;
const EXIT_CANCELLED: u8 = 130;

#[derive(Parser)]
#[command(name = "timon", version, about = "Lead model steering bounded workers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Worker supervision.
    #[command(subcommand)]
    Worker(WorkerCommand),
}

#[derive(Subcommand)]
enum WorkerCommand {
    /// Run one worker. The task is read from stdin; the outcome is printed as JSON.
    Run(RunArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Private directory for stdout.log and stderr.log (created with mode 0700).
    #[arg(long)]
    output_dir: PathBuf,
    /// Wall-clock limit for the attempt, in seconds.
    #[arg(long)]
    deadline_secs: u64,
    /// Maximum task size in bytes.
    #[arg(long, default_value_t = timon::worker::DEFAULT_MAX_TASK_BYTES)]
    max_task_bytes: usize,
    /// Maximum bytes captured per output stream.
    #[arg(long, default_value_t = timon::worker::DEFAULT_MAX_OUTPUT_BYTES)]
    max_output_bytes: u64,
    /// Directory holding the concurrency slot files.
    #[arg(long, requires = "slots")]
    slot_dir: Option<PathBuf>,
    /// Number of concurrency slots.
    #[arg(long, requires = "slot_dir")]
    slots: Option<NonZeroU16>,
    /// Require slot files to exist instead of creating them (shared hosts).
    #[arg(long, requires = "slot_dir")]
    provisioned_slots: bool,
    /// Working directory for the worker.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Worker program followed by its arguments.
    #[arg(last = true, required = true, num_args = 1..)]
    command: Vec<OsString>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("timon: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<u8> {
    let cli = Cli::parse();
    match cli.command {
        Command::Worker(WorkerCommand::Run(args)) => worker_run(args),
    }
}

fn worker_run(args: RunArgs) -> Result<u8> {
    let task = read_task(args.max_task_bytes)?;

    let _lease = match (&args.slot_dir, args.slots) {
        (Some(dir), Some(limit)) => {
            let mode = if args.provisioned_slots {
                SlotMode::Provisioned
            } else {
                SlotMode::CreateMissing
            };
            match SlotPool::new(dir, limit, mode).try_acquire() {
                Ok(lease) => Some(lease),
                Err(SlotError::Full) => {
                    eprintln!("timon: all worker slots are in use; retry later");
                    return Ok(EXIT_NO_SLOT);
                }
                Err(error) => return Err(error.into()),
            }
        }
        _ => None,
    };

    let mut command = args.command.into_iter();
    let program = PathBuf::from(command.next().context("worker program is missing")?);
    let mut limits = WorkerLimits::with_deadline(Duration::from_secs(args.deadline_secs));
    limits.max_task_bytes = args.max_task_bytes;
    limits.max_output_bytes = args.max_output_bytes;
    let spec = WorkerSpec {
        program,
        args: command.collect(),
        cwd: args.cwd,
        env_set: Vec::new(),
        env_remove: Vec::new(),
        task,
        output_dir: args.output_dir,
        limits,
    };

    let runtime = tokio::runtime::Runtime::new().context("failed to start async runtime")?;
    let outcome = runtime.block_on(run_worker(&spec, shutdown_signal()))?;
    println!("{}", serde_json::to_string_pretty(&outcome)?);

    Ok(if outcome.cancelled {
        EXIT_CANCELLED
    } else if outcome.timed_out {
        EXIT_TIMED_OUT
    } else if outcome.succeeded() {
        0
    } else {
        EXIT_WORKER_FAILED
    })
}

fn read_task(limit: usize) -> Result<String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .context("failed to read task from stdin")?;
    if bytes.len() > limit {
        bail!("task exceeds {limit} bytes");
    }
    String::from_utf8(bytes).context("task is not valid UTF-8")
}

async fn shutdown_signal() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}
