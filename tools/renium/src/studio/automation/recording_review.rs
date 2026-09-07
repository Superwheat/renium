use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use mp4::{MediaType, Mp4Reader, Mp4Track};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use serde_json::{Value, json};

use crate::app::output::print_json_output;
use crate::cli::RecordReviewArgs;
use crate::system::files::{canonical_path, replace_file_with_backup, sibling_temp_path};

const PAGE_SIZE: u32 = 12;
const TILE_WIDTH: usize = 480;
const TILE_HEIGHT: usize = 270;
const LABEL_HEIGHT: usize = 24;

struct Video {
    reader: Mp4Reader<BufReader<File>>,
    track_id: u32,
    width: usize,
    height: usize,
    times: Vec<u64>,
    duration_ms: u64,
    keys: Vec<u32>,
    headers: Vec<u8>,
    nal_length: usize,
}

fn timeline(track: &Mp4Track) -> Result<(Vec<u64>, u64)> {
    let count = track.sample_count() as usize;
    let scale = u64::from(track.timescale());
    if scale == 0 || count == 0 || count > 1_000_000 || !track.trafs.is_empty() {
        bail!("Review needs a nonempty, non-fragmented Renium MP4 (at most 1,000,000 frames)");
    }
    let table = &track.trak.mdia.minf.stbl;
    if table
        .ctts
        .as_ref()
        .is_some_and(|ctts| ctts.entries.iter().any(|entry| entry.sample_offset != 0))
    {
        bail!("Review does not support reordered video frames; use the original Renium recording");
    }
    let mut times = Vec::with_capacity(count);
    let mut ticks = 0u64;
    for entry in &table.stts.entries {
        if entry.sample_delta == 0 || times.len() + entry.sample_count as usize > count {
            bail!("Invalid recording frame timing table");
        }
        for _ in 0..entry.sample_count {
            times.push(
                ticks
                    .checked_mul(1000)
                    .context("Recording timestamp overflow")?
                    / scale,
            );
            ticks = ticks
                .checked_add(u64::from(entry.sample_delta))
                .context("Recording duration overflow")?;
        }
    }
    if times.len() != count {
        bail!("Recording frame count does not match its timing table");
    }
    Ok((
        times,
        ticks
            .checked_mul(1000)
            .context("Recording duration overflow")?
            / scale,
    ))
}

impl Video {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("Could not open recording {}", path.display()))?;
        let length = file.metadata()?.len();
        let reader = Mp4Reader::read_header(BufReader::new(file), length)
            .context("Could not read MP4; finish the recording with rbx re first")?;
        let tracks = reader
            .tracks()
            .values()
            .filter(|track| track.media_type().ok() == Some(MediaType::H264))
            .collect::<Vec<_>>();
        if tracks.len() != 1 {
            bail!("Review needs exactly one H.264 video track; use the original Renium MP4");
        }
        let track = tracks[0];
        let (width, height) = (usize::from(track.width()), usize::from(track.height()));
        if width == 0 || height == 0 || width * height > 33_554_432 {
            bail!("Recording dimensions are unsupported: {width}x{height}");
        }
        let (times, duration_ms) = timeline(track)?;
        let mut headers = Vec::new();
        let avcc = &track
            .trak
            .mdia
            .minf
            .stbl
            .stsd
            .avc1
            .as_ref()
            .context("Missing H.264 configuration")?
            .avcc;
        for nal in avcc
            .sequence_parameter_sets
            .iter()
            .chain(&avcc.picture_parameter_sets)
        {
            headers.extend_from_slice(&[0, 0, 0, 1]);
            headers.extend_from_slice(&nal.bytes);
        }
        let keys = track.trak.mdia.minf.stbl.stss.as_ref().map_or_else(
            || (1..=track.sample_count()).collect(),
            |stss| stss.entries.clone(),
        );
        if keys.first() != Some(&1)
            || keys.windows(2).any(|pair| pair[0] >= pair[1])
            || keys.last().is_some_and(|key| *key > track.sample_count())
        {
            bail!("Invalid recording keyframe table");
        }
        let track_id = track.track_id();
        let nal_length = usize::from(avcc.length_size_minus_one & 3) + 1;
        Ok(Self {
            reader,
            track_id,
            width,
            height,
            times,
            duration_ms,
            keys,
            headers,
            nal_length,
        })
    }

    fn decoder(&self) -> Result<Decoder> {
        let mut decoder = Decoder::new().context("Could not initialize recording decoder")?;
        decoder
            .decode(&self.headers)
            .context("Invalid recording H.264 parameters")?;
        Ok(decoder)
    }

    fn render(&mut self, selected: &[u32], sheet: &mut Sheet) -> Result<()> {
        let mut decoder = self.decoder()?;
        let mut decoded = 0;
        let mut packet = Vec::new();
        let mut rgb = vec![0; self.width * self.height * 3];
        for (tile, &frame) in selected.iter().enumerate() {
            // Seek to the nearest keyframe instead of decoding the whole clip for each view.
            let key = self.keys[self.keys.partition_point(|key| *key <= frame) - 1];
            if key > decoded + 1 {
                decoder = self.decoder()?;
                decoded = key - 1;
            }
            for index in decoded + 1..=frame {
                let sample = self
                    .reader
                    .read_sample(self.track_id, index)?
                    .with_context(|| format!("Recording frame {index} is missing"))?;
                annex_b(&sample.bytes, self.nal_length, &mut packet)?;
                let picture = decoder
                    .decode(&packet)
                    .with_context(|| format!("Could not decode recording frame {index}"))?
                    .with_context(|| format!("Recording frame {index} produced no image"))?;
                if picture.dimensions() != (self.width, self.height) {
                    bail!("Recording dimensions changed at frame {index}");
                }
                if index == frame {
                    picture.write_rgb8(&mut rgb);
                    sheet.add(tile, &rgb, frame, self.times[(frame - 1) as usize]);
                }
            }
            decoded = frame;
        }
        Ok(())
    }
}

fn annex_b(mut sample: &[u8], length_size: usize, output: &mut Vec<u8>) -> Result<()> {
    output.clear();
    while !sample.is_empty() {
        let length_bytes = sample
            .get(..length_size)
            .context("Truncated recording NAL length")?;
        let length = length_bytes
            .iter()
            .fold(0usize, |length, byte| (length << 8) | usize::from(*byte));
        sample = &sample[length_size..];
        if length == 0 || length > sample.len() {
            bail!("Invalid recording NAL length {length}");
        }
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(&sample[..length]);
        sample = &sample[length..];
    }
    if output.is_empty() {
        bail!("Recording contains an empty frame");
    }
    Ok(())
}

fn selection(times: &[u64], page: Option<u32>, frame: Option<u32>) -> Result<Vec<u32>> {
    let count = times.len() as u32;
    if page.is_some() && frame.is_some() {
        bail!("Choose --page or --frame, not both");
    }
    if let Some(frame) = frame {
        if frame == 0 || frame > count {
            bail!("Frame {frame} is out of range; recording has frames 1..={count}");
        }
        return Ok(vec![frame]);
    }
    if let Some(page) = page {
        let pages = count.div_ceil(PAGE_SIZE);
        if page == 0 || page > pages {
            bail!("Page {page} is out of range; recording has pages 1..={pages}");
        }
        let start = (page - 1) * PAGE_SIZE + 1;
        return Ok((start..=(start + PAGE_SIZE - 1).min(count)).collect());
    }
    if count <= PAGE_SIZE {
        return Ok((1..=count).collect());
    }
    // Sample presentation time, not assumed FPS: slow captures have uneven timestamps.
    let last = *times.last().context("Recording has no frames")?;
    let mut frames = (0..PAGE_SIZE)
        .map(|index| {
            let target = last * u64::from(index) / u64::from(PAGE_SIZE - 1);
            times.partition_point(|time| *time <= target).max(1) as u32
        })
        .collect::<Vec<_>>();
    frames[0] = 1;
    frames.dedup();
    Ok(frames)
}

struct Sheet {
    pixels: Vec<u8>,
    width: usize,
    height: usize,
    source: (usize, usize),
    tile: (usize, usize),
    columns: usize,
    labels: bool,
}

impl Sheet {
    fn new(source: (usize, usize), count: usize, labels: bool) -> Self {
        let scale = if labels {
            (TILE_WIDTH as f64 / source.0 as f64)
                .min(TILE_HEIGHT as f64 / source.1 as f64)
                .min(1.0)
        } else {
            1.0
        };
        let tile = (
            (source.0 as f64 * scale).max(1.0) as usize,
            (source.1 as f64 * scale).max(1.0) as usize,
        );
        let columns = count.min(3);
        // Keep labels readable even for portrait or very small recordings.
        let width = columns * tile.0.max(if labels { 240 } else { 1 });
        let height = count.div_ceil(columns) * (tile.1 + if labels { LABEL_HEIGHT } else { 0 });
        Self {
            pixels: vec![24; width * height * 3],
            width,
            height,
            source,
            tile,
            columns,
            labels,
        }
    }

    fn add(&mut self, index: usize, source: &[u8], frame: u32, timestamp: u64) {
        let cell_width = self.width / self.columns;
        let left = (index % self.columns) * cell_width;
        let top =
            (index / self.columns) * (self.tile.1 + if self.labels { LABEL_HEIGHT } else { 0 });
        if self.tile == self.source {
            for (row, pixels) in source.chunks_exact(self.source.0 * 3).enumerate() {
                let offset = ((top + row) * self.width + left) * 3;
                self.pixels[offset..offset + pixels.len()].copy_from_slice(pixels);
            }
        } else {
            // Area averaging preserves thin UI strokes when shrinking; no extra image library.
            for y in 0..self.tile.1 {
                for x in 0..self.tile.0 {
                    let mut sum = [0u64; 3];
                    let x0 = x * self.source.0 / self.tile.0;
                    let x1 = (x + 1) * self.source.0 / self.tile.0;
                    let y0 = y * self.source.1 / self.tile.1;
                    let y1 = (y + 1) * self.source.1 / self.tile.1;
                    for row in y0..y1 {
                        for column in x0..x1 {
                            let offset = (row * self.source.0 + column) * 3;
                            for channel in 0..3 {
                                sum[channel] += u64::from(source[offset + channel]);
                            }
                        }
                    }
                    let count = ((x1 - x0) * (y1 - y0)) as u64;
                    let offset = ((top + y) * self.width + left + x) * 3;
                    for (channel, value) in sum.iter().enumerate() {
                        self.pixels[offset + channel] = (value / count) as u8;
                    }
                }
            }
        }
        if self.labels {
            self.label(
                left + 4,
                top + self.tile.1 + 4,
                &format!(
                    "#{frame} {}:{:02}.{:03}",
                    timestamp / 60_000,
                    timestamp / 1000 % 60,
                    timestamp % 1000
                ),
            );
        }
    }

    fn label(&mut self, x: usize, y: usize, text: &str) {
        // Five-column digits, #, colon and decimal point; no system fonts required.
        const GLYPHS: [[u8; 5]; 13] = [
            [62, 81, 73, 69, 62],
            [0, 66, 127, 64, 0],
            [66, 97, 81, 73, 70],
            [33, 65, 69, 75, 49],
            [24, 20, 18, 127, 16],
            [39, 69, 69, 69, 57],
            [60, 74, 73, 73, 48],
            [1, 113, 9, 5, 3],
            [54, 73, 73, 73, 54],
            [6, 73, 73, 41, 30],
            [20, 127, 20, 127, 20],
            [0, 54, 54, 0, 0],
            [0, 96, 96, 0, 0],
        ];
        for (index, byte) in text.bytes().enumerate() {
            let glyph = match byte {
                b'0'..=b'9' => usize::from(byte - b'0'),
                b'#' => 10,
                b':' => 11,
                b'.' => 12,
                _ => continue,
            };
            for (column, bits) in GLYPHS[glyph].iter().enumerate() {
                for row in 0..7 {
                    if bits & (1 << row) != 0 {
                        for dy in 0..2 {
                            for dx in 0..2 {
                                let (px, py) = (x + index * 12 + column * 2 + dx, y + row * 2 + dy);
                                if px < self.width && py < self.height {
                                    let offset = (py * self.width + px) * 3;
                                    self.pixels[offset..offset + 3].fill(235);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn save(&self, output: &Path) -> Result<()> {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = sibling_temp_path(output);
        let result = (|| {
            let file = File::create(&temp)?;
            let mut encoder = png::Encoder::new(
                std::io::BufWriter::new(file),
                self.width as u32,
                self.height as u32,
            );
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header()?;
            writer.write_image_data(&self.pixels)?;
            writer.finish()?;
            replace_file_with_backup(&temp, output, "recording review")
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

pub(crate) fn review(
    file: &Path,
    page: Option<u32>,
    frame: Option<u32>,
    output: Option<&Path>,
) -> Result<Value> {
    let file =
        canonical_path(file).with_context(|| format!("Recording not found: {}", file.display()))?;
    let mut video = Video::open(&file)?;
    let selected = selection(&video.times, page, frame)?;
    let mode = if frame.is_some() {
        "frame"
    } else if page.is_some() {
        "page"
    } else {
        "overview"
    };
    let mut folder = file.as_os_str().to_os_string();
    folder.push(".review");
    let name = match (page, frame) {
        (Some(page), _) => format!("page-{page:04}.png"),
        (_, Some(frame)) => format!("frame-{frame:06}.png"),
        _ => "overview.png".to_owned(),
    };
    let output = std::path::absolute(
        output
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(folder).join(name)),
    )?;
    if !output
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
    {
        bail!("Recording review output must be a .png file");
    }
    if canonical_path(&output).ok().as_ref() == Some(&file) {
        bail!("Review output cannot replace the recording");
    }
    let mut sheet = Sheet::new((video.width, video.height), selected.len(), frame.is_none());
    video.render(&selected, &mut sheet)?;
    sheet.save(&output)?;
    let total = video.times.len() as u32;
    let pages = total.div_ceil(PAGE_SIZE);
    let frames = selected
        .iter()
        .map(|&number| {
            json!({
                "frame": number, "timestampMs": video.times[(number - 1) as usize],
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "path": output, "mimeType": "image/png", "recording": file, "mode": mode,
        "width": sheet.width, "height": sheet.height,
        "sourceWidth": video.width, "sourceHeight": video.height,
        "totalFrames": total, "durationMs": video.duration_ms,
        "sampled": mode == "overview" && selected.len() < video.times.len(),
        "allFramesShown": selected.len() == video.times.len(),
        "frames": frames, "page": page, "totalPages": pages, "framesPerPage": PAGE_SIZE,
        "nextPage": page.filter(|page| *page < pages).map(|page| page + 1),
        "inspect": { "pageArgs": ["rf", file.to_string_lossy(), "--page", "1"],
            "frameArgs": ["rf", file.to_string_lossy(), "--frame", selected[0].to_string()] },
    }))
}

pub(super) fn attach_overview(recording: &mut Value) {
    // Finalize first: review failures must leave a successful recording usable.
    let result = recording
        .get("path")
        .and_then(Value::as_str)
        .context("Recording response has no video path")
        .and_then(|path| review(Path::new(path), None, None, None));
    match result {
        Ok(review) => recording["review"] = review,
        Err(error) => recording["reviewError"] = json!(format!("{error:#}")),
    }
}

pub(crate) fn command(args: RecordReviewArgs) -> Result<()> {
    print_json_output(
        &review(&args.file, args.page, args.frame, args.output.as_deref())?,
        false,
    )
}

#[cfg(test)]
#[path = "recording_review_tests.rs"]
mod tests;
