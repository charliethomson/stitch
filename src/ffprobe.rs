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
pub enum FfprobeError {
    #[error("cancellation requested")]
    Cancelled,

    #[error("failed to spawn: {inner_error}")]
    BadSpawn { inner_error: AnyError },

    #[error("exited unsuccessfully: {inner_error}")]
    BadExit { inner_error: AnyError },
}

#[derive(Debug)]
pub struct FfprobeExit {
    pub stdout_lines: Vec<String>,
    pub stderr_lines: Vec<String>,
    pub exit_code: Option<ExitStatus>,
}

#[tracing::instrument(skip_all)]
pub async fn ffprobe<Cb>(ct: CancellationToken, cb: Cb) -> Result<FfprobeExit, FfprobeError>
where
    Cb: FnOnce(&mut Command),
{
    let mut cmd = Command::new("ffprobe");

    cb(&mut cmd);

    tracing::debug!(args = ?cmd.as_std().get_args().collect::<Vec<_>>(), "Executing ffprobe command");

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| FfprobeError::BadSpawn {
        inner_error: e.into(),
    })?;

    let stdout = child.stdout.take().expect("ffprobe takes a stdout");
    let mut stdout = BufReader::new(stdout).lines();
    let stderr = child.stderr.take().expect("ffprobe takes a stderr");
    let mut stderr = BufReader::new(stderr).lines();

    let mut result = FfprobeExit {
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
                            tracing::trace!("ffprobe process completed successfully");
                        } else {
                            tracing::error!(
                                exit_code = ?status.code(),
                                stderr_lines = ?result.stderr_lines,
                                "ffprobe process completed with non-zero exit code"
                            );
                        }
                        return Ok(result);
                    },
                    Err(e) => {
                        tracing::error!(error = %e, "ffprobe process wait failed");
                        return Err(FfprobeError::BadExit { inner_error: e.into() })
                    }
                }
            }

            () = ct.cancelled() => {
                tracing::warn!("Cancellation requested, terminating ffprobe process");
                child.kill().await.expect("Failed to kill ffprobe");
                return Err(FfprobeError::Cancelled);
            }

            Ok(Some(line)) = stdout.next_line() => {
                tracing::debug!(line = line, "ffprobe wrote to stdout");
                result.stdout_lines.push(line);
            }
            Ok(Some(line)) = stderr.next_line() => {
                tracing::debug!(line = line, "ffprobe wrote to stderr");
                result.stderr_lines.push(line);
            }
        }
    }
}
