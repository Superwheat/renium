use anyhow::Result;

const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/renium-input-shield"));

pub(super) type InputShield = super::unix_shield::InputShield;

pub(super) fn input_shield(target_pid: i32, window_number: u32) -> Result<InputShield> {
    super::unix_shield::start(
        HELPER,
        "macOS",
        [
            target_pid.to_string(),
            window_number.to_string(),
            std::process::id().to_string(),
            concat!("Renium ", env!("CARGO_PKG_VERSION")).to_string(),
        ],
    )
}
