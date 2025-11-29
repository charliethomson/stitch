use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use tracing::{Level, instrument};

fn validate_binary<P: AsRef<Path>>(path: P) -> io::Result<PathBuf> {
    // Canonicalize will Err if not exists
    let path = PathBuf::from(path.as_ref()).canonicalize()?;

    let meta = std::fs::metadata(&path)?;

    if !meta.is_file() {
        return Err(io::Error::other(
            format!("Expected file, got {:?}", meta.file_type()),
        ));
    }

    // TODO: check +x? do i give a shit? do i look like i give a shit? dont fuckin pass some dumb shit man
    Ok(path)
}

#[instrument(level = Level::DEBUG)]
fn find_binary(bin: &str, given: Option<PathBuf>) -> io::Result<PathBuf> {
    // First look for an environment variable
    if let Some(given_path) = given {
        match validate_binary(&given_path) {
            Ok(path) => {
                tracing::info!(bin = bin, given_path =% given_path.display(), path =% path.display(), "Found {} at {} with {}", bin, path.display(), given_path.display());
                return Ok(path);
            }
            Err(e) => {
                tracing::warn!(bin = bin, error =% e, error_context =? e, "Invalid binary {} path for {}", given_path.display(),bin);
            }
        }
    }

    // Then look on the path
    let path_variable = std::env::var("PATH").unwrap_or_default();
    let search_paths = std::env::split_paths(OsStr::new(&path_variable));

    for search_path in search_paths {
        let search_path = search_path.join(bin);
        match validate_binary(&search_path) {
            Ok(path) => {
                tracing::info!(bin = bin, search_path =% search_path.display(), path =% path.display(), "Found {} in {} at {}", bin, search_path.display(), path.display());
                return Ok(path);
            }
            Err(e) => {
                tracing::debug!(bin = bin, search_path =% search_path.display(), error =% e, error_context =? e, "{} not found in {}", bin, search_path.display());
            }
        }
    }

    // TODO: Anywhere else to look? i dont think so

    tracing::debug!(bin = bin, "{} not found in PATH, no override provided", bin,);

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "Failed to find {} binary in PATH, no override provided",
            bin,
        ),
    ))
}

static FFMPEG_PATH: OnceLock<PathBuf> = OnceLock::new();
static FFPROBE_PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn find_binaries(ffmpeg: Option<PathBuf>, ffprobe: Option<PathBuf>) -> io::Result<()> {
    let ffmpeg = find_binary("ffmpeg", ffmpeg)?;
    let ffprobe = find_binary("ffprobe", ffprobe)?;

    FFMPEG_PATH.get_or_init(|| ffmpeg);
    FFPROBE_PATH.get_or_init(|| ffprobe);

    Ok(())
}

pub fn get_ffmpeg<'a>() -> Option<&'a PathBuf> {
    FFMPEG_PATH.get()
}
pub fn get_ffprobe<'a>() -> Option<&'a PathBuf> {
    FFPROBE_PATH.get()
}
