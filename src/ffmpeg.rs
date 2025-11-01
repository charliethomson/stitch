use std::process::{ExitStatus, Stdio};

use liberror::AnyError;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;
use valuable::Valuable;

use crate::env::get_ffmpeg;

#[derive(Debug, Clone, Serialize, Deserialize, Valuable, Error)]
pub enum FfmpegError {
    #[error("cancellation requested")]
    Cancelled,

    #[error("failed to spawn: {inner_error}")]
    BadSpawn { inner_error: AnyError },

    #[error("exited unsuccessfully: {inner_error}")]
    BadExit { inner_error: AnyError },

    #[error("acquire permit: {inner_error}")]
    Acquire { inner_error: AnyError },

    #[error("Unable to locate ffmpeg path, lock uninitialized")]
    UninitializedPath,
}

pub type FfmpegResult = Result<FfmpegExit, FfmpegError>;

#[derive(Debug, Clone, Valuable)]
pub struct FfmpegExit {
    pub stdout_lines: Vec<String>,
    pub stderr_lines: Vec<String>,
    #[valuable(skip)]
    pub exit_code: Option<ExitStatus>,
}

#[tracing::instrument(skip_all)]
pub async fn ffmpeg<Cb>(
    ct: CancellationToken,
    stdout_tx: tokio::sync::mpsc::Sender<String>,
    stderr_tx: tokio::sync::mpsc::Sender<String>,
    cb: Cb,
) -> FfmpegResult
where
    Cb: FnOnce(&mut Command),
{
    let ffmpeg_path = get_ffmpeg().ok_or(FfmpegError::UninitializedPath)?;

    let mut cmd = Command::new(ffmpeg_path);

    cb(&mut cmd);

    tracing::info!(args = ?cmd.as_std().get_args().collect::<Vec<_>>(), "Executing ffmpeg command");

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let _permit = crate::limits::LIMIT_PROCESSES
        .acquire()
        .await
        .map_err(|e| FfmpegError::Acquire {
            inner_error: e.into(),
        })
        .inspect_err(
            |e| tracing::error!(error =% e, error_context =? e, "Failed to acquire permit"),
        )?;

    let mut child = cmd.spawn().map_err(|e| FfmpegError::BadSpawn {
        inner_error: e.into(),
    })?;

    let stdout = child.stdout.take().expect("ffmpeg takes a stdout");
    let mut stdout = BufReader::new(stdout).lines();
    let stderr = child.stderr.take().expect("ffmpeg takes a stderr");
    let mut stderr = BufReader::new(stderr).lines();

    let mut result = FfmpegExit {
        stdout_lines: Vec::new(),
        stderr_lines: Vec::new(),
        exit_code: None,
    };

    loop {
        tokio::select! {
            exit_result = child.wait() => {
                match exit_result {
                    Ok(status) => {
                        result.exit_code = Some(status);
                        if status.success() {
                            tracing::trace!("ffmpeg process completed successfully");
                        } else {
                            tracing::error!(
                                exit_code = ?status.code(),
                                stderr_lines = ?result.stderr_lines,
                                "ffmpeg process completed with non-zero exit code"
                            );
                        }
                        return Ok(result);
                    },
                    Err(e) => {
                        tracing::error!(error = %e, "ffmpeg process wait failed");
                        return Err(FfmpegError::BadExit { inner_error: e.into() })
                    }
                }
            }

            () = ct.cancelled() => {
                tracing::warn!("Cancellation requested, terminating ffmpeg process");
                child.kill().await.expect("Failed to kill ffmpeg");
                return Err(FfmpegError::Cancelled);
            }

            Ok(Some(line)) = stdout.next_line() => {
                result.stdout_lines.push(line.clone());
                tracing::debug!(line = line, "ffmpeg wrote to stdout");
                if let Err(e) = stdout_tx.send(line).await {
                    tracing::error!(error =% e, error_context =? e, "Failed to write stdout_tx");
                };
            }
            Ok(Some(line)) = stderr.next_line() => {
                result.stderr_lines.push(line.clone());
                tracing::debug!(line = line, "ffmpeg wrote to stderr");
                if let Err(e) = stderr_tx.send(line).await {
                    tracing::error!(error =% e, error_context =? e, "Failed to write stderr_tx");
                };
            }
        }
    }
}
