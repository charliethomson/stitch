use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use lazy_static::lazy_static;
use liberror::AnyError;
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{io::AsyncWriteExt, task::JoinSet};
use tokio_util::{future::FutureExt, sync::CancellationToken};
use uuid::Uuid;
use valuable::Valuable;

use crate::{
    ffmpeg::{FfmpegError, FfmpegExit, ffmpeg},
    ffprobe::{FfprobeError, ffprobe},
    parse::Plan,
};

lazy_static! {
    static ref RE_OUT_TIME_US: Regex =
        Regex::new(r#"^out_time_us=(\d+)$"#).expect("Failed to compile RE_OUT_TIME_US");
}

#[derive(Debug, Clone, Serialize, Deserialize, Valuable, Error)]
pub enum ExecuteError {
    #[error(transparent)]
    Ffmpeg {
        #[from]
        inner_error: FfmpegError,
    },
    #[error(transparent)]
    Ffprobe {
        #[from]
        inner_error: FfprobeError,
    },
    #[error("Failed to send progress message: {inner_error}")]
    Send { inner_error: AnyError },
    #[error("Failed to create catfile at \"{catfile_path}\": {inner_error}")]
    CreateCatFile {
        catfile_path: String,
        inner_error: AnyError,
    },
    #[error("Failed to write to catfile at \"{catfile_path}\": {inner_error}")]
    WriteToCatFile {
        catfile_path: String,
        inner_error: AnyError,
    },
    #[error("Failed to get file duration, no output line found from ffprobe")]
    NoDuration,
    #[error(
        "Failed to get file duration, ffprobe returned an invalid float \"{line}\": {inner_error}"
    )]
    InvalidDuration { line: String, inner_error: AnyError },
}

pub type ExecuteResult = Result<(), ExecuteError>;

#[derive(Debug, Clone)]
pub enum ExecuteProgressPayload {
    Start {
        target_name: String,
    },
    Prepared {
        cat_path: PathBuf,
    },
    Finished(FfmpegExit),
    Failed(FfmpegError),
    Progress {
        total_seconds: f64,
        current_seconds: f64,
    },
    Spawned,
}

#[derive(Debug, Clone)]
pub struct ExecuteProgress {
    pub id: Uuid,
    pub seq: usize,
    pub payload: ExecuteProgressPayload,
}

struct Process {
    seq: AtomicUsize,
    id: Uuid,
    plan: Plan,
    tx: tokio::sync::mpsc::Sender<ExecuteProgress>,
    tmp_root: PathBuf,
    cancellation_token: CancellationToken,
}
impl Process {
    fn new(
        plan: Plan,
        tx: tokio::sync::mpsc::Sender<ExecuteProgress>,
        tmp_root: PathBuf,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            seq: AtomicUsize::new(0),
            id: Uuid::new_v4(),
            plan,
            tx,
            tmp_root,
            cancellation_token,
        }
    }

    async fn send(&self, payload: ExecuteProgressPayload) {
        if let Err(e) = self
            .tx
            .send(ExecuteProgress {
                id: self.id,
                seq: self.seq.fetch_add(1, Ordering::Relaxed),
                payload,
            })
            .await
            .map_err(|e| ExecuteError::Send {
                inner_error: e.into(),
            })
        {
            todo!("Failed to send progress message thru sender: {e}");
        }
    }
}
impl Process {
    async fn start(&self) {
        self.send(ExecuteProgressPayload::Start {
            target_name: self.plan.target_path.leaf.clone(),
        })
        .await;
    }

    async fn prepare_catfile(&self) -> Result<PathBuf, ExecuteError> {
        let catfile_path = self.tmp_root.join(format!(
            "{}.catfile",
            self.plan.target_path.leaf.replace(".", "_")
        ));

        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&catfile_path)
            .await
            .map_err(|e| ExecuteError::CreateCatFile {
                catfile_path: catfile_path.display().to_string(),
                inner_error: e.into(),
            })?;

        let content = self
            .plan
            .sources
            .iter()
            .map(|source| format!("file '{}'", source.path.display()))
            .collect::<Vec<_>>()
            .join("\n");

        file.write_all(content.as_bytes())
            .await
            .map_err(|e| ExecuteError::WriteToCatFile {
                catfile_path: catfile_path.display().to_string(),
                inner_error: e.into(),
            })?;

        self.send(ExecuteProgressPayload::Prepared {
            cat_path: catfile_path.clone(),
        })
        .await;

        Ok(catfile_path)
    }

    async fn get_expected_output_seconds(&self) -> Result<f64, ExecuteError> {
        let mut tasks = JoinSet::new();

        for source in self.plan.sources.iter() {
            let file_path = source.path.display().to_string();
            #[rustfmt::skip]
            let fut = ffprobe(self.cancellation_token.child_token(), |cmd| {
                cmd.arg("-v").arg("error"); // shut up
                cmd.arg("-show_entries").arg("format=duration"); // gimme duration
                cmd.arg("-of").arg("default=noprint_wrappers=1:nokey=1"); // make it not ugly
                cmd.arg(file_path);
            });

            tasks.spawn(fut);
        }

        let mut total_seconds = 0.0f64;

        while let Some(result) = tasks.join_next().await {
            let result = result.expect("Failed to join task")?;
            let source_seconds_str = result
                .stdout_lines
                .first()
                .ok_or(ExecuteError::NoDuration)?;
            let source_seconds =
                &source_seconds_str
                    .parse::<f64>()
                    .map_err(|e| ExecuteError::InvalidDuration {
                        line: source_seconds_str.to_string(),
                        inner_error: e.into(),
                    })?;
            total_seconds += source_seconds;
        }

        Ok(total_seconds)
    }

    async fn execute(self, catfile_path: PathBuf) -> ExecuteResult {
        let (stderr_tx, mut _todo_do_i_care_stderr_rx) = tokio::sync::mpsc::channel(100);
        let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::channel(100);

        let target_path = self.plan.target_path.path.display().to_string();
        let process = ffmpeg(
            self.cancellation_token.child_token(),
            stdout_tx,
            stderr_tx,
            |cmd| {
                cmd.arg("-f").arg("concat");
                cmd.arg("-safe").arg("0");
                cmd.arg("-i").arg(catfile_path);
                cmd.arg("-progress").arg("pipe:1");
                cmd.arg("-c").arg("copy");
                cmd.arg(target_path);
                cmd.arg("-y");
            },
        );

        let monitor_token = self.cancellation_token.child_token();
        let total_seconds = self.get_expected_output_seconds().await?;
        let this = Arc::new(self);
        let mut tasks = JoinSet::new();
        /* ffmpeg task */
        {
            let this = this.clone();
            let monitor_token = monitor_token.clone();
            tasks.spawn(async move {
                this.send(ExecuteProgressPayload::Spawned).await;
                match process.await {
                    // TODO: do i care
                    Ok(_result) => this.send(ExecuteProgressPayload::Finished(_result)).await,
                    Err(e) => this.send(ExecuteProgressPayload::Failed(e)).await,
                }
                monitor_token.cancel();
            });
        }

        /* stdout task */
        {
            let this = this.clone();
            let monitor_token = monitor_token.clone();
            tasks.spawn(async move {
                loop {
                    match stdout_rx.recv().with_cancellation_token(&monitor_token).await {
                        Some(Some(line)) => {
                            println!("{}", line);
                            if !RE_OUT_TIME_US.is_match(&line) { continue; }
                            let cap = RE_OUT_TIME_US.captures(&line).and_then(|caps| caps.get(1)).map(|cap| cap.as_str());
                            let Some(cap) = cap else { unreachable!("out_time_us is match but no capture") };
                            match cap.parse::<f64>() {
                                Err(e) => {
                                    todo!("Can this fail (out_time_us parse as f64): {e}");
                                }
                                Ok(out_time_us) => {
                                    this.send(ExecuteProgressPayload::Progress {
                                        total_seconds,
                                        current_seconds: out_time_us / 1_000_000.0,
                                    }).await;
                                }
                            }
                        },
                        Some(None) /* Channel closed */ => { break },
                        None /* cancelled */ => { break },
                    }
                }
            });
        }

        tasks.join_all().await;

        Ok(())
    }
}

pub async fn execute_plan(
    plan: Plan,
    tx: tokio::sync::mpsc::Sender<ExecuteProgress>,
    tmp_root: PathBuf,
    cancellation_token: CancellationToken,
) -> ExecuteResult {
    let process = Process::new(plan, tx, tmp_root, cancellation_token);

    process.start().await;
    let catfile_path = process.prepare_catfile().await?;
    process.execute(catfile_path).await?;

    Ok(())
}
