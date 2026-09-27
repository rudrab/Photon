//! Sidecar handling: XMP files and grouping of files that belong to one shot.

use photon_core::models::strip_edit_suffix;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Find the XMP sidecar for `path`, if any.
///
/// Darktable writes "IMG_001.ORF.xmp"; other tools write "IMG_001.xmp".
pub fn find_xmp(path: &Path) -> Option<PathBuf> {
    let mut appended = path.as_os_str().to_owned();
    appended.push(".xmp");
    let appended = PathBuf::from(appended);
    if appended.exists() {
        return Some(appended);
    }

    let replaced = path.with_extension("xmp");
    replaced.exists().then_some(replaced)
}

/// Where the XMP for `src` should live once `src` has been placed at `dest`.
pub fn xmp_destination(src: &Path, xmp: &Path, dest: &Path) -> PathBuf {
    if xmp.extension().is_some() && xmp.file_stem() == src.file_name() {
        let mut appended = dest.as_os_str().to_owned();
        appended.push(".xmp");
        PathBuf::from(appended)
    } else {
        dest.with_extension("xmp")
    }
}

/// Map each file that is a variant of the same shot as another file (same
/// directory, same base stem once edit suffixes are stripped) to a shared
/// group hash. Works on paths alone, so it can run before any file is read.
///
///   /photos/IMG_001.ORF, /photos/IMG_001.JPG, /photos/IMG_001_modified.jpg
///
/// All three map to one group. Singletons are absent from the map.
pub fn group_map(paths: &[PathBuf]) -> HashMap<PathBuf, String> {
    let mut groups: HashMap<(PathBuf, String), Vec<&PathBuf>> = HashMap::new();
    for path in paths {
        let parent = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        groups
            .entry((parent, strip_edit_suffix(&stem).to_string()))
            .or_default()
            .push(path);
    }

    let mut map = HashMap::new();
    for ((parent, stem), members) in groups {
        if members.len() < 2 {
            continue;
        }
        let key = format!("{}\0{}", parent.display(), stem);
        let group_hash = blake3::hash(key.as_bytes()).to_hex()[..16].to_string();
        for path in members {
            map.insert(path.clone(), group_hash.clone());
        }
    }
    map
}

/// Write or update an XMP sidecar file with rating, flag, tags, title, and description.
/// This writes metadata non-destructively to an adjacent sidecar file, preserving originals.
pub fn sync_xmp_metadata(
    image_path: &Path,
    rating: Option<i32>,
    flag: Option<i32>,
    tags: &[String],
    title: Option<&str>,
    description: Option<&str>,
) -> Result<PathBuf, std::io::Error> {
    let xmp_path = find_xmp(image_path).unwrap_or_else(|| {
        let mut appended = image_path.as_os_str().to_owned();
        appended.push(".xmp");
        PathBuf::from(appended)
    });

    let rating_attr = rating
        .map(|r| format!(" xmp:Rating=\"{}\"", r))
        .unwrap_or_default();
    let flag_attr = flag
        .map(|f| {
            let label = match f {
                1 => "Pick",
                -1 => "Reject",
                _ => "",
            };
            if !label.is_empty() {
                format!(" xmp:Label=\"{}\"", label)
            } else {
                String::new()
            }
        })
        .unwrap_or_default();

    let mut tags_xml = String::new();
    if !tags.is_empty() {
        tags_xml.push_str("   <dc:subject>\n    <rdf:Bag>\n");
        for tag in tags {
            tags_xml.push_str(&format!(
                "     <rdf:li>{}</rdf:li>\n",
                quick_xml_escape(tag)
            ));
        }
        tags_xml.push_str("    </rdf:Bag>\n   </dc:subject>\n");
    }

    let title_xml = title
        .filter(|t| !t.trim().is_empty())
        .map(|t| {
            format!(
                "   <dc:title>\n    <rdf:Alt>\n     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n    </rdf:Alt>\n   </dc:title>\n",
                quick_xml_escape(t)
            )
        })
        .unwrap_or_default();

    let desc_xml = description
        .filter(|d| !d.trim().is_empty())
        .map(|d| {
            format!(
                "   <dc:description>\n    <rdf:Alt>\n     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n    </rdf:Alt>\n   </dc:description>\n",
                quick_xml_escape(d)
            )
        })
        .unwrap_or_default();

    let content = format!(
        r#"<?xpacket begin="﻿" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"{}{}>
{title_xml}{desc_xml}{tags_xml}  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>
"#,
        rating_attr, flag_attr
    );

    let temp_path = xmp_path.with_extension("xmp.tmp");
    std::fs::write(&temp_path, content)?;
    std::fs::rename(&temp_path, &xmp_path)?;

    Ok(xmp_path)
}

fn quick_xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_raw_jpeg_and_edits_but_not_strangers() {
        let paths: Vec<PathBuf> = [
            "/p/IMG_001.ORF",
            "/p/IMG_001.JPG",
            "/p/IMG_001_modified.jpg",
            "/p/IMG_002.JPG",
            "/q/IMG_001.JPG", // same stem, different directory
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let groups = group_map(&paths);

        let g = &groups[&paths[0]];
        assert_eq!(&groups[&paths[1]], g);
        assert_eq!(&groups[&paths[2]], g);
        assert!(!groups.contains_key(&paths[3]));
        assert!(!groups.contains_key(&paths[4]));
    }

    #[test]
    fn xmp_follows_its_image() {
        let src = Path::new("/card/IMG_001.ORF");
        let dest = Path::new("/lib/2024/01/01/IMG_001_1.ORF");
        assert_eq!(
            xmp_destination(src, Path::new("/card/IMG_001.ORF.xmp"), dest),
            Path::new("/lib/2024/01/01/IMG_001_1.ORF.xmp")
        );
        assert_eq!(
            xmp_destination(src, Path::new("/card/IMG_001.xmp"), dest),
            Path::new("/lib/2024/01/01/IMG_001_1.xmp")
        );
    }

    #[test]
    fn sync_xmp_metadata_writes_valid_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("photo.jpg");
        std::fs::write(&img, b"fake jpg").unwrap();

        let tags = vec!["landscape".to_string(), "sunset".to_string()];
        let xmp = sync_xmp_metadata(
            &img,
            Some(4),
            Some(1),
            &tags,
            Some("Golden Hour"),
            Some("Beach sunset during golden hour"),
        )
        .unwrap();

        assert!(xmp.exists());
        let xml = std::fs::read_to_string(&xmp).unwrap();
        assert!(xml.contains("xmp:Rating=\"4\""));
        assert!(xml.contains("xmp:Label=\"Pick\""));
        assert!(xml.contains("<rdf:li>landscape</rdf:li>"));
        assert!(xml.contains("<rdf:li>sunset</rdf:li>"));
        assert!(xml.contains("Golden Hour"));
        assert!(xml.contains("Beach sunset during golden hour"));
    }
}
