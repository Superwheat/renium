use super::*;
use mp4::{AvcConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig};
use openh264::encoder::{Encoder, EncoderConfig, IntraFramePeriod, Profile};
use openh264::formats::{RgbSliceU8, YUVBuffer};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(crate::app::output::automation_token("recording-review"));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn video(&self, count: u32) -> PathBuf {
        self.video_sized(count, 64, 48)
    }

    fn video_sized(&self, count: u32, width: usize, height: usize) -> PathBuf {
        let path = self.0.join("clip with spaces.mp4");
        let mut encoder = Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            EncoderConfig::new()
                .profile(Profile::Baseline)
                .skip_frames(false)
                .intra_frame_period(IntraFramePeriod::from_num_frames(4)),
        )
        .unwrap();
        let mut writer = Mp4Writer::write_start(
            File::create(&path).unwrap(),
            &Mp4Config {
                major_brand: "isom".parse().unwrap(),
                minor_version: 512,
                compatible_brands: vec!["isom".parse().unwrap()],
                timescale: 1000,
            },
        )
        .unwrap();
        let mut start = 0;
        for index in 0..count {
            let color = if index % 2 == 0 {
                [240, 10, 10]
            } else {
                [10, 10, 240]
            };
            let mut rgb = color.repeat(width * height);
            if width >= 640 {
                // A moving bright square makes frame order visible in the contact sheet.
                let left = (index as usize * 13) % (width - 80);
                for y in height / 3..height / 3 + 80 {
                    for x in left..left + 80 {
                        rgb[(y * width + x) * 3..(y * width + x) * 3 + 3].fill(235);
                    }
                }
            }
            let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(&rgb, (width, height)));
            let bits = encoder.encode(&yuv).unwrap();
            let (mut sps, mut pps, mut bytes, mut sync) = (vec![], vec![], vec![], false);
            for layer in 0..bits.num_layers() {
                let layer = bits.layer(layer).unwrap();
                for nal in 0..layer.nal_count() {
                    let unit = layer.nal_unit(nal).unwrap();
                    let payload = if unit.starts_with(&[0, 0, 0, 1]) {
                        &unit[4..]
                    } else {
                        &unit[3..]
                    };
                    match payload[0] & 31 {
                        7 => sps = payload.to_vec(),
                        8 => pps = payload.to_vec(),
                        kind => {
                            sync |= kind == 5;
                            bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                            bytes.extend_from_slice(payload);
                        }
                    }
                }
            }
            if index == 0 {
                writer
                    .add_track(&TrackConfig::from(AvcConfig {
                        width: width as u16,
                        height: height as u16,
                        seq_param_set: sps,
                        pic_param_set: pps,
                    }))
                    .unwrap();
            }
            let duration = if index % 3 == 0 { 83 } else { 147 };
            writer
                .write_sample(
                    1,
                    &Mp4Sample {
                        start_time: start,
                        duration,
                        rendering_offset: 0,
                        is_sync: sync,
                        bytes: bytes.into(),
                    },
                )
                .unwrap();
            start += u64::from(duration);
        }
        writer.write_end().unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn png_pixels(path: &Path) -> (png::OutputInfo, Vec<u8>) {
    let mut reader = png::Decoder::new(File::open(path).unwrap())
        .read_info()
        .unwrap();
    let mut pixels = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut pixels).unwrap();
    (info, pixels)
}

#[test]
fn recording_review_covers_every_frame_with_timestamps_and_full_resolution() {
    let fixture = Fixture::new();
    let video = fixture.video(29);
    let original = fs::read(&video).unwrap();
    let overview = review(&video, None, None, None).unwrap();
    assert_eq!(overview["sampled"], true);
    assert_eq!(overview["frames"][0]["frame"], 1);
    assert_eq!(
        overview["frames"].as_array().unwrap().last().unwrap()["frame"],
        29
    );
    let mut numbers = Vec::new();
    for page in 1..=3 {
        let result = review(&video, Some(page), None, None).unwrap();
        assert_eq!(result["mode"], "page");
        assert_eq!(result["totalFrames"], 29);
        for item in result["frames"].as_array().unwrap() {
            numbers.push(item["frame"].as_u64().unwrap());
        }
        assert_eq!(
            result["nextPage"],
            if page < 3 {
                json!(page + 1)
            } else {
                Value::Null
            }
        );
    }
    assert_eq!(numbers, (1..=29).collect::<Vec<_>>());
    // Seek across GOPs, including the final P-frame; compare decoded colors, not only file existence.
    for frame in [1, 2, 12, 13, 28, 29] {
        let result = review(&video, None, Some(frame), None).unwrap();
        let (info, pixels) = png_pixels(Path::new(result["path"].as_str().unwrap()));
        assert_eq!((info.width, info.height), (64, 48));
        let expected_channel = if frame % 2 == 1 { 0 } else { 2 };
        assert!(
            pixels[expected_channel] > 190,
            "frame {frame}: {:?}",
            &pixels[..3]
        );
        assert!(pixels[1] < 50);
    }
    let second = review(&video, None, Some(2), None).unwrap();
    assert_eq!(second["frames"][0]["timestampMs"], 83);
    assert_eq!(fs::read(video).unwrap(), original);
}

#[test]
fn recording_review_short_clip_and_invalid_requests() {
    let fixture = Fixture::new();
    let video = fixture.video(1);
    let result = review(&video, None, None, None).unwrap();
    assert_eq!(result["sampled"], false);
    assert_eq!(result["totalPages"], 1);
    assert!(review(&video, Some(0), None, None).is_err());
    assert!(review(&video, Some(2), None, None).is_err());
    assert!(review(&video, None, Some(0), None).is_err());
    assert!(review(&video, None, Some(2), None).is_err());
    assert!(review(&video, Some(1), Some(1), None).is_err());
    assert!(review(&video, None, None, Some(&video)).is_err());
    let corrupt = fixture.0.join("corrupt.mp4");
    fs::write(&corrupt, b"not an mp4").unwrap();
    assert!(review(&corrupt, None, None, None).is_err());
    let mut bytes = Vec::new();
    for sample in [
        &[][..],
        &[0, 0, 0][..],
        &[0, 0, 0, 9, 1][..],
        &[0, 0, 0, 0][..],
    ] {
        assert!(annex_b(sample, 4, &mut bytes).is_err());
    }
}

#[test]
fn recording_review_portrait_and_thin_ui_downsampling() {
    let mut sheet = Sheet::new((2, 600), 1, true);
    let mut rgb = vec![0; 2 * 600 * 3];
    for row in 0..600 {
        rgb[row * 6..row * 6 + 3].fill(240);
    }
    sheet.add(0, &rgb, 123, 4567);
    assert_eq!(sheet.tile, (1, 270));
    assert_eq!(&sheet.pixels[..3], &[120, 120, 120]);
    assert!(sheet.pixels[sheet.width * sheet.tile.1 * 3..].contains(&235));
}

#[test]
fn recording_review_large_frames_and_cli_arguments() {
    let fixture = Fixture::new();
    let video = fixture.video_sized(25, 1280, 720);
    let result = review(&video, None, None, None).unwrap();
    let (info, _) = png_pixels(Path::new(result["path"].as_str().unwrap()));
    assert_eq!((info.width, info.height), (1440, 1176));
    assert_eq!(result["frames"].as_array().unwrap().len(), 12);
    for arguments in [
        vec!["rbx", "rf", "clip.mp4", "--page", "0"],
        vec!["rbx", "rf", "clip.mp4", "--page", "1", "--frame", "1"],
    ] {
        assert!(
            crate::cli::command()
                .try_get_matches_from(arguments)
                .is_err()
        );
    }
    assert!(
        crate::cli::command()
            .try_get_matches_from(["rbx", "re", "--no-review"])
            .is_ok()
    );
    assert!(
        crate::cli::command()
            .try_get_matches_from(["rbx", "record-frames", "clip.mp4", "--frame", "1"])
            .is_ok()
    );
    // Optional visual-QA artifact export; normal tests leave nothing behind.
    if let Some(destination) = std::env::var_os("RENIUM_RECORDING_REVIEW_ARTIFACTS") {
        fs::create_dir_all(&destination).unwrap();
        fs::copy(&video, Path::new(&destination).join("capture-fixture.mp4")).unwrap();
        fs::copy(
            result["path"].as_str().unwrap(),
            Path::new(&destination).join("overview.png"),
        )
        .unwrap();
    }
}
