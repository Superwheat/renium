pub(super) const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(super) const GIT_HASH: &str = match option_env!("BUILD_GIT_HASH") {
    Some(value) => value,
    None => "unknown",
};
pub(super) const TIMESTAMP_UNIX: &str = match option_env!("BUILD_TIMESTAMP_UNIX") {
    Some(value) => value,
    None => "0",
};
