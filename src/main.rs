use std::{
    collections::HashMap,
    io::{Write, stdout},
    path::PathBuf,
    time::SystemTime,
};

use clap::Parser;
use comfy_table::Table;
use crossterm::{
    cursor, execute,
    terminal::{Clear, ClearType},
};
use tokio::task::JoinSet;
use tokio_util::{future::FutureExt, sync::CancellationToken};
use uuid::Uuid;

use crate::{
    execute::{ExecuteProgress, ExecuteProgressPayload, execute_plan},
    parse::{ParseError, parse_spec},
};

pub mod execute;
pub mod ffmpeg;
pub mod ffprobe;
pub mod parse;

#[derive(Parser)]
pub struct Args {
    #[arg()]
    pub spec: PathBuf,
    #[arg(short = 'o', long)]
    pub target_dir: Option<PathBuf>,
    #[arg(short = 'i', long)]
    pub sources_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cancellation_token = CancellationToken::new();

    let cwd = std::env::current_dir().expect(
        "Failed to get current directory, please pass an directories with --target-dir and --sources-dir",
    );

    let target_dir = args.target_dir.unwrap_or(cwd.clone());
    let sources_dir = args.sources_dir.unwrap_or(cwd.clone());

    let spec = match parse_spec(args.spec, target_dir, sources_dir) {
        Ok(spec) => spec,

        Err(e) => match &e {
            ParseError::Validation { errors } => {
                eprintln!("Validation failed:");
                for error in errors {
                    eprintln!("\t{error}")
                }
                eprintln!("");

                return Err(e.into());
            }
            _ => return Err(e.into()),
        },
    };

    let mut executions = JoinSet::new();
    let (tx, rx) = tokio::sync::mpsc::channel(100);

    let epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("Why are you in the past?")
        .as_secs();
    let tmp_root = std::env::temp_dir().join(format!("dev_thmsn_stitch_{epoch}"));
    if let Err(e) = tokio::fs::create_dir_all(&tmp_root).await {
        eprintln!("Failed to create temp directory");
        return Err(e.into());
    }

    for plan in spec {
        let tx = tx.clone();
        let tmp_root = tmp_root.clone();
        executions.spawn(execute_plan(
            plan,
            tx,
            tmp_root,
            cancellation_token.child_token(),
        ));
    }

    let monitor_token = cancellation_token.child_token();

    tokio::spawn(monitor(monitor_token.clone(), rx));

    executions.join_all().await;
    monitor_token.cancel();

    Ok(())
}

async fn monitor(ct: CancellationToken, mut rx: tokio::sync::mpsc::Receiver<ExecuteProgress>) {
    #[derive(Debug)]
    enum Status {
        Starting,
        Running,
        Finished,
        Failed,
    }

    struct ProcessState {
        name: String,
        progress_pct: f64,
        status: Status,
    }

    fn render_table(processes: &HashMap<Uuid, ProcessState>) -> String {
        let mut table = Table::new();
        table.set_header(vec!["Name", "Progress", "Status"]);

        for process in processes.values() {
            table.add_row(vec![
                process.name.clone(),
                format!("{:.1}%", process.progress_pct),
                format!("{:?}", process.status),
            ]);
        }

        table.to_string()
    }

    fn update_display(processes: &HashMap<Uuid, ProcessState>) -> std::io::Result<()> {
        let mut stdout = stdout();

        // Move cursor to top-left and clear screen
        execute!(stdout, cursor::MoveTo(0, 0), Clear(ClearType::All))?;

        // Print the table
        println!("{}", render_table(processes));
        stdout.flush()?;

        Ok(())
    }

    let mut processes: HashMap<Uuid, ProcessState> = HashMap::new();

    loop {
        if let Err(e) = update_display(&processes) {
            todo!("Failed to update table, do i care tho???? >.<: {e}")
        };

        let delivery = rx.recv().with_cancellation_token(&ct).await;
        match delivery {
            Some(Some(delivery)) => {
                let entry = processes.entry(delivery.id).or_insert(ProcessState { name: "Uninitialized".into(), progress_pct: 0.0, status: Status::Starting });

                match delivery.payload {
                    ExecuteProgressPayload::Start { target_name } => entry.name = target_name,
                    ExecuteProgressPayload::Prepared { cat_path: _ } => { /* nop for now */ },
                    ExecuteProgressPayload::Finished(_ffmpeg_exit) => entry.status = Status::Finished,
                    ExecuteProgressPayload::Failed(_ffmpeg_error) => entry.status = Status::Failed,
                    ExecuteProgressPayload::Progress { total_seconds, current_seconds } => entry.progress_pct = current_seconds / total_seconds * 100.0,
                    ExecuteProgressPayload::Spawned => entry.status = Status::Running,
                }
            }
            Some(None) /* Channel closed */ => { break },
            None /* cancelled */ => { break },
        }
    }
}
