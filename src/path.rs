use std::{path::PathBuf, time::SystemTime};

const PRODUCT_NAME: &str = "dev.thmsn.stitch";

fn epoch() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("Why are you in the past?")
        .as_secs()
}

pub fn tmp_root() -> PathBuf {
    std::env::temp_dir().join(PRODUCT_NAME)
}

pub fn run_tmp_root() -> PathBuf {
    let dir = tmp_root().join(epoch().to_string());
    if !dir.exists() {
        std::fs::create_dir_all(&dir).expect("Failed to create tmp root dir");
    }
    dir
}

pub fn data_root() -> PathBuf {
    dirs::data_local_dir()
        .expect("cant find data local dir")
        .join(PRODUCT_NAME)
}

pub fn logs_root() -> PathBuf {
    data_root().join("logs")
}

pub fn logs_path() -> PathBuf {
    let parent = logs_root();
    if !parent.exists() {
        std::fs::create_dir_all(&parent).expect("Failed to create logs root dir");
    }
    parent.join(format!("{}_log.json", epoch()))
}
