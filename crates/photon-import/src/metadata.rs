//! EXIF metadata extraction.
//!
//! Strategy, cheapest first:
//!   1. kamadak-exif over the first 96 KB of the file. JPEG/PNG EXIF always
//!      fits (a JPEG APP1 segment is at most 64 KB). TIFF-based files (RAW,
//!      TIFF) are zero-padded to their real length before parsing: their maker
//!      notes and image data point far past the header, which kamadak rejects
//!      as truncated even though the dates are in the first few KB. (Measured:
//!      0/305 ORFs parsed from a plain 512 KB read; 305/305 from a padded 16 KB
//!      read.) Larger reads happen only when a sub-IFD or a date value really
//!      lies beyond what was read, or for WebP/HEIF.
//!   2. XMP dates (`exif:DateTimeOriginal`, `photoshop:DateCreated`,
//!      `xmp:CreateDate`) from the same header bytes. darktable, Lightroom
//!      and GIMP exports often carry the capture date *only* in XMP, while
//!      their EXIF `DateTime` is the export time.
//!   3. `exiftool -json` (if installed) for RAW/HEIF files kamadak can't
//!      parse (CR3, RAF, exotic containers). This spawns a process (~0.1 s),
//!      so it only runs for those formats and only when no capture date was
//!      found. A JPEG/PNG without EXIF (screenshots, exports) simply has no
//!      date; exiftool would not find one either.
//!
//!   4. EXIF `DateTime` (IFD0 modify date) as a last resort.
//!
//! The file-mtime fallback for missing dates lives in the import engine.

use anyhow::Result;
use chrono::NaiveDateTime;
use exif::{Exif, In, Reader as ExifReader, Tag, Value};
use photon_core::models::ImageFormat;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;
use std::process::Command;

/// Read sizes for EXIF attempts, smallest first.
const FIRST_READ: u64 = 96 * 1024;
const HEADER_BYTES: u64 = 512 * 1024;

#[derive(Debug, Default)]
pub struct ImageMetadata {
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Capture time as a unix timestamp. EXIF stores local wall-clock time
    /// without a zone; it is kept as-is (interpreted as UTC) so the
    /// year/month/day drill-down matches what the camera displayed.
    pub capture_date: Option<i64>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub f_number: Option<f64>,
    pub exposure_time: Option<String>,
    pub iso: Option<i32>,
    pub focal_length: Option<f64>,
    pub lens_model: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// EXIF orientation, 1–8.
    pub orientation: Option<u16>,
}

impl ImageMetadata {
    /// Fill every field still unset from `other`.
    fn merge(&mut self, other: ImageMetadata) {
        macro_rules! fill {
            ($($f:ident),*) => { $( if self.$f.is_none() { self.$f = other.$f; } )* };
        }
        fill!(
            width, height, capture_date, camera_make, camera_model, f_number, exposure_time, iso,
            focal_length, lens_model, latitude, longitude, orientation
        );
    }
}

pub fn extract(path: &Path) -> Result<ImageMetadata> {
    let mut meta = ImageMetadata::default();
    let format = format_of(path);
    let is_raw = format.is_raw();

    let mut modify_date = None;
    if let Ok(exif) = read_exif(path) {
        read_fields(&exif, &mut meta);
        modify_date = ascii(&exif, Tag::DateTime).and_then(|s| parse_exif_date(&s));
    }

    if meta.capture_date.is_none() {
        meta.capture_date = read_xmp_capture_date(path);
    }

    if meta.capture_date.is_none() && may_need_exiftool(format) {
        if let Some(fallback) = exiftool(path) {
            meta.merge(fallback);
        }
    }

    // Least trustworthy: exports stamp DateTime with the export time.
    if meta.capture_date.is_none() {
        meta.capture_date = modify_date;
    }

    // The image crate can't open RAW files, but can read plain-file dimensions cheaply.
    if (meta.width.is_none() || meta.height.is_none()) && !is_raw {
        if let Ok((w, h)) = image::image_dimensions(path) {
            meta.width.get_or_insert(w);
            meta.height.get_or_insert(h);
        }
    }

    Ok(meta)
}

pub fn detect_mime(path: &Path) -> Option<String> {
    infer::get_from_path(path)
        .ok()
        .flatten()
        .map(|t| t.mime_type().to_string())
}

/// Formats where kamadak may miss metadata that exiftool can read.
fn may_need_exiftool(format: ImageFormat) -> bool {
    format.is_raw() || matches!(format, ImageFormat::Heif | ImageFormat::Avif)
}

/// Formats whose EXIF block can sit beyond the first [`HEADER_BYTES`]:
/// TIFF-based RAW (IFDs anywhere), WebP (EXIF chunk after the image data),
/// HEIF/AVIF (the `meta` box may follow `mdat`). JPEG keeps EXIF in APP1 at
/// the start, and PNG writers put `eXIf` before the image data.
fn exif_may_be_beyond_header(format: ImageFormat) -> bool {
    format.is_raw()
        || matches!(
            format,
            ImageFormat::Tiff | ImageFormat::Webp | ImageFormat::Heif | ImageFormat::Avif
        )
}

pub(crate) fn format_of(path: &Path) -> ImageFormat {
    path.extension()
        .and_then(|e| e.to_str())
        .map(ImageFormat::from_extension)
        .unwrap_or(ImageFormat::Unknown)
}

// ═══════════════════════════════════════════════════════════
// kamadak-exif
// ═══════════════════════════════════════════════════════════

/// Parse the EXIF block of any file kamadak can handle, including TIFF-based
/// RAW files with non-standard headers (Olympus ORF, Panasonic RW2).
///
/// Offsets in the result are relative to the start of the file for
/// TIFF-family formats (see [`is_tiff_family`]); only the bytes actually read
/// are real, the rest of `Exif::buf()` is zero padding.
pub(crate) fn read_exif(path: &Path) -> Result<Exif> {
    let file_len = std::fs::metadata(path)?.len();
    let format = format_of(path);
    let tiff_family = is_tiff_family(format);

    // Growing reads, each smaller than the file; the whole file only where
    // EXIF can legitimately sit anywhere.
    let first = FIRST_READ.min(file_len);
    let mut limits = vec![first];
    if exif_may_be_beyond_header(format) {
        limits.extend([HEADER_BYTES, file_len].into_iter().filter(|&l| l > first));
        limits.dedup();
    }

    let mut last: Option<Result<Exif>> = None;
    for (i, &limit) in limits.iter().enumerate() {
        let buf = read_prefix(path, limit)?;
        let bytes_read = buf.len() as u64;
        let buf = if tiff_family { pad_to(buf, file_len) } else { buf };

        let parsed = parse_exif_bytes(buf);
        let is_last = i + 1 == limits.len();
        match &parsed {
            Ok(exif) if is_last || !tiff_family || is_complete(exif, bytes_read) => return parsed,
            _ => last = Some(parsed),
        }
    }
    last.unwrap_or_else(|| Err(anyhow::anyhow!("empty file")))
}

fn read_prefix(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(limit as usize);
    File::open(path)?.take(limit).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Extend `buf` with zeros to `len` bytes. Large zeroed allocations are
/// lazily mapped by the OS, so this costs address space, not memory or I/O.
fn pad_to(buf: Vec<u8>, len: u64) -> Vec<u8> {
    if buf.len() as u64 >= len {
        return buf;
    }
    let mut padded = vec![0u8; len as usize];
    padded[..buf.len()].copy_from_slice(&buf);
    padded
}

/// Whether a parse of a padded partial read saw everything we need: no
/// sub-IFD starts beyond the read, and every date tag has a real value (a
/// value stored beyond the read shows up as zeros).
fn is_complete(exif: &Exif, bytes_read: u64) -> bool {
    let pointers_ok = [Tag::ExifIFDPointer, Tag::GPSInfoIFDPointer]
        .into_iter()
        .filter_map(|tag| exif.get_field(tag, In::PRIMARY)?.value.get_uint(0))
        .all(|offset| (offset as u64) < bytes_read);
    let dates_ok = [Tag::DateTimeOriginal, Tag::DateTimeDigitized, Tag::DateTime]
        .into_iter()
        .filter(|&tag| exif.get_field(tag, In::PRIMARY).is_some())
        .all(|tag| ascii(exif, tag).and_then(|s| parse_exif_date(&s)).is_some());
    pointers_ok && dates_ok
}

// ═══════════════════════════════════════════════════════════
// XMP
// ═══════════════════════════════════════════════════════════

/// How far into a file to look for an XMP packet. In JPEG it is the APP1
/// segment after EXIF (each at most 64 KB); editors put it near the start of
/// PNG/TIFF too (darktable: within the first 28 KB).
const XMP_SCAN_BYTES: u64 = 192 * 1024;

/// Capture date from an XMP packet in the file header, if any.
fn read_xmp_capture_date(path: &Path) -> Option<i64> {
    let buf = read_prefix(path, XMP_SCAN_BYTES).ok()?;
    xmp_capture_date(&buf)
}

fn xmp_capture_date(buf: &[u8]) -> Option<i64> {
    let start = find_bytes(buf, b"<x:xmpmeta")?;
    let end = find_bytes(&buf[start..], b"</x:xmpmeta>").map_or(buf.len(), |e| start + e);
    let xmp = String::from_utf8_lossy(&buf[start..end]);
    ["exif:DateTimeOriginal", "photoshop:DateCreated", "xmp:CreateDate"]
        .into_iter()
        .find_map(|name| xmp_value(&xmp, name).and_then(|v| parse_exif_date(&v)))
}

/// Value of an XMP property in either attribute (`name="v"`) or element
/// (`<name>v</name>`) form.
fn xmp_value(xmp: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let key = format!("{name}={quote}");
        if let Some(i) = xmp.find(&key) {
            let rest = &xmp[i + key.len()..];
            return rest.find(quote).map(|j| rest[..j].to_string());
        }
    }
    let open = format!("<{name}>");
    let i = xmp.find(&open)? + open.len();
    let j = xmp[i..].find('<')?;
    Some(xmp[i..i + j].trim().to_string())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// TIFF-structured formats, whose IFD offsets are relative to the file start.
pub(crate) fn is_tiff_family(format: ImageFormat) -> bool {
    format.is_raw() || format == ImageFormat::Tiff
}

fn parse_exif_bytes(mut buf: Vec<u8>) -> Result<Exif> {
    let reader = ExifReader::new();

    if let Ok(exif) = reader.read_from_container(&mut Cursor::new(&buf)) {
        return Ok(exif);
    }

    // Olympus/Panasonic RAW are TIFF with a private magic number; patch it to
    // the standard one and parse as raw TIFF (all offsets are file-relative).
    if patch_raw_tiff_magic(&mut buf) {
        return Ok(reader.read_raw(buf)?);
    }

    anyhow::bail!("no EXIF block found")
}

/// Rewrite vendor TIFF magic ("IIRO", "IIRS", "IIU\0", "MMOR") to plain TIFF.
/// Returns whether the buffer looked like one of those.
fn patch_raw_tiff_magic(buf: &mut [u8]) -> bool {
    if buf.len() < 8 {
        return false;
    }
    match &buf[..4] {
        b"IIRO" | b"IIRS" | [b'I', b'I', b'U', 0] => {
            buf[2..4].copy_from_slice(&[0x2a, 0x00]);
            true
        }
        b"MMOR" => {
            buf[2..4].copy_from_slice(&[0x00, 0x2a]);
            true
        }
        _ => false,
    }
}

fn read_fields(exif: &Exif, meta: &mut ImageMetadata) {
    // Capture dates only; the IFD0 modify date is a separate, last fallback.
    meta.capture_date = [Tag::DateTimeOriginal, Tag::DateTimeDigitized]
        .into_iter()
        .find_map(|tag| ascii(exif, tag).and_then(|s| parse_exif_date(&s)));

    meta.camera_make = ascii(exif, Tag::Make);
    meta.camera_model = ascii(exif, Tag::Model);
    meta.lens_model = ascii(exif, Tag::LensModel);

    meta.f_number = rational(exif, Tag::FNumber);
    meta.focal_length = rational(exif, Tag::FocalLength);
    meta.exposure_time = exif
        .get_field(Tag::ExposureTime, In::PRIMARY)
        .map(|f| f.display_value().to_string());
    meta.iso = uint(exif, Tag::PhotographicSensitivity).map(|v| v as i32);

    meta.width = uint(exif, Tag::PixelXDimension).or_else(|| uint(exif, Tag::ImageWidth));
    meta.height = uint(exif, Tag::PixelYDimension).or_else(|| uint(exif, Tag::ImageLength));

    meta.orientation = uint(exif, Tag::Orientation)
        .filter(|o| (1..=8).contains(o))
        .map(|o| o as u16);

    meta.latitude = gps_coordinate(exif, Tag::GPSLatitude, Tag::GPSLatitudeRef, b'S', 90.0);
    meta.longitude = gps_coordinate(exif, Tag::GPSLongitude, Tag::GPSLongitudeRef, b'W', 180.0);
}

fn ascii(exif: &Exif, tag: Tag) -> Option<String> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    let Value::Ascii(parts) = &field.value else {
        return None;
    };
    let s = clean_str(&String::from_utf8_lossy(parts.first()?));
    (!s.is_empty()).then_some(s)
}

fn uint(exif: &Exif, tag: Tag) -> Option<u32> {
    exif.get_field(tag, In::PRIMARY)?.value.get_uint(0)
}

fn rational(exif: &Exif, tag: Tag) -> Option<f64> {
    match &exif.get_field(tag, In::PRIMARY)?.value {
        Value::Rational(v) => v.first().map(|r| r.to_f64()).filter(|f| f.is_finite()),
        _ => None,
    }
}

/// Degrees/minutes/seconds + hemisphere reference → signed decimal degrees.
fn gps_coordinate(exif: &Exif, tag: Tag, ref_tag: Tag, negative_ref: u8, limit: f64) -> Option<f64> {
    let Value::Rational(dms) = &exif.get_field(tag, In::PRIMARY)?.value else {
        return None;
    };
    let [deg, min, sec] = dms.as_slice() else {
        return None;
    };
    let mut value = deg.to_f64() + min.to_f64() / 60.0 + sec.to_f64() / 3600.0;

    let hemisphere = exif.get_field(ref_tag, In::PRIMARY).and_then(|f| match &f.value {
        Value::Ascii(v) => v.first().and_then(|s| s.first().copied()),
        _ => None,
    });
    if hemisphere.map(|h| h.to_ascii_uppercase()) == Some(negative_ref) {
        value = -value;
    }

    (value.is_finite() && value.abs() <= limit).then_some(value)
}

// ═══════════════════════════════════════════════════════════
// exiftool fallback
// ═══════════════════════════════════════════════════════════

fn exiftool(path: &Path) -> Option<ImageMetadata> {
    // -n: machine-readable values (signed GPS, numeric exposure, raw date strings).
    let output = Command::new("exiftool")
        .args(["-json", "-n", "-q"])
        .args([
            "-DateTimeOriginal",
            "-CreateDate",
            "-Make",
            "-Model",
            "-LensModel",
            "-FNumber",
            "-ExposureTime",
            "-ISO",
            "-FocalLength",
            "-ImageWidth",
            "-ImageHeight",
            "-GPSLatitude",
            "-GPSLongitude",
            "-Orientation",
        ])
        .arg(path)
        .output()
        .ok()?; // exiftool not installed

    if !output.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let tags = json.get(0)?;

    let text = |key: &str| tags.get(key).and_then(|v| v.as_str()).map(clean_str);
    let num = |key: &str| tags.get(key).and_then(|v| v.as_f64());

    Some(ImageMetadata {
        capture_date: text("DateTimeOriginal")
            .or_else(|| text("CreateDate"))
            .and_then(|s| parse_exif_date(&s)),
        camera_make: text("Make"),
        camera_model: text("Model"),
        lens_model: text("LensModel"),
        f_number: num("FNumber"),
        exposure_time: num("ExposureTime").map(format_exposure),
        iso: num("ISO").map(|v| v as i32),
        focal_length: num("FocalLength"),
        width: num("ImageWidth").map(|v| v as u32),
        height: num("ImageHeight").map(|v| v as u32),
        latitude: num("GPSLatitude"),
        longitude: num("GPSLongitude"),
        orientation: num("Orientation")
            .map(|v| v as u16)
            .filter(|o| (1..=8).contains(o)),
    })
}

/// 0.004 → "1/250", 2.0 → "2".
fn format_exposure(seconds: f64) -> String {
    if seconds > 0.0 && seconds < 1.0 {
        format!("1/{}", (1.0 / seconds).round() as u32)
    } else {
        format!("{}", seconds)
    }
}

// ═══════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════

fn parse_exif_date(raw: &str) -> Option<i64> {
    const FORMATS: &[&str] = &[
        "%Y:%m:%d %H:%M:%S%.f", // standard EXIF (fractional seconds optional)
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y:%m:%d %H:%M",
        "%Y-%m-%d %H:%M",
    ];

    let s = clean_str(raw);
    if s.is_empty() || s.starts_with("0000") {
        return None;
    }

    if let Some(ts) = FORMATS
        .iter()
        .find_map(|fmt| NaiveDateTime::parse_from_str(&s, fmt).ok())
    {
        return Some(ts.and_utc().timestamp());
    }

    // Date and time followed by something else (XMP zone suffix "+05:30"):
    // keep the wall-clock time, like EXIF.
    if let Some(prefix) = s.get(..19) {
        if let Some(ts) = ["%Y-%m-%dT%H:%M:%S", "%Y:%m:%d %H:%M:%S", "%Y-%m-%d %H:%M:%S"]
            .iter()
            .find_map(|fmt| NaiveDateTime::parse_from_str(prefix, fmt).ok())
        {
            return Some(ts.and_utc().timestamp());
        }
    }

    // Date only, or a date followed by something unparseable.
    let date = s.get(..10)?.replace(':', "-");
    let day = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok()?;
    Some(day.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
}

fn clean_str(s: &str) -> String {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\0' || c == '"' || c == '\'')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    #[test]
    fn parses_exif_date_variants() {
        let expect = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_opt(15, 13, 32)
            .unwrap()
            .and_utc()
            .timestamp();
        assert_eq!(parse_exif_date("2024:01:01 15:13:32"), Some(expect));
        assert_eq!(parse_exif_date("2024-01-01T15:13:32"), Some(expect));
        assert_eq!(parse_exif_date("2024:01:01 15:13:32.45"), Some(expect));
        assert_eq!(parse_exif_date("2024:01:01 15:13:32\0"), Some(expect));
        assert!(parse_exif_date("0000:00:00 00:00:00").is_none());
        assert!(parse_exif_date("garbage").is_none());
    }

    #[test]
    fn slow_fallbacks_only_apply_where_they_can_help() {
        for f in [ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::Gif, ImageFormat::Unknown] {
            assert!(!may_need_exiftool(f), "{f:?}");
            assert!(!exif_may_be_beyond_header(f), "{f:?}");
        }
        for f in [ImageFormat::RawOrf, ImageFormat::RawCr3, ImageFormat::Heif] {
            assert!(may_need_exiftool(f), "{f:?}");
            assert!(exif_may_be_beyond_header(f), "{f:?}");
        }
        assert!(exif_may_be_beyond_header(ImageFormat::Webp));
        assert!(!may_need_exiftool(ImageFormat::Webp));
    }

    #[test]
    fn reads_xmp_dates_in_attribute_and_element_form() {
        let at = |s: &str| {
            let dt = chrono::DateTime::from_timestamp(parse_exif_date(s).unwrap(), 0).unwrap();
            dt.format("%Y-%m-%d %H:%M:%S").to_string()
        };
        let fmt = |ts: i64| {
            let dt = chrono::DateTime::from_timestamp(ts, 0).unwrap();
            dt.format("%Y-%m-%d %H:%M:%S").to_string()
        };
        // exif:DateTimeOriginal wins over xmp:CreateDate.
        let attr = br#"junk<x:xmpmeta><rdf:Description exif:DateTimeOriginal="2023:04:08 19:45:41.000" xmp:CreateDate="2020-01-01T00:00:00"/></x:xmpmeta>"#;
        assert_eq!(fmt(xmp_capture_date(attr).unwrap()), "2023-04-08 19:45:41");

        let elem = b"<x:xmpmeta><xmp:CreateDate>2021-05-06T07:08:09+05:30</xmp:CreateDate></x:xmpmeta>";
        assert_eq!(fmt(xmp_capture_date(elem).unwrap()), "2021-05-06 07:08:09");

        assert!(xmp_capture_date(b"no xmp here").is_none());
        assert_eq!(at("2021-05-06T07:08:09+05:30"), "2021-05-06 07:08:09");
    }

    #[test]
    fn xmp_date_beats_exif_modify_date() {
        // An export: EXIF has only DateTime (export time); XMP has the capture date.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.jpg");
        testutil::write_jpeg(&path, &testutil::Spec { exif: false, ..Default::default() });
        let jpeg = std::fs::read(&path).unwrap();
        let xmp = b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta><rdf:Description exif:DateTimeOriginal=\"2019:02:03 04:05:06\"/></x:xmpmeta>";
        let mut out = jpeg[..2].to_vec();
        out.extend([0xFF, 0xE1]);
        out.extend(((xmp.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(xmp);
        out.extend_from_slice(&jpeg[2..]);
        std::fs::write(&path, out).unwrap();

        let meta = extract(&path).unwrap();
        let date = chrono::DateTime::from_timestamp(meta.capture_date.unwrap(), 0).unwrap();
        assert_eq!(date.format("%Y-%m-%d %H:%M:%S").to_string(), "2019-02-03 04:05:06");
    }

    #[test]
    fn formats_exposure() {
        assert_eq!(format_exposure(0.004), "1/250");
        assert_eq!(format_exposure(2.0), "2");
    }

    #[test]
    fn patches_olympus_magic() {
        let mut buf = *b"IIRO\x08\x00\x00\x00";
        assert!(patch_raw_tiff_magic(&mut buf));
        assert_eq!(&buf[..4], b"II\x2a\x00");
        let mut standard = *b"II*\0\x08\0\0\0";
        assert!(!patch_raw_tiff_magic(&mut standard));
    }

    #[test]
    fn reads_full_exif_from_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        testutil::write_jpeg(&path, &testutil::Spec::default());

        let meta = extract(&path).unwrap();
        assert_eq!(meta.camera_make.as_deref(), Some("PhotonCam"));
        assert_eq!(meta.camera_model.as_deref(), Some("P1"));
        assert_eq!(meta.lens_model.as_deref(), Some("50mm Prime"));
        assert_eq!(meta.iso, Some(400));
        assert_eq!(meta.orientation, Some(6));
        assert_eq!(meta.f_number, Some(2.8));
        assert_eq!((meta.width, meta.height), (Some(8), Some(4)));

        // 48°30'0" N, 2°15'0" W
        assert_eq!(meta.latitude, Some(48.5));
        assert_eq!(meta.longitude, Some(-2.25));

        let date = chrono::DateTime::from_timestamp(meta.capture_date.unwrap(), 0).unwrap();
        assert_eq!(date.format("%Y-%m-%d %H:%M:%S").to_string(), "2023-07-04 09:30:00");
    }

    #[test]
    fn file_without_exif_yields_no_date() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.jpg");
        testutil::write_jpeg(&path, &testutil::Spec { exif: false, ..Default::default() });

        let meta = extract(&path).unwrap();
        assert!(meta.capture_date.is_none());
        assert_eq!((meta.width, meta.height), (Some(8), Some(4))); // image-crate fallback
    }
}
