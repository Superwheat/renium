pub(crate) mod bytecode;
#[cfg(test)]
mod codec_sim;
pub(crate) mod equivalence;
pub(crate) mod held;
pub(crate) mod instance;
pub(crate) mod tree;

pub(crate) const EXTERNAL_SOURCE_MARKER: &str = "__SOURCE_EXTERNAL__";
