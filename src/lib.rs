use base64::Engine as _;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::io::{Cursor, Read, Write};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{CanvasPattern, CanvasRenderingContext2d, HtmlCanvasElement, HtmlImageElement};
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

// ── Types ──

#[derive(Clone, Debug)]
pub struct Point {
    pub x: f32,
    pub y: f32,
    pub pressure: u16,
    pub tilt_x: u8,
    pub tilt_y: u8,
    pub cum_time: f64,
}

#[derive(Clone, Debug)]
pub struct Stroke {
    pub uuid: String,
    pub points: Vec<Point>,
}

#[derive(Clone, Debug)]
pub struct Note {
    pub path: String,
    pub header: Vec<u8>,
    pub strokes: Vec<Stroke>,
}

#[derive(Clone, Debug)]
pub struct ShapeMeta {
    pub pen_type: i32,
    pub thickness: f32,
    pub color_rgba: (u8, u8, u8, f32),
    pub fill_color: Option<(u8, u8, u8, f32)>,
    pub point_list: Vec<[f32; 2]>,
    pub page_id: Option<String>,
    pub matrix: Option<[f32; 6]>,
    pub created_ts: u64,
    pub bounding_rect: Option<[f32; 4]>, // [top, left, bottom, right]
    pub text: Option<String>,
    pub rich_text: Option<String>,
    pub text_style: Option<String>,
    pub resource_path: Option<String>,
    pub extra_json: Option<String>,
}

impl Default for ShapeMeta {
    fn default() -> Self {
        ShapeMeta {
            pen_type: 2,
            thickness: 3.0,
            color_rgba: (0, 0, 0, 1.0),
            fill_color: None,
            point_list: Vec::new(),
            page_id: None,
            matrix: None,
            created_ts: 0,
            bounding_rect: None,
            text: None,
            rich_text: None,
            text_style: None,
            resource_path: None,
            extra_json: None,
        }
    }
}

#[derive(PartialEq)]
struct CostNode(usize, f32);
impl Eq for CostNode {}
impl PartialOrd for CostNode {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        o.1.partial_cmp(&self.1)
    }
}
impl Ord for CostNode {
    fn cmp(&self, o: &Self) -> Ordering {
        self.partial_cmp(o).unwrap_or(Ordering::Equal)
    }
}

// ── NoteFile (high-level entry point) ──

pub struct NoteFile {
    pub pages: Vec<String>,
    pub notes: Vec<Note>,
    pub shape_meta: HashMap<String, ShapeMeta>,
    pub canvas_w: f32,
    pub canvas_h: f32,
    pub resources: HashMap<String, Vec<u8>>,
    pub note_background: Option<String>,
    pub templates: HashMap<String, serde_json::Value>,
}

impl NoteFile {
    pub fn open(zip_bytes: &[u8]) -> Result<Self, String> {
        let mut archive =
            ZipArchive::new(Cursor::new(zip_bytes)).map_err(|_| "Invalid ZIP file".to_string())?;
        let mut notes = Vec::new();
        let mut shape_meta = HashMap::new();
        let mut canvas_w = 1860.0;
        let mut canvas_h = 2480.0;
        let mut resources = HashMap::new();
        let mut note_background = None;
        let mut templates: HashMap<String, serde_json::Value> = HashMap::new();
        let mut page_list: Vec<String> = Vec::new();

        for i in 0..archive.len() {
            let Ok(mut file) = archive.by_index(i) else {
                continue;
            };
            let name = file.name().to_string();

            if name.ends_with("#points") {
                let mut data = Vec::new();
                if file.read_to_end(&mut data).is_err() {
                    continue;
                }
                if let Ok(note) = parse_points(&data, &name) {
                    notes.push(note);
                }
            } else if name.ends_with(".zip") && name.contains("shape") && !name.contains("stash") {
                let page_id_from_path: Option<String> = {
                    let parts: Vec<&str> = name.split('/').collect();
                    parts
                        .iter()
                        .position(|&x| x == "shape")
                        .and_then(|i| parts.get(i + 1))
                        .map(|s| s.split('#').next().unwrap_or(s).to_string())
                };
                let mut z_data = Vec::new();
                if file.read_to_end(&mut z_data).is_err() {
                    continue;
                }
                if let Ok(mut inner) = ZipArchive::new(Cursor::new(&z_data)) {
                    for j in 0..inner.len() {
                        let Ok(mut sf) = inner.by_index(j) else {
                            continue;
                        };
                        let mut sh_data = Vec::new();
                        if sf.read_to_end(&mut sh_data).is_ok() {
                            parse_protobuf(
                                &sh_data,
                                &mut shape_meta,
                                page_id_from_path.as_deref(),
                            );
                        }
                    }
                }
            } else if name.ends_with("note_info") || name == "note_tree" {
                let mut d = Vec::new();
                if file.read_to_end(&mut d).is_ok() {
                    parse_note_info(&d, &mut canvas_w, &mut canvas_h, &mut note_background, &mut page_list);
                }
            } else if name.contains("resource/pb/") && !name.contains("stash") {
                let res_key = name
                    .split("resource/pb/")
                    .last()
                    .unwrap_or("")
                    .to_string();
                if !res_key.is_empty() {
                    let mut data = Vec::new();
                    if file.read_to_end(&mut data).is_ok() && !data.is_empty() {
                        resources.insert(res_key, data);
                    }
                }
            } else if name.contains("template/json/") && name.ends_with(".template_json") {
                let page_id = name
                    .split('/')
                    .last()
                    .unwrap_or("")
                    .trim_end_matches(".template_json")
                    .to_string();
                if !page_id.is_empty() {
                    let mut data = Vec::new();
                    if file.read_to_end(&mut data).is_ok() {
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&data) {
                            templates.insert(page_id, json);
                        }
                    }
                }
            }
        }

        // Use pageNameList from note_info if available; otherwise discover from paths
        let pages = if !page_list.is_empty() {
            // Filter to pages that actually have content (points or shapes)
            let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();
            for n in &notes {
                let parts: Vec<&str> = n.path.split('/').collect();
                if let Some(pos) = parts.iter().position(|&x| x == "point") {
                    if pos + 1 < parts.len() {
                        known.insert(parts[pos + 1].to_string());
                    }
                }
            }
            for m in shape_meta.values() {
                if let Some(pid) = &m.page_id {
                    known.insert(pid.clone());
                }
            }
            let mut ordered: Vec<String> = page_list.into_iter().filter(|p| known.contains(p)).collect();
            // Add any known pages not in the list (shouldn't happen but be safe)
            for k in &known {
                if !ordered.contains(k) {
                    ordered.push(k.clone());
                }
            }
            ordered
        } else {
            let mut pages = Vec::new();
            for n in &notes {
                let parts: Vec<&str> = n.path.split('/').collect();
                if let Some(pos) = parts.iter().position(|&x| x == "point") {
                    if pos + 1 < parts.len() && !pages.contains(&parts[pos + 1].to_string()) {
                        pages.push(parts[pos + 1].to_string());
                    }
                }
            }
            for m in shape_meta.values() {
                if let Some(pid) = &m.page_id {
                    if !pages.contains(pid) {
                        pages.push(pid.clone());
                    }
                }
            }
            pages
        };

        Ok(NoteFile {
            pages,
            notes,
            shape_meta,
            canvas_w,
            canvas_h,
            resources,
            note_background,
            templates,
        })
    }
}

// ── GeoJSON feature extraction ──

pub fn extract_features(
    meta: &ShapeMeta,
) -> Option<(Vec<serde_json::Value>, [f32; 6])> {
    let extra = meta.extra_json.as_ref()?;
    let outer: serde_json::Value = serde_json::from_str(extra).ok()?;
    let fc_str = outer.get("featureCollection")?.as_str()?;
    if fc_str.is_empty() {
        return None;
    }
    let fc: serde_json::Value = serde_json::from_str(fc_str).ok()?;
    let features = fc.get("features")?.as_array()?;
    let matrix = meta.matrix.unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    Some((features.clone(), matrix))
}

// ── Coordinate helpers ──

pub fn transform_point(p: &[f64], m: &[f32; 6]) -> (f64, f64) {
    let x = p[0];
    let y = p[1];
    (
        m[0] as f64 * x + m[1] as f64 * y + m[2] as f64,
        m[3] as f64 * x + m[4] as f64 * y + m[5] as f64,
    )
}

pub fn parse_coord(v: &serde_json::Value) -> Option<[f64; 2]> {
    let a = v.as_array()?;
    Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?])
}

// ── Point parsing ──

pub fn parse_points(data: &[u8], path: &str) -> Result<Note, String> {
    if data.len() < 76 {
        return Err("data too short".to_string());
    }
    let entries_start = Cursor::new(&data[data.len() - 4..])
        .read_u32::<BigEndian>()
        .map_err(|_| "Read err".to_string())? as usize;
    if entries_start > data.len() {
        return Err("Invalid EOF index".to_string());
    }

    let mut strokes = Vec::new();
    let num_strokes = (data.len() - 4 - entries_start) / 44;
    for i in 0..num_strokes {
        let pos = entries_start + i * 44;
        if pos + 44 > data.len() {
            break;
        }
        let uuid = String::from_utf8_lossy(&data[pos..pos + 36]).to_string();
        let Ok(offset) = Cursor::new(&data[pos + 36..pos + 40]).read_u32::<BigEndian>() else {
            continue;
        };
        let Ok(size) = Cursor::new(&data[pos + 40..pos + 44]).read_u32::<BigEndian>() else {
            continue;
        };
        let (offset, size) = (offset as usize, size as usize);

        if offset + size > data.len() || size < 4 {
            continue;
        }

        let mut points = Vec::new();
        for j in 0..(size - 4) / 16 {
            if offset + 4 + j * 16 + 16 > data.len() {
                break;
            }
            let mut cur = Cursor::new(&data[offset + 4 + j * 16..]);
            if let (Ok(x), Ok(y), Ok(tx), Ok(ty), Ok(pr), Ok(raw_t)) = (
                cur.read_f32::<BigEndian>(),
                cur.read_f32::<BigEndian>(),
                cur.read_u8(),
                cur.read_u8(),
                cur.read_u16::<BigEndian>(),
                cur.read_u32::<BigEndian>(),
            ) {
                points.push(Point {
                    x,
                    y,
                    tilt_x: tx,
                    tilt_y: ty,
                    pressure: pr,
                    cum_time: raw_t as f64,
                });
            }
        }
        // Stored values are cumulative timestamps (ms from stroke start)
        if !points.is_empty() {
            strokes.push(Stroke { uuid, points });
        }
    }
    Ok(Note {
        path: path.to_string(),
        header: data[..76].to_vec(),
        strokes,
    })
}

// ── Protobuf parsing ──

pub fn decode_var(data: &[u8], off: &mut usize) -> u64 {
    let (mut r, mut s) = (0, 0);
    while *off < data.len() {
        let b = data[*off];
        *off += 1;
        r |= ((b & 0x7F) as u64) << s;
        if b & 0x80 == 0 || s >= 63 {
            break;
        }
        s += 7;
    }
    r
}

pub fn parse_protobuf(data: &[u8], meta: &mut HashMap<String, ShapeMeta>, page_id: Option<&str>) {
    let mut off = 0;
    while off < data.len() {
        let tag = decode_var(data, &mut off);
        let (fn_num, wt) = ((tag >> 3) as u32, (tag & 0x07) as u8);
        if fn_num == 1 && wt == 2 {
            let len = decode_var(data, &mut off) as usize;
            if off + len > data.len() {
                break;
            }
            let msg = &data[off..off + len];
            off += len;
            let (mut so, mut u, mut pt, mut th, mut c, mut mat, mut ts) =
                (0, String::new(), 2, 3.0, (0, 0, 0, 1.0), None, 0);
            let mut fill_c: Option<(u8, u8, u8, f32)> = None;
            let mut point_list: Vec<[f32; 2]> = Vec::new();
            let mut bounding_rect: Option<[f32; 4]> = None;
            let mut text: Option<String> = None;
            let mut rich_text: Option<String> = None;
            let mut text_style: Option<String> = None;
            let mut resource_path: Option<String> = None;
            let mut extra_json: Option<String> = None;
            while so < msg.len() {
                let stag = decode_var(msg, &mut so);
                let (sfn, swt) = ((stag >> 3) as u32, (stag & 0x07) as u8);
                match (sfn, swt) {
                    (1, 2) => {
                        let l = decode_var(msg, &mut so) as usize;
                        if so + l > msg.len() {
                            break;
                        }
                        u = String::from_utf8_lossy(&msg[so..so + l]).to_string();
                        so += l;
                    }
                    (2, 0) => ts = decode_var(msg, &mut so),
                    (4, 0) => {
                        let cv = decode_var(msg, &mut so) as u32;
                        c = (
                            ((cv >> 16) & 0xFF) as u8,
                            ((cv >> 8) & 0xFF) as u8,
                            (cv & 0xFF) as u8,
                            ((cv >> 24) & 0xFF) as f32 / 255.0,
                        );
                    }
                    (5, 5) => {
                        if so + 4 > msg.len() {
                            break;
                        }
                        let mut b = [0; 4];
                        b.copy_from_slice(&msg[so..so + 4]);
                        th = f32::from_le_bytes(b);
                        so += 4;
                    }
                    (7, 2) => {
                        let l = decode_var(msg, &mut so) as usize;
                        if so + l > msg.len() {
                            break;
                        }
                        if let Ok(j) = serde_json::from_slice::<serde_json::Value>(&msg[so..so + l])
                        {
                            if let (Some(t), Some(l_v), Some(b), Some(r)) = (
                                j.get("top").and_then(|v| v.as_f64()),
                                j.get("left").and_then(|v| v.as_f64()),
                                j.get("bottom").and_then(|v| v.as_f64()),
                                j.get("right").and_then(|v| v.as_f64()),
                            ) {
                                bounding_rect =
                                    Some([t as f32, l_v as f32, b as f32, r as f32]);
                            }
                        }
                        so += l;
                    }
                    (8, 2) => {
                        let l = decode_var(msg, &mut so) as usize;
                        if so + l > msg.len() {
                            break;
                        }
                        if let Ok(j) = serde_json::from_slice::<serde_json::Value>(&msg[so..so + l])
                        {
                            if let Some(a) = j.get("values").and_then(|v| v.as_array()) {
                                if a.len() >= 6 {
                                    mat = Some([
                                        a[0].as_f64().unwrap_or(1.0) as f32,
                                        a[1].as_f64().unwrap_or(0.0) as f32,
                                        a[2].as_f64().unwrap_or(0.0) as f32,
                                        a[3].as_f64().unwrap_or(0.0) as f32,
                                        a[4].as_f64().unwrap_or(1.0) as f32,
                                        a[5].as_f64().unwrap_or(0.0) as f32,
                                    ]);
                                }
                            }
                        }
                        so += l;
                    }
                    (9, 2) => {
                        let l = decode_var(msg, &mut so) as usize;
                        if so + l > msg.len() {
                            break;
                        }
                        text_style = Some(String::from_utf8_lossy(&msg[so..so + l]).to_string());
                        so += l;
                    }
                    (10, 2) => {
                        let l = decode_var(msg, &mut so) as usize;
                        if so + l > msg.len() {
                            break;
                        }
                        text = Some(String::from_utf8_lossy(&msg[so..so + l]).to_string());
                        so += l;
                    }
                    (12, 0) => pt = decode_var(msg, &mut so) as i32,
                    (14, 2) => {
                        let l = decode_var(msg, &mut so) as usize;
                        if so + l > msg.len() {
                            break;
                        }
                        if let Ok(j) = serde_json::from_slice::<serde_json::Value>(&msg[so..so + l])
                        {
                            if let Some(rp) = j.get("relativePath").and_then(|v| v.as_str()) {
                                resource_path = Some(rp.to_string());
                            }
                        }
                        so += l;
                    }
                    (20, 2) => {
                        let l = decode_var(msg, &mut so) as usize;
                        if so + l > msg.len() {
                            break;
                        }
                        extra_json =
                            Some(String::from_utf8_lossy(&msg[so..so + l]).to_string());
                        so += l;
                    }
                    (22, 2) => {
                        let l = decode_var(msg, &mut so) as usize;
                        if so + l > msg.len() {
                            break;
                        }
                        rich_text =
                            Some(String::from_utf8_lossy(&msg[so..so + l]).to_string());
                        so += l;
                    }
                    (23, 0) => {
                        let fv = decode_var(msg, &mut so) as u32;
                        fill_c = Some((
                            ((fv >> 16) & 0xFF) as u8,
                            ((fv >> 8) & 0xFF) as u8,
                            (fv & 0xFF) as u8,
                            ((fv >> 24) & 0xFF) as f32 / 255.0,
                        ));
                    }
                    (25, 2) => {
                        let l = decode_var(msg, &mut so) as usize;
                        if so + l > msg.len() {
                            break;
                        }
                        let pl_data = &msg[so..so + l];
                        so += l;
                        if l > 4 {
                            let mut po = 4;
                            while po + 16 <= l {
                                let mut cur = Cursor::new(&pl_data[po..]);
                                if let (Ok(x), Ok(y)) = (
                                    cur.read_f32::<BigEndian>(),
                                    cur.read_f32::<BigEndian>(),
                                ) {
                                    point_list.push([x, y]);
                                }
                                po += 16;
                            }
                        }
                    }
                    (_, 0) => {
                        decode_var(msg, &mut so);
                    }
                    (_, 1) => so += 8,
                    (_, 2) => {
                        let l = decode_var(msg, &mut so) as usize;
                        if so + l > msg.len() {
                            break;
                        }
                        so += l;
                    }
                    (_, 5) => so += 4,
                    _ => break,
                }
            }
            if !u.is_empty() {
                meta.insert(
                    u,
                    ShapeMeta {
                        pen_type: pt,
                        thickness: th,
                        color_rgba: c,
                        fill_color: fill_c,
                        point_list,
                        page_id: page_id.map(|s| s.to_string()),
                        matrix: mat,
                        created_ts: ts,
                        bounding_rect,
                        text,
                        rich_text,
                        text_style,
                        resource_path,
                        extra_json,
                    },
                );
            }
        } else {
            match wt {
                0 => {
                    decode_var(data, &mut off);
                }
                1 => off += 8,
                2 => {
                    let l = decode_var(data, &mut off) as usize;
                    if off + l > data.len() {
                        break;
                    }
                    off += l;
                }
                5 => off += 4,
                _ => break,
            }
        }
    }
}

pub fn parse_note_info(data: &[u8], w: &mut f32, h: &mut f32, bg: &mut Option<String>, page_list: &mut Vec<String>) {
    let mut off = 0;
    while off < data.len() {
        let tag = decode_var(data, &mut off);
        let (fn_num, wt) = ((tag >> 3) as u32, (tag & 0x07) as u8);
        if fn_num == 1 && wt == 2 {
            let l = decode_var(data, &mut off) as usize;
            let next_off = off.saturating_add(l);
            if next_off <= data.len() {
                parse_note_info(&data[off..next_off], w, h, bg, page_list);
            }
            off = next_off;
        } else if fn_num == 13 && wt == 2 {
            let l = decode_var(data, &mut off) as usize;
            if off + l <= data.len() {
                let s = String::from_utf8_lossy(&data[off..off + l]).to_string();
                if !s.is_empty() {
                    *bg = Some(s);
                }
            }
            off += l;
        } else if fn_num == 20 && wt == 2 {
            // pageNameList JSON: {"pageNameList":["uuid1","uuid2",...]}
            let l = decode_var(data, &mut off) as usize;
            if off + l <= data.len() {
                if let Ok(s) = std::str::from_utf8(&data[off..off + l]) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
                        if let Some(arr) = v.get("pageNameList").and_then(|a| a.as_array()) {
                            let names: Vec<String> = arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect();
                            if !names.is_empty() {
                                *page_list = names;
                            }
                        }
                    }
                }
            }
            off += l;
        } else if (fn_num == 22 || fn_num == 23) && wt == 5 {
            if off + 4 <= data.len() {
                let mut b = [0; 4];
                b.copy_from_slice(&data[off..off + 4]);
                let val = f32::from_le_bytes(b);
                if val > 0.0 {
                    if fn_num == 22 {
                        *w = val;
                    } else {
                        *h = val;
                    }
                }
                off += 4;
            }
        } else {
            match wt {
                0 => {
                    decode_var(data, &mut off);
                }
                1 => off += 8,
                2 => {
                    let l = decode_var(data, &mut off) as usize;
                    off += l;
                }
                5 => off += 4,
                _ => break,
            }
        }
    }
}

// ── Binary serialization ──

pub fn encode_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        if v <= 0x7F { buf.push(v as u8); return; }
        buf.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
}

/// Rewrite shape protobuf, applying pen_type and thickness overrides for given UUIDs.
pub fn patch_shape_protobuf(data: &[u8], overrides: &HashMap<String, (i32, f32)>) -> Vec<u8> {
    let mut result = Vec::new();
    let mut off = 0;
    while off < data.len() {
        let msg_start = off;
        let tag = decode_var(data, &mut off);
        let (fn_num, wt) = ((tag >> 3) as u32, (tag & 0x07) as u8);
        if fn_num == 1 && wt == 2 {
            let len = decode_var(data, &mut off) as usize;
            if off + len > data.len() { break; }
            let msg = &data[off..off + len];
            off += len;

            // Extract UUID from the inner message to check if it needs patching
            let mut so = 0;
            let mut uuid = String::new();
            while so < msg.len() {
                let stag = decode_var(msg, &mut so);
                let (sfn, swt) = ((stag >> 3) as u32, (stag & 0x07) as u8);
                if sfn == 1 && swt == 2 {
                    let l = decode_var(msg, &mut so) as usize;
                    if so + l <= msg.len() {
                        uuid = String::from_utf8_lossy(&msg[so..so + l]).to_string();
                    }
                    so += l;
                    break;
                }
                match swt {
                    0 => { decode_var(msg, &mut so); }
                    1 => so += 8,
                    2 => { let l = decode_var(msg, &mut so) as usize; so += l; }
                    5 => so += 4,
                    _ => break,
                }
            }

            if let Some(&(new_pt, new_th)) = overrides.get(&uuid) {
                // Rebuild inner message with overridden pen_type and thickness
                let mut inner = Vec::new();
                let mut so = 0;
                while so < msg.len() {
                    let field_start = so;
                    let stag = decode_var(msg, &mut so);
                    let (sfn, swt) = ((stag >> 3) as u32, (stag & 0x07) as u8);
                    match (sfn, swt) {
                        (5, 5) => {
                            // thickness — replace with new value
                            if so + 4 > msg.len() { break; }
                            encode_varint(&mut inner, (5 << 3) | 5);
                            inner.extend_from_slice(&new_th.to_le_bytes());
                            so += 4;
                        }
                        (12, 0) => {
                            // pen_type — replace with new value
                            let _old = decode_var(msg, &mut so);
                            encode_varint(&mut inner, (12 << 3) | 0);
                            encode_varint(&mut inner, new_pt as u64);
                        }
                        (_, 0) => {
                            decode_var(msg, &mut so);
                            inner.extend_from_slice(&msg[field_start..so]);
                        }
                        (_, 1) => {
                            inner.extend_from_slice(&msg[field_start..so + 8]);
                            so += 8;
                        }
                        (_, 2) => {
                            let l = decode_var(msg, &mut so) as usize;
                            inner.extend_from_slice(&msg[field_start..so + l]);
                            so += l;
                        }
                        (_, 5) => {
                            inner.extend_from_slice(&msg[field_start..so + 4]);
                            so += 4;
                        }
                        _ => break,
                    }
                }
                // Write patched message
                encode_varint(&mut result, (1 << 3) | 2);
                encode_varint(&mut result, inner.len() as u64);
                result.extend_from_slice(&inner);
            } else {
                // No override — copy original bytes verbatim
                result.extend_from_slice(&data[msg_start..off]);
            }
        } else {
            // Non-message field — copy verbatim
            match wt {
                0 => { decode_var(data, &mut off); }
                1 => off += 8,
                2 => { let l = decode_var(data, &mut off) as usize; off += l; }
                5 => off += 4,
                _ => break,
            }
            result.extend_from_slice(&data[msg_start..off]);
        }
    }
    result
}

pub fn build_points(nd: &Note) -> Vec<u8> {
    let mut bs = Vec::new();
    let mut idxs = Vec::new();
    let mut c_off = nd.header.len() as u32;
    for s in &nd.strokes {
        let mut b = vec![0, 0, 0, 0];
        for p in &s.points {
            b.write_f32::<BigEndian>(p.x).unwrap();
            b.write_f32::<BigEndian>(p.y).unwrap();
            b.write_u8(p.tilt_x).unwrap();
            b.write_u8(p.tilt_y).unwrap();
            b.write_u16::<BigEndian>(p.pressure).unwrap();
            // Write cumulative timestamp (matching device .note format)
            b.write_u32::<BigEndian>(p.cum_time.round() as u32).unwrap();
        }
        idxs.push((s.uuid.clone(), c_off, b.len() as u32));
        c_off += b.len() as u32;
        bs.push(b);
    }
    let i_st = c_off;
    let mut res = nd.header.clone();
    for b in bs {
        res.extend(b);
    }
    for (u, o, sz) in idxs {
        let mut ub = u.into_bytes();
        ub.resize(36, 0);
        res.extend(ub);
        res.write_u32::<BigEndian>(o).unwrap();
        res.write_u32::<BigEndian>(sz).unwrap();
    }
    res.write_u32::<BigEndian>(i_st).unwrap();
    res
}

// ── Math utilities ──

pub fn smooth_angle_ema(angles: &[f32], alpha: f32) -> Vec<f32> {
    let mut out = vec![0.0; angles.len()];
    if angles.is_empty() {
        return out;
    }
    out[0] = angles[0];
    for i in 1..angles.len() {
        let mut diff = angles[i] - out[i - 1];
        diff = (diff + std::f32::consts::PI).rem_euclid(2.0 * std::f32::consts::PI)
            - std::f32::consts::PI;
        out[i] = out[i - 1] + alpha * diff;
    }
    out
}

pub fn unwrap_8bit(arr: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0; arr.len()];
    if arr.is_empty() {
        return out;
    }
    out[0] = arr[0];
    let mut cum = 0.0;
    for i in 1..arr.len() {
        let d = arr[i] - arr[i - 1];
        cum += ((d + 128.0).rem_euclid(256.0)) - 128.0 - d;
        out[i] = arr[i] + cum;
    }
    out
}

pub fn decimate(pts: &[[f32; 5]], t: f32) -> Vec<bool> {
    let n = pts.len();
    let mut mask = vec![true; n];
    let mut prev: Vec<i32> = (0..n).map(|i| i as i32 - 1).collect();
    let mut next: Vec<i32> = (0..n).map(|i| i as i32 + 1).collect();
    next[n - 1] = -1;
    let mut costs = vec![f32::INFINITY; n];
    let mut heap = BinaryHeap::new();

    for i in 1..n - 1 {
        let c = cost(&pts[i - 1], &pts[i], &pts[i + 1]);
        costs[i] = c;
        if c < f32::INFINITY {
            heap.push(CostNode(i, c));
        }
    }

    while let Some(CostNode(idx, min)) = heap.pop() {
        if !mask[idx] || (min - costs[idx]).abs() > 1e-6 {
            continue;
        }
        if min > t {
            break;
        }
        mask[idx] = false;
        costs[idx] = f32::INFINITY;

        let (p, nxt) = (prev[idx], next[idx]);
        if p != -1 {
            next[p as usize] = nxt;
        }
        if nxt != -1 {
            prev[nxt as usize] = p;
        }

        if p != -1 && prev[p as usize] != -1 {
            let cp = cost(
                &pts[prev[p as usize] as usize],
                &pts[p as usize],
                &pts[nxt as usize],
            );
            costs[p as usize] = cp;
            heap.push(CostNode(p as usize, cp));
        }
        if nxt != -1 && next[nxt as usize] != -1 {
            let cn = cost(
                &pts[p as usize],
                &pts[nxt as usize],
                &pts[next[nxt as usize] as usize],
            );
            costs[nxt as usize] = cn;
            heap.push(CostNode(nxt as usize, cn));
        }
    }
    mask
}

pub fn cost(p: &[f32; 5], c: &[f32; 5], n: &[f32; 5]) -> f32 {
    let (vxi, vyi, vxo, vyo) = (c[0] - p[0], c[1] - p[1], n[0] - c[0], n[1] - c[1]);
    let (ni, no) = (vxi.hypot(vyi), vxo.hypot(vyo));
    if ni > 0.5 && no > 0.5 && (vxi * vxo + vyi * vyo) / (ni * no) < 0.866 {
        return f32::INFINITY;
    }
    let (vxb, vyb) = (n[0] - p[0], n[1] - p[1]);
    let blen = vxb.hypot(vyb);
    let (s_dev, t) = if blen > 1e-5 {
        (
            (vxb * vyi - vyb * vxi).abs() / blen,
            ((vxi * vxb + vyi * vyb) / (blen * blen)).clamp(0.0, 1.0),
        )
    } else {
        (ni, 0.5)
    };
    let mut a_dev = 0.0_f32;
    for i in 2..5 {
        a_dev = a_dev.max((c[i] - (p[i] + t * (n[i] - p[i]))).abs());
    }
    s_dev.max(a_dev)
}

#[wasm_bindgen]
pub struct AppEngine {
    zip_bytes: Vec<u8>,
    notes: Vec<Note>,
    deb_notes: Vec<Note>,
    shape_meta: HashMap<String, ShapeMeta>,
    meta_overrides: HashMap<String, (i32, f32)>,  // uuid -> (new_pen_type, new_thickness)
    pages: Vec<String>,
    canvas_w: f32,
    canvas_h: f32,
    resources: HashMap<String, Vec<u8>>,
    images: HashMap<String, HtmlImageElement>,
    bg_images: HashMap<String, HtmlImageElement>,
    note_background: Option<String>,
    templates: HashMap<String, serde_json::Value>,  // page_id -> template JSON
}

#[wasm_bindgen]
impl AppEngine {
    #[wasm_bindgen(constructor)]
    pub fn new(zip_bytes: &[u8]) -> Result<AppEngine, JsValue> {
        let nf = NoteFile::open(zip_bytes).map_err(|e| JsValue::from_str(&e))?;
        Ok(AppEngine {
            zip_bytes: zip_bytes.to_vec(), deb_notes: nf.notes.clone(), notes: nf.notes,
            shape_meta: nf.shape_meta, meta_overrides: HashMap::new(),
            pages: nf.pages, canvas_w: nf.canvas_w, canvas_h: nf.canvas_h,
            resources: nf.resources, images: HashMap::new(), bg_images: HashMap::new(),
            note_background: nf.note_background, templates: nf.templates,
        })
    }

    pub fn get_canvas_width(&self) -> f32 { self.canvas_w }
    pub fn get_canvas_height(&self) -> f32 { self.canvas_h }
    pub fn prepare_debloat(&mut self) { self.deb_notes = self.notes.clone(); self.meta_overrides.clear(); }
    pub fn get_note_count(&self) -> usize { self.notes.len() }

    pub fn debloat_note(&mut self, idx: usize, threshold: f32, press_eq: f32, tilt_eq: f32) {
        if idx >= self.notes.len() { return; }
        let p_scale = 1.0 / press_eq;
        let t_scale = 1.0 / tilt_eq;

        let stroke_data = &self.notes[idx];
        let mut new_strokes = Vec::new();

        for stroke in &stroke_data.strokes {
            if stroke.points.len() < 3 { new_strokes.push(stroke.clone()); continue; }
            let u_tx = unwrap_8bit(&stroke.points.iter().map(|p| p.tilt_x as f32).collect::<Vec<_>>());
            let u_ty = unwrap_8bit(&stroke.points.iter().map(|p| p.tilt_y as f32).collect::<Vec<_>>());
            let math_pts: Vec<[f32; 5]> = stroke.points.iter().enumerate().map(|(i, p)| { [p.x, p.y, p.pressure as f32 * p_scale, u_tx[i] * t_scale, u_ty[i] * t_scale] }).collect();
            let mask = decimate(&math_pts, threshold);

            // Replace nBrush (pen_type=21) with ballpoint (pen_type=2) for decimation
            // nBrush is too sensitive to point spacing on the device; ballpoint handles sparse points well
            let is_nbrush = self.shape_meta.get(&stroke.uuid).map_or(false, |m| m.pen_type == 21);
            if is_nbrush {
                let meta = &self.shape_meta[&stroke.uuid];
                // Compute equivalent ballpoint thickness from nbrush formula at median pressure
                let median_p: f32 = {
                    let mut pressures: Vec<f32> = stroke.points.iter().map(|p| p.pressure as f32 / 4095.0).collect();
                    pressures.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    pressures[pressures.len() / 2]
                };
                let nbrush_width = 2.07 * meta.thickness * median_p.powf(0.49) + 1.21;
                self.meta_overrides.insert(stroke.uuid.clone(), (2, nbrush_width));
            }

            let opt_pts: Vec<Point> = stroke.points.iter().enumerate().filter(|(i, _)| mask[*i]).map(|(_, p)| p.clone()).collect();

            new_strokes.push(Stroke { uuid: stroke.uuid.clone(), points: opt_pts });
        }
        self.deb_notes[idx] = Note { path: stroke_data.path.clone(), header: stroke_data.header.clone(), strokes: new_strokes };
    }

    pub fn get_orig_points(&self) -> usize { self.notes.iter().flat_map(|n| &n.strokes).map(|s| s.points.len()).sum() }
    pub fn get_deb_points(&self) -> usize { self.deb_notes.iter().flat_map(|n| &n.strokes).map(|s| s.points.len()).sum() }
    pub fn get_page_count(&self) -> usize { self.pages.len() }

    pub fn render_page(&mut self, canvas: &HtmlCanvasElement, use_deb: bool, page_idx: usize) {
        let ctx = canvas.get_context("2d").unwrap().unwrap().dyn_into::<CanvasRenderingContext2d>().unwrap();
        ctx.clear_rect(0.0, 0.0, self.canvas_w as f64, self.canvas_h as f64);
        ctx.set_fill_style_str("#ffffff");
        ctx.fill_rect(0.0, 0.0, self.canvas_w as f64, self.canvas_h as f64);
        ctx.set_line_cap("round"); ctx.set_line_join("round");

        if page_idx >= self.pages.len() { return; }
        let page_id = self.pages[page_idx].clone();

        // Draw page background image if available
        if let Some(bg_img) = self.bg_images.get(&page_id) {
            let _ = ctx.draw_image_with_html_image_element_and_dw_and_dh(
                bg_img, 0.0, 0.0, self.canvas_w as f64, self.canvas_h as f64
            );
        }

        // Draw template (ruled lines, grids, etc.) — loaded from CDN SVG
        let tmpl_key = format!("tmpl_{}", page_id);
        if let Some(tmpl_img) = self.bg_images.get(&tmpl_key) {
            Self::render_template(&ctx, tmpl_img, self.canvas_w as f64, self.canvas_h as f64);
        }

        // Build unified render list: strokes (Left) and geometric shapes (Right)
        enum RenderItem<'a> { Stroke(&'a Stroke), Shape(String) }
        let mut render_list: Vec<(u64, RenderItem)> = Vec::new();

        let strokes: Vec<&Stroke> = if use_deb {
            self.deb_notes.iter().filter(|n| n.path.contains(&page_id)).flat_map(|n| &n.strokes).collect()
        } else {
            self.notes.iter().filter(|n| n.path.contains(&page_id)).flat_map(|n| &n.strokes).collect()
        };

        let mut stroke_uuids = std::collections::HashSet::new();
        for s in &strokes {
            stroke_uuids.insert(s.uuid.clone());
            let ts = self.shape_meta.get(&s.uuid).map(|m| m.created_ts).unwrap_or(0);
            render_list.push((ts, RenderItem::Stroke(s)));
        }

        // Collect geometric shapes, images, text boxes, and feature shapes that belong to this page
        let shape_keys: Vec<String> = self.shape_meta.iter()
            .filter(|(k, m)| {
                !stroke_uuids.contains(*k) && m.page_id.as_deref() == Some(&page_id)
                && (!m.point_list.is_empty() || m.pen_type == 19 || m.pen_type == 6 || m.pen_type == 16
                    || (m.pen_type == 40 && m.extra_json.is_some()))
            })
            .map(|(k, _)| k.clone()).collect();
        for k in shape_keys { let ts = self.shape_meta[&k].created_ts; render_list.push((ts, RenderItem::Shape(k))); }

        render_list.sort_by_key(|(ts, item)| {
            let is_fill = match item {
                RenderItem::Stroke(s) => self.shape_meta.get(&s.uuid).map_or(false, |m| m.pen_type == 37),
                RenderItem::Shape(_) => false,
            };
            (if is_fill { 0u8 } else { 1 }, *ts)
        });

        for (_, item) in &render_list {
            match item {
                RenderItem::Shape(uuid) => {
                    let meta = self.shape_meta[uuid].clone();
                    match meta.pen_type {
                        19 => { // Image
                            if let Some(img) = self.images.get(uuid) {
                                Self::render_image(&ctx, &meta, img);
                            }
                        },
                        6 | 16 => { // Text box
                            Self::render_text(&ctx, &meta);
                        },
                        40 => { // GeoJSON feature shapes
                            Self::render_feature_shape(&ctx, &meta);
                        },
                        _ => {
                            if !meta.point_list.is_empty() {
                                Self::render_geometric_shape(&ctx, &meta);
                            }
                        }
                    }
                },
                RenderItem::Stroke(stroke) => {
                    if stroke.points.len() < 2 { continue; }
                    let mut meta = self.shape_meta.get(&stroke.uuid).cloned().unwrap_or_default();
                    // Apply pen_type/thickness overrides for debloated strokes
                    if use_deb {
                        if let Some(&(new_pt, new_th)) = self.meta_overrides.get(&stroke.uuid) {
                            meta.pen_type = new_pt;
                            meta.thickness = new_th;
                        }
                    }

                    // Text box in #points — render as text, not stroke
                    if meta.pen_type == 16 || meta.pen_type == 6 {
                        Self::render_text(&ctx, &meta);
                        continue;
                    }

                    // If this stroke's meta has point_list, render as geometric shape instead
                    if !meta.point_list.is_empty() {
                        Self::render_geometric_shape(&ctx, &meta);
                        continue;
                    }

                    // pen_type=40 shapes use extra_json (featureCollection), not point_list
                    if meta.pen_type == 40 && meta.extra_json.is_some() {
                        Self::render_feature_shape(&ctx, &meta);
                        continue;
                    }

                    Self::render_stroke(&ctx, stroke, &meta);
                }
            }
        }
    }

    pub fn export_stroke_data(&self, page_idx: usize) -> String {
        if page_idx >= self.pages.len() {
            return r#"{"canvas_w":0,"canvas_h":0,"strokes":[]}"#.to_string();
        }
        let page_id = &self.pages[page_idx];
        let mut strokes: Vec<&Stroke> = self.notes.iter()
            .filter(|n| n.path.contains(page_id))
            .flat_map(|n| &n.strokes)
            .collect();
        // Sort by created_ts to match render_page z-order (fills first, then timestamp)
        strokes.sort_by_key(|s| {
            let meta = self.shape_meta.get(&s.uuid);
            let is_fill = meta.map_or(false, |m| m.pen_type == 37);
            let ts = meta.map_or(0, |m| m.created_ts);
            (if is_fill { 0u8 } else { 1 }, ts)
        });

        let mut out_strokes = Vec::new();
        for stroke in &strokes {
            if stroke.points.len() < 2 { continue; }
            let meta = match self.shape_meta.get(&stroke.uuid) {
                Some(m) => m,
                None => continue,
            };
            // Skip non-stroke types (same logic as render_page)
            if meta.pen_type == 16 || meta.pen_type == 6 { continue; }
            if !meta.point_list.is_empty() { continue; }
            if meta.pen_type == 40 && meta.extra_json.is_some() { continue; }

            let mat = meta.matrix.unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
            let tilt_x_raw: Vec<f32> = stroke.points.iter().map(|p| p.tilt_x as f32).collect();
            let tilt_x_unwrapped = unwrap_8bit(&tilt_x_raw);

            let (r, g, b, a) = meta.color_rgba;
            let mut pts_json = Vec::new();
            for (i, p) in stroke.points.iter().enumerate() {
                let x = mat[0] * p.x + mat[1] * p.y + mat[2];
                let y = mat[3] * p.x + mat[4] * p.y + mat[5];
                pts_json.push(format!("[{},{},{},{},{}]",
                    x, y, p.pressure, tilt_x_unwrapped[i], p.tilt_y));
            }
            out_strokes.push(format!(
                r#"{{"pen_type":{},"thickness":{},"color":[{},{},{}],"alpha":{},"points":[{}]}}"#,
                meta.pen_type, meta.thickness, r, g, b, a,
                pts_json.join(",")
            ));
        }
        format!(
            r#"{{"canvas_w":{},"canvas_h":{},"strokes":[{}]}}"#,
            self.canvas_w, self.canvas_h, out_strokes.join(",")
        )
    }

    fn render_template(ctx: &CanvasRenderingContext2d, img: &HtmlImageElement, w: f64, h: f64) {
        let _ = ctx.draw_image_with_html_image_element_and_dw_and_dh(img, 0.0, 0.0, w, h);
    }

    /// Compute thickness in page coordinates by scaling with the matrix scale factor.
    /// Thickness in .note files is stored in local (untransformed) space; when we manually
    /// transform points by the matrix, we must also scale the line width.
    fn scaled_thickness(meta: &ShapeMeta) -> f64 {
        let t = meta.thickness as f64;
        if let Some(m) = meta.matrix {
            let sx = ((m[0] as f64).powi(2) + (m[3] as f64).powi(2)).sqrt();
            let sy = ((m[1] as f64).powi(2) + (m[4] as f64).powi(2)).sqrt();
            t * (sx + sy) / 2.0
        } else {
            t
        }
    }

    fn render_geometric_shape(ctx: &CanvasRenderingContext2d, meta: &ShapeMeta) {
        let pts: Vec<[f32; 2]> = meta.point_list.iter().map(|p| {
            if let Some(m) = meta.matrix { [m[0]*p[0] + m[1]*p[1] + m[2], m[3]*p[0] + m[4]*p[1] + m[5]] }
            else { *p }
        }).collect();
        if pts.is_empty() { return; }

        ctx.save();
        ctx.set_global_alpha(meta.color_rgba.3 as f64);
        let col = format!("rgb({},{},{})", meta.color_rgba.0, meta.color_rgba.1, meta.color_rgba.2);
        ctx.set_stroke_style_str(&col);
        ctx.set_line_width(Self::scaled_thickness(meta));
        ctx.set_line_cap("round"); ctx.set_line_join("round");

        match meta.pen_type {
            0 => { // Circle/Ellipse — 2 points define bounding box
                if pts.len() >= 2 {
                    let cx = (pts[0][0] + pts[1][0]) as f64 / 2.0;
                    let cy = (pts[0][1] + pts[1][1]) as f64 / 2.0;
                    let rx = ((pts[1][0] - pts[0][0]) / 2.0).abs() as f64;
                    let ry = ((pts[1][1] - pts[0][1]) / 2.0).abs() as f64;
                    ctx.begin_path();
                    ctx.ellipse(cx, cy, rx, ry, 0.0, 0.0, 2.0 * std::f64::consts::PI).unwrap();
                    if let Some(fc) = meta.fill_color {
                        ctx.set_fill_style_str(&format!("rgba({},{},{},{})", fc.0, fc.1, fc.2, fc.3));
                        ctx.fill();
                    }
                    ctx.stroke();
                }
            },
            1 => { // Rectangle — 2 points define corners
                if pts.len() >= 2 {
                    let x = pts[0][0].min(pts[1][0]) as f64;
                    let y = pts[0][1].min(pts[1][1]) as f64;
                    let w = (pts[1][0] - pts[0][0]).abs() as f64;
                    let h = (pts[1][1] - pts[0][1]).abs() as f64;
                    ctx.begin_path();
                    ctx.rect(x, y, w, h);
                    if let Some(fc) = meta.fill_color {
                        ctx.set_fill_style_str(&format!("rgba({},{},{},{})", fc.0, fc.1, fc.2, fc.3));
                        ctx.fill();
                    }
                    ctx.stroke();
                }
            },
            7 => { // Line
                if pts.len() >= 2 {
                    ctx.begin_path();
                    ctx.move_to(pts[0][0] as f64, pts[0][1] as f64);
                    ctx.line_to(pts[1][0] as f64, pts[1][1] as f64);
                    ctx.stroke();
                }
            },
            28 => { // Arrow Line — line + arrowhead
                if pts.len() >= 2 {
                    ctx.begin_path();
                    ctx.move_to(pts[0][0] as f64, pts[0][1] as f64);
                    ctx.line_to(pts[1][0] as f64, pts[1][1] as f64);
                    ctx.stroke();
                    // Filled triangular arrowhead
                    let dx = (pts[1][0] - pts[0][0]) as f64;
                    let dy = (pts[1][1] - pts[0][1]) as f64;
                    let angle = dy.atan2(dx);
                    let head_len = (meta.thickness as f64 * 4.0).max(12.0);
                    let spread = 0.45;
                    ctx.begin_path();
                    ctx.move_to(pts[1][0] as f64, pts[1][1] as f64);
                    ctx.line_to(pts[1][0] as f64 - head_len * (angle - spread).cos(), pts[1][1] as f64 - head_len * (angle - spread).sin());
                    ctx.line_to(pts[1][0] as f64 - head_len * (angle + spread).cos(), pts[1][1] as f64 - head_len * (angle + spread).sin());
                    ctx.close_path();
                    ctx.fill();
                }
            },
            31 => { // Polyline — open
                if pts.len() >= 2 {
                    ctx.begin_path();
                    ctx.move_to(pts[0][0] as f64, pts[0][1] as f64);
                    for p in pts.iter().skip(1) { ctx.line_to(p[0] as f64, p[1] as f64); }
                    ctx.stroke();
                }
            },
            8 | 10 | 11 | 12 | 17 | 18 | 24 | 26 | 27 => {
                // Closed polygons: Triangle variants (8,10,11,12), Polygon (17),
                // Rhombus (18), Regular Polygon (24), Trapezoid (26), Hexagon (27)
                if pts.len() >= 3 {
                    ctx.begin_path();
                    ctx.move_to(pts[0][0] as f64, pts[0][1] as f64);
                    for p in pts.iter().skip(1) { ctx.line_to(p[0] as f64, p[1] as f64); }
                    ctx.close_path();
                    if let Some(fc) = meta.fill_color {
                        ctx.set_fill_style_str(&format!("rgba({},{},{},{})", fc.0, fc.1, fc.2, fc.3));
                        ctx.fill();
                    }
                    ctx.stroke();
                }
            },
            _ => { // Unknown geometric shape — render as closed polygon if 3+ points, line if 2
                if pts.len() >= 3 {
                    ctx.begin_path();
                    ctx.move_to(pts[0][0] as f64, pts[0][1] as f64);
                    for p in pts.iter().skip(1) { ctx.line_to(p[0] as f64, p[1] as f64); }
                    ctx.close_path();
                    if let Some(fc) = meta.fill_color {
                        ctx.set_fill_style_str(&format!("rgba({},{},{},{})", fc.0, fc.1, fc.2, fc.3));
                        ctx.fill();
                    }
                    ctx.stroke();
                } else if pts.len() == 2 {
                    ctx.begin_path();
                    ctx.move_to(pts[0][0] as f64, pts[0][1] as f64);
                    ctx.line_to(pts[1][0] as f64, pts[1][1] as f64);
                    ctx.stroke();
                }
            }
        }
        ctx.restore();
    }

    fn render_image(ctx: &CanvasRenderingContext2d, meta: &ShapeMeta, img: &HtmlImageElement) {
        ctx.save();
        // Get position from point_list (2 points: top-left, bottom-right) or bounding_rect
        let (x, y, w, h) = if meta.point_list.len() >= 2 {
            let mut p0 = meta.point_list[0];
            let mut p1 = meta.point_list[1];
            if let Some(m) = meta.matrix {
                p0 = [m[0]*p0[0] + m[1]*p0[1] + m[2], m[3]*p0[0] + m[4]*p0[1] + m[5]];
                p1 = [m[0]*p1[0] + m[1]*p1[1] + m[2], m[3]*p1[0] + m[4]*p1[1] + m[5]];
            }
            let x = p0[0].min(p1[0]);
            let y = p0[1].min(p1[1]);
            ((x as f64), (y as f64), (p0[0] - p1[0]).abs() as f64, (p0[1] - p1[1]).abs() as f64)
        } else if let Some(rect) = meta.bounding_rect {
            // bounding_rect is [top, left, bottom, right]
            let mut top = rect[0]; let mut left = rect[1];
            let mut bottom = rect[2]; let mut right = rect[3];
            if let Some(m) = meta.matrix {
                let tl = [m[0]*left + m[1]*top + m[2], m[3]*left + m[4]*top + m[5]];
                let br = [m[0]*right + m[1]*bottom + m[2], m[3]*right + m[4]*bottom + m[5]];
                left = tl[0]; top = tl[1]; right = br[0]; bottom = br[1];
            }
            (left as f64, top as f64, (right - left) as f64, (bottom - top) as f64)
        } else {
            ctx.restore();
            return;
        };

        ctx.set_global_alpha(meta.color_rgba.3 as f64);
        let _ = ctx.draw_image_with_html_image_element_and_dw_and_dh(img, x, y, w, h);
        ctx.restore();
    }

    fn render_text(ctx: &CanvasRenderingContext2d, meta: &ShapeMeta) {
        let text = match &meta.text {
            Some(t) if !t.is_empty() => t.clone(),
            _ => match &meta.rich_text {
                Some(rt) if !rt.is_empty() => {
                    // Strip HTML tags for plain-text fallback
                    let mut plain = String::new();
                    let mut in_tag = false;
                    for ch in rt.chars() {
                        match ch {
                            '<' => in_tag = true,
                            '>' => in_tag = false,
                            _ if !in_tag => plain.push(ch),
                            _ => {}
                        }
                    }
                    if plain.trim().is_empty() { return; }
                    plain
                },
                _ => return,
            }
        };

        ctx.save();

        // Get bounding box from bounding_rect or point_list
        let (x, y, box_w, _box_h) = if let Some(rect) = meta.bounding_rect {
            let mut top = rect[0]; let mut left = rect[1];
            let mut bottom = rect[2]; let mut right = rect[3];
            if let Some(m) = meta.matrix {
                let tl = [m[0]*left + m[1]*top + m[2], m[3]*left + m[4]*top + m[5]];
                let br = [m[0]*right + m[1]*bottom + m[2], m[3]*right + m[4]*bottom + m[5]];
                left = tl[0]; top = tl[1]; right = br[0]; bottom = br[1];
            }
            (left as f64, top as f64, (right - left) as f64, (bottom - top) as f64)
        } else if meta.point_list.len() >= 2 {
            let mut p0 = meta.point_list[0];
            let mut p1 = meta.point_list[1];
            if let Some(m) = meta.matrix {
                p0 = [m[0]*p0[0] + m[1]*p0[1] + m[2], m[3]*p0[0] + m[4]*p0[1] + m[5]];
                p1 = [m[0]*p1[0] + m[1]*p1[1] + m[2], m[3]*p1[0] + m[4]*p1[1] + m[5]];
            }
            let bx = p0[0].min(p1[0]) as f64;
            let by = p0[1].min(p1[1]) as f64;
            (bx, by, (p0[0] - p1[0]).abs() as f64, (p0[1] - p1[1]).abs() as f64)
        } else {
            ctx.restore();
            return;
        };

        // Parse text style
        let mut font_size = 32.0_f64;
        let mut bold = false;
        let mut italic = false;
        let mut align = "left";
        let mut line_spacing = 1.2_f64;
        let mut border_width = 0.0_f64;

        if let Some(ref style_json) = meta.text_style {
            if let Ok(style) = serde_json::from_str::<serde_json::Value>(style_json) {
                if let Some(sz) = style.get("textSize").and_then(|v| v.as_f64()) { font_size = sz; }
                if let Some(b) = style.get("textBold").and_then(|v| v.as_bool()) { bold = b; }
                if let Some(i) = style.get("textItalic").and_then(|v| v.as_bool()) { italic = i; }
                if let Some(a) = style.get("alignType").and_then(|v| v.as_i64()) {
                    align = match a { 1 => "center", 2 => "right", _ => "left" };
                }
                if let Some(sp) = style.get("textSpacing").and_then(|v| v.as_f64()) {
                    if sp > 0.0 { line_spacing = sp; }
                }
                if let Some(bw) = style.get("borderWidth").and_then(|v| v.as_f64()) {
                    border_width = bw;
                }
            }
        }

        let font_str = format!("{}{}{:.0}px sans-serif",
            if italic { "italic " } else { "" },
            if bold { "bold " } else { "" },
            font_size);
        ctx.set_font(&font_str);

        let col = format!("rgba({},{},{},{})", meta.color_rgba.0, meta.color_rgba.1, meta.color_rgba.2, meta.color_rgba.3);
        ctx.set_fill_style_str(&col);
        ctx.set_text_align(align);

        let line_height = font_size * line_spacing;
        let text_x = match align {
            "center" => x + box_w / 2.0,
            "right" => x + box_w,
            _ => x,
        };
        ctx.set_text_baseline("top");

        // Vertical positioning: distribute extra space proportionally to
        // font ascent/descent ratio (~0.8/0.2 for Latin fonts), matching
        // Android's text layout behavior.
        let extra_space = (_box_h - 2.0 * border_width - font_size).max(0.0);
        let y_offset = border_width + extra_space * 0.8;
        let mut cur_y = y + y_offset;
        for paragraph in text.split('\n') {
            if paragraph.is_empty() {
                cur_y += line_height;
                continue;
            }
            let words: Vec<&str> = paragraph.split_whitespace().collect();
            if words.is_empty() { cur_y += line_height; continue; }

            let mut line = String::new();
            for word in &words {
                let test = if line.is_empty() { word.to_string() } else { format!("{} {}", line, word) };
                let measured = ctx.measure_text(&test).unwrap_or_else(|_| ctx.measure_text("").unwrap());
                if !line.is_empty() && measured.width() > box_w {
                    let _ = ctx.fill_text(&line, text_x, cur_y);
                    cur_y += line_height;
                    line = word.to_string();
                } else {
                    line = test;
                }
            }
            if !line.is_empty() {
                let _ = ctx.fill_text(&line, text_x, cur_y);
                cur_y += line_height;
            }
        }

        ctx.restore();
    }

    fn render_feature_shape(ctx: &CanvasRenderingContext2d, meta: &ShapeMeta) {
        let extra = match &meta.extra_json {
            Some(s) if !s.is_empty() => s,
            _ => return,
        };

        // Parse the outer extra JSON which contains a featureCollection string
        let outer: serde_json::Value = match serde_json::from_str(extra) {
            Ok(v) => v,
            _ => return,
        };

        // featureCollection is a JSON string inside the outer JSON
        let fc_str = match outer.get("featureCollection").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s,
            _ => return,
        };

        let fc: serde_json::Value = match serde_json::from_str(fc_str) {
            Ok(v) => v,
            _ => return,
        };

        let features = match fc.get("features").and_then(|v| v.as_array()) {
            Some(f) => f,
            _ => return,
        };

        ctx.save();
        ctx.set_global_alpha(meta.color_rgba.3 as f64);
        let col = format!("rgb({},{},{})", meta.color_rgba.0, meta.color_rgba.1, meta.color_rgba.2);
        ctx.set_stroke_style_str(&col);
        ctx.set_fill_style_str(&col);
        let effective_thickness = Self::scaled_thickness(meta);
        ctx.set_line_width(effective_thickness);
        ctx.set_line_cap("round");
        ctx.set_line_join("round");

        let matrix = meta.matrix.unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);

        for feature in features {
            Self::render_feature(ctx, feature, &matrix, effective_thickness, &meta.color_rgba);
        }

        ctx.restore();
    }



    fn render_feature(ctx: &CanvasRenderingContext2d, feature: &serde_json::Value, matrix: &[f32; 6], thickness: f64, _color: &(u8, u8, u8, f32)) {
        let geometry = match feature.get("geometry") {
            Some(g) => g,
            _ => return,
        };
        let geo_type = geometry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let sub_type = geometry.get("subType").and_then(|v| v.as_str())
            .or_else(|| feature.get("properties").and_then(|p| p.get("subType")).and_then(|v| v.as_str()))
            .unwrap_or("");
        let coords = geometry.get("coordinates");

        // Check for per-feature fill (in properties.fillAttr)
        let fill_color: Option<String> = feature.get("properties")
            .and_then(|p| p.get("fillAttr"))
            .and_then(|fa| {
                if fa.get("enableColor").and_then(|v| v.as_bool()).unwrap_or(false) {
                    fa.get("color").and_then(|v| v.as_i64()).map(|c| {
                        let cu = c as u32;
                        let r = (cu >> 16) & 0xFF; let g = (cu >> 8) & 0xFF; let b = cu & 0xFF;
                        let a = ((cu >> 24) & 0xFF) as f64 / 255.0;
                        format!("rgba({},{},{},{})", r, g, b, a)
                    })
                } else { None }
            });

        // Check for per-feature stroke overrides (in properties.strokeAttr)
        if let Some(stroke_attr) = feature.get("properties").and_then(|p| p.get("strokeAttr")) {
            if let Some(w) = stroke_attr.get("width").and_then(|v| v.as_f64()) {
                // strokeAttr lineWidth is also in local coords; scale by matrix
                let sx = ((matrix[0] as f64).powi(2) + (matrix[3] as f64).powi(2)).sqrt();
                let sy = ((matrix[1] as f64).powi(2) + (matrix[4] as f64).powi(2)).sqrt();
                ctx.set_line_width(w * (sx + sy) / 2.0);
            }
            if let Some(c) = stroke_attr.get("color").and_then(|v| v.as_i64()) {
                let cu = c as u32;
                let r = (cu >> 16) & 0xFF; let g = (cu >> 8) & 0xFF; let b = cu & 0xFF;
                let a = ((cu >> 24) & 0xFF) as f64 / 255.0;
                let col = format!("rgba({},{},{},{})", r, g, b, a);
                ctx.set_stroke_style_str(&col);
            }
        }

        // Dashed lines: set line dash if lineStyle present (in properties)
        let dash_intervals = feature.get("properties").and_then(|p| p.get("lineStyle"))
            .or_else(|| feature.get("lineStyle"))
            .and_then(|ls| ls.get("dashLineIntervals"))
            .and_then(|v| v.as_array())
            .map(|intervals| {
                let sx = ((matrix[0] as f64).powi(2) + (matrix[3] as f64).powi(2)).sqrt();
                let sy = ((matrix[1] as f64).powi(2) + (matrix[4] as f64).powi(2)).sqrt();
                let avg_scale = (sx + sy) / 2.0;
                intervals.iter().filter_map(|v| v.as_f64()).map(|n| {
                    n * avg_scale + thickness * 2.0
                }).collect::<Vec<f64>>()
            });

        let has_dash = if let Some(ref dash) = dash_intervals {
            if !dash.is_empty() {
                let arr = js_sys::Array::new();
                for d in dash { arr.push(&JsValue::from_f64(*d)); }
                ctx.set_line_dash(&arr).ok();
                true
            } else { false }
        } else { false };

        match geo_type {
            "LineString" => {
                let pts = match coords.and_then(|c| c.as_array()) {
                    Some(a) if a.len() >= 2 => a,
                    _ => { if has_dash { ctx.set_line_dash(&js_sys::Array::new()).ok(); } return; },
                };

                if sub_type == "WaveLine" {
                    let p0 = parse_coord(&pts[0]).unwrap_or([0.0; 2]);
                    let p1 = parse_coord(&pts[pts.len()-1]).unwrap_or([0.0; 2]);
                    let (x0, y0) = transform_point(&p0, matrix);
                    let (x1, y1) = transform_point(&p1, matrix);
                    // Read waveAttr from feature properties
                    let mut wavy_len = 24.0_f64;
                    let mut wavy_peak = 6.0_f64;
                    if let Some(wa) = feature.get("properties").and_then(|p| p.get("waveAttr"))
                        .or_else(|| feature.get("waveAttr"))
                        .or_else(|| geometry.get("waveAttr")) {
                        if let Some(wl) = wa.get("wavyLength").and_then(|v| v.as_f64()) { wavy_len = wl; }
                        if let Some(wp) = wa.get("wavyPeak").and_then(|v| v.as_f64()) { wavy_peak = wp; }
                    }
                    let dx = x1 - x0; let dy = y1 - y0;
                    let len = (dx*dx + dy*dy).sqrt();
                    if len < 0.1 { if has_dash { ctx.set_line_dash(&js_sys::Array::new()).ok(); } return; }
                    let waves = (len / wavy_len).max(1.0);
                    let steps = (waves * 12.0) as usize;
                    ctx.begin_path();
                    for i in 0..=steps {
                        let t = i as f64 / steps as f64;
                        let wave_y = wavy_peak * (t * waves * 2.0 * std::f64::consts::PI).sin();
                        let nx = -dy / len; let ny = dx / len;
                        let px = x0 + dx * t + nx * wave_y;
                        let py = y0 + dy * t + ny * wave_y;
                        if i == 0 { ctx.move_to(px, py); } else { ctx.line_to(px, py); }
                    }
                    ctx.stroke();
                } else {
                    ctx.begin_path();
                    for (i, pt) in pts.iter().enumerate() {
                        if let Some(p) = parse_coord(pt) {
                            let (x, y) = transform_point(&p, matrix);
                            if i == 0 { ctx.move_to(x, y); } else { ctx.line_to(x, y); }
                        }
                    }
                    ctx.stroke();
                }
            },
            "DirectionLine" => {
                let pts = match coords.and_then(|c| c.as_array()) {
                    Some(a) if a.len() >= 2 => a,
                    _ => { if has_dash { ctx.set_line_dash(&js_sys::Array::new()).ok(); } return; },
                };
                let p0 = parse_coord(&pts[0]).unwrap_or([0.0; 2]);
                let p1 = parse_coord(&pts[pts.len()-1]).unwrap_or([0.0; 2]);
                let (x0, y0) = transform_point(&p0, matrix);
                let (x1, y1) = transform_point(&p1, matrix);
                ctx.begin_path();
                ctx.move_to(x0, y0); ctx.line_to(x1, y1); ctx.stroke();
                Self::draw_arrowhead(ctx, x0, y0, x1, y1, thickness);
            },
            "BidirectionalLine" => {
                let pts = match coords.and_then(|c| c.as_array()) {
                    Some(a) if a.len() >= 2 => a,
                    _ => { if has_dash { ctx.set_line_dash(&js_sys::Array::new()).ok(); } return; },
                };
                let p0 = parse_coord(&pts[0]).unwrap_or([0.0; 2]);
                let p1 = parse_coord(&pts[pts.len()-1]).unwrap_or([0.0; 2]);
                let (x0, y0) = transform_point(&p0, matrix);
                let (x1, y1) = transform_point(&p1, matrix);
                ctx.begin_path();
                ctx.move_to(x0, y0); ctx.line_to(x1, y1); ctx.stroke();
                Self::draw_arrowhead(ctx, x0, y0, x1, y1, thickness);
                Self::draw_arrowhead(ctx, x1, y1, x0, y0, thickness);
            },
            "MultiLineString" => {
                let segments = match coords.and_then(|c| c.as_array()) {
                    Some(a) => a,
                    _ => { if has_dash { ctx.set_line_dash(&js_sys::Array::new()).ok(); } return; },
                };
                for seg in segments {
                    if let Some(pts) = seg.as_array() {
                        if pts.len() < 2 { continue; }
                        ctx.begin_path();
                        for (i, pt) in pts.iter().enumerate() {
                            if let Some(p) = parse_coord(pt) {
                                let (x, y) = transform_point(&p, matrix);
                                if i == 0 { ctx.move_to(x, y); } else { ctx.line_to(x, y); }
                            }
                        }
                        ctx.stroke();
                    }
                }
            },
            "Polygon" => {
                // Coords: [[ [start,end], [start,end], ... ]]
                // Each element is a pair [startPt, endPt]. Extract first point of each pair as vertex.
                let rings = match coords.and_then(|c| c.as_array()) {
                    Some(a) => a,
                    _ => { if has_dash { ctx.set_line_dash(&js_sys::Array::new()).ok(); } return; },
                };
                for ring in rings {
                    let pairs = match ring.as_array() {
                        Some(a) if !a.is_empty() => a,
                        _ => continue,
                    };
                    ctx.begin_path();
                    let mut first = true;
                    for pair in pairs {
                        let arr = match pair.as_array() {
                            Some(a) if !a.is_empty() => a,
                            _ => continue,
                        };
                        // pair is [[x0,y0],[x1,y1]] — take first element
                        let vertex = if let Some(inner) = arr[0].as_array() {
                            // Nested: [[x,y], ...]
                            [inner[0].as_f64().unwrap_or(0.0), inner[1].as_f64().unwrap_or(0.0)]
                        } else {
                            // Flat: [x, y] — the pair itself is the vertex
                            [arr[0].as_f64().unwrap_or(0.0), arr.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0)]
                        };
                        let (x, y) = transform_point(&vertex, matrix);
                        if first { ctx.move_to(x, y); first = false; } else { ctx.line_to(x, y); }
                    }
                    ctx.close_path();
                    if let Some(ref fc) = fill_color {
                        ctx.set_fill_style_str(fc);
                        ctx.fill();
                    }
                    ctx.stroke();
                }
            },
            "MultiPoint" => {
                let pts = match coords.and_then(|c| c.as_array()) {
                    Some(a) => a,
                    _ => { if has_dash { ctx.set_line_dash(&js_sys::Array::new()).ok(); } return; },
                };
                match sub_type {
                    "Oval" => {
                        if pts.len() >= 2 {
                            let p0 = parse_coord(&pts[0]).unwrap_or([0.0; 2]);
                            let p1 = parse_coord(&pts[1]).unwrap_or([0.0; 2]);
                            // Bounding box [min, max] → ellipse
                            // Transform all 4 corners to handle non-uniform scaling
                            let (x0, y0) = transform_point(&p0, matrix);
                            let (x1, y1) = transform_point(&p1, matrix);
                            let cx = (x0 + x1) / 2.0;
                            let cy = (y0 + y1) / 2.0;
                            let rx = ((x1 - x0) / 2.0).abs();
                            let ry = ((y1 - y0) / 2.0).abs();
                            if rx > 0.1 && ry > 0.1 {
                                ctx.begin_path();
                                let _ = ctx.ellipse(cx, cy, rx, ry, 0.0, 0.0, 2.0 * std::f64::consts::PI);
                                if let Some(ref fc) = fill_color {
                                    ctx.set_fill_style_str(fc);
                                    ctx.fill();
                                }
                                ctx.stroke();
                            }
                        }
                    },
                    "Curve" => {
                        if pts.len() >= 3 {
                            let p0 = parse_coord(&pts[0]).unwrap_or([0.0; 2]);
                            let p1 = parse_coord(&pts[1]).unwrap_or([0.0; 2]);
                            let p2 = parse_coord(&pts[2]).unwrap_or([0.0; 2]);
                            let (x0, y0) = transform_point(&p0, matrix);
                            let (cx, cy) = transform_point(&p1, matrix);
                            let (x2, y2) = transform_point(&p2, matrix);
                            ctx.begin_path();
                            ctx.move_to(x0, y0);
                            ctx.quadratic_curve_to(cx, cy, x2, y2);
                            ctx.stroke();
                        }
                    },
                    "Arc" => {
                        // Elliptical arc: [bboxMin, bboxMax, angleControl]
                        // angle_ctrl[0] < 90 → front/visible half, >= 90 → back/hidden half
                        // Arcs are always half-ellipses (full ellipses use Oval subType).
                        if pts.len() >= 3 {
                            let bbox_min = parse_coord(&pts[0]).unwrap_or([0.0; 2]);
                            let bbox_max = parse_coord(&pts[1]).unwrap_or([0.0; 2]);
                            let angle_ctrl = parse_coord(&pts[2]).unwrap_or([0.0; 2]);

                            let (x0, y0) = transform_point(&bbox_min, matrix);
                            let (x1, y1) = transform_point(&bbox_max, matrix);
                            let ecx = (x0 + x1) / 2.0;
                            let ecy = (y0 + y1) / 2.0;
                            let rx = ((x1 - x0) / 2.0).abs();
                            let ry = ((y1 - y0) / 2.0).abs();

                            if rx > 0.1 && ry > 0.1 {
                                // angle_ctrl[0] < 90 = "front" arc → bottom half in canvas
                                // y_flip inverts which canvas half to use
                                let is_front = (angle_ctrl[0] as i32) < 90;
                                let y_flip = matrix[4] < 0.0;
                                let draw_lower = is_front ^ y_flip;
                                let (start, end) = if draw_lower {
                                    (0.0, std::f64::consts::PI)                     // bottom half
                                } else {
                                    (std::f64::consts::PI, 2.0 * std::f64::consts::PI) // top half
                                };
                                ctx.begin_path();
                                let _ = ctx.ellipse(ecx, ecy, rx, ry, 0.0, start, end);
                                if let Some(ref fc) = fill_color {
                                    ctx.set_fill_style_str(fc);
                                    ctx.fill();
                                }
                                ctx.stroke();
                            }
                        }
                    },
                    "Bracket" => {
                        // Curly brace {: 3 points [tip, end1, end2]
                        // Cubic Bezier S-curves computed in LOCAL coords so the
                        // bracket's aspect ratio keeps curves tight.
                        if pts.len() >= 3 {
                            let p0 = parse_coord(&pts[0]).unwrap_or([0.0; 2]); // tip
                            let p1 = parse_coord(&pts[1]).unwrap_or([0.0; 2]); // end1
                            let p2 = parse_coord(&pts[2]).unwrap_or([0.0; 2]); // end2
                            // Control points in LOCAL space:
                            // - vertical tangent at each end (same x)
                            // - horizontal tangent at tip (same y)
                            let fy = 0.6_f64; // y-fraction: how far P1 extends toward tip
                            let fx = 0.4_f64; // x-fraction: how far P2 sits from tip
                            // end1 → tip
                            let c1 = [p1[0], p1[1] + (p0[1] - p1[1]) * fy];
                            let c2 = [p0[0] + (p1[0] - p0[0]) * fx, p0[1]];
                            // tip → end2
                            let c3 = [p0[0] + (p2[0] - p0[0]) * fx, p0[1]];
                            let c4 = [p2[0], p2[1] + (p0[1] - p2[1]) * fy];
                            // Transform to page coords
                            let (tipx, tipy) = transform_point(&p0, matrix);
                            let (x1, y1) = transform_point(&p1, matrix);
                            let (x2, y2) = transform_point(&p2, matrix);
                            let (c1x, c1y) = transform_point(&c1, matrix);
                            let (c2x, c2y) = transform_point(&c2, matrix);
                            let (c3x, c3y) = transform_point(&c3, matrix);
                            let (c4x, c4y) = transform_point(&c4, matrix);
                            ctx.begin_path();
                            ctx.move_to(x1, y1);
                            ctx.bezier_curve_to(c1x, c1y, c2x, c2y, tipx, tipy);
                            ctx.bezier_curve_to(c3x, c3y, c4x, c4y, x2, y2);
                            ctx.stroke();
                        }
                    },
                    _ => {
                        // Unknown subtype — draw as polyline/polygon
                        if !pts.is_empty() {
                            ctx.begin_path();
                            for (i, pt) in pts.iter().enumerate() {
                                if let Some(p) = parse_coord(pt) {
                                    let (x, y) = transform_point(&p, matrix);
                                    if i == 0 { ctx.move_to(x, y); } else { ctx.line_to(x, y); }
                                }
                            }
                            if fill_color.is_some() { ctx.close_path(); }
                            if let Some(ref fc) = fill_color {
                                ctx.set_fill_style_str(fc);
                                ctx.fill();
                            }
                            ctx.stroke();
                        }
                    }
                }
            },
            "FeatureCollection" => {
                // Recurse into nested features (can be at geometry or feature level)
                if let Some(sub_features) = geometry.get("features").and_then(|v| v.as_array())
                    .or_else(|| feature.get("features").and_then(|v| v.as_array())) {
                    for sf in sub_features {
                        Self::render_feature(ctx, sf, matrix, thickness, _color);
                    }
                }
            },
            _ => {
                // Check for FeatureCollection at feature level
                if let Some(sub_features) = feature.get("features").and_then(|v| v.as_array()) {
                    for sf in sub_features {
                        Self::render_feature(ctx, sf, matrix, thickness, _color);
                    }
                }
            }
        }

        // Reset dash pattern after this feature
        if has_dash {
            ctx.set_line_dash(&js_sys::Array::new()).ok();
        }
    }

    fn draw_arrowhead(ctx: &CanvasRenderingContext2d, _from_x: f64, _from_y: f64, to_x: f64, to_y: f64, thickness: f64) {
        let dx = to_x - _from_x;
        let dy = to_y - _from_y;
        let angle = dy.atan2(dx);
        let head_len = (thickness * 2.0).max(8.0);
        let spread = 0.5;
        // Filled + stroked arrowhead for rounded, bold appearance
        ctx.begin_path();
        ctx.move_to(to_x, to_y);
        ctx.line_to(to_x - head_len * (angle - spread).cos(), to_y - head_len * (angle - spread).sin());
        ctx.line_to(to_x - head_len * (angle + spread).cos(), to_y - head_len * (angle + spread).sin());
        ctx.close_path();
        ctx.fill();
        ctx.stroke();
    }

    fn render_stroke(ctx: &CanvasRenderingContext2d, stroke: &Stroke, meta: &ShapeMeta) {
        ctx.save();
        ctx.set_global_alpha(meta.color_rgba.3 as f64);
        if meta.pen_type == 15 {
            ctx.set_global_composite_operation("multiply").unwrap();
            ctx.set_global_alpha(meta.color_rgba.3 as f64 * 0.5);
        }

        let col = format!("rgb({},{},{})", meta.color_rgba.0, meta.color_rgba.1, meta.color_rgba.2);
        ctx.set_stroke_style_str(&col);
        ctx.set_fill_style_str(&col);

        let u_tx = unwrap_8bit(&stroke.points.iter().map(|p| p.tilt_x as f32).collect::<Vec<_>>());
        let pts: Vec<[f32; 5]> = stroke.points.iter().enumerate().map(|(i, p)| {
            let (mut x, mut y) = (p.x, p.y);
            if let Some(m) = meta.matrix { x = m[0]*p.x + m[1]*p.y + m[2]; y = m[3]*p.x + m[4]*p.y + m[5]; }
            [x, y, (p.pressure as f32).clamp(1.0, 4095.0), u_tx[i], p.tilt_y as f32]
        }).collect();

        match meta.pen_type {
            5 | 21 => { // Fountain & nBrush
                let is_fountain = meta.pen_type == 5;
                for i in 0..pts.len() - 1 {
                    let w1 = (if is_fountain { meta.thickness * 1.37 * (pts[i][2]/4095.0).powf(0.59) } else { 2.07 * meta.thickness * (pts[i][2]/4095.0).powf(0.49) + 1.21 }).max(0.5);
                    let w2 = (if is_fountain { meta.thickness * 1.37 * (pts[i+1][2]/4095.0).powf(0.59) } else { 2.07 * meta.thickness * (pts[i+1][2]/4095.0).powf(0.49) + 1.21 }).max(0.5);
                    ctx.begin_path(); ctx.set_line_width(((w1 + w2) / 2.0) as f64);
                    ctx.move_to(pts[i][0] as f64, pts[i][1] as f64); ctx.line_to(pts[i+1][0] as f64, pts[i+1][1] as f64); ctx.stroke();
                }
            },
            60 | 61 => { // Calligraphy Brushes
                let n = pts.len();
                let mut smooth_nib = vec![0.0; n];
                smooth_nib[0] = pts[0][3] * (2.0 * std::f32::consts::PI / 256.0);
                for i in 1..n { smooth_nib[i] = smooth_nib[i-1] + 0.15 * (pts[i][3] * (2.0 * std::f32::consts::PI / 256.0) - smooth_nib[i-1]); }

                let mut stroke_angle = vec![0.0; n];
                for i in 0..n {
                    let j0 = i.saturating_sub(3); let j1 = (i + 3).min(n - 1);
                    stroke_angle[i] = (pts[j1][1] - pts[j0][1]).atan2(pts[j1][0] - pts[j0][0]);
                }
                let smooth_dir = smooth_angle_ema(&stroke_angle, 0.3);

                let min_frac = if meta.pen_type == 60 { 0.18 } else { 0.211 };
                let mut raw_widths = vec![0.0; n];
                for i in 0..n {
                    let diff = if meta.pen_type == 60 { smooth_dir[i] - smooth_nib[i] } else { smooth_dir[i] - 0.782 };
                    let chisel = if meta.pen_type == 60 { diff.cos().abs() } else { diff.sin().abs() };
                    let nib_w = if meta.pen_type == 60 { meta.thickness * 0.95 * (pts[i][2] / 4095.0).powf(0.5) } else { 0.85 * meta.thickness + 1.64 };
                    let mut w = nib_w * (min_frac + (1.0 - min_frac) * chisel);
                    if i < 3 { w *= 0.67 + 0.33 * (i as f32 / 2.0); }
                    raw_widths[i] = w.max(0.5);
                }

                let mut widths = vec![0.0; n]; widths[0] = raw_widths[0];
                for i in 1..n { widths[i] = (widths[i-1] + 0.25 * (raw_widths[i] - widths[i-1])).max(0.3); }

                let half_w: Vec<f32> = widths.iter().map(|w| w / 2.0).collect();
                Self::fill_stroke_outline(&ctx, &pts, &half_w);
            },
            22 => {
                let n = pts.len();
                let mut half_w = vec![0.0; n];
                for i in 0..n { half_w[i] = ((meta.thickness * 1.37 * (pts[i][2]/4095.0).powf(0.59)).max(0.5)) / 2.0; }

                let pat = Self::get_charcoal_pattern(&ctx, meta.color_rgba.0, meta.color_rgba.1, meta.color_rgba.2, &stroke.uuid);
                ctx.set_fill_style_canvas_pattern(&pat);
                Self::fill_stroke_outline(&ctx, &pts, &half_w);
            },
            37 => { // Scanline Fill
                // Points are pairs: [left_edge, right_edge] per scan row in local space.
                // We compute strip quads from consecutive rows, then transform corners
                // so rotated fills render correctly.
                let raw: Vec<[f32; 2]> = stroke.points.iter().map(|p| [p.x, p.y]).collect();
                let pairs = raw.len() / 2;
                if pairs == 0 { ctx.restore(); return; }

                let mat = meta.matrix.unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
                let xf = |p: [f32; 2]| -> (f64, f64) {
                    ((mat[0] as f64 * p[0] as f64 + mat[1] as f64 * p[1] as f64 + mat[2] as f64),
                     (mat[3] as f64 * p[0] as f64 + mat[4] as f64 * p[1] as f64 + mat[5] as f64))
                };

                ctx.begin_path();
                for i in 0..pairs {
                    let l0 = [raw[i * 2][0] - 2.0, raw[i * 2][1]];
                    let r0 = [raw[i * 2 + 1][0] + 2.0, raw[i * 2 + 1][1]];
                    let y_top = l0[1].min(r0[1]);
                    let y_bot = if i + 1 < pairs {
                        let nl = raw[(i + 1) * 2];
                        let nr = raw[(i + 1) * 2 + 1];
                        let cand = nl[1].min(nr[1]);
                        if cand > y_top + 0.01 { cand } else {
                            // Search forward for next distinct row
                            let mut found = y_top + 1.0;
                            for j in i + 1..pairs {
                                let c = raw[j * 2][1].min(raw[j * 2 + 1][1]);
                                if c > y_top + 0.01 { found = c; break; }
                            }
                            found
                        }
                    } else { y_top + 1.0 };
                    let y_bot = y_bot + 1.5; // overlap to prevent gaps after transform

                    // Four corners of the strip in local space
                    let tl = xf([l0[0], y_top]);
                    let tr = xf([r0[0], y_top]);
                    let br = xf([r0[0], y_bot]);
                    let bl = xf([l0[0], y_bot]);

                    ctx.move_to(tl.0, tl.1);
                    ctx.line_to(tr.0, tr.1);
                    ctx.line_to(br.0, br.1);
                    ctx.line_to(bl.0, bl.1);
                    ctx.close_path();
                }
                ctx.fill();
            },
            _ => { // Ballpoint / Highlight
                ctx.begin_path(); ctx.set_line_width(meta.thickness as f64);
                ctx.move_to(pts[0][0] as f64, pts[0][1] as f64);
                for p in pts.iter().skip(1) { ctx.line_to(p[0] as f64, p[1] as f64); }
                ctx.stroke();
            }
        }

        ctx.restore();
    }

    pub fn export(&self) -> Result<js_sys::Uint8Array, JsValue> {
        let mut out = Vec::new();
        {
            let mut wr = ZipWriter::new(Cursor::new(&mut out));
            let opt = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            let mut arc = ZipArchive::new(Cursor::new(&self.zip_bytes)).map_err(|_| JsValue::from_str("ZIP err"))?;
            let r_map: HashMap<_, _> = self.deb_notes.iter().map(|nd| (nd.path.clone(), build_points(nd))).collect();

            for i in 0..arc.len() {
                let Ok(mut f) = arc.by_index(i) else { continue; }; let name = f.name().to_string();
                if name.contains("/stash/") { continue; }
                if wr.start_file(&name, opt).is_err() { continue; }
                if let Some(d) = r_map.get(&name) { wr.write_all(d).unwrap(); }
                else if !self.meta_overrides.is_empty() && name.ends_with(".zip") && name.contains("shape") {
                    // Rewrite shape protobuf with pen_type/thickness overrides
                    let mut z_data = Vec::new();
                    if f.read_to_end(&mut z_data).is_ok() {
                        if let Ok(mut inner_arc) = ZipArchive::new(Cursor::new(&z_data)) {
                            let mut inner_buf = Vec::new();
                            {
                                let mut inner_wr = ZipWriter::new(Cursor::new(&mut inner_buf));
                                for j in 0..inner_arc.len() {
                                    let Ok(mut sf) = inner_arc.by_index(j) else { continue; };
                                    let inner_name = sf.name().to_string();
                                    let _ = inner_wr.start_file(&inner_name, opt);
                                    let mut sh_data = Vec::new();
                                    if sf.read_to_end(&mut sh_data).is_ok() {
                                        let patched = patch_shape_protobuf(&sh_data, &self.meta_overrides);
                                        inner_wr.write_all(&patched).unwrap();
                                    }
                                }
                                inner_wr.finish().unwrap();
                            }
                            wr.write_all(&inner_buf).unwrap();
                        } else {
                            wr.write_all(&z_data).unwrap();
                        }
                    }
                }
                else { let mut b = Vec::new(); if f.read_to_end(&mut b).is_ok() { wr.write_all(&b).unwrap(); } }
            }
            if wr.finish().is_err() { return Err(JsValue::from_str("ZIP finish err")); }
        }
        Ok(js_sys::Uint8Array::from(out.as_slice()))
    }

    pub async fn load_images(&mut self) -> Result<(), JsValue> {
        let doc = web_sys::window().unwrap().document().unwrap();

        // Load images for shapeType 19 (SHAPE_IMAGE)
        let shape_entries: Vec<(String, Option<String>)> = self.shape_meta.iter()
            .filter(|(_, m)| m.pen_type == 19 && m.resource_path.is_some())
            .map(|(k, m)| (k.clone(), m.resource_path.clone()))
            .collect();

        for (uuid, res_path) in shape_entries {
            let Some(path) = res_path else { continue; };
            // Try to find the resource by matching the end of the key against the relativePath
            let data = self.resources.iter()
                .find(|(k, _)| path.ends_with(k.as_str()) || k.contains(&path))
                .or_else(|| {
                    // Also try matching just the filename part
                    let fname = path.rsplit('/').next().unwrap_or(&path);
                    self.resources.iter().find(|(k, _)| k.contains(fname))
                })
                .map(|(_, v)| v.clone());

            if let Some(data) = data {
                if let Ok(img) = Self::load_image_from_bytes(&doc, &data).await {
                    self.images.insert(uuid, img);
                }
            }
        }

        // Load background images
        if let Some(ref bg_json) = self.note_background {
            if let Ok(bg) = serde_json::from_str::<serde_json::Value>(bg_json) {
                let use_doc = bg.get("useDocBKGround").and_then(|v| v.as_bool()).unwrap_or(true);
                let doc_bg = bg.get("docBKGround");
                let page_map = bg.get("pageBKGroundMap");

                for page_id in &self.pages {
                    let bg_entry = if !use_doc {
                        page_map.and_then(|m| m.get(page_id)).or(doc_bg)
                    } else {
                        doc_bg
                    };

                    if let Some(entry) = bg_entry {
                        let bg_type = entry.get("type").and_then(|v| v.as_i64()).unwrap_or(0);
                        if bg_type == 1 { // IMAGE_FILE
                            let res_id = entry.get("resId").and_then(|v| v.as_str()).unwrap_or("");
                            if !res_id.is_empty() {
                                let data = self.resources.iter()
                                    .find(|(k, _)| k.contains(res_id))
                                    .map(|(_, v)| v.clone());
                                if let Some(data) = data {
                                    if let Ok(img) = Self::load_image_from_bytes(&doc, &data).await {
                                        self.bg_images.insert(page_id.clone(), img);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Load template SVGs from Boox CDN
        let template_pages: Vec<(String, String)> = self.templates.iter()
            .filter_map(|(page_id, tmpl)| {
                let res_name = tmpl.pointer("/properties/resourceAttr/resName")
                    .and_then(|v| v.as_str()).unwrap_or("");
                let basename = res_name.rsplit('/').next().unwrap_or("");
                if basename.is_empty() || basename == "template_empty" { return None; }
                Some((page_id.clone(), basename.to_string()))
            }).collect();

        for (page_id, basename) in template_pages {
            let tmpl_key = format!("tmpl_{}", page_id);
            if self.bg_images.contains_key(&tmpl_key) { continue; }

            // Try SVG first, then WebP, then PNG
            let urls = [
                format!("https://static.send2boox.com/device/note/template/{}.svg", basename),
                format!("https://static.send2boox.com/device/note/template/{}.webp", basename),
                format!("https://static.send2boox.com/device/note/template/{}.png", basename),
            ];

            for url in &urls {
                if let Ok(img) = Self::load_image_from_url(&doc, url).await {
                    self.bg_images.insert(tmpl_key.clone(), img);
                    break;
                }
            }
        }

        Ok(())
    }

    async fn load_image_from_bytes(doc: &web_sys::Document, data: &[u8]) -> Result<HtmlImageElement, JsValue> {
        let img = doc.create_element("img").map_err(|e| e)?.dyn_into::<HtmlImageElement>()?;

        // Detect MIME type from magic bytes
        let mime = if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) { "image/png" }
            else if data.starts_with(&[0xFF, 0xD8, 0xFF]) { "image/jpeg" }
            else if data.starts_with(b"GIF") { "image/gif" }
            else if data.starts_with(b"RIFF") && data.len() > 12 && &data[8..12] == b"WEBP" { "image/webp" }
            else { "image/png" }; // fallback

        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        let data_url = format!("data:{};base64,{}", mime, b64);

        let promise = js_sys::Promise::new(&mut |resolve, reject| {
            let resolve2 = resolve.clone();
            let reject2 = reject.clone();
            let onload = Closure::once(move || { resolve2.call0(&JsValue::NULL).unwrap(); });
            let onerror = Closure::once(move || { reject2.call1(&JsValue::NULL, &JsValue::from_str("Image load failed")).unwrap(); });
            img.set_onload(Some(onload.as_ref().unchecked_ref()));
            img.set_onerror(Some(onerror.as_ref().unchecked_ref()));
            onload.forget();
            onerror.forget();
        });

        img.set_src(&data_url);
        JsFuture::from(promise).await?;
        Ok(img)
    }

    async fn load_image_from_url(doc: &web_sys::Document, url: &str) -> Result<HtmlImageElement, JsValue> {
        let img = doc.create_element("img").map_err(|e| e)?.dyn_into::<HtmlImageElement>()?;
        img.set_cross_origin(Some("anonymous"));

        let promise = js_sys::Promise::new(&mut |resolve, reject| {
            let resolve2 = resolve.clone();
            let reject2 = reject.clone();
            let onload = Closure::once(move || { resolve2.call0(&JsValue::NULL).unwrap(); });
            let onerror = Closure::once(move || { reject2.call1(&JsValue::NULL, &JsValue::from_str("Image load failed")).unwrap(); });
            img.set_onload(Some(onload.as_ref().unchecked_ref()));
            img.set_onerror(Some(onerror.as_ref().unchecked_ref()));
            onload.forget();
            onerror.forget();
        });

        img.set_src(url);
        JsFuture::from(promise).await?;
        Ok(img)
    }

    fn get_charcoal_pattern(ctx: &CanvasRenderingContext2d, r: u8, g: u8, b: u8, uuid: &str) -> CanvasPattern {
        let doc = web_sys::window().unwrap().document().unwrap();
        let cvs = doc.create_element("canvas").unwrap().dyn_into::<HtmlCanvasElement>().unwrap();
        cvs.set_width(64); cvs.set_height(64);
        let t_ctx = cvs.get_context("2d").unwrap().unwrap().dyn_into::<CanvasRenderingContext2d>().unwrap();

        t_ctx.set_fill_style_str(&format!("rgb({},{},{})", r, g, b));
        t_ctx.fill_rect(0.0, 0.0, 64.0, 64.0);
        t_ctx.set_global_composite_operation("destination-out").unwrap();
        t_ctx.set_fill_style_str("black");

        // Seed RNG organically from the stroke's UUID to prevent static background tiling
        let mut seed = 0x43484152u32;
        for byte in uuid.bytes() { seed = seed.wrapping_mul(31).wrapping_add(byte as u32); }
        let mut rand = || -> f32 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed as f64 / std::u32::MAX as f64) as f32
        };

        t_ctx.begin_path();
        let n_dots = (64.0 * 64.0 * 0.3) as usize;
        for _ in 0..n_dots {
            let cx = rand() * 64.0; let cy = rand() * 64.0; let rad = rand() * 0.5 + 0.3;
            t_ctx.move_to((cx + rad) as f64, cy as f64);
            t_ctx.arc_with_anticlockwise(cx as f64, cy as f64, rad as f64, 0.0, 2.0 * std::f64::consts::PI, false).unwrap();
        }
        t_ctx.fill();
        ctx.create_pattern_with_html_canvas_element(&cvs, "repeat").unwrap().unwrap()
    }

    fn fill_stroke_outline(ctx: &CanvasRenderingContext2d, pts: &[[f32; 5]], hw: &[f32]) {
        let n = pts.len(); if n < 2 { return; }
        let mut normals = vec![[0.0, 0.0]; n];
        for i in 0..n {
            let (dx, dy) = if i == 0 { (pts[1][0] - pts[0][0], pts[1][1] - pts[0][1]) }
            else if i == n - 1 { (pts[n-1][0] - pts[n-2][0], pts[n-1][1] - pts[n-2][1]) }
            else { (pts[i+1][0] - pts[i-1][0], pts[i+1][1] - pts[i-1][1]) };
            let len = dx.hypot(dy).max(1e-5); normals[i] = [-dy / len, dx / len];
        }

        ctx.begin_path();
        // Forward pass (Right Edge)
        for i in 0..n {
            let (nx, ny) = (normals[i][0] * hw[i], normals[i][1] * hw[i]);
            if i == 0 { ctx.move_to((pts[i][0] + nx) as f64, (pts[i][1] + ny) as f64); }
            else { ctx.line_to((pts[i][0] + nx) as f64, (pts[i][1] + ny) as f64); }
        }
        // End cap
        let a_end = (normals[n-1][1] as f64).atan2(normals[n-1][0] as f64);
        ctx.arc_with_anticlockwise(pts[n-1][0] as f64, pts[n-1][1] as f64, hw[n-1] as f64, a_end, a_end - std::f64::consts::PI, true).unwrap();
        // Backward pass (Left Edge)
        for i in (0..n).rev() {
            let (nx, ny) = (normals[i][0] * hw[i], normals[i][1] * hw[i]);
            ctx.line_to((pts[i][0] - nx) as f64, (pts[i][1] - ny) as f64);
        }
        // Start cap
        let a_start = (normals[0][1] as f64).atan2(normals[0][0] as f64);
        ctx.arc_with_anticlockwise(pts[0][0] as f64, pts[0][1] as f64, hw[0] as f64, a_start - std::f64::consts::PI, a_start - 2.0 * std::f64::consts::PI, true).unwrap();
        ctx.close_path();
        ctx.fill();
    }
}
