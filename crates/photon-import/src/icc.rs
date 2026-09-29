//! Colour management: read the ICC profile a photo was saved in, and convert
//! pixels between it and the spaces Photon shows (sRGB) and exports (sRGB,
//! Adobe RGB, Display P3).

use image::{ImageBuffer, Rgb, RgbImage};
use lcms2::{
    CIExyY, CIExyYTRIPLE, Intent, Locale, PixelFormat, Profile, Tag, TagSignature, ToneCurve, Transform, MLU,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub type Rgb16Image = ImageBuffer<Rgb<u16>, Vec<u16>>;

/// The colour space an export is written in, and tagged with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ColorSpace {
    /// For the web, phones and most labs.
    #[default]
    Srgb,
    /// Adobe RGB (1998): wider greens and cyans, for print workflows.
    AdobeRgb,
    /// Display P3: the wide gamut of Apple and recent phone screens.
    DisplayP3,
}

impl ColorSpace {
    pub const ALL: [ColorSpace; 3] = [ColorSpace::Srgb, ColorSpace::AdobeRgb, ColorSpace::DisplayP3];

    pub fn label(self) -> &'static str {
        match self {
            ColorSpace::Srgb => "sRGB",
            ColorSpace::AdobeRgb => "Adobe RGB (1998)",
            ColorSpace::DisplayP3 => "Display P3",
        }
    }

    pub fn profile(self) -> Profile {
        // Both wide spaces share sRGB's D65 white point.
        const D65: CIExyY = CIExyY { x: 0.3127, y: 0.3290, Y: 1.0 };
        let xy = |x, y| CIExyY { x, y, Y: 1.0 };
        let (primaries, curve, name) = match self {
            ColorSpace::Srgb => return Profile::new_srgb(),
            ColorSpace::AdobeRgb => (
                CIExyYTRIPLE { Red: xy(0.64, 0.33), Green: xy(0.21, 0.71), Blue: xy(0.15, 0.06) },
                // The Adobe RGB (1998) specification: gamma 563/256.
                ToneCurve::new(563.0 / 256.0),
                "Adobe RGB (1998) compatible",
            ),
            ColorSpace::DisplayP3 => (
                CIExyYTRIPLE { Red: xy(0.680, 0.320), Green: xy(0.265, 0.690), Blue: xy(0.150, 0.060) },
                // sRGB's transfer curve (IEC 61966-2-1) as ICC parametric type 4.
                ToneCurve::new_parametric(4, &[2.4, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045])
                    .expect("valid parametric curve"),
                "Display P3",
            ),
        };
        let mut profile = Profile::new_rgb(&D65, &primaries, &[&curve, &curve, &curve])
            .expect("a matrix-shaper RGB profile from fixed primaries");
        let mut desc = MLU::new(1);
        desc.set_text(name, Locale::none());
        profile.write_tag(TagSignature::ProfileDescriptionTag, Tag::MLU(&desc));
        profile
    }

    pub fn icc_bytes(self) -> Vec<u8> {
        self.profile().icc().expect("serializing a built-in profile")
    }

    /// The name darktable-cli's `--icc-type` takes for this space.
    pub(crate) fn darktable_name(self) -> &'static str {
        match self {
            ColorSpace::Srgb => "SRGB",
            ColorSpace::AdobeRgb => "ADOBERGB",
            ColorSpace::DisplayP3 => "DISPLAY_P3",
        }
    }
}

/// The profile described by `icc`, or sRGB when there is none or it can't be
/// read (untagged images are sRGB by convention).
pub fn source_profile(icc: Option<&[u8]>) -> Profile {
    icc.and_then(|bytes| match Profile::new_icc(bytes) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("Unreadable embedded ICC profile ({e}); assuming sRGB");
            None
        }
    })
    .unwrap_or_else(Profile::new_srgb)
}

/// Convert 8-bit pixels from colour space `from` to `to`, in place.
pub fn convert_rgb8(img: &mut RgbImage, from: &Profile, to: &Profile) {
    match Transform::<u8, u8>::new(from, PixelFormat::RGB_8, to, PixelFormat::RGB_8, Intent::Perceptual) {
        Ok(t) => t.transform_in_place(img.as_flat_samples_mut().as_mut_slice()),
        Err(e) => log::warn!("No colour transform ({e}); pixels left as they are"),
    }
}

/// Convert 16-bit pixels from colour space `from` to `to`, in place.
pub fn convert_rgb16(img: &mut Rgb16Image, from: &Profile, to: &Profile) {
    // lcms's optimised 16-bit pipelines lose precision near black and at
    // saturated colours (errors of ~1.5 %); unoptimised they're exact.
    let transform = Transform::<[u16; 3], [u16; 3]>::new_flags(
        from,
        PixelFormat::RGB_16,
        to,
        PixelFormat::RGB_16,
        Intent::Perceptual,
        lcms2::Flags::NO_OPTIMIZE,
    );
    match transform {
        Ok(t) => t.transform_in_place(bytemuck::cast_slice_mut(img.as_flat_samples_mut().as_mut_slice())),
        Err(e) => log::warn!("No colour transform ({e}); pixels left as they are"),
    }
}

/// Extract embedded ICC profile bytes from a JPEG file (APP2 ICC_PROFILE chunks).
/// Returns None if no ICC data present.
pub fn extract_jpeg_icc(path: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    let jpeg = img_parts::jpeg::Jpeg::from_bytes(bytes.into()).ok()?;

    let mut chunks: Vec<(u8, Vec<u8>)> = Vec::new();

    for segment in jpeg.segments_by_marker(img_parts::jpeg::markers::APP2) {
        let contents = segment.contents();
        if contents.starts_with(b"ICC_PROFILE\0") && contents.len() >= 14 {
            let seq = contents[12];
            let data = contents[14..].to_vec();
            chunks.push((seq, data));
        }
    }

    if chunks.is_empty() {
        return None;
    }

    chunks.sort_by_key(|c| c.0);

    let mut profile_bytes = Vec::new();
    for (_, data) in chunks {
        profile_bytes.extend_from_slice(&data);
    }

    Some(profile_bytes)
}

/// Extract embedded ICC from a TIFF/ORF/raw file (tag 34675).
pub fn extract_tiff_icc(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;

    if let Some(field) = exif.get_field(exif::Tag(exif::Context::Tiff, 34675), exif::In::PRIMARY) {
        if let exif::Value::Undefined(ref data, _) = field.value {
            return Some(data.clone());
        }
        if let exif::Value::Byte(ref data) = field.value {
            return Some(data.clone());
        }
    }
    None
}

/// Extract ICC from PNG (iCCP chunk).
pub fn extract_png_icc(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut decoder = image::codecs::png::PngDecoder::new(reader).ok()?;
    image::ImageDecoder::icc_profile(&mut decoder)
}

/// Extract ICC from WebP (ICCP chunk).
pub fn extract_webp_icc(path: &Path) -> Option<Vec<u8>> {
    use img_parts::ImageICC;
    let webp = img_parts::webp::WebP::from_bytes(std::fs::read(path).ok()?.into()).ok()?;
    webp.icc_profile().map(|b| b.to_vec())
}

/// Extract the colour profile of a HEIC/AVIF file (`colr` box): an ICC
/// profile as is, or an nclx description with Display P3 primaries as the
/// Display P3 profile (iPhones write either). Other nclx spaces are treated as
/// sRGB.
pub fn extract_heif_icc(path: &Path) -> Option<Vec<u8>> {
    use libheif_rs::{ColorPrimaries, HeifContext};
    let ctx = HeifContext::read_from_file(path.to_str()?).ok()?;
    let handle = ctx.primary_image_handle().ok()?;
    if let Some(raw) = handle.color_profile_raw() {
        if Profile::new_icc(&raw.data).is_ok() {
            return Some(raw.data);
        }
    }
    let nclx = handle.color_profile_nclx()?;
    (nclx.color_primaries() == ColorPrimaries::SMPTE_EG_432_1).then(|| ColorSpace::DisplayP3.icc_bytes())
}

/// Given raw ICC profile bytes, convert an RgbImage from the source profile to sRGB.
/// If icc_bytes is None, returns the image unchanged (assume sRGB).
pub fn to_srgb(mut img: RgbImage, icc_bytes: Option<Vec<u8>>) -> RgbImage {
    if let Some(bytes) = icc_bytes {
        convert_rgb8(&mut img, &source_profile(Some(&bytes)), &Profile::new_srgb());
    }
    img
}

/// Returns the standard sRGB ICC profile bytes (built-in minimal sRGB profile via lcms2).
pub fn srgb_profile_bytes() -> Vec<u8> {
    ColorSpace::Srgb.icc_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_profile_bytes_is_valid_icc() {
        let bytes = srgb_profile_bytes();
        let _profile = Profile::new_icc(&bytes).expect("Failed to parse ICC");
        // Not much to assert beyond that it parses, lcms2 doesn't have an `is_srgb` method on profile
    }

    #[test]
    fn to_srgb_passthrough_when_no_profile() {
        let mut img = RgbImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        let out = to_srgb(img.clone(), None);
        assert_eq!(img.into_raw(), out.into_raw());
    }

    #[test]
    fn wide_spaces_have_named_parseable_profiles() {
        for space in ColorSpace::ALL {
            let profile = Profile::new_icc(&space.icc_bytes()).expect("parses");
            let desc = profile.info(lcms2::InfoType::Description, Locale::none()).unwrap_or_default();
            assert!(!desc.is_empty(), "{space:?} has no description");
        }
    }

    #[test]
    fn srgb_red_sits_inside_the_wider_gamuts() {
        // Adobe RGB shares sRGB's red primary (only its encoding differs:
        // about 219,0,0); Display P3's red is deeper, so sRGB red mixes in
        // some green (about 234,51,35).
        let convert = |space: ColorSpace| {
            let mut img = RgbImage::from_pixel(1, 1, Rgb([255, 0, 0]));
            convert_rgb8(&mut img, &Profile::new_srgb(), &space.profile());
            img.get_pixel(0, 0).0
        };
        let [r, g, b] = convert(ColorSpace::AdobeRgb);
        assert!((210..=228).contains(&r) && g < 8 && b < 8, "Adobe RGB: {r},{g},{b}");
        let [r, g, b] = convert(ColorSpace::DisplayP3);
        assert!((225..=242).contains(&r) && (40..=62).contains(&g) && b < 50, "Display P3: {r},{g},{b}");
    }

    #[test]
    fn sixteen_bit_round_trip_is_nearly_lossless() {
        let mut img = Rgb16Image::from_fn(16, 1, |x, _| Rgb([x as u16 * 4000, 30000, 65535 - x as u16 * 4000]));
        let original = img.clone();
        let (srgb, p3) = (Profile::new_srgb(), ColorSpace::DisplayP3.profile());
        convert_rgb16(&mut img, &srgb, &p3);
        assert_ne!(img, original);
        convert_rgb16(&mut img, &p3, &srgb);
        for (a, b) in img.as_raw().iter().zip(original.as_raw()) {
            assert!(a.abs_diff(*b) < 16, "{a} vs {b}");
        }
    }
}
