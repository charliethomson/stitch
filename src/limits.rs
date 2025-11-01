use tokio::sync::Semaphore;

// TODO: Configurable?
pub static LIMIT_PROCESSES: Semaphore = Semaphore::const_new(8);
