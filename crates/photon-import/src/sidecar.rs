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
}
