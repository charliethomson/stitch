use std::path::PathBuf;

use clap::Parser;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use valuable::Valuable;

use crate::{
    env::find_binaries,
    execute::{ExecuteProgress, ExecuteProgressPayload, execute_plan},
    parse::{ParseError, parse_spec},
};

pub mod env;
pub mod execute;
pub mod ffmpeg;
pub mod ffprobe;
pub mod limits;
pub mod logging;
pub mod parse;
pub mod path;

/// ffmpeg wrapper to bulk stitch video files together based on a specification file
#[derive(Parser)]
#[command(
    version,
    author,
    about,
    long_about = None,
    help_template = "\
{name} ({version})
{author-with-newline}
{about-with-newline}
{usage-heading} {usage}

{all-args}"
)]
pub struct Args {
    /// Path to the specification file containing stitch instructions
    #[arg(value_name = "SPEC_FILE")]
    pub spec: PathBuf,

    /// Output directory for stitched video files (default: current directory)
    #[arg(short = 'o', long, value_name = "DIR", help_heading = "Directories")]
    pub target_dir: Option<PathBuf>,

    /// Input directory containing source video files (default: current directory)
    #[arg(short = 'i', long, value_name = "DIR", help_heading = "Directories")]
    pub sources_dir: Option<PathBuf>,

    /// Enable verbose logging (configure with RUST_LOG environment variable)
    #[arg(short, long)]
    pub verbose: bool,

    #[arg(env = "STITCH_BIN_FFMPEG", long, help_heading = "Binaries")]
    pub ffmpeg_path: Option<PathBuf>,

    #[arg(env = "STITCH_BIN_FFPROBE", long, help_heading = "Binaries")]
    pub ffprobe_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    logging::register_tracing_subscriber(!args.verbose);
    let cancellation_token = CancellationToken::new();

    libsignal::cancel_after_signal(cancellation_token.clone());

    let span = tracing::info_span!("main").entered();

    find_binaries(args.ffmpeg_path, args.ffprobe_path)?;

    let cwd = std::env::current_dir().expect(
        "Failed to get current directory, please pass an directories with --target-dir and --sources-dir",
    );

    let target_dir = args.target_dir.unwrap_or(cwd.clone());
    let sources_dir = args.sources_dir.unwrap_or(cwd.clone());

    if !target_dir.exists() {
        std::fs::create_dir_all(&target_dir).expect("Failed to create target directory");
    }

    let spec = match parse_spec(args.spec, target_dir, sources_dir) {
        Ok(spec) => spec,

        Err(e) => match &e {
            ParseError::Validation { errors } => {
                if !args.verbose {
                    eprintln!("Validation failed:");
                    for error in errors {
                        eprintln!("\t{error}")
                    }
                    eprintln!();
                }

                return Err(e.into());
            }
            _ => return Err(e.into()),
        },
    };

    let mut executions = JoinSet::new();
    let (tx, rx) = tokio::sync::mpsc::channel(100);

    for plan in spec {
        let tx = tx.clone();
        let tmp_root = path::run_tmp_root();
        executions.spawn(execute_plan(
            plan,
            tx,
            tmp_root,
            cancellation_token.child_token(),
        ));
    }

    let handle = tokio::spawn(monitor(rx, args.verbose));

    executions.join_next().await;

    // Drop the original sender so channel closes
    drop(tx);

    // Monitor will exit naturally when channel closes, just wait for it
    match handle.await {
        Ok(_) => { /* monitor closed normally */ }
        Err(join_error) => {
            tracing::error!(error =% join_error, error_context =? join_error,"Failed to join monitor thread")
        }
    }

    span.exit();

    Ok(())
}

async fn monitor(mut rx: tokio::sync::mpsc::Receiver<ExecuteProgress>, verbose: bool) {
    use crossterm::{
        ExecutableCommand, cursor,
        terminal::{Clear, ClearType},
    };
    use std::collections::HashMap;
    use std::io::{Write, stdout};
    use uuid::Uuid;

    struct ProcessState {
        name: String,
        progress_pct: f64,
        current_seconds: Option<f64>,
        total_seconds: Option<f64>,
        phase: Option<String>,
        warning: Option<String>,
        error: Option<String>,
        finished: bool,
        failed: bool,
    }

    fn render_progress_bar(pct: f64, width: usize) -> String {
        let filled = ((pct / 100.0) * width as f64) as usize;
        let empty = width.saturating_sub(filled);
        format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
    }

    fn render_compact(processes: &HashMap<Uuid, ProcessState>) -> String {
        let mut output = String::new();

        for process in processes.values() {
            // Status icon
            let icon = if process.failed {
                "✗"
            } else if process.finished {
                "✓"
            } else {
                "⟳"
            };

            // Name and status line
            output.push_str(&format!("{} {} ", icon, process.name));

            if let Some(phase) = &process.phase {
                output.push_str(&format!("({}) ", phase));
            }

            output.push('\n');

            // Progress bar (always present)
            output.push_str(&format!(
                "  {} {:>5.1}%\n",
                render_progress_bar(process.progress_pct, 50),
                process.progress_pct
            ));

            // Time info (always present, use placeholders if not available)
            match (process.current_seconds, process.total_seconds) {
                (Some(current), Some(total)) => {
                    let remaining = total - current;
                    output.push_str(&format!(
                        "  Time: {:.1}s / {:.1}s  (remaining: {:.1}s)\n",
                        current, total, remaining
                    ));
                }
                _ => {
                    output.push_str("  Time: -/- (remaining: -)\n");
                }
            }

            // Warning (always present, use placeholder if not available)
            if let Some(warning) = &process.warning {
                output.push_str(&format!("  ⚠️  {}\n", warning));
            } else {
                output.push_str("  \n");
            }

            // Error (always present, use placeholder if not available)
            if let Some(error) = &process.error {
                output.push_str(&format!("  ❌ {}\n", error));
            } else {
                output.push_str("  \n");
            }

            output.push('\n');
        }

        output
    }

    let mut processes: HashMap<Uuid, ProcessState> = HashMap::new();

    while let Some(delivery) = rx.recv().await {
        tracing::info!(id =% delivery.id, seq = delivery.seq, delivery = delivery.payload.as_value(), "Received delivery");

        let entry = processes.entry(delivery.id).or_insert(ProcessState {
            name: "Unknown".into(),
            progress_pct: 0.0,
            current_seconds: None,
            total_seconds: None,
            phase: None,
            warning: None,
            error: None,
            finished: false,
            failed: false,
        });

        match delivery.payload {
            ExecuteProgressPayload::Start { target_name } => {
                entry.name = target_name;
            }
            ExecuteProgressPayload::Info {
                total_duration_seconds,
                ..
            } => {
                entry.total_seconds = Some(total_duration_seconds);
            }
            ExecuteProgressPayload::Phase { phase } => {
                entry.phase = Some(phase);
            }
            ExecuteProgressPayload::Warning { message } => {
                entry.warning = Some(message);
            }
            ExecuteProgressPayload::Progress {
                total_seconds,
                current_seconds,
            } => {
                entry.total_seconds = Some(total_seconds);
                entry.current_seconds = Some(current_seconds);
                entry.progress_pct = (current_seconds / total_seconds * 100.0).min(100.0);
            }
            ExecuteProgressPayload::Finished(_) => {
                entry.finished = true;
                entry.progress_pct = 100.0;
                entry.phase = Some("Complete".to_string());
            }
            ExecuteProgressPayload::Failed(err) => {
                entry.failed = true;
                entry.error = Some(err.to_string());
            }
            _ => {}
        }

        if !verbose {
            let mut stdout = stdout();
            let _ = stdout.execute(cursor::MoveTo(0, 0));
            let _ = stdout.execute(Clear(ClearType::All));
            print!("{}", render_compact(&processes));
            let _ = stdout.flush();
        }
    }

    // Final display
    if !verbose {
        let mut stdout = stdout();
        let _ = stdout.execute(cursor::MoveTo(0, 0));
        let _ = stdout.execute(Clear(ClearType::All));
        print!("{}", render_compact(&processes));
        let _ = stdout.flush();
    }
}
