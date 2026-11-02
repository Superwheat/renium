/// Scanline filter for every PNG Renium writes (screenshots, recording
/// review sheets). A fixed Paeth filter measured 2-7% smaller than Sub and
/// 2-4% smaller than Up on three Studio captures (a 2223x895 editor, a
/// 1345x639 client, a 1440x1108 review sheet), and a fixed choice keeps the
/// files byte-identical across png crate versions, whose default filter has
/// changed before.
pub(crate) const CAPTURE_PNG_FILTER: png::FilterType = png::FilterType::Paeth;

pub(crate) mod audio;
pub(crate) mod automation;
pub(crate) mod bridge;
pub(crate) mod diagnosis;
pub(crate) mod input;
pub(crate) mod native;
pub(crate) mod performance;
pub(crate) mod target;
