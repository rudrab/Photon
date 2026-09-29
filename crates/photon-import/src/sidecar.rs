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

/// The fields to change in a photo's XMP sidecar. `None` leaves a field as it
/// is in the file, so an edit to one field never rewrites the others.
#[derive(Debug, Clone, Copy, Default)]
pub struct XmpUpdate<'a> {
    /// Stars, 0–5. Written as `xmp:Rating`.
    pub rating: Option<i32>,
    /// Rejected photos are written as `xmp:Rating="-1"` (the convention of
    /// Lightroom, Bridge and darktable), overriding `rating`.
    pub rejected: Option<bool>,
    /// The photo's keywords (`dc:subject`), together with every keyword the
    /// library knows: keywords in the file that the library doesn't know
    /// (e.g. from Lightroom) are kept; known ones follow `tags`.
    pub keywords: Option<Keywords<'a>>,
    /// Empty removes the title.
    pub title: Option<&'a str>,
    /// Empty removes the description.
    pub description: Option<&'a str>,
    /// EXIF orientation tag (1–8), written as `tiff:Orientation`.
    pub orientation: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
pub struct Keywords<'a> {
    pub tags: &'a [String],
    pub known: &'a [String],
}

const XMP_NS: &str = "http://ns.adobe.com/xap/1.0/";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const TIFF_NS: &str = "http://ns.adobe.com/tiff/1.0/";

const EMPTY_XMP: &str = "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">
  <rdf:Description rdf:about=\"\"/>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end=\"w\"?>
";

/// Merge `update` into the XMP sidecar of `image_path`, creating one
/// (`IMG_001.ORF.xmp`, as darktable names it) if there is none.
///
/// Everything else in an existing sidecar — darktable's edit history,
/// Lightroom's develop settings, other tools' fields — is left byte for byte
/// as it was. A sidecar that can't be understood is not touched, and one
/// that would not change is not rewritten.
pub fn sync_xmp_metadata(image_path: &Path, update: &XmpUpdate) -> std::io::Result<PathBuf> {
    let existing = find_xmp(image_path);
    let xmp_path = existing.clone().unwrap_or_else(|| {
        let mut appended = image_path.as_os_str().to_owned();
        appended.push(".xmp");
        PathBuf::from(appended)
    });

    let original = match &existing {
        Some(path) => std::fs::read_to_string(path)?,
        None => EMPTY_XMP.to_string(),
    };
    let merged = merge_xmp(&original, update).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: no rdf:Description; not modifying it", xmp_path.display()),
        )
    })?;

    if existing.is_some() && merged == original {
        return Ok(xmp_path);
    }

    let mut temp_path = xmp_path.as_os_str().to_owned();
    temp_path.push(".tmp");
    let temp_path = PathBuf::from(temp_path);
    let written = (|| {
        let mut file = std::fs::File::create(&temp_path)?;
        std::io::Write::write_all(&mut file, merged.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp_path, &xmp_path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    written.map(|()| xmp_path)
}

/// An XMP packet: `doc` (or an empty packet) with `update` applied.
pub(crate) fn merged_xmp(doc: Option<&str>, update: &XmpUpdate) -> Option<String> {
    doc.and_then(|d| merge_xmp(d, update)).or_else(|| merge_xmp(EMPTY_XMP, update))
}

/// `doc` with `update` applied, or `None` if `doc` has no `rdf:Description`.
fn merge_xmp(doc: &str, update: &XmpUpdate) -> Option<String> {
    let mut doc = doc.to_string();
    description_tag(&doc)?;

    let rating = match (update.rejected, update.rating) {
        (Some(true), _) => Some(-1),
        // Un-rejecting without a new rating: back to no stars.
        (Some(false), None) if property(&doc, "xmp:Rating").as_deref() == Some("-1") => Some(0),
        // Stars only: a reject is only ever written through `rejected`.
        (_, rating) => rating.map(|r| r.clamp(0, 5)),
    };
    if let Some(rating) = rating {
        set_simple(&mut doc, "xmp:Rating", &rating.to_string());
        ensure_namespace(&mut doc, "xmp", XMP_NS);
    }
    if update.rejected.is_some() || update.rating.is_some() {
        // Earlier Photon versions stored flags as colour labels, which other
        // tools show as a custom label. Real colour labels are left alone.
        if matches!(property(&doc, "xmp:Label").as_deref(), Some("Pick" | "Reject")) {
            remove_attribute(&mut doc, "xmp:Label");
        }
    }

    if let Some(title) = update.title {
        set_lang_alt(&mut doc, "dc:title", title);
    }
    if let Some(description) = update.description {
        set_lang_alt(&mut doc, "dc:description", description);
    }
    if let Some(keywords) = update.keywords {
        set_keywords(&mut doc, keywords);
    }
    if let Some(orientation) = update.orientation {
        set_simple(&mut doc, "tiff:Orientation", &orientation.to_string());
        ensure_namespace(&mut doc, "tiff", TIFF_NS);
    }
    Some(doc)
}

fn set_keywords(doc: &mut String, keywords: Keywords) {
    let existing = element(doc, "dc:subject")
        .map(|(s, e)| list_items(&doc[s..e]))
        .unwrap_or_default();
    let is_known = |k: &str| {
        keywords.known.iter().chain(keywords.tags).any(|t| t.eq_ignore_ascii_case(k))
    };

    let mut merged: Vec<String> = existing.iter().filter(|k| !is_known(k)).cloned().collect();
    for tag in keywords.tags {
        if !merged.iter().any(|k| k.eq_ignore_ascii_case(tag)) {
            merged.push(tag.clone());
        }
    }
    if merged == existing {
        return;
    }

    if merged.is_empty() {
        remove_element(doc, "dc:subject");
        return;
    }
    let items: String = merged
        .iter()
        .map(|k| format!("     <rdf:li>{}</rdf:li>\n", xml_escape(k)))
        .collect();
    replace_or_insert(doc, "dc:subject", &format!("   <dc:subject>\n    <rdf:Bag>\n{items}    </rdf:Bag>\n   </dc:subject>\n"));
}

/// Set a language-alternative property (title, description) to `text` in the
/// default language; empty `text` removes it.
fn set_lang_alt(doc: &mut String, name: &str, text: &str) {
    let text = text.trim();
    let current = element(doc, name).map(|(s, e)| list_items(&doc[s..e]));
    if current.as_deref().map_or(text.is_empty(), |c| c.first().map(String::as_str) == Some(text)) {
        return;
    }
    remove_attribute(doc, name);
    if text.is_empty() {
        remove_element(doc, name);
        return;
    }
    let xml = format!(
        "   <{name}>\n    <rdf:Alt>\n     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n    </rdf:Alt>\n   </{name}>\n",
        xml_escape(text)
    );
    replace_or_insert(doc, name, &xml);
}

/// Put the `dc:` element `xml` where element `name` is, keeping its place in
/// the file, or add it as a new child.
fn replace_or_insert(doc: &mut String, name: &str, xml: &str) {
    match element(doc, name) {
        Some((start, end)) => doc.replace_range(start..end, xml.trim()),
        None => insert_child(doc, xml),
    }
    ensure_namespace(doc, "dc", DC_NS);
}

/// Set a simple property, wherever and in whichever form (attribute or
/// element) the file already has it; new ones become attributes.
fn set_simple(doc: &mut String, name: &str, value: &str) {
    let value = xml_escape(value);
    if let Some((s, e)) = attribute_value(doc, name) {
        doc.replace_range(s..e, &value);
    } else if let Some((s, e)) = element(doc, name) {
        doc.replace_range(s..e, &format!("<{name}>{value}</{name}>"));
    } else {
        let (_, end, self_closing) = description_tag(doc).unwrap();
        let at = if self_closing { end - 2 } else { end - 1 };
        doc.insert_str(at, &format!("\n    {name}=\"{value}\""));
    }
}

/// The value of a simple property, in attribute or element form.
fn property(doc: &str, name: &str) -> Option<String> {
    if let Some((s, e)) = attribute_value(doc, name) {
        return Some(xml_unescape(&doc[s..e]));
    }
    let (s, e) = element(doc, name)?;
    let inner = &doc[s..e];
    let open_end = inner.find('>')? + 1;
    let close = inner.rfind("</")?;
    Some(xml_unescape(inner.get(open_end..close)?.trim()))
}

/// Declare `prefix` on the first `rdf:Description` unless the file declares it.
fn ensure_namespace(doc: &mut String, prefix: &str, uri: &str) {
    if doc.contains(&format!("xmlns:{prefix}=")) {
        return;
    }
    let (start, _, _) = description_tag(doc).unwrap();
    let at = start + "<rdf:Description".len();
    doc.insert_str(at, &format!("\n    xmlns:{prefix}=\"{uri}\""));
}

/// Insert `xml` as the first child of the first `rdf:Description`, opening
/// the element up if it is self-closing.
fn insert_child(doc: &mut String, xml: &str) {
    let (_, end, self_closing) = description_tag(doc).unwrap();
    if self_closing {
        doc.replace_range(end - 2..end, &format!(">\n{xml}  </rdf:Description>"));
    } else {
        doc.insert_str(end, &format!("\n{}", xml.trim_end_matches('\n')));
    }
}

/// Start and end of the first `<rdf:Description …>` start tag, and whether
/// it is self-closing.
fn description_tag(doc: &str) -> Option<(usize, usize, bool)> {
    let start = find_tag(doc, "rdf:Description", 0)?;
    let end = tag_end(doc, start)?;
    Some((start, end, doc[..end].ends_with("/>")))
}

/// Offset of the first `<name` start tag at or after `from`.
fn find_tag(doc: &str, name: &str, from: usize) -> Option<usize> {
    let open = format!("<{name}");
    let mut at = from;
    while let Some(i) = doc[at..].find(&open) {
        let i = at + i;
        let next = doc[i + open.len()..].chars().next();
        if matches!(next, Some(c) if c == '>' || c == '/' || c.is_whitespace()) {
            return Some(i);
        }
        at = i + open.len();
    }
    None
}

/// Offset just past the `>` closing the tag that starts at `start`.
fn tag_end(doc: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (i, c) in doc[start..].char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '>') => return Some(start + i + 1),
            _ => {}
        }
    }
    None
}

/// Span of the whole `<name>…</name>` element (or `<name/>`).
fn element(doc: &str, name: &str) -> Option<(usize, usize)> {
    let start = find_tag(doc, name, 0)?;
    let open_end = tag_end(doc, start)?;
    if doc[..open_end].ends_with("/>") {
        return Some((start, open_end));
    }
    let close = format!("</{name}>");
    let end = open_end + doc[open_end..].find(&close)? + close.len();
    Some((start, end))
}

/// Remove an element together with the indentation and line it sits on.
fn remove_element(doc: &mut String, name: &str) {
    if let Some((mut start, mut end)) = element(doc, name) {
        let line_start = doc[..start].rfind('\n').map_or(0, |i| i + 1);
        if doc[line_start..start].trim().is_empty() {
            start = line_start;
            if doc[end..].starts_with('\n') {
                end += 1;
            }
        }
        doc.replace_range(start..end, "");
    }
}

/// Span of the value of attribute `name` (between its quotes) in any start tag.
fn attribute_value(doc: &str, name: &str) -> Option<(usize, usize)> {
    let mut at = 0;
    while let Some(i) = doc[at..].find(name) {
        let i = at + i;
        at = i + name.len();
        let preceded = doc[..i].chars().next_back().is_some_and(char::is_whitespace);
        let rest = doc[at..].trim_start();
        if !preceded || !rest.starts_with('=') {
            continue;
        }
        let rest = rest[1..].trim_start();
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let value_start = doc.len() - rest.len() + 1;
        let value_end = value_start + doc[value_start..].find(quote)?;
        return Some((value_start, value_end));
    }
    None
}

fn remove_attribute(doc: &mut String, name: &str) {
    if let Some((_, value_end)) = attribute_value(doc, name) {
        let name_start = doc[..value_end].rfind(name).unwrap();
        let start = doc[..name_start].trim_end().len();
        doc.replace_range(start..value_end + 1, "");
    }
}

/// The text of every `rdf:li` in `xml`.
fn list_items(xml: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut at = 0;
    while let Some(start) = find_tag(xml, "rdf:li", at) {
        let Some(open_end) = tag_end(xml, start) else { break };
        let Some(close) = xml[open_end..].find("</rdf:li>") else { break };
        items.push(xml_unescape(xml[open_end..open_end + close].trim()));
        at = open_end + close;
    }
    items
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub use photon_core::models::XmpReadResult;

pub fn read_xmp_metadata(path: &Path) -> std::io::Result<XmpReadResult> {
    let doc = std::fs::read_to_string(path)?;
    let mut result = XmpReadResult::default();

    if let Some(r) = property(&doc, "xmp:Rating") {
        match r.trim().parse::<i8>() {
            Ok(-1) => result.rejected = Some(true),
            Ok(n @ 0..=5) => {
                result.rating = Some(n);
                result.rejected = Some(false);
            }
            _ => log::warn!("Ignoring xmp:Rating {r:?} in {}", path.display()),
        }
    }

    if let Some((s, e)) = element(&doc, "dc:title") {
        if let Some(item) = list_items(&doc[s..e]).first() {
            if !item.is_empty() { result.title = Some(item.clone()); }
        }
    }
    if let Some((s, e)) = element(&doc, "dc:description") {
        if let Some(item) = list_items(&doc[s..e]).first() {
            if !item.is_empty() { result.description = Some(item.clone()); }
        }
    }

    if let Some(o) = property(&doc, "tiff:Orientation") {
        if let Ok(n) = o.parse::<u8>() {
            result.orientation = Some(n);
        }
    }

    if let Some((s, e)) = element(&doc, "dc:subject") {
        let items = list_items(&doc[s..e]);
        result.tags = items.into_iter().filter(|tag| !tag.starts_with("darktable|")).collect();
    }

    Ok(result)
}

/// Seconds since the epoch at which `path` was last modified.
pub fn file_mtime(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    modified.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
}

/// Merge `update` into the sidecar of image `image_id`, and record the
/// sidecar's new mtime so Photon's own write isn't read back later as an edit
/// made in another tool.
pub fn write_image_xmp(
    conn: &rusqlite::Connection,
    image_id: i64,
    image_path: &Path,
    update: &XmpUpdate,
) -> std::io::Result<PathBuf> {
    let xmp_path = sync_xmp_metadata(image_path, update)?;
    if let Some(mtime) = file_mtime(&xmp_path) {
        if let Err(e) = photon_core::db::queries::set_xmp_mtime(conn, image_id, mtime) {
            log::warn!("Recording XMP mtime for {}: {e}", image_path.display());
        }
    }
    Ok(xmp_path)
}

/// Read the sidecar of image `image_id` into the library if it changed since
/// Photon last read or wrote it (`last_mtime`). Returns whether it was applied.
pub fn read_image_xmp(
    conn: &mut rusqlite::Connection,
    image_id: i64,
    image_path: &Path,
    last_mtime: Option<i64>,
) -> anyhow::Result<bool> {
    let Some(xmp_path) = find_xmp(image_path) else { return Ok(false) };
    let Some(mtime) = file_mtime(&xmp_path) else { return Ok(false) };
    if last_mtime.is_some_and(|last| mtime <= last) {
        return Ok(false);
    }
    let xmp = read_xmp_metadata(&xmp_path)?;
    photon_core::db::queries::update_from_xmp(conn, image_id, &xmp, mtime)?;
    Ok(true)
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

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// As darktable 4.x writes it, trimmed.
    const DARKTABLE_XMP: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="XMP Core 4.4.0-Exiv2">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:xmpMM="http://ns.adobe.com/xap/1.0/mm/"
    xmlns:darktable="http://darktable.sf.net/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmp:Rating="1"
    xmpMM:DerivedFrom="P1010001.ORF"
    darktable:history_end="2"
    darktable:iop_order_version="4">
   <darktable:history>
    <rdf:Seq>
     <rdf:li
      darktable:num="0"
      darktable:operation="exposure"
      darktable:enabled="1"
      darktable:params="0000000000000000cdcccc3e0000804000000000"/>
     <rdf:li
      darktable:num="1"
      darktable:operation="colorbalancergb"
      darktable:enabled="1"
      darktable:params="gz12eJxjYGiwZ2B4YM/AUP7ADgAckQRM"/>
    </rdf:Seq>
   </darktable:history>
   <dc:subject>
    <rdf:Bag>
     <rdf:li>darktable|format|orf</rdf:li>
     <rdf:li>wedding</rdf:li>
    </rdf:Bag>
   </dc:subject>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

    fn history(xml: &str) -> &str {
        let (s, e) = element(xml, "darktable:history").unwrap();
        &xml[s..e]
    }

    #[test]
    fn rating_keeps_darktable_history() {
        let merged = merge_xmp(
            DARKTABLE_XMP,
            &XmpUpdate { rating: Some(4), rejected: Some(false), ..Default::default() },
        )
        .unwrap();
        assert_eq!(property(&merged, "xmp:Rating").as_deref(), Some("4"));
        assert_eq!(history(&merged), history(DARKTABLE_XMP));
        assert!(merged.contains(r#"darktable:history_end="2""#));
        assert!(merged.contains(r#"xmpMM:DerivedFrom="P1010001.ORF""#));
        // Only the rating changed.
        assert_eq!(merged.replace(r#"xmp:Rating="4""#, r#"xmp:Rating="1""#), DARKTABLE_XMP);
    }

    #[test]
    fn reject_is_rating_minus_one_and_unreject_clears_it() {
        let reject = XmpUpdate { rejected: Some(true), ..Default::default() };
        let merged = merge_xmp(DARKTABLE_XMP, &reject).unwrap();
        assert_eq!(property(&merged, "xmp:Rating").as_deref(), Some("-1"));
        assert!(!merged.contains("xmp:Label"));

        let unreject = XmpUpdate { rejected: Some(false), ..Default::default() };
        let merged = merge_xmp(&merged, &unreject).unwrap();
        assert_eq!(property(&merged, "xmp:Rating").as_deref(), Some("0"));
    }

    #[test]
    fn keywords_unknown_to_the_library_survive() {
        let known = strings(&["wedding", "family"]);
        let tags = strings(&["family"]);
        let update = XmpUpdate {
            keywords: Some(Keywords { tags: &tags, known: &known }),
            ..Default::default()
        };
        let merged = merge_xmp(DARKTABLE_XMP, &update).unwrap();
        let (s, e) = element(&merged, "dc:subject").unwrap();
        // darktable's own tag stays, "wedding" was removed in Photon, "family" added.
        assert_eq!(list_items(&merged[s..e]), strings(&["darktable|format|orf", "family"]));
        assert_eq!(history(&merged), history(DARKTABLE_XMP));

        // Removing the last Photon tag keeps the foreign one.
        let update = XmpUpdate {
            keywords: Some(Keywords { tags: &[], known: &known }),
            ..Default::default()
        };
        let merged = merge_xmp(&merged, &update).unwrap();
        let (s, e) = element(&merged, "dc:subject").unwrap();
        assert_eq!(list_items(&merged[s..e]), strings(&["darktable|format|orf"]));
    }

    #[test]
    fn title_and_description_are_added_replaced_and_removed() {
        let merged = merge_xmp(
            DARKTABLE_XMP,
            &XmpUpdate { title: Some("First dance"), description: Some("A & B <3"), ..Default::default() },
        )
        .unwrap();
        assert_eq!(history(&merged), history(DARKTABLE_XMP));
        let (s, e) = element(&merged, "dc:description").unwrap();
        assert_eq!(list_items(&merged[s..e]), strings(&["A & B <3"]));

        let merged = merge_xmp(&merged, &XmpUpdate { title: Some("Cake"), ..Default::default() }).unwrap();
        let (s, e) = element(&merged, "dc:title").unwrap();
        assert_eq!(list_items(&merged[s..e]), strings(&["Cake"]));

        let merged = merge_xmp(&merged, &XmpUpdate { title: Some(""), description: Some(""), ..Default::default() })
            .unwrap();
        assert!(element(&merged, "dc:title").is_none());
        assert!(element(&merged, "dc:description").is_none());
        assert_eq!(merged, DARKTABLE_XMP);
    }

    #[test]
    fn element_form_and_nested_descriptions() {
        // Lightroom style: properties as elements, a nested rdf:Description.
        let lr = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:Exposure2012="+0.35">
   <xmp:Rating>2</xmp:Rating>
   <xmp:Label>Red</xmp:Label>
   <crs:Look>
    <rdf:Description crs:Name="Adobe Color"/>
   </crs:Look>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;
        let merged = merge_xmp(lr, &XmpUpdate { rating: Some(5), ..Default::default() }).unwrap();
        assert!(merged.contains("<xmp:Rating>5</xmp:Rating>"));
        assert!(merged.contains("<xmp:Label>Red</xmp:Label>"), "real colour labels are kept");
        assert!(merged.contains(r#"crs:Exposure2012="+0.35""#));
        assert!(merged.contains(r#"<rdf:Description crs:Name="Adobe Color"/>"#));

        let tags = strings(&["client"]);
        let merged = merge_xmp(
            &merged,
            &XmpUpdate { keywords: Some(Keywords { tags: &tags, known: &tags }), ..Default::default() },
        )
        .unwrap();
        assert!(merged.contains(r#"xmlns:dc="http://purl.org/dc/elements/1.1/""#));
        assert!(merged.contains("<rdf:li>client</rdf:li>"));
    }

    #[test]
    fn old_photon_pick_label_is_dropped() {
        let old = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="3" xmp:Label="Pick"/>
</rdf:RDF></x:xmpmeta>"#;
        let merged = merge_xmp(old, &XmpUpdate { rating: Some(3), ..Default::default() }).unwrap();
        assert!(!merged.contains("xmp:Label"));
        assert!(merged.contains(r#"xmp:Rating="3""#));
    }

    #[test]
    fn creates_new_sidecar_and_refuses_unreadable_ones() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("P1010001.ORF");
        std::fs::write(&img, b"raw").unwrap();

        let tags = strings(&["landscape", "sunset"]);
        let xmp = sync_xmp_metadata(
            &img,
            &XmpUpdate {
                rating: Some(4),
                rejected: Some(false),
                keywords: Some(Keywords { tags: &tags, known: &tags }),
                title: Some("Golden Hour"),
                description: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(xmp, dir.path().join("P1010001.ORF.xmp"));
        let xml = std::fs::read_to_string(&xmp).unwrap();
        assert_eq!(property(&xml, "xmp:Rating").as_deref(), Some("4"));
        assert!(xml.contains("<rdf:li>landscape</rdf:li>"));
        assert!(xml.contains("Golden Hour"));
        assert!(xml.contains(r#"xmlns:xmp="http://ns.adobe.com/xap/1.0/""#));
        assert!(!dir.path().join("P1010001.ORF.xmp.tmp").exists());

        let garbage = "not xmp at all";
        std::fs::write(&xmp, garbage).unwrap();
        assert!(sync_xmp_metadata(&img, &XmpUpdate { rating: Some(1), ..Default::default() }).is_err());
        assert_eq!(std::fs::read_to_string(&xmp).unwrap(), garbage);
    }

    #[test]
    fn existing_darktable_sidecar_on_disk_is_merged() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("P1010001.ORF");
        std::fs::write(&img, b"raw").unwrap();
        let xmp = dir.path().join("P1010001.ORF.xmp");
        std::fs::write(&xmp, DARKTABLE_XMP).unwrap();

        sync_xmp_metadata(&img, &XmpUpdate { rating: Some(5), ..Default::default() }).unwrap();
        let xml = std::fs::read_to_string(&xmp).unwrap();
        assert_eq!(history(&xml), history(DARKTABLE_XMP));
        assert_eq!(property(&xml, "xmp:Rating").as_deref(), Some("5"));
    }

    #[test]
    fn orientation_is_written_to_xmp() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("P1010001.ORF");
        std::fs::write(&img, b"raw").unwrap();

        let xmp = sync_xmp_metadata(
            &img,
            &XmpUpdate {
                orientation: Some(6),
                ..Default::default()
            },
        )
        .unwrap();
        let xml = std::fs::read_to_string(&xmp).unwrap();
        assert_eq!(property(&xml, "tiff:Orientation").as_deref(), Some("6"));
        assert!(xml.contains(r#"xmlns:tiff="http://ns.adobe.com/tiff/1.0/""#));

        // Now update orientation to 3
        sync_xmp_metadata(
            &img,
            &XmpUpdate {
                orientation: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        let xml2 = std::fs::read_to_string(&xmp).unwrap();
        assert_eq!(property(&xml2, "tiff:Orientation").as_deref(), Some("3"));
    }


    #[test]
    fn xmp_read_result_parses_rating_tags_and_title() {
        let dir = tempfile::tempdir().unwrap();
        let xmp = dir.path().join("test.xmp");
        let xml = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
         <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
          <rdf:Description rdf:about="" xmp:Rating="4">
           <dc:title><rdf:Alt><rdf:li xml:lang="x-default">My Title</rdf:li></rdf:Alt></dc:title>
           <dc:subject><rdf:Bag><rdf:li>vacation</rdf:li><rdf:li>darktable|auto-applied</rdf:li></rdf:Bag></dc:subject>
          </rdf:Description>
         </rdf:RDF>
        </x:xmpmeta>"#;
        std::fs::write(&xmp, xml).unwrap();
        let res = read_xmp_metadata(&xmp).unwrap();
        assert_eq!(res.rating, Some(4));
        assert_eq!(res.title, Some("My Title".to_string()));
        assert_eq!(res.tags, vec!["vacation".to_string()]);
    }

    #[test]
    fn xmp_read_result_returns_reject_for_minus_one() {
        let dir = tempfile::tempdir().unwrap();
        let xmp = dir.path().join("test2.xmp");
        let xml = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmp:Rating="-1"/></rdf:RDF></x:xmpmeta>"#;
        std::fs::write(&xmp, xml).unwrap();
        let res = read_xmp_metadata(&xmp).unwrap();
        assert_eq!(res.rating, None);
        assert_eq!(res.rejected, Some(true));
    }

    #[test]
    fn photon_writes_are_not_read_back_but_outside_edits_are() {
        let db = photon_core::db::Database::open_in_memory().unwrap();
        let mut conn = db.conn().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("a.jpg");
        std::fs::write(&img_path, b"jpeg").unwrap();
        let id = {
            let tx = conn.transaction().unwrap();
            let img = photon_core::models::Image::new(img_path.clone(), "h-a".into(), 4);
            let id = photon_core::db::queries::insert_image(&tx, &img).unwrap().unwrap();
            tx.commit().unwrap();
            id
        };
        let last = |conn: &rusqlite::Connection| {
            photon_core::db::queries::get_image(conn, id).unwrap().unwrap().xmp_mtime
        };

        // Photon rejects the photo: the sidecar gets -1 and its mtime is recorded.
        let reject = XmpUpdate { rejected: Some(true), ..Default::default() };
        write_image_xmp(&conn, id, &img_path, &reject).unwrap();
        assert!(last(&conn).is_some());
        let seen = last(&conn);
        assert!(!read_image_xmp(&mut conn, id, &img_path, seen).unwrap());

        // darktable un-rejects it with 3 stars a little later.
        let xmp = find_xmp(&img_path).unwrap();
        let doc = std::fs::read_to_string(&xmp).unwrap().replace(r#"xmp:Rating="-1""#, r#"xmp:Rating="3""#);
        std::fs::write(&xmp, doc).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&xmp).unwrap().set_modified(later).unwrap();

        let seen = last(&conn);
        assert!(read_image_xmp(&mut conn, id, &img_path, seen).unwrap());
        let img = photon_core::db::queries::get_image(&conn, id).unwrap().unwrap();
        assert_eq!((img.rating, img.flagged), (3, 0));
    }
}
