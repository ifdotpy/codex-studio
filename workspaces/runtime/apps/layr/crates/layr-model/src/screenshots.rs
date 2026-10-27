//! Pure pixel and structure comparisons of stored screen outputs.
//!
//! `compare` accepts path-to-byte maps and a `probes::Probe`. It returns every screen,
//! including unchanged screens, in path order. Each JSON row contains PNG paths,
//! dimensions, changed pixel counts, the exact share, rounded percent, and regions.
//! Structure fields contain element and text counts, a unified diff, and native
//! `structure_regions`. Structure-only screens have null pixel counts and shares.
//! All inputs, inventory entries, and result rows support serde. `probes::diff`
//! includes these rows in each screenshot probe's `changes`, with limit reasons.
//! The caller supplies decrypted bytes. This module does not access the filesystem.
//! The CLI can call `compare` once per inventory entry to bound retained input bytes.
//! PNG decode and comparison canvases retain the image crate's 512 MiB limit.
//! Encoded PNG input is limited to 512 MiB, and structure text to 64 MiB.
use crate::probes::Probe;
use anyhow::{bail, Context, Result};
use image::{ImageFormat, ImageReader, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

mod structure;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Screen {
    pub png: Option<String>,
    pub structure: Option<String>,
}
pub fn inventory<'a>(paths: impl IntoIterator<Item = &'a str>) -> Result<BTreeMap<String, Screen>> {
    let mut screens = BTreeMap::<String, Screen>::new();
    for path in paths {
        let (stem, ext) = path.rsplit_once('.').context("screen outputs need a file extension")?;
        let name = stem.rsplit('/').next().unwrap_or(stem);
        let (screen, device) = name.rsplit_once('@').context("screen outputs must be named <screen>@<device>.<extension>")?;
        if screen.is_empty() || device.is_empty() {
            bail!("invalid screen name '{path}'");
        }
        let entry = screens.entry(stem.into()).or_default();
        match ext {
            "png" => entry.png = Some(path.to_owned()),
            "html" | "xml" | "json" | "txt" => {
                if entry.structure.replace(path.to_owned()).is_some() {
                    bail!("screen '{stem}' has more than one structure file");
                }
            }
            _ => bail!("screen output '{path}' must be PNG, HTML, XML, JSON, or TXT"),
        }
    }
    Ok(screens)
}
/// Maximum encoded PNG bytes, decoded image allocation, and comparison canvas pixels.
pub const MAX_IMAGE_BYTES: u64 = 512 << 20;
/// Maximum UTF-8 structure bytes. Matches the CLI text limit.
pub const MAX_TEXT_BYTES: u64 = 64 << 20;

fn read_png(bytes: &[u8], path: &str) -> Result<RgbaImage> {
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        bail!("PNG screen '{path}' exceeds the image allocation limit");
    }
    ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png)
        .decode()
        .with_context(|| format!("invalid PNG screen '{path}'"))
        .map(|image| image.into_rgba8())
}
fn read_structure(bytes: &[u8], path: &str, probe: &Probe) -> Result<structure::Document> {
    if bytes.len() as u64 > MAX_TEXT_BYTES {
        bail!("structure file '{path}' exceeds the text size limit");
    }
    let text = std::str::from_utf8(bytes)?;
    structure::normalize(text, path.rsplit('.').next().unwrap_or(""), &probe.ignore_attributes)
        .with_context(|| format!("invalid structure file '{path}'"))
}
/// Validate one output file. Inventory validation checks the names and duplicate structures.
pub fn validate_file(path: &str, bytes: &[u8], probe: &Probe) -> Result<()> {
    if path.ends_with(".png") {
        read_png(bytes, path)?;
    } else {
        read_structure(bytes, path, probe)?;
    }
    Ok(())
}
/// Validate the supplied screen files without file or network access.
pub fn validate(files: &BTreeMap<String, Vec<u8>>, probe: &Probe) -> Result<()> {
    inventory(files.keys().map(String::as_str))?;
    for (path, bytes) in files {
        validate_file(path, bytes, probe)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Region {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}
impl Region {
    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            width: (self.x + self.width).max(other.x + other.width) - x,
            height: (self.y + self.height).max(other.y + other.height) - y,
        }
    }
    fn gap(self, other: Self) -> u32 {
        let x = self.x.saturating_sub(other.x + other.width).max(other.x.saturating_sub(self.x + self.width));
        let y = self.y.saturating_sub(other.y + other.height).max(other.y.saturating_sub(self.y + self.height));
        x.max(y)
    }
}
// Keep all changed pixels covered. More than 16 distant areas combine into the nearest boxes.
fn insert_region(regions: &mut Vec<Region>, mut region: Region) {
    loop {
        if let Some(i) = regions.iter().position(|r| r.gap(region) < 8) {
            region = region.union(regions.remove(i));
        } else {
            break;
        }
    }
    regions.push(region);
    if regions.len() > 16 {
        let (_, a, b) = (0..regions.len())
            .flat_map(|a| (a + 1..regions.len()).map(move |b| (a, b)))
            .map(|(a, b)| (regions[a].gap(regions[b]), a, b))
            .min()
            .unwrap();
        let joined = regions.remove(b).union(regions.remove(a));
        insert_region(regions, joined);
    }
}
fn regions(mask: &mut [bool], width: usize, height: usize) -> Vec<Region> {
    let mut out = Vec::new();
    let mut pending = Vec::new();
    for start in 0..mask.len() {
        if !mask[start] {
            continue;
        }
        mask[start] = false;
        pending.push(start);
        let (mut left, mut right, mut top, mut bottom) = (start % width, start % width, start / width, start / width);
        while let Some(i) = pending.pop() {
            let (x, y) = (i % width, i / width);
            left = left.min(x);
            right = right.max(x);
            top = top.min(y);
            bottom = bottom.max(y);
            for ny in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                    let next = ny * width + nx;
                    if mask[next] {
                        mask[next] = false;
                        pending.push(next);
                    }
                }
            }
        }
        insert_region(
            &mut out,
            Region { x: left as u32, y: top as u32, width: (right - left + 1) as u32, height: (bottom - top + 1) as u32 },
        );
    }
    out.sort_by_key(|r| (r.y, r.x, r.width, r.height));
    out
}
fn pixels(a: Option<&RgbaImage>, b: Option<&RgbaImage>, tolerance: u8) -> Result<Value> {
    let dimensions = |image: Option<&RgbaImage>| image.map(|i| json!({"width":i.width(),"height":i.height()}));
    if a.is_none() && b.is_none() {
        return Ok(json!({"status":"not available","before":null,"after":null,"size_changed":false,
            "changed_pixels":null,"total_pixels":null,"changed_share":null,"changed_percent":null,"regions":[]}));
    }
    let (aw, ah) = a.map(RgbaImage::dimensions).unwrap_or_default();
    let (bw, bh) = b.map(RgbaImage::dimensions).unwrap_or_default();
    let (w, h) = (aw.max(bw), ah.max(bh));
    let len = (w as usize).checked_mul(h as usize).context("screen dimensions overflow")?;
    if image::Limits::default().max_alloc.is_some_and(|limit| len as u64 > limit) {
        bail!("screen comparison canvas exceeds the image allocation limit");
    }
    let mut mask = Vec::new();
    mask.try_reserve_exact(len).context("screen comparison is too large")?;
    mask.resize(len, false);
    let mut count = 0u64;
    for y in 0..h {
        for x in 0..w {
            let old = a.filter(|_| x < aw && y < ah).map(|i| i.get_pixel(x, y));
            let new = b.filter(|_| x < bw && y < bh).map(|i| i.get_pixel(x, y));
            let changed = match (old, new) {
                (Some(a), Some(b)) => a.0.iter().zip(b.0).any(|(a, b)| a.abs_diff(b) > tolerance),
                (None, None) => false,
                _ => true,
            };
            mask[y as usize * w as usize + x as usize] = changed;
            count += u64::from(changed);
        }
    }
    // Pixels outside both images do not enter the denominator.
    let total = u64::from(aw) * u64::from(ah) + u64::from(bw) * u64::from(bh) - u64::from(aw.min(bw)) * u64::from(ah.min(bh));
    let share = if total == 0 { 0.0 } else { count as f64 / total as f64 };
    Ok(json!({"status":if a.is_none(){"added"}else if b.is_none(){"removed"}else{"compared"},
        "before":dimensions(a),"after":dimensions(b),"size_changed":a.is_some() && b.is_some() && (aw,ah)!=(bw,bh),
        "changed_pixels":count,"total_pixels":total,"changed_share":share,"changed_percent":(share*1000.0).round()/10.0,
        "regions":regions(&mut mask,w as usize,h as usize)}))
}

pub fn compare(before: &BTreeMap<String, Vec<u8>>, after: &BTreeMap<String, Vec<u8>>, probe: &Probe) -> Result<Vec<Value>> {
    let old = inventory(before.keys().map(String::as_str))?;
    let new = inventory(after.keys().map(String::as_str))?;
    let mut out = Vec::new();
    for path in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        let (a, b) = (old.get(path), new.get(path));
        let ap = a.and_then(|s| s.png.as_deref());
        let bp = b.and_then(|s| s.png.as_deref());
        let ai = ap.map(|p| read_png(&before[p], p)).transpose()?;
        let bi = bp.map(|p| read_png(&after[p], p)).transpose()?;
        let mut row = pixels(ai.as_ref(), bi.as_ref(), probe.tolerance)?;
        row["kind"] = json!("screenshots");
        row["screen"] = json!(path.rsplit('/').next().unwrap_or(path));
        row["path"] = json!(path);
        row["base_png"] = json!(ap);
        row["head_png"] = json!(bp);
        row["status"] = json!(if a.is_none() {
            "added"
        } else if b.is_none() {
            "removed"
        } else {
            "compared"
        });
        let ast = a.and_then(|s| s.structure.as_deref());
        let bst = b.and_then(|s| s.structure.as_deref());
        row["base_structure"] = json!(ast);
        row["head_structure"] = json!(bst);
        row["structure_available"] = json!(ast.is_some() || bst.is_some());
        row["elements_added"] = json!(0);
        row["elements_removed"] = json!(0);
        row["text_changes"] = json!(0);
        row["structure_diff"] = json!("");
        row["structure_regions"] = json!([]);
        if ast.is_some() || bst.is_some() {
            let a = ast.map(|p| read_structure(&before[p], p, probe)).transpose()?.unwrap_or_default();
            let b = bst.map(|p| read_structure(&after[p], p, probe)).transpose()?.unwrap_or_default();
            let delta = structure::compare(&a, &b, ast.unwrap_or("(absent)"), bst.unwrap_or("(absent)"));
            for (key, value) in delta.as_object().unwrap() {
                row[key] = value.clone();
            }
        }
        out.push(row);
    }
    Ok(out)
}
pub fn changed(row: &Value) -> bool {
    row["status"] != "compared"
        || row["changed_pixels"].as_u64().unwrap_or(0) > 0
        || row["size_changed"] == true
        || row["base_png"].is_null() != row["head_png"].is_null()
        || row["base_structure"].is_null() != row["head_structure"].is_null()
        || row["structure_diff"].as_str().is_some_and(|diff| !diff.is_empty())
}
pub fn limit_reasons(name: &str, row: &Value, limit: f64) -> Vec<String> {
    row["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let share = c["changed_share"].as_f64()?;
            (share > limit).then(|| {
                format!(
                    "probe {name}: {} {:.1} % of pixels changed is over the limit {:.1} %",
                    c["screen"].as_str().unwrap_or(""),
                    share * 100.0,
                    limit * 100.0
                )
            })
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    #[test]
    fn oversized_png_is_rejected_before_pixel_allocation() {
        // A 1 by 134217729 RGBA PNG requires more than the 512 MiB decode limit.
        let bytes = &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 8, 0, 0, 1, 8, 6, 0, 0, 0, 164, 205, 102,
            145, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 0, 2, 0, 0, 5, 0, 1, 122, 94, 171, 63, 0, 0, 0, 0, 73, 69, 78,
            68, 174, 66, 96, 130,
        ];
        let error = read_png(bytes, "Tall@Phone.png").unwrap_err();
        assert!(format!("{error:#}").contains("limit"), "{error:#}");
    }
    #[test]
    fn screen_inventory_rejects_invalid_names_and_multiple_structures() {
        for paths in [
            vec!["Phone.png"],
            vec!["@Phone.png"],
            vec!["Home@.png"],
            vec!["Home@Phone.jpg"],
            vec!["Home@Phone.html", "Home@Phone.xml"],
        ] {
            assert!(inventory(paths).is_err());
        }
        let screens = inventory(["out/Home@Phone.png", "out/Home@Phone.xml"]).unwrap();
        assert_eq!(screens.len(), 1);
        let encoded = serde_json::to_vec(&screens).unwrap();
        let decoded: BTreeMap<String, Screen> = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded["out/Home@Phone"].png.as_deref(), Some("out/Home@Phone.png"));
    }
    #[test]
    fn tolerance_includes_alpha_and_uses_a_strict_boundary() {
        let a = RgbaImage::from_pixel(10, 10, Rgba([20, 20, 20, 255]));
        let mut b = a.clone();
        b.put_pixel(2, 3, Rgba([28, 20, 20, 247]));
        assert_eq!(pixels(Some(&a), Some(&b), 8).unwrap()["changed_pixels"], 0);
        b.put_pixel(2, 3, Rgba([28, 20, 20, 246]));
        let row = pixels(Some(&a), Some(&b), 8).unwrap();
        assert_eq!(row["changed_pixels"], 1);
        assert_eq!(row["regions"], json!([{"x":2,"y":3,"width":1,"height":1}]));
        assert_eq!(pixels(Some(&a), Some(&b), 255).unwrap()["changed_pixels"], 0);
    }
    #[test]
    fn dimension_changes_count_the_union_of_the_images() {
        let a = RgbaImage::new(2, 4);
        let b = RgbaImage::new(4, 2);
        let row = pixels(Some(&a), Some(&b), 255).unwrap();
        assert_eq!(row["total_pixels"], 12);
        assert_eq!(row["changed_pixels"], 8);
        assert_eq!(row["size_changed"], true);
        assert_eq!(pixels(None, Some(&a), 8).unwrap()["changed_share"], 1.0);
        assert!(pixels(None, None, 8).unwrap()["changed_share"].is_null());
    }
    #[test]
    fn regions_merge_transitively_and_respect_the_eight_pixel_gap() {
        let mut boxes = vec![];
        for x in [0, 18, 9] {
            insert_region(&mut boxes, Region { x, y: 0, width: 2, height: 2 });
        }
        assert_eq!(boxes, vec![Region { x: 0, y: 0, width: 20, height: 2 }]);
        insert_region(&mut boxes, Region { x: 28, y: 0, width: 1, height: 1 });
        assert_eq!(boxes.len(), 2);
    }
    #[test]
    fn mismatched_narrow_images_cannot_allocate_an_unbounded_canvas() {
        let a = RgbaImage::new(1, 40000);
        let b = RgbaImage::new(40000, 1);
        assert!(pixels(Some(&a), Some(&b), 8).unwrap_err().to_string().contains("allocation limit"));
    }
    #[test]
    fn limits_use_the_unrounded_share_and_ignore_structure_only_rows() {
        let row = json!({"changes":[{"screen":"A@Phone","changed_share":0.05001},{"screen":"B@Phone","changed_share":null}]});
        assert_eq!(limit_reasons("screens", &row, 0.05).len(), 1);
        assert!(limit_reasons("screens", &row, 0.05001).is_empty());
    }
}
