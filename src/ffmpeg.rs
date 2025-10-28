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

#[derive(Debug, Clone, Serialize, Deserialize, Valuable, Error)]
pub enum FfmpegError {
    #[error("cancellation requested")]
    Cancelled,

    #[error("failed to spawn: {inner_error}")]
    BadSpawn { inner_error: AnyError },

    #[error("exited unsuccessfully: {inner_error}")]
    BadExit { inner_error: AnyError },
}

pub type FfmpegResult = Result<FfmpegExit, FfmpegError>;

#[derive(Debug, Clone)]
pub struct FfmpegExit {
    pub stdout_lines: Vec<String>,
    pub stderr_lines: Vec<String>,
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
    let mut cmd = Command::new("ffmpeg");

    cb(&mut cmd);

    tracing::debug!(args = ?cmd.as_std().get_args().collect::<Vec<_>>(), "Executing ffmpeg command");

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

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
                if let Err(e) = stdout_tx.send(line).await {
                    todo!("Failed to write stdout_tx: {e}");
                };
            }
            Ok(Some(line)) = stderr.next_line() => {
                result.stderr_lines.push(line.clone());
                if let Err(e) = stderr_tx.send(line).await {
                    todo!("Failed to write stderr_tx: {e}");
                };
            }
        }
    }
}
