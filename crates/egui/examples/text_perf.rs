//! CPU-only text/layout/tessellation performance harness for egui 0.36/0.37 migration.
//!
//! This is a test-only example kept to `std` + `egui`, plus the `skrifa`
//! dev-dependency used only for a version-independent fixture cmap check. It
//! produces machine-readable raw samples and a summary; it does **not** render
//! with a GPU or claim native/Web/GPU acceptance.
//!
//! Run (release is required for meaningful numbers):
//!
//! ```text
//! cargo run --release -p egui --example text_perf --features default_fonts -- \
//!     --out /path/to/evidence --label old036fork \
//!     --font /path/to/crates/epaint_default_fonts/fonts/Ubuntu-Light.ttf
//! ```
//!
//! Paired benchmark comparisons require the *same* `--font` file for old036fork,
//! stock037release and final037fork; the default bundled font sets differ between
//! 0.36 and 0.37. ASCII/Latin is exercised here. Non-Latin requires a licensed
//! fallback fixture (for example the pinned craft-fonts Arabic/Thai/CJK fonts)
//! and is not silently covered by tofu glyphs.

use core::{fmt::Write as _, hint::black_box, time::Duration};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use egui::{
    CentralPanel, Color32, Context, FontFamily, FontId, Grid, MenuBar, Mesh, Panel, Pos2, RawInput,
    Rect, Shape, Slider, TextEdit, Vec2, ViewportId, Visuals,
    epaint::{
        Primitive, tessellator::Tessellator, text::Fonts, text::LayoutJob, text::TextOptions,
    },
};

const DEFAULT_WARMUPS: usize = 10;
const DEFAULT_STEADY: usize = 50;
const DEFAULT_COLD_STARTS: usize = 20;
const DEFAULT_FRAMES: usize = 1_000;
const DEFAULT_ASCII_CHARS: usize = 10_000;
const DEFAULT_VIEWPORT_WIDTH: f32 = 1280.0;
const DEFAULT_VIEWPORT_HEIGHT: f32 = 800.0;
const DEFAULT_DPI: f32 = 1.0;
const FONT_SIZE: f32 = 14.0;
const WRAP_WIDTH: f32 = 720.0;
const FRAME_TEXT_CHARS: usize = 900;

const HARNESS_SOURCE: &[u8] = include_bytes!("text_perf.rs");

const ASCII_BASE: &str = "The quick brown fox jumps over the lazy dog 0123456789. \
Pack my box with five dozen liquor jugs; Sphinx of black quartz, judge my vow! \
ASCII layout and tessellation stay stable while counts, geometry and meshes are checked. ";

struct Args {
    out: PathBuf,
    label: String,
    font: Option<PathBuf>,
    warmups: usize,
    steady: usize,
    cold_starts: usize,
    frames: usize,
    ascii_chars: usize,
    dpi: f32,
    allow_missing_glyphs: bool,
    stdout: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            out: PathBuf::from("egui-text-perf-out"),
            label: "unlabeled".to_owned(),
            font: None,
            warmups: DEFAULT_WARMUPS,
            steady: DEFAULT_STEADY,
            cold_starts: DEFAULT_COLD_STARTS,
            frames: DEFAULT_FRAMES,
            ascii_chars: DEFAULT_ASCII_CHARS,
            dpi: DEFAULT_DPI,
            allow_missing_glyphs: false,
            stdout: false,
        }
    }
}

#[expect(clippy::print_stdout, reason = "CLI help is written to stdout")]
fn print_help() {
    println!(
        "text_perf: CPU-only egui text layout/tessellation/UI frame harness\n\
         \n\
         Options (defaults in parentheses):\n\
           --out DIR                 output directory (EGUI_PERF_OUT or egui-text-perf-out)\n\
           --label NAME              build label for the summary (unlabeled)\n\
           --font PATH               install one TTF/OTF as Proportional+Monospace (required for comparisons)\n\
           --warmups N               warmup samples per micro case (10)\n\
           --steady N                measured samples per micro case (50)\n\
           --cold-starts N           fresh Context/font startups (20)\n\
           --frames N                individual UI frame timings for p99 (1000)\n\
           --chars N                 ASCII fixture character count (10000)\n\
           --dpi P                   pixels_per_point (1.0)\n\
           --allow-missing-glyphs    record but do not fail missing-glyph checks\n\
           --stdout                  also print the summary JSON to stdout\n\
           --help                    show this help"
    );
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args::default();
    if let Ok(path) = std::env::var("EGUI_PERF_OUT")
        && !path.is_empty()
    {
        args.out = PathBuf::from(path);
    }

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("missing value for {name}"));
        match arg.as_str() {
            "--out" => args.out = PathBuf::from(value("--out")?),
            "--label" => args.label = value("--label")?,
            "--font" => args.font = Some(PathBuf::from(value("--font")?)),
            "--warmups" => args.warmups = parse_usize("--warmups", &value("--warmups")?)?,
            "--steady" => args.steady = parse_usize("--steady", &value("--steady")?)?,
            "--cold-starts" => {
                args.cold_starts = parse_usize("--cold-starts", &value("--cold-starts")?)?;
            }
            "--frames" => args.frames = parse_usize("--frames", &value("--frames")?)?,
            "--chars" => args.ascii_chars = parse_usize("--chars", &value("--chars")?)?,
            "--dpi" => {
                let p = value("--dpi")?;
                let dpi: f32 = p
                    .parse()
                    .map_err(|_err| format!("invalid --dpi value: {p}"))?;
                if !dpi.is_finite() || dpi <= 0.0 {
                    return Err(format!("--dpi must be positive and finite, got {p}"));
                }
                args.dpi = dpi;
            }
            "--allow-missing-glyphs" => args.allow_missing_glyphs = true,
            "--stdout" => args.stdout = true,
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            other => return Err(format!("unknown argument: {other} (use --help)")),
        }
    }

    if args.ascii_chars < 64 {
        return Err("--chars must be at least 64".to_owned());
    }
    if args.steady == 0 || args.warmups == 0 || args.frames == 0 || args.cold_starts == 0 {
        return Err("sample counts must be non-zero".to_owned());
    }
    Ok(Some(args))
}

fn parse_usize(name: &str, s: &str) -> Result<usize, String> {
    s.parse()
        .map_err(|_err| format!("invalid {name} value: {s}"))
}

#[derive(Clone, Copy, Default)]
struct Stats {
    count: usize,
    min_ns: u64,
    max_ns: u64,
    mean_ns: f64,
    median_ns: f64,
    p95_ns: f64,
    p99_ns: f64,
    stddev_ns: f64,
    cv_pct: f64,
}

fn summarize(values: &[u64]) -> Stats {
    if values.is_empty() {
        return Stats::default();
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let n = v.len();
    let mean = v.iter().map(|&x| x as f64).sum::<f64>() / n as f64;
    let var = v
        .iter()
        .map(|&x| {
            let d = x as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / n as f64;
    let stddev = var.sqrt();
    let median = if n % 2 == 1 {
        v[n / 2] as f64
    } else {
        (v[n / 2 - 1] as f64 + v[n / 2] as f64) * 0.5
    };
    let percentile = |p: f64| -> f64 {
        let rank = ((p / 100.0) * n as f64).ceil() as usize;
        v[rank.clamp(1, n) - 1] as f64
    };
    Stats {
        count: n,
        min_ns: v[0],
        max_ns: v[n - 1],
        mean_ns: mean,
        median_ns: median,
        p95_ns: percentile(95.0),
        p99_ns: percentile(99.0),
        stddev_ns: stddev,
        cv_pct: if mean > 0.0 {
            stddev / mean * 100.0
        } else {
            0.0
        },
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct GalleyInfo {
    rows: usize,
    glyphs: usize,
    vertices: usize,
    indices: usize,
    rect_width: f32,
    rect_height: f32,
    mesh_bounds_width: f32,
    mesh_bounds_height: f32,
    pixels_per_point: f32,
    identity: u64,
}

fn galley_info(galley: &egui::text::Galley) -> GalleyInfo {
    let mut h = Fnv64::new();
    h.write(b"galley");
    h.f32(galley.rect.min.x);
    h.f32(galley.rect.min.y);
    h.f32(galley.rect.max.x);
    h.f32(galley.rect.max.y);
    h.f32(galley.mesh_bounds.min.x);
    h.f32(galley.mesh_bounds.min.y);
    h.f32(galley.mesh_bounds.max.x);
    h.f32(galley.mesh_bounds.max.y);
    h.f32(galley.pixels_per_point);
    h.usize(galley.rows.len());
    h.usize(galley.num_vertices);
    h.usize(galley.num_indices);

    let mut glyphs = 0;
    for row in &galley.rows {
        h.f32(row.pos.x);
        h.f32(row.pos.y);
        h.f32(row.size.x);
        h.f32(row.size.y);
        h.usize(row.glyphs.len());
        for glyph in &row.glyphs {
            glyphs += 1;
            h.u32(glyph.chr as u32);
            h.f32(glyph.pos.x);
            h.f32(glyph.pos.y);
            h.f32(glyph.advance_width);
            h.f32(glyph.line_height);
            h.f32(glyph.font_ascent);
            h.f32(glyph.font_height);
            h.f32(glyph.font_face_ascent);
            h.f32(glyph.font_face_height);
            h.f32(glyph.uv_rect.offset.x);
            h.f32(glyph.uv_rect.offset.y);
            h.f32(glyph.uv_rect.size.x);
            h.f32(glyph.uv_rect.size.y);
            h.u32(u32::from(glyph.uv_rect.min[0]));
            h.u32(u32::from(glyph.uv_rect.min[1]));
            h.u32(u32::from(glyph.uv_rect.max[0]));
            h.u32(u32::from(glyph.uv_rect.max[1]));
        }
        hash_mesh(&mut h, &row.visuals.mesh);
    }

    GalleyInfo {
        rows: galley.rows.len(),
        glyphs,
        vertices: galley.num_vertices,
        indices: galley.num_indices,
        rect_width: galley.rect.width(),
        rect_height: galley.rect.height(),
        mesh_bounds_width: galley.mesh_bounds.width(),
        mesh_bounds_height: galley.mesh_bounds.height(),
        pixels_per_point: galley.pixels_per_point,
        identity: h.finish(),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct MeshInfo {
    vertices: usize,
    indices: usize,
    identity: u64,
}

fn mesh_info(mesh: &Mesh) -> MeshInfo {
    let mut h = Fnv64::new();
    h.write(b"mesh");
    hash_mesh(&mut h, mesh);
    MeshInfo {
        vertices: mesh.vertices.len(),
        indices: mesh.indices.len(),
        identity: h.finish(),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PrimitivesInfo {
    meshes: usize,
    callbacks: usize,
    vertices: usize,
    indices: usize,
    identity: u64,
}

fn primitives_info(prims: &[egui::epaint::ClippedPrimitive]) -> PrimitivesInfo {
    let mut h = Fnv64::new();
    h.write(b"primitives");
    h.usize(prims.len());
    let mut meshes = 0;
    let mut callbacks = 0;
    let mut vertices = 0;
    let mut indices = 0;
    for p in prims {
        h.f32(p.clip_rect.min.x);
        h.f32(p.clip_rect.min.y);
        h.f32(p.clip_rect.max.x);
        h.f32(p.clip_rect.max.y);
        match &p.primitive {
            Primitive::Mesh(mesh) => {
                meshes += 1;
                vertices += mesh.vertices.len();
                indices += mesh.indices.len();
                h.write(b"mesh");
                hash_mesh(&mut h, mesh);
            }
            Primitive::Callback(_) => {
                callbacks += 1;
                h.write(b"callback");
            }
        }
    }
    PrimitivesInfo {
        meshes,
        callbacks,
        vertices,
        indices,
        identity: h.finish(),
    }
}

fn hash_mesh(h: &mut Fnv64, mesh: &Mesh) {
    h.usize(mesh.vertices.len());
    h.usize(mesh.indices.len());
    let texture = format!("texture:{:?}", mesh.texture_id);
    h.write(texture.as_bytes());
    for v in &mesh.vertices {
        h.f32(v.pos.x);
        h.f32(v.pos.y);
        h.f32(v.uv.x);
        h.f32(v.uv.y);
        h.write(&v.color.to_array());
    }
    for &i in &mesh.indices {
        h.u32(i);
    }
}

#[derive(Clone, Debug)]
struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn u32(&mut self, value: u32) {
        self.write(&value.to_le_bytes());
    }

    fn usize(&mut self, value: usize) {
        self.write(&(value as u64).to_le_bytes());
    }

    fn f32(&mut self, value: f32) {
        self.u32(value.to_bits());
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

fn fnv64_hex(data: &[u8]) -> String {
    let mut hasher = Fnv64::new();
    hasher.write(data);
    format!("{:016x}", hasher.finish())
}

// ---------------------------------------------------------------------------
// Fixtures and fonts.

fn ascii_text(chars: usize) -> String {
    let mut s = String::with_capacity(chars);
    while s.len() < chars {
        s.push_str(ASCII_BASE);
    }
    s.truncate(chars);
    s
}

fn unique_miss_texts(base: &str, count: usize) -> Result<Vec<String>, String> {
    // Keep the miss fixture at the same byte length and change a decimal suffix.
    // The suffix makes every job distinct even when `base` is periodic and the
    // rotations repeat (for example when `--chars` is a multiple of 97).
    let width = count.saturating_sub(1).to_string().len().max(1);
    if base.len() < width {
        return Err(format!(
            "--chars {} is too short for {count} unique same-length miss fixtures (need at least {width})",
            base.len()
        ));
    }
    if !base.is_ascii() {
        return Err("miss fixture base must be ASCII".to_owned());
    }

    let mut seen = BTreeSet::new();
    let mut texts = Vec::with_capacity(count);
    for variant in 0..count {
        let mut bytes = base.as_bytes().to_vec();
        let shift = (variant.wrapping_mul(97).wrapping_add(13)) % bytes.len();
        bytes.rotate_left(shift);

        let suffix = variant.to_string();
        let start = bytes.len() - width;
        bytes[start..].fill(b'0');
        bytes[start + width - suffix.len()..].copy_from_slice(suffix.as_bytes());
        let text = String::from_utf8(bytes)
            .map_err(|_err| format!("--chars {} produced a non-UTF-8 miss fixture", base.len()))?;
        if text.len() != base.len() || !text.is_ascii() {
            return Err(format!(
                "--chars {} cannot provide same-length ASCII miss fixtures",
                base.len()
            ));
        }
        if !seen.insert(text.clone()) {
            return Err(format!(
                "--chars {} produced a duplicate miss fixture at variant {variant}",
                base.len()
            ));
        }
        texts.push(text);
    }
    Ok(texts)
}

struct FontDataEntry {
    name: String,
    bytes: usize,
    index: u32,
    fnv64: String,
}

struct FontSetup {
    definitions: egui::FontDefinitions,
    mode: &'static str,
    resolved_path: Option<PathBuf>,
    font_data_fnv64: String,
    font_data_entries: Vec<FontDataEntry>,
}

fn font_data_identity(definitions: &egui::FontDefinitions) -> (String, Vec<FontDataEntry>) {
    let mut combined = Fnv64::new();
    combined.write(b"font-definitions");
    let mut entries = Vec::with_capacity(definitions.font_data.len());
    for (name, data) in &definitions.font_data {
        let bytes = data.font.as_ref().as_ref();
        let mut one = Fnv64::new();
        one.write(bytes);
        one.u32(data.index);
        combined.write(name.as_bytes());
        combined.usize(bytes.len());
        combined.u32(data.index);
        combined.write(bytes);
        entries.push(FontDataEntry {
            name: name.clone(),
            bytes: bytes.len(),
            index: data.index,
            fnv64: format!("{:016x}", one.finish()),
        });
    }
    (format!("{:016x}", combined.finish()), entries)
}

fn load_font_definitions(font_path: Option<&Path>) -> Result<FontSetup, String> {
    let mut definitions = egui::FontDefinitions::default();
    let mut mode = "bundled_default";
    let mut resolved_path = None;

    if let Some(path) = font_path {
        let bytes =
            fs::read(path).map_err(|e| format!("failed to read font {}: {e}", path.display()))?;
        if bytes.is_empty() {
            return Err(format!("font {} is empty", path.display()));
        }
        mode = "file";
        resolved_path = Some(fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));

        // Leak once: this is a short-lived benchmark process, and `from_static`
        // keeps FontDefinitions clones cheap and identical across cold contexts.
        let static_bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        let data = Arc::new(egui::FontData::from_static(static_bytes));
        let mut font_data = BTreeMap::new();
        font_data.insert("perf-font".to_owned(), data);
        definitions.font_data = font_data;

        let mut families = BTreeMap::new();
        families.insert(FontFamily::Proportional, vec!["perf-font".to_owned()]);
        families.insert(FontFamily::Monospace, vec!["perf-font".to_owned()]);
        definitions.families = families;
    }

    if definitions.font_data.is_empty() {
        return Err(
            "no fonts are installed: run with `--features default_fonts` or pass `--font PATH`"
                .to_owned(),
        );
    }

    let (font_data_fnv64, font_data_entries) = font_data_identity(&definitions);
    Ok(FontSetup {
        definitions,
        mode,
        resolved_path,
        font_data_fnv64,
        font_data_entries,
    })
}

fn proportional_glyph_coverage(
    definitions: &egui::FontDefinitions,
    unique_chars: &BTreeSet<char>,
) -> Result<(bool, String), String> {
    // Inspect only the fonts actually listed for the proportional family used by
    // the fixture. Unrelated emoji/fallback families must not abort an ASCII run.
    let names = definitions
        .families
        .get(&FontFamily::Proportional)
        .ok_or_else(|| "font definitions have no Proportional family".to_owned())?;
    if names.is_empty() {
        return Err("Proportional family has no font names".to_owned());
    }

    use skrifa::MetadataProvider as _;
    let mut supported = BTreeSet::new();
    let mut unparseable = Vec::new();
    for name in names {
        let Some(data) = definitions.font_data.get(name) else {
            unparseable.push(name.clone());
            continue;
        };
        let Ok(face) = skrifa::FontRef::from_index(data.font.as_ref().as_ref(), data.index) else {
            unparseable.push(name.clone());
            continue;
        };
        let charmap = face.charmap();
        for &ch in unique_chars {
            if charmap.map(ch).is_some() {
                supported.insert(ch);
            }
        }
    }

    let missing: String = unique_chars.difference(&supported).copied().collect();
    let detail = format!(
        "missing_chars={}, unparseable_family_fonts={}",
        missing.chars().count(),
        unparseable.join("|")
    );
    Ok((missing.is_empty(), detail))
}

// ---------------------------------------------------------------------------
// Representative UI.

struct BenchState {
    text_edit: String,
    multiline: String,
    checked: bool,
    value: f32,
}

impl Default for BenchState {
    fn default() -> Self {
        Self {
            text_edit: "Single-line text edit with stable ASCII content.".to_owned(),
            multiline: ascii_text(420),
            checked: true,
            value: 42.0,
        }
    }
}

fn build_ui(ui: &mut egui::Ui, state: &mut BenchState, text: &str) {
    Panel::top("perf_menu").show(ui, |ui| {
        MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                let _ = ui.button("New");
                let _ = ui.button("Open");
                let _ = ui.button("Save");
            });
            ui.menu_button("Edit", |ui| {
                let _ = ui.button("Undo");
                let _ = ui.button("Redo");
            });
            ui.separator();
            ui.label("egui text performance fixture");
        });
    });

    Panel::left("perf_side")
        .resizable(false)
        .default_size(230.0)
        .show(ui, |ui| {
            ui.heading("Inspector");
            ui.checkbox(&mut state.checked, "Enable feature");
            ui.add(Slider::new(&mut state.value, 0.0..=100.0).text("Value"));
            ui.separator();
            for row in 0..10 {
                ui.label(format!("Row {row}: stable ASCII label"));
            }
        });

    CentralPanel::default().show(ui, |ui| {
        ui.heading("Text layout and tessellation");
        ui.horizontal(|ui| {
            let _ = ui.button("One");
            let _ = ui.button("Two");
            let _ = ui.button("Three");
        });
        ui.add(TextEdit::singleline(&mut state.text_edit).desired_width(f32::INFINITY));
        ui.add(
            TextEdit::multiline(&mut state.multiline)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );
        ui.separator();
        ui.label(text);
        ui.separator();
        Grid::new("perf_grid")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                ui.label("Mode");
                ui.label("CPU headless");
                ui.end_row();
                ui.label("Font size");
                ui.label(format!("{FONT_SIZE} pt"));
                ui.end_row();
            });
    });
}

fn base_input(viewport: Rect) -> RawInput {
    RawInput {
        screen_rect: Some(viewport),
        time: Some(0.0),
        focused: true,
        ..Default::default()
    }
}

fn new_context(definitions: &egui::FontDefinitions, dpi: f32) -> Context {
    let ctx = Context::default();
    ctx.set_fonts(definitions.clone());
    ctx.set_pixels_per_point(dpi);
    ctx.set_visuals(Visuals::dark());
    ctx.global_style_mut(|style| {
        style.animation_time = 0.0;
    });
    ctx.options_mut(|options| {
        options.tessellation_options.parallel_tessellation = false;
        options.tessellation_options.validate_meshes = false;
    });
    ctx
}

// ---------------------------------------------------------------------------
// Evidence writer.

#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckKind {
    Correctness,
    Acceptance,
}

struct CheckEntry {
    name: String,
    ok: bool,
    detail: String,
    kind: CheckKind,
}

struct Checks {
    entries: Vec<CheckEntry>,
}

impl Checks {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn check(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        self.push(CheckKind::Correctness, name, ok, detail);
    }

    fn acceptance(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        self.push(CheckKind::Acceptance, name, ok, detail);
    }

    fn push(&mut self, kind: CheckKind, name: &str, ok: bool, detail: impl Into<String>) {
        self.entries.push(CheckEntry {
            name: name.to_owned(),
            ok,
            detail: detail.into(),
            kind,
        });
    }

    fn failed(&self, kind: CheckKind) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == kind && !entry.ok)
            .map(|entry| entry.name.as_str())
            .collect()
    }

    fn passed(&self, kind: CheckKind) -> bool {
        self.failed(kind).is_empty()
    }

    fn push_json(&self, out: &mut String, kind: CheckKind) {
        out.push('[');
        let mut first = true;
        for entry in self.entries.iter().filter(|entry| entry.kind == kind) {
            if !first {
                out.push(',');
            }
            first = false;
            let _ = write!(
                out,
                "{{\"name\":{},\"ok\":{},\"detail\":{}}}",
                json_string(&entry.name),
                entry.ok,
                json_string(&entry.detail)
            );
        }
        out.push(']');
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_optional_string(value: Option<&str>) -> String {
    value.map_or_else(|| "null".to_owned(), json_string)
}

fn push_stats(out: &mut String, name: &str, s: &Stats) {
    let _ = write!(
        out,
        "{}:{{\"count\":{},\"min_ns\":{},\"max_ns\":{},\"mean_ns\":{:.3},\"median_ns\":{:.3},\"p95_ns\":{:.3},\"p99_ns\":{:.3},\"stddev_ns\":{:.3},\"cv_pct\":{:.3}}}",
        json_string(name),
        s.count,
        s.min_ns,
        s.max_ns,
        s.mean_ns,
        s.median_ns,
        s.p95_ns,
        s.p99_ns,
        s.stddev_ns,
        s.cv_pct
    );
}

#[derive(Clone, Copy, Default)]
struct TextureUploads {
    deltas: usize,
    bytes: usize,
}

impl TextureUploads {
    fn add(&mut self, other: Self) {
        self.deltas += other.deltas;
        self.bytes = self.bytes.saturating_add(other.bytes);
    }
}

fn take_textures(out: &mut egui::FullOutput) -> TextureUploads {
    let mut uploads = TextureUploads::default();
    // These integer totals are commutative; sorting would add benchmark overhead.
    #[expect(
        clippy::iter_over_hash_type,
        reason = "order-independent upload totals"
    )]
    for deltas in out.textures_delta.set.values() {
        for delta in deltas {
            uploads.deltas += 1;
            let [width, height] = delta.image.size();
            uploads.bytes = uploads
                .bytes
                .saturating_add(width * height * delta.image.bytes_per_pixel());
        }
    }
    out.textures_delta.clear();
    uploads
}

fn read_linux_value(path: &str, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .map(|value| {
            let value = value.trim();
            value.strip_prefix(':').unwrap_or(value).trim().to_owned()
        })
}

fn cpu_model() -> Option<String> {
    read_linux_value("/proc/cpuinfo", "model name")
}

fn kernel_release() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|value| value.trim().to_owned())
}

fn peak_rss_kib() -> Option<u64> {
    read_linux_value("/proc/self/status", "VmHWM:")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn push_optional_f64(out: &mut String, value: Option<f64>) {
    if let Some(value) = value {
        let _ = write!(out, "{value:.3}");
    } else {
        out.push_str("null");
    }
}

#[expect(clippy::print_stderr, reason = "CLI failures are reported to stderr")]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("text_perf: completed with failed correctness checks");
            std::process::ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("text_perf: error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "CLI diagnostics and explicitly requested JSON output"
)]
fn run() -> Result<bool, String> {
    let Some(args) = parse_args()? else {
        return Ok(true);
    };

    fs::create_dir_all(&args.out)
        .map_err(|e| format!("failed to create output dir {}: {e}", args.out.display()))?;
    let out_dir = fs::canonicalize(&args.out).unwrap_or_else(|_err| args.out.clone());

    let font = load_font_definitions(args.font.as_deref())?;
    let text = ascii_text(args.ascii_chars);
    let frame_text = ascii_text(FRAME_TEXT_CHARS);
    let text_fnv64 = fnv64_hex(text.as_bytes());
    let frame_text_fnv64 = fnv64_hex(frame_text.as_bytes());
    let font_id = FontId::proportional(FONT_SIZE);
    let viewport = Rect::from_min_size(
        Pos2::ZERO,
        Vec2::new(DEFAULT_VIEWPORT_WIDTH, DEFAULT_VIEWPORT_HEIGHT),
    );
    let input = base_input(viewport);

    let mut checks = Checks::new();

    let text_options = TextOptions::default();
    let mut fonts_cached = Fonts::new(text_options, font.definitions.clone());
    let mut fonts_miss = Fonts::new(text_options, font.definitions.clone());

    let base_job = LayoutJob::simple(text.clone(), font_id.clone(), Color32::WHITE, WRAP_WIDTH);

    // Warm the cached layout path before any measured call.
    for _ in 0..args.warmups {
        let galley = fonts_cached
            .with_pixels_per_point(args.dpi)
            .layout_job(base_job.clone());
        black_box(&galley);
    }
    let cache_after_cached_warmup = fonts_cached.num_galleys_in_cache();

    let mut layout_cache_hit = Vec::with_capacity(args.steady);
    let mut cached_galley = None;
    for _ in 0..args.steady {
        let job = base_job.clone();
        let t = Instant::now();
        let galley = fonts_cached.with_pixels_per_point(args.dpi).layout_job(job);
        let dt = t.elapsed().as_nanos() as u64;
        black_box(&galley);
        layout_cache_hit.push(dt);
        cached_galley = Some(galley);
    }
    let cache_after_cached_steady = fonts_cached.num_galleys_in_cache();
    let cached_galley = cached_galley.ok_or_else(|| "no cached galley produced".to_owned())?;
    let cached_galley_info = galley_info(&cached_galley);

    checks.check(
        "cached_layout_does_not_grow_cache",
        cache_after_cached_steady == cache_after_cached_warmup,
        format!(
            "num_galleys after warmup={cache_after_cached_warmup}, after steady={cache_after_cached_steady}"
        ),
    );

    // Forced cache misses: unique, same-length, same-alphabet job keys.
    let miss_texts = unique_miss_texts(&text, args.warmups + args.steady)?;
    checks.check(
        "miss_fixtures_are_unique_same_length",
        miss_texts.len() == args.warmups + args.steady
            && miss_texts.iter().all(|text| text.len() == args.ascii_chars)
            && miss_texts.iter().collect::<BTreeSet<_>>().len() == miss_texts.len(),
        format!(
            "count={}, chars={}, unique={}",
            miss_texts.len(),
            args.ascii_chars,
            miss_texts.iter().collect::<BTreeSet<_>>().len()
        ),
    );
    let mut miss_jobs: Vec<LayoutJob> = miss_texts
        .iter()
        .map(|s| LayoutJob::simple(s.clone(), font_id.clone(), Color32::WHITE, WRAP_WIDTH))
        .collect();
    for job in miss_jobs.drain(..args.warmups) {
        let galley = fonts_miss.with_pixels_per_point(args.dpi).layout_job(job);
        black_box(&galley);
    }
    let cache_after_miss_warmup = fonts_miss.num_galleys_in_cache();

    let mut layout_miss = Vec::with_capacity(args.steady);
    let mut miss_galley = None;
    for job in miss_jobs {
        let t = Instant::now();
        let galley = fonts_miss.with_pixels_per_point(args.dpi).layout_job(job);
        let dt = t.elapsed().as_nanos() as u64;
        black_box(&galley);
        layout_miss.push(dt);
        miss_galley = Some(galley);
    }
    let cache_after_miss_steady = fonts_miss.num_galleys_in_cache();
    let miss_galley = miss_galley.ok_or_else(|| "no miss galley produced".to_owned())?;
    let miss_galley_info = galley_info(&miss_galley);

    checks.check(
        "miss_path_populates_cache",
        cache_after_miss_steady == cache_after_miss_warmup + args.steady,
        format!(
            "num_galleys after miss warmup={cache_after_miss_warmup}, after steady={cache_after_miss_steady}, expected_growth={}",
            args.steady
        ),
    );
    let cache_hit_median = summarize(&layout_cache_hit).median_ns;
    let forced_miss_median = summarize(&layout_miss).median_ns;
    checks.check(
        "forced_miss_slower_than_cache_hit",
        forced_miss_median > cache_hit_median,
        format!(
            "cache_hit_median_ns={cache_hit_median:.3}, forced_miss_median_ns={forced_miss_median:.3}"
        ),
    );

    // Same job, fresh Fonts: first layout is a miss and must produce the same
    // geometry as the cached result (apart from cache identity).
    let mut fonts_fresh = Fonts::new(text_options, font.definitions.clone());
    let fresh_galley = fonts_fresh
        .with_pixels_per_point(args.dpi)
        .layout_job(base_job.clone());
    let fresh_galley_info = galley_info(&fresh_galley);
    checks.check(
        "cached_and_fresh_galley_geometry_identical",
        cached_galley_info.identity == fresh_galley_info.identity,
        format!(
            "cached={:016x}, fresh={:016x}",
            cached_galley_info.identity, fresh_galley_info.identity
        ),
    );

    // Fixture coverage: inspect only the proportional family actually used by
    // the fixture. The cmap oracle is version-independent and outside timing.
    let unique_chars: BTreeSet<char> = text.chars().collect();
    let (glyph_coverage, coverage_detail) =
        proportional_glyph_coverage(&font.definitions, &unique_chars)?;
    checks.check(
        "fixture_glyphs_available",
        glyph_coverage || args.allow_missing_glyphs,
        format!("cmap_coverage={glyph_coverage}, {coverage_detail}"),
    );
    checks.check(
        "ascii_fixture_is_ascii",
        text.is_ascii(),
        format!(
            "non_ascii_chars={}",
            text.chars().filter(|c| !c.is_ascii()).count()
        ),
    );

    let expected_glyphs = text.chars().count();
    checks.check(
        "galley_glyph_count_matches_text",
        cached_galley_info.glyphs == expected_glyphs,
        format!(
            "galley_glyphs={}, expected={expected_glyphs}",
            cached_galley_info.glyphs
        ),
    );
    checks.check(
        "galley_pixels_per_point_matches",
        (cached_galley_info.pixels_per_point - args.dpi).abs() < 1e-6,
        format!(
            "galley_ppp={}, requested={}",
            cached_galley_info.pixels_per_point, args.dpi
        ),
    );

    // Shaped ligatures retain zero-advance continuation entries without atlas UVs.
    let mut invalid_uv = 0usize;
    for row in &cached_galley.rows {
        for glyph in &row.glyphs {
            if !glyph.chr.is_whitespace() && glyph.advance_width > 0.0 && glyph.uv_rect.is_nothing()
            {
                invalid_uv += 1;
            }
        }
    }
    checks.check(
        "visible_glyphs_have_atlas_uv",
        invalid_uv == 0,
        format!("invalid_uv_visible_glyphs={invalid_uv}"),
    );

    // Job-clone cost is reported separately so cached/miss layout numbers are
    // not silently dominated by fixture copying.
    let mut job_clone = Vec::with_capacity(args.steady);
    for _ in 0..args.warmups {
        black_box(base_job.clone());
    }
    for _ in 0..args.steady {
        let t = Instant::now();
        let job = base_job.clone();
        let dt = t.elapsed().as_nanos() as u64;
        black_box(&job);
        job_clone.push(dt);
    }

    // Tessellation of the cached ASCII galley.
    let atlas_fill_ratio = fonts_cached.font_atlas_fill_ratio();
    let atlas = fonts_cached.texture_atlas();
    let atlas_size = atlas.size();
    let prepared_discs = atlas.prepared_discs();
    let tess_options = egui::epaint::tessellator::TessellationOptions {
        coarse_tessellation_culling: false,
        parallel_tessellation: false,
        validate_meshes: false,
        debug_paint_clip_rects: false,
        debug_paint_text_rects: false,
        debug_ignore_clip_rects: false,
        ..Default::default()
    };
    let mut tessellator = Tessellator::new(args.dpi, tess_options, atlas_size, prepared_discs);
    let text_shape = Shape::galley(
        Pos2::new(10.0, 10.0),
        Arc::clone(&cached_galley),
        Color32::WHITE,
    );
    tessellator.set_clip_rect(text_shape.visual_bounding_rect());

    for _ in 0..args.warmups {
        let mut mesh = Mesh::default();
        tessellator.tessellate_shape(text_shape.clone(), &mut mesh);
        black_box(&mesh);
    }
    let mut tessellation = Vec::with_capacity(args.steady);
    let mut tess_info = MeshInfo::default();
    for i in 0..args.steady {
        let shape = text_shape.clone();
        let mut mesh = Mesh::default();
        let t = Instant::now();
        tessellator.tessellate_shape(shape, &mut mesh);
        let dt = t.elapsed().as_nanos() as u64;
        black_box(&mesh);
        tessellation.push(dt);
        if i + 1 == args.steady {
            tess_info = mesh_info(&mesh);
        }
    }
    checks.check(
        "tessellation_vertices_match_galley",
        tess_info.vertices == cached_galley.num_vertices,
        format!(
            "mesh_vertices={}, galley_vertices={}",
            tess_info.vertices, cached_galley.num_vertices
        ),
    );
    checks.check(
        "tessellation_indices_match_galley",
        tess_info.indices == cached_galley.num_indices,
        format!(
            "mesh_indices={}, galley_indices={}",
            tess_info.indices, cached_galley.num_indices
        ),
    );
    // Tessellate once more and require identical output.
    let mut second_mesh = Mesh::default();
    tessellator.tessellate_shape(text_shape.clone(), &mut second_mesh);
    let second_mesh_info = mesh_info(&second_mesh);
    checks.check(
        "tessellation_geometry_deterministic",
        tess_info.identity == second_mesh_info.identity,
        format!(
            "first={:016x}, second={:016x}",
            tess_info.identity, second_mesh_info.identity
        ),
    );

    // Representative UI frame.
    let ctx = new_context(&font.definitions, args.dpi);
    let mut state = BenchState::default();
    let mut shapes_template = None;
    let mut warm_frame_identity = None;
    let mut warmup_uploads = TextureUploads::default();
    let mut warmup_pass_counts = Vec::with_capacity(args.warmups);

    for warmup in 0..args.warmups {
        let mut frame_input = input.clone();
        frame_input.time = Some(warmup as f64 / 60.0);
        let mut out = ctx.run_ui(frame_input, |ui| build_ui(ui, &mut state, &frame_text));
        if shapes_template.is_none() {
            shapes_template = Some(out.shapes.clone());
        }
        warmup_pass_counts.push(out.platform_output.num_completed_passes);
        warmup_uploads.add(take_textures(&mut out));
        let ppp = out.pixels_per_point;
        let prims = ctx.tessellate(out.shapes, ppp);
        black_box(&prims);
        warm_frame_identity = Some(primitives_info(&prims).identity);
    }
    let shapes_template = shapes_template.ok_or_else(|| "no warmup frame shapes".to_owned())?;

    let mut shapes_clone = Vec::with_capacity(args.steady);
    for _ in 0..args.warmups {
        black_box(shapes_template.clone());
    }
    for _ in 0..args.steady {
        let t = Instant::now();
        let cloned = shapes_template.clone();
        let dt = t.elapsed().as_nanos() as u64;
        black_box(&cloned);
        shapes_clone.push(dt);
    }

    let mut frame_ui = Vec::with_capacity(args.frames);
    let mut frame_tess = Vec::with_capacity(args.frames);
    let mut frame_total = Vec::with_capacity(args.frames);
    let mut frame_first = PrimitivesInfo::default();
    let mut frame_last = PrimitivesInfo::default();
    let mut frame_shapes = 0usize;
    let mut frame_ppp = args.dpi;
    let mut frame_pass_counts = Vec::with_capacity(args.frames);
    let mut frame_identities = Vec::with_capacity(args.frames);
    let mut frame_shape_counts = Vec::with_capacity(args.frames);
    let mut frame_uploads = TextureUploads::default();
    let mut repaint_frames = 0usize;
    let mut repaint_min_ms = None::<f64>;
    let mut frame_root_viewport_missing = 0usize;
    for i in 0..args.frames {
        let mut frame_input = input.clone();
        frame_input.time = Some((args.warmups + i) as f64 / 60.0);
        let t0 = Instant::now();
        let mut out = ctx.run_ui(frame_input, |ui| build_ui(ui, &mut state, &frame_text));
        let t1 = Instant::now();
        frame_pass_counts.push(out.platform_output.num_completed_passes);
        if let Some(viewport) = out.viewport_output.get(&ViewportId::ROOT) {
            let delay = viewport.repaint_delay;
            if delay < Duration::MAX {
                repaint_frames += 1;
                let delay_ms = delay.as_secs_f64() * 1000.0;
                repaint_min_ms = Some(repaint_min_ms.map_or(delay_ms, |min| min.min(delay_ms)));
            }
        } else {
            frame_root_viewport_missing += 1;
        }
        frame_shapes = out.shapes.len();
        frame_shape_counts.push(frame_shapes);
        frame_ppp = out.pixels_per_point;
        let shapes = core::mem::take(&mut out.shapes);
        frame_uploads.add(take_textures(&mut out));
        let t_tess = Instant::now();
        let prims = ctx.tessellate(shapes, frame_ppp);
        let t2 = Instant::now();

        black_box(&prims);
        let info = primitives_info(&prims);
        frame_identities.push(info.identity);
        if i == 0 {
            frame_first = info;
        }
        if i + 1 == args.frames {
            frame_last = info;
        }
        let ui_ns = (t1 - t0).as_nanos() as u64;
        let tess_ns = (t2 - t_tess).as_nanos() as u64;
        frame_ui.push(ui_ns);
        frame_tess.push(tess_ns);
        frame_total.push(ui_ns + tess_ns);
    }

    checks.check(
        "frame_pixels_per_point_matches",
        (frame_ppp - args.dpi).abs() < 1e-6,
        format!("frame_ppp={frame_ppp}, requested={}", args.dpi),
    );
    checks.check(
        "frame_has_no_paint_callbacks",
        frame_last.callbacks == 0,
        format!("callbacks={}", frame_last.callbacks),
    );
    let max_frame_passes = frame_pass_counts.iter().copied().max().unwrap_or(0);
    checks.check(
        "measured_frames_single_pass",
        frame_pass_counts.iter().all(|&passes| passes == 1),
        format!(
            "max_completed_passes={max_frame_passes}, multi_pass_frames={}",
            frame_pass_counts
                .iter()
                .filter(|&&passes| passes > 1)
                .count()
        ),
    );
    let frame_identity_distinct = frame_identities
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .len();
    checks.check(
        "frame_identity_stable_across_measured_frames",
        frame_identity_distinct == 1,
        format!(
            "first={:016x}, last={:016x}, distinct_hashes={frame_identity_distinct}",
            frame_first.identity, frame_last.identity,
        ),
    );
    checks.check(
        "measured_frame_shape_count_stable",
        frame_shape_counts
            .iter()
            .all(|&count| count == shapes_template.len()),
        format!(
            "template_shapes={}, measured_min={}, measured_max={}",
            shapes_template.len(),
            frame_shape_counts.iter().copied().min().unwrap_or(0),
            frame_shape_counts.iter().copied().max().unwrap_or(0)
        ),
    );
    checks.check(
        "measured_frames_have_root_viewport",
        frame_root_viewport_missing == 0,
        format!("missing_root_viewport_frames={frame_root_viewport_missing}"),
    );
    checks.check(
        "frame_mesh_work_is_nonzero",
        frame_last.meshes > 0 && frame_last.vertices > 0 && frame_last.indices > 0,
        format!(
            "meshes={}, vertices={}, indices={}",
            frame_last.meshes, frame_last.vertices, frame_last.indices
        ),
    );
    if let Some(warm_identity) = warm_frame_identity {
        checks.check(
            "warmup_and_measured_frame_identity",
            warm_identity == frame_first.identity,
            format!(
                "warm={:016x}, measured_first={:016x}",
                warm_identity, frame_first.identity
            ),
        );
    }

    // Cold Context/font startup. This is an in-process cold Context, not an
    // OS-cold process launch or a system-font discovery measurement. The first
    // Grid frame completes two UI passes, so the UI phase is a composite.
    let mut cold_context = Vec::with_capacity(args.cold_starts);
    let mut cold_ui = Vec::with_capacity(args.cold_starts);
    let mut cold_tess = Vec::with_capacity(args.cold_starts);
    let mut cold_total = Vec::with_capacity(args.cold_starts);
    let mut cold_pass_counts = Vec::with_capacity(args.cold_starts);
    let mut cold_identities = Vec::with_capacity(args.cold_starts);
    let mut cold_uploads = TextureUploads::default();
    for _ in 0..args.cold_starts {
        let t0 = Instant::now();
        let cold_ctx = new_context(&font.definitions, args.dpi);
        let mut cold_state = BenchState::default();
        let mut cold_input = input.clone();
        cold_input.time = Some(0.0);
        let t1 = Instant::now();
        let mut out = cold_ctx.run_ui(cold_input, |ui| build_ui(ui, &mut cold_state, &frame_text));
        let t2 = Instant::now();
        cold_pass_counts.push(out.platform_output.num_completed_passes);
        cold_uploads.add(take_textures(&mut out));
        let t_tess = Instant::now();
        let prims = cold_ctx.tessellate(out.shapes, out.pixels_per_point);
        let t3 = Instant::now();
        black_box(&prims);
        cold_identities.push(primitives_info(&prims).identity);
        let context_ns = (t1 - t0).as_nanos() as u64;
        let ui_ns = (t2 - t1).as_nanos() as u64;
        let tess_ns = (t3 - t_tess).as_nanos() as u64;
        cold_context.push(context_ns);
        cold_ui.push(ui_ns);
        cold_tess.push(tess_ns);
        cold_total.push(context_ns + ui_ns + tess_ns);
    }
    let cold_first = cold_identities.first().copied().unwrap_or_default();
    let cold_distinct = cold_identities
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .len();
    checks.check(
        "cold_start_identity_stable",
        cold_distinct == 1,
        format!("distinct_hashes={cold_distinct}"),
    );
    let cold_pass_min = cold_pass_counts.iter().copied().min().unwrap_or(0);
    let cold_pass_max = cold_pass_counts.iter().copied().max().unwrap_or(0);
    checks.check(
        "cold_start_pass_counts_stable",
        cold_pass_min >= 1 && cold_pass_min == cold_pass_max,
        format!("min_completed_passes={cold_pass_min}, max_completed_passes={cold_pass_max}"),
    );
    checks.check(
        "cold_start_matches_measured_frame_identity",
        cold_first == frame_last.identity,
        format!(
            "cold_first={:016x}, measured_last={:016x}",
            cold_first, frame_last.identity
        ),
    );

    // Sample-shape eligibility is separate from correctness. A reduced run can
    // still produce valid correctness evidence, but it is not acceptance-ready.
    checks.acceptance(
        "frame_sample_count_for_p99",
        args.frames >= 1_000,
        format!("frames={}, required>=1000", args.frames),
    );
    checks.acceptance(
        "micro_sample_counts",
        args.warmups >= 10 && args.steady >= 50,
        format!("warmups={}, steady={}", args.warmups, args.steady),
    );
    checks.acceptance(
        "cold_start_count",
        args.cold_starts >= 20,
        format!("cold_starts={}", args.cold_starts),
    );
    checks.acceptance(
        "paired_font_pinned",
        args.font.is_some(),
        format!(
            "font_mode={}, pass --font for paired comparisons",
            font.mode
        ),
    );

    let mut cases: Vec<(String, Stats)> = vec![
        (
            "layout_cache_hit_hash_and_lookup".to_owned(),
            summarize(&layout_cache_hit),
        ),
        ("layout_forced_miss".to_owned(), summarize(&layout_miss)),
        ("layout_job_clone".to_owned(), summarize(&job_clone)),
        ("tessellation".to_owned(), summarize(&tessellation)),
        ("frame_shapes_clone".to_owned(), summarize(&shapes_clone)),
        ("frame_ui".to_owned(), summarize(&frame_ui)),
        ("frame_tessellate".to_owned(), summarize(&frame_tess)),
    ];
    cases.sort_by(|a, b| a.0.cmp(&b.0));

    // Raw CSV.
    let mut csv = String::from("case,phase,sample,duration_ns,work_units\n");
    let push_series = |csv: &mut String, case: &str, phase: &str, values: &[u64], units: usize| {
        for (i, value) in values.iter().enumerate() {
            let _ = writeln!(csv, "{case},{phase},{i},{value},{units}");
        }
    };
    push_series(
        &mut csv,
        "layout_cache_hit_hash_and_lookup",
        "steady",
        &layout_cache_hit,
        args.ascii_chars,
    );
    push_series(
        &mut csv,
        "layout_forced_miss",
        "steady",
        &layout_miss,
        args.ascii_chars,
    );
    push_series(
        &mut csv,
        "layout_job_clone",
        "steady",
        &job_clone,
        args.ascii_chars,
    );
    push_series(
        &mut csv,
        "tessellation",
        "steady",
        &tessellation,
        cached_galley_info.glyphs,
    );
    push_series(
        &mut csv,
        "frame_shapes_clone",
        "steady",
        &shapes_clone,
        shapes_template.len(),
    );
    push_series(&mut csv, "frame", "ui", &frame_ui, frame_shapes);
    push_series(&mut csv, "frame", "tessellate", &frame_tess, frame_shapes);
    push_series(&mut csv, "frame", "total", &frame_total, frame_shapes);
    push_series(&mut csv, "cold_start", "context", &cold_context, 1);
    push_series(&mut csv, "cold_start", "ui", &cold_ui, 1);
    push_series(&mut csv, "cold_start", "tessellate", &cold_tess, 1);
    push_series(&mut csv, "cold_start", "total", &cold_total, 1);

    let frame_stats = summarize(&frame_total);
    let cold_stats = summarize(&cold_total);
    let correctness_passed = checks.passed(CheckKind::Correctness);
    let acceptance_ready = checks.passed(CheckKind::Acceptance);

    // Summary JSON.
    let mut summary = String::new();
    summary.push('{');
    let _ = write!(
        summary,
        "\"schema\":\"egui-text-perf-v1\",\"label\":{},\"tool\":\"crates/egui/examples/text_perf.rs\",",
        json_string(&args.label)
    );

    let parallelism = std::thread::available_parallelism()
        .ok()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|| "null".to_owned());
    let _ = write!(
        summary,
        "\"build\":{{\"egui_version\":{},\"profile\":{},\"debug_assertions\":{},\"panic_abort\":{},\"target_os\":{},\"target_arch\":{},\"target_family\":{},\"default_fonts_feature\":{},\"rayon_feature\":{},\"available_parallelism\":{}}},",
        json_string(env!("CARGO_PKG_VERSION")),
        json_string(if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }),
        cfg!(debug_assertions),
        cfg!(panic = "abort"),
        json_string(std::env::consts::OS),
        json_string(std::env::consts::ARCH),
        json_string(std::env::consts::FAMILY),
        cfg!(feature = "default_fonts"),
        egui::epaint::HAS_RAYON,
        parallelism,
    );

    let cpu = cpu_model();
    let kernel = kernel_release();
    let _ = write!(
        summary,
        "\"host\":{{\"cpu_model\":{},\"kernel\":{}}},",
        json_optional_string(cpu.as_deref()),
        json_optional_string(kernel.as_deref()),
    );

    let lockfile = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let _ = write!(
        summary,
        "\"identity\":{{\"source_root\":{},\"cargo_lock\":{},\"harness_source\":\"crates/egui/examples/text_perf.rs\",\"harness_source_fnv64\":{},\"text_fnv64\":{},\"frame_text_fnv64\":{}}},",
        json_string(env!("CARGO_MANIFEST_DIR")),
        json_string(&lockfile.display().to_string()),
        json_string(&fnv64_hex(HARNESS_SOURCE)),
        json_string(&text_fnv64),
        json_string(&frame_text_fnv64),
    );

    let counts_acceptance_shaped =
        args.warmups >= 10 && args.steady >= 50 && args.cold_starts >= 20 && args.frames >= 1_000;
    let _ = write!(
        summary,
        "\"workload\":{{\"kind\":\"cpu_headless_ascii\",\"miss_strategy\":\"unique_rotated_same_length_same_alphabet\",\"frame_workload\":\"panels+menu_bar+text_edit+labels+grid\",\"ascii_chars\":{},\"frame_text_chars\":{},\"viewport_points\":[{},{}],\"pixels_per_point\":{},\"font_size_points\":{},\"wrap_width_points\":{},\"warmups\":{},\"steady_samples\":{},\"cold_starts\":{},\"frame_samples\":{},\"frame_p99_eligible\":{},\"sample_counts_acceptance_shaped\":{},\"allow_missing_glyphs\":{},\"theme\":\"dark\"}},",
        args.ascii_chars,
        FRAME_TEXT_CHARS,
        DEFAULT_VIEWPORT_WIDTH,
        DEFAULT_VIEWPORT_HEIGHT,
        args.dpi,
        FONT_SIZE,
        WRAP_WIDTH,
        args.warmups,
        args.steady,
        args.cold_starts,
        args.frames,
        args.frames >= 1_000,
        counts_acceptance_shaped,
        args.allow_missing_glyphs,
    );
    let _ = write!(
        summary,
        "\"case_work_units\":{{\"layout_cache_hit_hash_and_lookup\":{},\"layout_forced_miss\":{},\"layout_job_clone\":{},\"tessellation_glyphs\":{},\"frame_shapes\":{},\"frame_samples\":{},\"cold_start_phase_samples\":{}}},",
        args.ascii_chars,
        args.ascii_chars,
        args.ascii_chars,
        cached_galley_info.glyphs,
        frame_shapes,
        args.frames,
        args.cold_starts * 3,
    );

    let resolved_path = font
        .resolved_path
        .as_ref()
        .map(|path| path.display().to_string());
    let _ = write!(
        summary,
        "\"fonts\":{{\"mode\":{},\"resolved_path\":{},\"font_data_fnv64\":{},\"font_data\":[",
        json_string(font.mode),
        json_optional_string(resolved_path.as_deref()),
        json_string(&font.font_data_fnv64),
    );
    for (i, entry) in font.font_data_entries.iter().enumerate() {
        if i > 0 {
            summary.push(',');
        }
        let _ = write!(
            summary,
            "{{\"name\":{},\"bytes\":{},\"index\":{},\"fnv64\":{}}}",
            json_string(&entry.name),
            entry.bytes,
            entry.index,
            json_string(&entry.fnv64),
        );
    }
    summary.push_str("],\"family_proportional\":[");
    if let Some(names) = font.definitions.families.get(&FontFamily::Proportional) {
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                summary.push(',');
            }
            summary.push_str(&json_string(name));
        }
    }
    summary.push_str("],\"family_monospace\":[");
    if let Some(names) = font.definitions.families.get(&FontFamily::Monospace) {
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                summary.push(',');
            }
            summary.push_str(&json_string(name));
        }
    }
    summary.push_str("]},");

    let _ = write!(
        summary,
        "\"galley_cached\":{{\"rows\":{},\"glyphs\":{},\"vertices\":{},\"indices\":{},\"rect_width\":{:.3},\"rect_height\":{:.3},\"mesh_bounds_width\":{:.3},\"mesh_bounds_height\":{:.3},\"pixels_per_point\":{},\"identity\":\"{:016x}\"}},",
        cached_galley_info.rows,
        cached_galley_info.glyphs,
        cached_galley_info.vertices,
        cached_galley_info.indices,
        cached_galley_info.rect_width,
        cached_galley_info.rect_height,
        cached_galley_info.mesh_bounds_width,
        cached_galley_info.mesh_bounds_height,
        cached_galley_info.pixels_per_point,
        cached_galley_info.identity,
    );
    let _ = write!(
        summary,
        "\"galley_fresh_miss\":{{\"rows\":{},\"glyphs\":{},\"vertices\":{},\"indices\":{},\"identity\":\"{:016x}\"}},",
        fresh_galley_info.rows,
        fresh_galley_info.glyphs,
        fresh_galley_info.vertices,
        fresh_galley_info.indices,
        fresh_galley_info.identity,
    );
    let _ = write!(
        summary,
        "\"galley_forced_miss_last\":{{\"rows\":{},\"glyphs\":{},\"vertices\":{},\"indices\":{},\"identity\":\"{:016x}\"}},",
        miss_galley_info.rows,
        miss_galley_info.glyphs,
        miss_galley_info.vertices,
        miss_galley_info.indices,
        miss_galley_info.identity,
    );
    let _ = write!(
        summary,
        "\"tessellation_result\":{{\"vertices\":{},\"indices\":{},\"identity\":\"{:016x}\"}},",
        tess_info.vertices, tess_info.indices, tess_info.identity
    );
    let _ = write!(
        summary,
        "\"frame_result\":{{\"shapes\":{},\"meshes\":{},\"callbacks\":{},\"vertices\":{},\"indices\":{},\"first_identity\":\"{:016x}\",\"last_identity\":\"{:016x}\",\"identity_distinct\":{}}},",
        frame_shapes,
        frame_last.meshes,
        frame_last.callbacks,
        frame_last.vertices,
        frame_last.indices,
        frame_first.identity,
        frame_last.identity,
        frame_identity_distinct,
    );
    let _ = write!(
        summary,
        "\"cache_counts\":{{\"after_cache_hit_warmup\":{cache_after_cached_warmup},\"after_cache_hit_steady\":{cache_after_cached_steady},\"after_miss_warmup\":{cache_after_miss_warmup},\"after_miss_steady\":{cache_after_miss_steady}}},",
    );

    let frame_passes_total: usize = frame_pass_counts.iter().sum();
    let warmup_passes_total: usize = warmup_pass_counts.iter().sum();
    let warmup_multi_pass_frames = warmup_pass_counts
        .iter()
        .filter(|&&passes| passes > 1)
        .count();
    let warmup_pass_max = warmup_pass_counts.iter().copied().max().unwrap_or(0);
    let frame_multi_pass_frames = frame_pass_counts
        .iter()
        .filter(|&&passes| passes > 1)
        .count();
    let cold_passes_total: usize = cold_pass_counts.iter().sum();
    let cold_pass_distinct = cold_pass_counts
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .len();
    let _ = write!(
        summary,
        "\"pass_counts\":{{\"warmup_frame_passes_total\":{warmup_passes_total},\"warmup_frames_multi_pass\":{warmup_multi_pass_frames},\"warmup_frame_passes_max\":{warmup_pass_max},\"measured_frame_passes_total\":{frame_passes_total},\"measured_frames_multi_pass\":{frame_multi_pass_frames},\"measured_frame_passes_max\":{max_frame_passes},\"cold_start_passes_total\":{cold_passes_total},\"cold_start_passes_min\":{cold_pass_min},\"cold_start_passes_max\":{cold_pass_max},\"cold_start_pass_values_distinct\":{cold_pass_distinct}}},",
    );
    let _ = write!(
        summary,
        "\"atlas\":{{\"size\":[{},{}],\"fill_ratio\":{:.6}}},",
        atlas_size[0], atlas_size[1], atlas_fill_ratio,
    );
    let _ = write!(
        summary,
        "\"texture_uploads\":{{\"warmup_frames\":{{\"deltas\":{},\"bytes\":{}}},\"measured_frames\":{{\"deltas\":{},\"bytes\":{}}},\"cold_starts\":{{\"deltas\":{},\"bytes\":{}}}}},",
        warmup_uploads.deltas,
        warmup_uploads.bytes,
        frame_uploads.deltas,
        frame_uploads.bytes,
        cold_uploads.deltas,
        cold_uploads.bytes,
    );
    summary.push_str("\"repaint\":{\"measured_frames_with_pending_repaint\":");
    let _ = write!(summary, "{repaint_frames}");
    summary.push_str(",\"missing_root_viewport_frames\":");
    let _ = write!(summary, "{frame_root_viewport_missing}");
    summary.push_str(",\"min_pending_delay_ms\":");
    push_optional_f64(&mut summary, repaint_min_ms);
    summary.push_str("},");

    let rss = peak_rss_kib()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_owned());
    let _ = write!(summary, "\"process\":{{\"peak_rss_kib\":{rss}}},");

    summary.push_str("\"cases\":{");
    for (i, (name, stats)) in cases.iter().enumerate() {
        if i > 0 {
            summary.push(',');
        }
        push_stats(&mut summary, name, stats);
    }
    summary.push_str("},");
    summary.push_str("\"frame\":{");
    push_stats(&mut summary, "total", &frame_stats);
    summary.push(',');
    push_stats(&mut summary, "ui", &summarize(&frame_ui));
    summary.push(',');
    push_stats(&mut summary, "tessellate", &summarize(&frame_tess));
    summary.push_str("},");
    summary.push_str("\"cold_start\":{");
    push_stats(&mut summary, "total", &cold_stats);
    summary.push(',');
    push_stats(&mut summary, "context", &summarize(&cold_context));
    summary.push(',');
    push_stats(&mut summary, "ui", &summarize(&cold_ui));
    summary.push(',');
    push_stats(&mut summary, "tessellate", &summarize(&cold_tess));
    let _ = write!(
        summary,
        ",\"identity_distinct\":{cold_distinct},\"completed_passes\":{{\"min\":{cold_pass_min},\"max\":{cold_pass_max},\"distinct\":{cold_pass_distinct}}}",
    );
    summary.push_str("},");

    summary.push_str("\"correctness\":{\"passed\":");
    summary.push_str(if correctness_passed { "true" } else { "false" });
    summary.push_str(",\"checks\":");
    checks.push_json(&mut summary, CheckKind::Correctness);
    summary.push_str(",\"failed\":[");
    for (i, name) in checks.failed(CheckKind::Correctness).iter().enumerate() {
        if i > 0 {
            summary.push(',');
        }
        summary.push_str(&json_string(name));
    }
    summary.push_str("]},");

    summary.push_str("\"acceptance\":{\"ready\":");
    summary.push_str(if acceptance_ready { "true" } else { "false" });
    summary.push_str(",\"checks\":");
    checks.push_json(&mut summary, CheckKind::Acceptance);
    summary.push_str(",\"failed\":[");
    for (i, name) in checks.failed(CheckKind::Acceptance).iter().enumerate() {
        if i > 0 {
            summary.push(',');
        }
        summary.push_str(&json_string(name));
    }
    summary.push_str("]},");

    summary.push_str("\"limitations\":[");
    let limitations = [
        "CPU-only headless harness: no GPU upload/rasterization/draw calls, no native window, compositor, focus, IME, clipboard, or Web behavior.",
        "Cold starts recreate an egui Context and its fonts in one process. They are not OS-cold process launches and do not measure system-font discovery. The cold context, UI, and tessellation times are split; the first Grid frame completes two UI passes.",
        "layout_cache_hit_hash_and_lookup times FontsView entry, hashing the full fixture LayoutJob, and the memoized lookup. layout_forced_miss has warm fonts/atlas but a cold galley; it excludes font parsing, first glyph rasterization, and system-font discovery. Cache-hit and forced-miss timings are separate and must not be added together.",
        "Frame timings include egui layout plus CPU tessellation, exclude texture-delta clearing, logging, GPU/renderer work, evidence serialization, event-loop/window overhead, texture upload, and draw calls. Measured frames are checked to complete exactly one UI pass.",
        "ASCII/Latin only. Non-Latin, CJK, Arabic, Thai, emoji, bidi, IME, and fallback fonts are not claimed as covered; pass a suitable licensed fixture font with --font for exploratory work and do not label tofu as coverage. Fixture cmap coverage inspects only the proportional family, not unrelated font families.",
        "Micro-case p95/p99 from 50 samples is a diagnostic, not a tail estimate. Frame p99 is reported only when at least 1,000 individual frame timings were captured; cold-start p99 is from 20 composite in-process samples and is not a process-startup tail estimate.",
        "Default bundled font sets differ between 0.36 and 0.37. Paired comparisons require the same --font file; actual font bytes, family ordering, source path, and noncryptographic FNV64 fingerprints are recorded.",
        "FNV64 values are fixture/font fingerprints only, not cryptographic digests or cross-version equality guarantees. Source SHA, lockfile SHA, compiler identity, and final evidence hashes are owned by the external pipeline.",
        "The repaint count is observed in synthetic measured frames, not a 60-second idle CPU trace. Texture upload bytes are derived from texture deltas before clearing; no GPU upload is performed.",
        "This is an initial CPU baseline, not full native/Web/GPU acceptance and not a substitute for the suite's managed paired performance run gates.",
    ];
    for (i, text) in limitations.iter().enumerate() {
        if i > 0 {
            summary.push(',');
        }
        summary.push_str(&json_string(text));
    }
    summary.push_str("]}");

    let csv_path = out_dir.join("samples.csv");
    let summary_path = out_dir.join("summary.json");
    fs::write(&csv_path, &csv)
        .map_err(|e| format!("failed to write {}: {e}", csv_path.display()))?;
    fs::write(&summary_path, &summary)
        .map_err(|e| format!("failed to write {}: {e}", summary_path.display()))?;

    eprintln!(
        "text_perf: {} (summary={}, samples={}, correctness_failed={}, acceptance_ready={})",
        out_dir.display(),
        summary_path.display(),
        csv_path.display(),
        checks.failed(CheckKind::Correctness).len(),
        acceptance_ready,
    );
    if args.stdout {
        println!("{summary}");
    }
    Ok(correctness_passed)
}
