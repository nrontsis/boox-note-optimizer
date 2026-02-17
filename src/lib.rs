use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::io::{Cursor, Read, Write};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CanvasPattern, CanvasRenderingContext2d, HtmlCanvasElement};
use zip::{write::FileOptions, ZipArchive, ZipWriter};

#[derive(Clone)]
struct Point { x: f32, y: f32, pressure: u16, tilt_x: u8, tilt_y: u8, cum_time: f64 }

#[derive(Clone)]
struct Stroke { uuid: String, points: Vec<Point> }

#[derive(Clone)]
struct Note { path: String, header: Vec<u8>, strokes: Vec<Stroke> }

#[derive(Clone)]
struct ShapeMeta {
    pen_type: i32, thickness: f32, color_rgba: (u8, u8, u8, f32),
    matrix: Option<[f32; 6]>, created_ts: u64,
}

#[derive(PartialEq)]
struct CostNode(usize, f32);
impl Eq for CostNode {}
impl PartialOrd for CostNode { fn partial_cmp(&self, o: &Self) -> Option<Ordering> { o.1.partial_cmp(&self.1) } }
impl Ord for CostNode { fn cmp(&self, o: &Self) -> Ordering { self.partial_cmp(o).unwrap_or(Ordering::Equal) } }

#[wasm_bindgen]
pub struct AppEngine {
    zip_bytes: Vec<u8>,
    notes: Vec<Note>,
    deb_notes: Vec<Note>,
    shape_meta: HashMap<String, ShapeMeta>,
    pages: Vec<String>,
    charcoal_cache: HashMap<(u8, u8, u8), CanvasPattern>,
}

#[wasm_bindgen]
impl AppEngine {
    #[wasm_bindgen(constructor)]
    pub fn new(zip_bytes: &[u8]) -> Result<AppEngine, JsValue> {
        let mut archive = ZipArchive::new(Cursor::new(zip_bytes)).map_err(|e| JsValue::from_str(&e.to_string()))?;
        let mut notes = Vec::new();
        let mut shape_meta = HashMap::new();

        for i in 0..archive.len() {
            let mut file = archive.by_index(i).unwrap();
            let name = file.name().to_string();

            if name.ends_with("#points") {
                let mut data = Vec::new(); file.read_to_end(&mut data).unwrap();
                notes.push(Self::parse_points(&data, &name));
            } else if name.ends_with(".zip") && name.contains("shape") && !name.contains("stash") {
                let mut z_data = Vec::new(); file.read_to_end(&mut z_data).unwrap();
                if let Ok(mut inner) = ZipArchive::new(Cursor::new(&z_data)) {
                    for j in 0..inner.len() {
                        let mut sh_data = Vec::new(); inner.by_index(j).unwrap().read_to_end(&mut sh_data).unwrap();
                        Self::parse_protobuf(&sh_data, &mut shape_meta);
                    }
                }
            }
        }

        let mut pages = Vec::new();
        for n in &notes {
            let parts: Vec<&str> = n.path.split('/').collect();
            if let Some(pos) = parts.iter().position(|&x| x == "point") {
                if pos + 1 < parts.len() && !pages.contains(&parts[pos + 1].to_string()) { pages.push(parts[pos + 1].to_string()); }
            }
        }

        Ok(AppEngine { zip_bytes: zip_bytes.to_vec(), deb_notes: notes.clone(), notes, shape_meta, pages, charcoal_cache: HashMap::new() })
    }

    pub fn debloat(&mut self, threshold: f32, press_eq: f32, tilt_eq: f32) {
        // Physical Equivalence: Delta change equating geometrically to exactly 1 pixel of error.
        let p_scale = 1.0 / press_eq;
        let t_scale = 1.0 / tilt_eq;

        self.deb_notes = self.notes.iter().map(|nd| {
            let mut new_strokes = Vec::new();
            for stroke in &nd.strokes {
                if stroke.points.len() < 3 { new_strokes.push(stroke.clone()); continue; }

                let u_tx = Self::unwrap_8bit(&stroke.points.iter().map(|p| p.tilt_x as f32).collect::<Vec<_>>());
                let u_ty = Self::unwrap_8bit(&stroke.points.iter().map(|p| p.tilt_y as f32).collect::<Vec<_>>());
                
                let math_pts: Vec<[f32; 5]> = stroke.points.iter().enumerate().map(|(i, p)| {
                    [p.x, p.y, p.pressure as f32 * p_scale, u_tx[i] * t_scale, u_ty[i] * t_scale]
                }).collect();

                let mask = Self::decimate(&math_pts, threshold);
                
                let opt_pts: Vec<Point> = stroke.points.iter().enumerate().filter(|(i, _)| mask[*i]).map(|(_, p)| p.clone()).collect();
                new_strokes.push(Stroke { uuid: stroke.uuid.clone(), points: opt_pts });
            }
            Note { path: nd.path.clone(), header: nd.header.clone(), strokes: new_strokes }
        }).collect();
    }

    pub fn get_orig_points(&self) -> usize { self.notes.iter().flat_map(|n| &n.strokes).map(|s| s.points.len()).sum() }
    pub fn get_deb_points(&self) -> usize { self.deb_notes.iter().flat_map(|n| &n.strokes).map(|s| s.points.len()).sum() }
    pub fn get_page_count(&self) -> usize { self.pages.len() }

    pub fn render_page(&mut self, canvas: &HtmlCanvasElement, use_deb: bool, page_idx: usize, simplified: bool) {
        let ctx = canvas.get_context("2d").unwrap().unwrap().dyn_into::<CanvasRenderingContext2d>().unwrap();
        ctx.clear_rect(0.0, 0.0, 1860.0, 2480.0);
        ctx.set_fill_style(&JsValue::from_str("#ffffff"));
        ctx.fill_rect(0.0, 0.0, 1860.0, 2480.0);
        ctx.set_line_cap("round"); ctx.set_line_join("round");

        if page_idx >= self.pages.len() { return; }
        let page_id = self.pages[page_idx].clone();

        let mut strokes: Vec<&Stroke> = if use_deb {
            self.deb_notes.iter().filter(|n| n.path.contains(&page_id)).flat_map(|n| &n.strokes).collect()
        } else {
            self.notes.iter().filter(|n| n.path.contains(&page_id)).flat_map(|n| &n.strokes).collect()
        };
        strokes.sort_by_key(|s| self.shape_meta.get(&s.uuid).map(|m| m.created_ts).unwrap_or(0));

        for stroke in strokes {
            if stroke.points.len() < 2 { continue; }
            let meta = self.shape_meta.get(&stroke.uuid).cloned().unwrap_or(ShapeMeta { pen_type: 2, thickness: 3.0, color_rgba: (0,0,0,1.0), matrix: None, created_ts: 0 });

            ctx.save();
            ctx.set_global_alpha(meta.color_rgba.3 as f64);
            if meta.pen_type == 15 { 
                ctx.set_global_composite_operation("multiply").unwrap(); 
                ctx.set_global_alpha(meta.color_rgba.3 as f64 * 0.5); 
            }
            
            let col = format!("rgb({},{},{})", meta.color_rgba.0, meta.color_rgba.1, meta.color_rgba.2);
            ctx.set_stroke_style(&JsValue::from_str(&col)); 
            ctx.set_fill_style(&JsValue::from_str(&col));

            let u_tx = Self::unwrap_8bit(&stroke.points.iter().map(|p| p.tilt_x as f32).collect::<Vec<_>>());
            let pts: Vec<[f32; 5]> = stroke.points.iter().enumerate().map(|(i, p)| {
                let (mut x, mut y) = (p.x, p.y);
                if let Some(m) = meta.matrix { x = m[0]*p.x + m[1]*p.y + m[2]; y = m[3]*p.x + m[4]*p.y + m[5]; }
                [x, y, (p.pressure as f32).clamp(1.0, 4095.0), u_tx[i], p.tilt_y as f32]
            }).collect();

            if simplified {
                ctx.begin_path(); ctx.set_line_width(meta.thickness as f64);
                ctx.move_to(pts[0][0] as f64, pts[0][1] as f64);
                for p in pts.iter().skip(1) { ctx.line_to(p[0] as f64, p[1] as f64); }
                ctx.stroke();
                ctx.restore();
                continue;
            }

            match meta.pen_type {
                5 | 21 => { // Fountain & Marker
                    let is_fountain = meta.pen_type == 5;
                    for i in 0..pts.len() - 1 {
                        let w1 = (meta.thickness * if is_fountain { 1.37 * (pts[i][2]/4095.0).powf(0.59) } else { 2.55 * (pts[i][2]/4095.0).powf(0.43) }).max(0.5);
                        let w2 = (meta.thickness * if is_fountain { 1.37 * (pts[i+1][2]/4095.0).powf(0.59) } else { 2.55 * (pts[i+1][2]/4095.0).powf(0.43) }).max(0.5);
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
                    let smooth_dir = Self::smooth_angle_ema(&stroke_angle, 0.3);

                    let min_frac = if meta.pen_type == 60 { 0.18 } else { 0.35 };
                    let mut raw_widths = vec![0.0; n];
                    for i in 0..n {
                        let diff = smooth_dir[i] - smooth_nib[i];
                        let chisel = if meta.pen_type == 60 { diff.cos().abs() } else { diff.sin().abs() };
                        let nib_w = meta.thickness * 0.95 * (pts[i][2] / 4095.0).powf(0.5);
                        let mut w = nib_w * (min_frac + (1.0 - min_frac) * chisel);
                        if i < 8 { w *= ((i + 1) as f32 / 8.0).powi(2); }
                        raw_widths[i] = w.max(0.5);
                    }

                    let mut widths = vec![0.0; n]; widths[0] = raw_widths[0];
                    for i in 1..n { widths[i] = (widths[i-1] + 0.25 * (raw_widths[i] - widths[i-1])).max(0.3); }

                    let half_w: Vec<f32> = widths.iter().map(|w| w / 2.0).collect();
                    Self::fill_stroke_outline(&ctx, &pts, &half_w);
                },
                22 => { // Explicit disjoint struct mutable borrow passed flawlessly
                    let n = pts.len();
                    let mut half_w = vec![0.0; n];
                    for i in 0..n { half_w[i] = ((meta.thickness * 1.37 * (pts[i][2]/4095.0).powf(0.59)).max(0.5)) / 2.0; }
                    
                    let pat = Self::get_charcoal_pattern(&mut self.charcoal_cache, &ctx, meta.color_rgba.0, meta.color_rgba.1, meta.color_rgba.2);
                    ctx.set_fill_style(&pat);
                    Self::fill_stroke_outline(&ctx, &pts, &half_w);
                },
                37 => { // Scanline Fill
                    ctx.begin_path();
                    let pairs = pts.len() / 2;
                    for i in 0..pairs {
                        let p0 = pts[i*2]; let p1 = pts[i*2+1];
                        let y_top = p0[1].min(p1[1]);
                        let mut y_bot = y_top + 1.0;
                        if i + 1 < pairs {
                            y_bot = pts[(i+1)*2][1].min(pts[(i+1)*2+1][1]);
                            if y_bot <= y_top {
                                y_bot = y_top;
                                for j in i+1..pairs {
                                    let cand = pts[j*2][1].min(pts[j*2+1][1]);
                                    if cand > y_top + 0.01 { y_bot = cand; break; }
                                }
                                if y_bot == y_top { y_bot = y_top + 1.0; }
                            }
                        }
                        ctx.rect(p0[0] as f64, y_top as f64, (p1[0] - p0[0]) as f64, (y_bot - y_top) as f64);
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
    }

    pub fn export(&self) -> js_sys::Uint8Array {
        let mut out = Vec::new();
        {
            let mut wr = ZipWriter::new(Cursor::new(&mut out));
            let opt = FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            let mut arc = ZipArchive::new(Cursor::new(&self.zip_bytes)).unwrap();
            let r_map: HashMap<_, _> = self.deb_notes.iter().map(|nd| (nd.path.clone(), Self::build_points(nd))).collect();

            for i in 0..arc.len() {
                let mut f = arc.by_index(i).unwrap(); let name = f.name().to_string();
                if name.contains("/stash/") { continue; }
                wr.start_file(&name, opt).unwrap();
                if let Some(d) = r_map.get(&name) { wr.write_all(d).unwrap(); } 
                else { let mut b = Vec::new(); f.read_to_end(&mut b).unwrap(); wr.write_all(&b).unwrap(); }
            }
            wr.finish().unwrap();
        }
        js_sys::Uint8Array::from(out.as_slice())
    }

    fn get_charcoal_pattern(cache: &mut HashMap<(u8, u8, u8), CanvasPattern>, ctx: &CanvasRenderingContext2d, r: u8, g: u8, b: u8) -> CanvasPattern {
        let key = (r, g, b);
        if let Some(pat) = cache.get(&key) { return pat.clone(); }

        let doc = web_sys::window().unwrap().document().unwrap();
        let cvs = doc.create_element("canvas").unwrap().dyn_into::<HtmlCanvasElement>().unwrap();
        cvs.set_width(64); cvs.set_height(64);
        let t_ctx = cvs.get_context("2d").unwrap().unwrap().dyn_into::<CanvasRenderingContext2d>().unwrap();

        t_ctx.set_fill_style(&JsValue::from_str(&format!("rgb({},{},{})", r, g, b)));
        t_ctx.fill_rect(0.0, 0.0, 64.0, 64.0);
        t_ctx.set_global_composite_operation("destination-out").unwrap();
        t_ctx.set_fill_style(&JsValue::from_str("black"));

        let mut seed = 0x43484152u32;
        let mut rand = || -> f32 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed as f64 / std::u32::MAX as f64) as f32
        };
        
        t_ctx.begin_path();
        let n_dots = (64.0 * 64.0 * 0.3) as usize;
        for _ in 0..n_dots {
            let cx = rand() * 64.0; let cy = rand() * 64.0; let rad = rand() * 0.5 + 0.3;
            t_ctx.move_to((cx + rad) as f64, cy as f64);
            t_ctx.arc(cx as f64, cy as f64, rad as f64, 0.0, 2.0 * std::f64::consts::PI).unwrap();
        }
        t_ctx.fill();

        let pattern = ctx.create_pattern_with_html_canvas_element(&cvs, "repeat").unwrap().unwrap();
        cache.insert(key, pattern.clone());
        pattern
    }

    fn smooth_angle_ema(angles: &[f32], alpha: f32) -> Vec<f32> {
        let mut out = vec![0.0; angles.len()]; if angles.is_empty() { return out; }
        out[0] = angles[0];
        for i in 1..angles.len() {
            let mut diff = angles[i] - out[i-1];
            diff = (diff + std::f32::consts::PI).rem_euclid(2.0 * std::f32::consts::PI) - std::f32::consts::PI;
            out[i] = out[i-1] + alpha * diff;
        } out
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
        for i in 0..n - 1 {
            let (nx0, ny0) = (normals[i][0] * hw[i], normals[i][1] * hw[i]);
            let (nx1, ny1) = (normals[i+1][0] * hw[i+1], normals[i+1][1] * hw[i+1]);
            ctx.move_to((pts[i][0] + nx0) as f64, (pts[i][1] + ny0) as f64);
            ctx.line_to((pts[i+1][0] + nx1) as f64, (pts[i+1][1] + ny1) as f64);
            ctx.line_to((pts[i+1][0] - nx1) as f64, (pts[i+1][1] - ny1) as f64);
            ctx.line_to((pts[i][0] - nx0) as f64, (pts[i][1] - ny0) as f64);
            ctx.close_path();
        }
        ctx.fill();

        ctx.begin_path(); ctx.arc(pts[0][0] as f64, pts[0][1] as f64, hw[0] as f64, 0.0, 2.0 * std::f64::consts::PI).unwrap(); ctx.fill();
        ctx.begin_path(); ctx.arc(pts[n-1][0] as f64, pts[n-1][1] as f64, hw[n-1] as f64, 0.0, 2.0 * std::f64::consts::PI).unwrap(); ctx.fill();
    }

    fn decimate(pts: &[[f32; 5]], t: f32) -> Vec<bool> {
        let n = pts.len(); let mut mask = vec![true; n];
        let mut prev: Vec<i32> = (0..n).map(|i| i as i32 - 1).collect();
        let mut next: Vec<i32> = (0..n).map(|i| i as i32 + 1).collect(); next[n - 1] = -1;
        let mut costs = vec![f32::INFINITY; n]; let mut heap = BinaryHeap::new();

        for i in 1..n - 1 {
            let c = Self::cost(&pts[i - 1], &pts[i], &pts[i + 1]);
            costs[i] = c; if c < f32::INFINITY { heap.push(CostNode(i, c)); }
        }

        while let Some(CostNode(idx, min)) = heap.pop() {
            if !mask[idx] || (min - costs[idx]).abs() > 1e-6 { continue; }
            if min > t { break; }
            mask[idx] = false; costs[idx] = f32::INFINITY;

            let (p, nxt) = (prev[idx], next[idx]);
            if p != -1 { next[p as usize] = nxt; }
            if nxt != -1 { prev[nxt as usize] = p; }

            if p != -1 && prev[p as usize] != -1 {
                let cp = Self::cost(&pts[prev[p as usize] as usize], &pts[p as usize], &pts[nxt as usize]);
                costs[p as usize] = cp; heap.push(CostNode(p as usize, cp));
            }
            if nxt != -1 && next[nxt as usize] != -1 {
                let cn = Self::cost(&pts[p as usize], &pts[nxt as usize], &pts[next[nxt as usize] as usize]);
                costs[nxt as usize] = cn; heap.push(CostNode(nxt as usize, cn));
            }
        }
        mask
    }

    fn cost(p: &[f32; 5], c: &[f32; 5], n: &[f32; 5]) -> f32 {
        let (vxi, vyi, vxo, vyo) = (c[0]-p[0], c[1]-p[1], n[0]-c[0], n[1]-c[1]);
        let (ni, no) = (vxi.hypot(vyi), vxo.hypot(vyo));
        if ni > 0.5 && no > 0.5 && (vxi*vxo + vyi*vyo)/(ni*no) < 0.866 { return f32::INFINITY; }
        let (vxb, vyb) = (n[0]-p[0], n[1]-p[1]); let blen = vxb.hypot(vyb);
        let (s_dev, t) = if blen > 1e-5 { ((vxb*vyi - vyb*vxi).abs()/blen, ((vxi*vxb + vyi*vyb)/(blen*blen)).clamp(0.0, 1.0)) } else { (ni, 0.5) };
        let mut a_dev = 0.0_f32; for i in 2..5 { a_dev = a_dev.max((c[i] - (p[i] + t * (n[i] - p[i]))).abs()); }
        s_dev.max(a_dev)
    }

    fn unwrap_8bit(arr: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0; arr.len()]; if arr.is_empty() { return out; }
        out[0] = arr[0]; let mut cum = 0.0;
        for i in 1..arr.len() { let d = arr[i] - arr[i-1]; cum += ((d + 128.0).rem_euclid(256.0)) - 128.0 - d; out[i] = arr[i] + cum; } out
    }

    fn parse_points(data: &[u8], path: &str) -> Note {
        let entries_start = Cursor::new(&data[data.len() - 4..]).read_u32::<BigEndian>().unwrap() as usize;
        let mut strokes = Vec::new();
        for i in 0..(data.len() - 4 - entries_start) / 44 {
            let pos = entries_start + i * 44;
            let uuid = String::from_utf8_lossy(&data[pos..pos+36]).to_string();
            let offset = Cursor::new(&data[pos+36..pos+40]).read_u32::<BigEndian>().unwrap() as usize;
            let size = Cursor::new(&data[pos+40..pos+44]).read_u32::<BigEndian>().unwrap() as usize;
            
            let mut points = Vec::new(); let mut cum = 0.0;
            for j in 0..(size - 4) / 16 {
                let mut cur = Cursor::new(&data[offset + 4 + j * 16..]);
                let (x, y, tx, ty, pr) = (cur.read_f32::<BigEndian>().unwrap(), cur.read_f32::<BigEndian>().unwrap(), cur.read_u8().unwrap(), cur.read_u8().unwrap(), cur.read_u16::<BigEndian>().unwrap());
                cum += cur.read_u32::<BigEndian>().unwrap() as f64;
                points.push(Point { x, y, tilt_x: tx, tilt_y: ty, pressure: pr, cum_time: cum });
            }
            if !points.is_empty() { strokes.push(Stroke { uuid, points }); }
        }
        Note { path: path.to_string(), header: data[..76].to_vec(), strokes }
    }

    fn decode_var(data: &[u8], off: &mut usize) -> u64 {
        let (mut r, mut s) = (0, 0);
        while *off < data.len() { let b = data[*off]; *off += 1; r |= ((b & 0x7F) as u64) << s; if b & 0x80 == 0 { break; } s += 7; } r
    }

    fn parse_protobuf(data: &[u8], meta: &mut HashMap<String, ShapeMeta>) {
        let mut off = 0;
        while off < data.len() {
            let tag = Self::decode_var(data, &mut off); let (fn_num, wt) = ((tag >> 3) as u32, (tag & 0x07) as u8);
            if fn_num == 1 && wt == 2 {
                let len = Self::decode_var(data, &mut off) as usize; let msg = &data[off..off + len]; off += len;
                let (mut so, mut u, mut pt, mut th, mut c, mut mat, mut ts) = (0, String::new(), 2, 3.0, (0,0,0,1.0), None, 0);
                while so < msg.len() {
                    let stag = Self::decode_var(msg, &mut so); let (sfn, swt) = ((stag >> 3) as u32, (stag & 0x07) as u8);
                    match (sfn, swt) {
                        (1, 2) => { let l = Self::decode_var(msg, &mut so) as usize; u = String::from_utf8_lossy(&msg[so..so+l]).to_string(); so += l; },
                        (2, 0) => ts = Self::decode_var(msg, &mut so),
                        (4, 0) => { let cv = Self::decode_var(msg, &mut so) as u32; c = (((cv>>16)&0xFF) as u8, ((cv>>8)&0xFF) as u8, (cv&0xFF) as u8, ((cv>>24)&0xFF) as f32 / 255.0); },
                        (5, 5) => { let mut b = [0;4]; b.copy_from_slice(&msg[so..so+4]); th = f32::from_le_bytes(b); so += 4; },
                        (8, 2) => { let l = Self::decode_var(msg, &mut so) as usize; if let Ok(j) = serde_json::from_slice::<serde_json::Value>(&msg[so..so+l]) { if let Some(a) = j.get("values").and_then(|v| v.as_array()) { mat = Some([a[0].as_f64().unwrap() as f32, a[1].as_f64().unwrap() as f32, a[2].as_f64().unwrap() as f32, a[3].as_f64().unwrap() as f32, a[4].as_f64().unwrap() as f32, a[5].as_f64().unwrap() as f32]); } } so += l; },
                        (12, 0) => pt = Self::decode_var(msg, &mut so) as i32,
                        (_, 0) => { Self::decode_var(msg, &mut so); }, (_, 1) => so += 8, (_, 2) => { let l = Self::decode_var(msg, &mut so) as usize; so += l; }, (_, 5) => so += 4, _ => break,
                    }
                }
                meta.insert(u, ShapeMeta { pen_type: pt, thickness: th, color_rgba: c, matrix: mat, created_ts: ts });
            } else { match wt { 0 => { Self::decode_var(data, &mut off); }, 1 => off += 8, 2 => { let l = Self::decode_var(data, &mut off) as usize; off += l; }, 5 => off += 4, _ => break, } }
        }
    }

    fn build_points(nd: &Note) -> Vec<u8> {
        let mut bs = Vec::new(); let mut idxs = Vec::new(); let mut c_off = nd.header.len() as u32;
        for s in &nd.strokes {
            let mut b = vec![0, 0, 0, 0]; let mut last = 0;
            for (i, p) in s.points.iter().enumerate() {
                b.write_f32::<BigEndian>(p.x).unwrap(); b.write_f32::<BigEndian>(p.y).unwrap();
                b.write_u8(p.tilt_x).unwrap(); b.write_u8(p.tilt_y).unwrap(); b.write_u16::<BigEndian>(p.pressure).unwrap();
                let cint = p.cum_time.round() as u32; b.write_u32::<BigEndian>(if i == 0 { cint } else { cint.saturating_sub(last) }).unwrap(); last = cint;
            }
            idxs.push((s.uuid.clone(), c_off, b.len() as u32)); c_off += b.len() as u32; bs.push(b);
        }
        let i_st = c_off; let mut res = nd.header.clone(); for b in bs { res.extend(b); }
        for (u, o, sz) in idxs { let mut ub = u.into_bytes(); ub.resize(36, 0); res.extend(ub); res.write_u32::<BigEndian>(o).unwrap(); res.write_u32::<BigEndian>(sz).unwrap(); }
        res.write_u32::<BigEndian>(i_st).unwrap(); res
    }
}
