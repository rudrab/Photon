//! Test fixtures: tiny real JPEGs with (optional) hand-built EXIF.

use exif::experimental::Writer;
use exif::{Field, In, Rational, Tag, Value};
use std::io::Cursor;
use std::path::Path;

pub struct Spec {
    /// Varies the pixel content so files hash differently.
    pub seed: u8,
    pub exif: bool,
    /// EXIF "YYYY:MM:DD HH:MM:SS".
    pub date: &'static str,
    pub width: u32,
    pub height: u32,
    pub orientation: u16,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            seed: 0,
            exif: true,
            date: "2023:07:04 09:30:00",
            width: 8,
            height: 4,
            orientation: 6,
        }
    }
}

fn ascii(tag: Tag, s: &str) -> Field {
    Field {
        tag,
        ifd_num: In::PRIMARY,
        value: Value::Ascii(vec![s.as_bytes().to_vec()]),
    }
}

fn rationals(tag: Tag, v: &[(u32, u32)]) -> Field {
    Field {
        tag,
        ifd_num: In::PRIMARY,
        value: Value::Rational(v.iter().map(|&(num, denom)| Rational { num, denom }).collect()),
    }
}

fn exif_block(spec: &Spec) -> Vec<u8> {
    let fields = [
        ascii(Tag::Make, "PhotonCam"),
        ascii(Tag::Model, "P1"),
        ascii(Tag::LensModel, "50mm Prime"),
        ascii(Tag::DateTimeOriginal, spec.date),
        Field {
            tag: Tag::PhotographicSensitivity,
            ifd_num: In::PRIMARY,
            value: Value::Short(vec![400]),
        },
        Field {
            tag: Tag::Orientation,
            ifd_num: In::PRIMARY,
            value: Value::Short(vec![spec.orientation]),
        },
        rationals(Tag::FNumber, &[(28, 10)]),
        ascii(Tag::GPSLatitudeRef, "N"),
        rationals(Tag::GPSLatitude, &[(48, 1), (30, 1), (0, 1)]),
        ascii(Tag::GPSLongitudeRef, "W"),
        rationals(Tag::GPSLongitude, &[(2, 1), (15, 1), (0, 1)]),
    ];

    let mut writer = Writer::new();
    for f in &fields {
        writer.push_field(f);
    }
    let mut out = Cursor::new(Vec::new());
    writer.write(&mut out, false).expect("write EXIF");
    out.into_inner()
}

/// Encode a JPEG per `spec` and write it to `path`.
pub fn write_jpeg(path: &Path, spec: &Spec) {
    let img = image::RgbImage::from_fn(spec.width, spec.height, |x, y| {
        image::Rgb([spec.seed, (x * 30) as u8, (y * 60) as u8])
    });
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&img)
        .expect("encode JPEG");

    if spec.exif {
        // SOI, APP1("Exif\0\0" + TIFF), then the rest of the encoded stream.
        let tiff = exif_block(spec);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        let seg_len = (app1.len() + 2) as u16;

        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&seg_len.to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpeg[2..]);
        jpeg = out;
    }

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, jpeg).unwrap();
}
