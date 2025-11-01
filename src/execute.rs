use std::{
    collections::HashMap,
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
use tracing::{Instrument, Level, Span, instrument};
use uuid::Uuid;
use valuable::Valuable;

use crate::{
    ffmpeg::{FfmpegError, FfmpegExit, ffmpeg},
    ffprobe::{FfprobeError, ffprobe},
    parse::{Flag, Plan},
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
    #[error("Failed to determine if some sources had audio tracks")]
    AudioFailures { inner_errors: Vec<FfprobeError> },
}

pub type ExecuteResult = Result<(), ExecuteError>;

#[derive(Debug, Clone, Valuable)]
pub enum ExecuteProgressPayload {
    Start {
        target_name: String,
    },
    Prepared {
        cat_path: PathBuf,
    },
    Info {
        source_count: usize,
        total_duration_seconds: f64,
        has_audio: bool,
        mode: String,
    },
    Phase {
        phase: String,
    },
    Warning {
        message: String,
    },
    Finished(FfmpegExit),
    Failed(ExecuteError),
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

#[derive(Debug)]
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
        tracing::info!(id =% self.id, "Process started");
        self.send(ExecuteProgressPayload::Start {
            target_name: self.plan.target_path.leaf.clone(),
        })
        .await;
    }

    #[instrument(level = Level::INFO)]
    async fn prepare_catfile(&self) -> Result<PathBuf, ExecuteError> {
        self.send(ExecuteProgressPayload::Phase {
            phase: "Preparing concatenation file".to_string(),
        })
        .await;

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
            })
            .inspect(|_| tracing::info!(catfile_path =% catfile_path.display(), "Successfully opened catfile"))
            .inspect_err(|e| tracing::error!(catfile_path =% catfile_path.display(), error =% e, error_context =? e, "Failed to open catfile"))?;

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
            })
            .inspect(|_| tracing::info!(catfile_path =% catfile_path.display(), "Successfully wrote to catfile"))
            .inspect_err(|e| tracing::error!(catfile_path =% catfile_path.display(), error =% e, error_context =? e, "Failed to write to catfile"))?;

        self.send(ExecuteProgressPayload::Prepared {
            cat_path: catfile_path.clone(),
        })
        .await;

        Ok(catfile_path)
    }

    #[instrument(level = Level::INFO)]
    async fn get_expected_output_seconds(&self) -> Result<f64, ExecuteError> {
        self.send(ExecuteProgressPayload::Phase {
            phase: "Calculating total duration".to_string(),
        })
        .await;

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

    async fn get_source_has_audio(&self) -> Result<HashMap<String, bool>, ExecuteError> {
        self.send(ExecuteProgressPayload::Phase {
            phase: "Detecting audio tracks".to_string(),
        })
        .await;

        let mut tasks: JoinSet<Result<(String, bool), FfprobeError>> = JoinSet::new();
        let span = Span::current();

        for source in self.plan.sources.iter() {
            let source = source.clone();
            let ct = self.cancellation_token.child_token();

            tasks.spawn(
                async move {
                    let results = ffprobe(ct, |cmd| {
                        cmd.arg("-v").arg("error");
                        cmd.arg("-select_streams").arg("a");
                        cmd.arg("-show_entries").arg("stream=codec_type");
                        cmd.arg("-of").arg("default=noprint_wrappers=1:nokey=1");
                        cmd.arg(source.path);
                    })
                    .await?;

                    let has_audio = {
                        let exited_normally = results
                            .exit_code
                            .map(|code| code.success())
                            .unwrap_or_default();
                        let has_stdout = !results.stdout_lines.is_empty();
                        let stdout_has_text = !results
                            .stdout_lines
                            .into_iter()
                            .next()
                            .unwrap_or_default()
                            .is_empty();

                        exited_normally && has_stdout && stdout_has_text
                    };

                    Ok((source.leaf, has_audio))
                }
                .instrument(span.clone()),
            );
        }

        let mut map = HashMap::new();
        let mut errors = Vec::new();

        while let Some(result) = tasks.join_next().await {
            let result = result.expect("Failed to join task");

            match result {
                Ok((leaf, has_audio)) => {
                    map.insert(leaf, has_audio);
                }
                Err(e) => {
                    errors.push(e);
                }
            }
        }

        if !errors.is_empty() {
            return Err(ExecuteError::AudioFailures {
                inner_errors: errors,
            });
        }

        Ok(map)
    }

    #[instrument(level = Level::INFO)]
    async fn execute(self: Arc<Self>, catfile_path: PathBuf) -> Result<FfmpegExit, ExecuteError> {
        let (stderr_tx, mut stderr_rx) = tokio::sync::mpsc::channel(100);
        let (stdout_tx, mut stdout_rx) = tokio::sync::mpsc::channel(100);

        let plan = self.plan.clone();

        let source_has_audio = self.get_source_has_audio().await?;

        let total_seconds = self.get_expected_output_seconds().await?;

        let all_have_audio = plan
            .sources
            .iter()
            .all(|source| source_has_audio.get(&source.leaf).copied().unwrap_or(false));

        let using_filter_complex = plan
            .flags
            .iter()
            .copied()
            .any(|flag| flag == Flag::ConcatFilter);

        self.send(ExecuteProgressPayload::Info {
            source_count: plan.sources.len(),
            total_duration_seconds: total_seconds,
            has_audio: all_have_audio,
            mode: if using_filter_complex {
                "filter_complex".to_string()
            } else {
                "concat".to_string()
            },
        })
        .await;

        if using_filter_complex && !all_have_audio {
            let sources_with_audio = source_has_audio.values().filter(|&&v| v).count();
            self.send(ExecuteProgressPayload::Warning {
                message: format!(
                    "Only {}/{} sources have audio - output will be video-only",
                    sources_with_audio,
                    plan.sources.len()
                ),
            })
            .await;
        }

        self.send(ExecuteProgressPayload::Phase {
            phase: "Encoding".to_string(),
        })
        .await;

        let target_path = self.plan.target_path.path.display().to_string();
        let process = ffmpeg(
            self.cancellation_token.child_token(),
            stdout_tx,
            stderr_tx,
            move |cmd| {
                let flags = plan.flags;
                let sources = plan.sources;
                let catf = flags.iter().copied().any(|flag| flag == Flag::ConcatFilter);
                if catf {
                    for source in sources.iter() {
                        cmd.arg("-i").arg(&source.path);
                    }

                    let all_have_audio = sources
                        .iter()
                        .all(|source| source_has_audio.get(&source.leaf).copied().unwrap_or(false));

                    cmd.arg("-vsync").arg("cfr");
                    cmd.arg("-r").arg("30");

                    if all_have_audio {
                        // All have audio - concat video and audio
                        let input_list = (0..sources.len())
                            .map(|i| format!("[{i}:v]fps=30,format=yuv420p[v{i}];"))
                            .collect::<Vec<_>>()
                            .join("");

                        let audio_prep = (0..sources.len())
                            .map(|i| format!("[{i}:a]anull[a{i}];"))
                            .collect::<Vec<_>>()
                            .join("");

                        let video_directives = (0..sources.len())
                            .map(|i| format!("[v{i}][a{i}]"))
                            .collect::<Vec<_>>()
                            .join("");

                        let opts = format!("concat=n={}:v=1:a=1[outv][outa]", sources.len());
                        let filter_complex =
                            format!("{input_list}{audio_prep}{video_directives}{opts}");

                        cmd.arg("-filter_complex").arg(filter_complex);
                        cmd.arg("-map").arg("[outv]");
                        cmd.arg("-map").arg("[outa]");
                        cmd.arg("-c:a").arg("aac");
                        cmd.arg("-b:a").arg("128k");
                    } else {
                        // Not all have audio - video only
                        let input_list = (0..sources.len())
                            .map(|i| format!("[{i}:v]fps=30,format=yuv420p[v{i}];"))
                            .collect::<Vec<_>>()
                            .join("");

                        let video_directives = (0..sources.len())
                            .map(|i| format!("[v{i}]"))
                            .collect::<Vec<_>>()
                            .join("");

                        let opts = format!("concat=n={}:v=1:a=0[outv]", sources.len());
                        let filter_complex = format!("{input_list}{video_directives}{opts}");

                        cmd.arg("-filter_complex").arg(filter_complex);
                        cmd.arg("-map").arg("[outv]");
                    }

                    cmd.arg("-c:v").arg("libx264");
                    cmd.arg("-preset").arg("medium");
                    cmd.arg("-crf").arg("23");
                    cmd.arg("-progress").arg("pipe:1");
                } else {
                    cmd.arg("-f").arg("concat");
                    cmd.arg("-safe").arg("0");
                    cmd.arg("-i").arg(catfile_path);
                    cmd.arg("-progress").arg("pipe:1");
                    cmd.arg("-c").arg("copy");
                }
                cmd.arg(target_path);
                cmd.arg("-y");
            },
        );

        let monitor_token = self.cancellation_token.child_token();
        let this = self.clone();
        let mut tasks = JoinSet::new();
        let span = Span::current();

        /* stdout task */
        {
            let this = this.clone();
            let monitor_token = monitor_token.clone();
            tasks.spawn(async move {
                loop {
                    match stdout_rx.recv().with_cancellation_token(&monitor_token).await {
                        Some(Some(line)) => {
                            if !RE_OUT_TIME_US.is_match(&line) { continue; }
                            let cap = RE_OUT_TIME_US.captures(&line).and_then(|caps| caps.get(1)).map(|cap| cap.as_str());
                            let Some(cap) = cap else { unreachable!("out_time_us is match but no capture") };
                            match cap.parse::<f64>() {
                                Err(e) => {
                                    tracing::error!(error =% e, error_context =? e, line = line, cap = cap, "UNHANDLED: Failed to parse rhs cap of the out_time_us progress log")
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
            }
            .instrument(span.clone()),
            );
        }

        /* stderr task - drain stderr to prevent blocking */
        {
            let monitor_token = monitor_token.clone();
            tasks.spawn(
                async move {
                    loop {
                        match stderr_rx.recv().with_cancellation_token(&monitor_token).await {
                        Some(Some(_line)) => {
                            // Just drain stderr, it's already logged in ffmpeg.rs
                        },
                        Some(None) /* Channel closed */ => { break },
                        None /* cancelled */ => { break },
                    }
                    }
                }
                .instrument(span.clone()),
            );
        }

        let result = process.await;
        monitor_token.cancel();

        tasks.join_all().await;

        Ok(result?)
    }
}
pub async fn execute_plan(
    plan: Plan,
    tx: tokio::sync::mpsc::Sender<ExecuteProgress>,
    tmp_root: PathBuf,
    cancellation_token: CancellationToken,
) {
    let process = Arc::new(Process::new(plan, tx, tmp_root, cancellation_token));

    match _execute_plan(process.clone()).await {
        Ok(result) => process.send(ExecuteProgressPayload::Finished(result)).await,
        Err(err) => process.send(ExecuteProgressPayload::Failed(err)).await,
    };
}

#[instrument(level = Level::INFO)]
async fn _execute_plan(process: Arc<Process>) -> Result<FfmpegExit, ExecuteError> {
    process.start().await;
    let catfile_path = process.prepare_catfile().await?;
    process.execute(catfile_path).await
}
