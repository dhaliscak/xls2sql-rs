//! File loading: .xls/.xlsx via calamine, .csv via csv crate.
//!
//! Sheets are stored compactly: every cell is a `u32` id into a per-sheet text pool
//! (one big `String` + end offsets). Cell values are formatted to text once, while
//! loading, and repeated shared strings are stored only once.
//!
//! For .xlsx the cells are streamed straight from calamine's cell reader, so neither a
//! sparse cell list nor a `Range<Data>` (32 bytes + a heap `String` per cell) is built.

use calamine::{open_workbook_auto, Data, DataRef, Reader, Sheets};
use std::collections::HashMap;
use std::fmt::Write;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::Path;

/// Text id of an empty cell.
const EMPTY: u32 = 0;

pub struct Sheet {
    pub name: String,
    width: usize,
    /// `height * width` text ids, row-major.
    cells: Vec<u32>,
    /// All cell texts back to back.
    text: String,
    /// `ends[id]` = end offset of text `id` in `text`; text `id` starts at `ends[id - 1]`.
    /// `ends[0] == 0`, so id 0 is the empty string.
    ends: Vec<usize>,
}

impl Sheet {
    /// Builds a sheet from string rows (rows may be ragged; missing cells are empty).
    #[cfg(test)]
    pub fn from_rows(name: String, rows: Vec<Vec<String>>) -> Self {
        let mut b = SheetBuilder::new(None);
        let mut width = 0;
        let height = rows.len();
        for (r, row) in rows.iter().enumerate() {
            width = width.max(row.len());
            for (c, v) in row.iter().enumerate() {
                let id = b.push_text(v);
                b.set(r, c, id);
            }
        }
        b.finish_full(name, height, width)
    }

    pub fn width(&self) -> usize {
        self.width
    }

    #[cfg(test)]
    pub fn height(&self) -> usize {
        self.cells.len() / self.width.max(1)
    }

    /// Rows as slices of text ids (each `width()` long).
    pub fn rows(&self) -> impl Iterator<Item = &[u32]> {
        // `max(1)` keeps chunks_exact valid for an empty sheet (cells is empty then).
        self.cells.chunks_exact(self.width.max(1))
    }

    #[inline]
    pub fn text(&self, id: u32) -> &str {
        let id = id as usize;
        let start = if id == 0 { 0 } else { self.ends[id - 1] };
        &self.text[start..self.ends[id]]
    }
}

pub const SUPPORTED_EXTENSIONS: [&str; 3] = ["xls", "xlsx", "csv"];

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED_EXTENSIONS.iter().any(|s| s.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

pub fn load_file(path: &Path) -> Result<Vec<Sheet>, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if ext == "csv" {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("csv")
            .to_string();
        return Ok(vec![parse_csv(&bytes, name)?]);
    }

    let mut wb = open_workbook_auto(path).map_err(|e| e.to_string())?;
    let mut sheets = Vec::new();
    for name in wb.sheet_names().to_owned() {
        let sheet = match &mut wb {
            Sheets::Xlsx(x) => load_xlsx_sheet(x, name)?,
            other => {
                let range = other.worksheet_range(&name).map_err(|e| e.to_string())?;
                sheet_from_range(name, &range)
            }
        };
        sheets.push(sheet);
    }
    Ok(sheets)
}

fn load_xlsx_sheet<RS: std::io::Read + std::io::Seek>(
    wb: &mut calamine::Xlsx<RS>,
    name: String,
) -> Result<Sheet, String> {
    let mut reader = match wb.worksheet_cells_reader(&name) {
        Ok(r) => r,
        // Chart sheets etc.: calamine returns an empty range for these.
        Err(calamine::XlsxError::NotAWorksheet(_)) => {
            return Ok(SheetBuilder::new(None).finish_cropped(name))
        }
        Err(e) => return Err(e.to_string()),
    };

    let dims = reader.dimensions();
    let hint = (dims.len() > 0).then(|| (dims.end.0 as usize + 1, dims.end.1 as usize + 1));
    let mut b = SheetBuilder::new(hint);
    // Shared strings live in calamine's table for the whole read, so a borrowed
    // shared string is identified by its address: each distinct one is stored once.
    let mut shared: HashMap<usize, u32, BuildHasherDefault<PtrHasher>> = HashMap::default();
    let mut scratch = String::new();

    while let Some(cell) = reader.next_cell().map_err(|e| e.to_string())? {
        let (r, c) = cell.get_position();
        let id = match cell.get_value() {
            // Not part of the sheet's bounds, same as calamine's worksheet_range.
            DataRef::Empty => continue,
            DataRef::SharedString(s) => *shared
                .entry(s.as_ptr() as usize)
                .or_insert_with(|| b.push_text(s)),
            v => {
                let t = data_ref_text(v, &mut scratch);
                b.push_text(t)
            }
        };
        b.set_bounded(r as usize, c as usize, id);
    }
    Ok(b.finish_cropped(name))
}

fn sheet_from_range(name: String, range: &calamine::Range<Data>) -> Sheet {
    let (height, width) = range.get_size();
    let mut b = SheetBuilder::new(Some((height, width)));
    let mut scratch = String::new();
    for (r, c, v) in range.used_cells() {
        let id = b.push_text(cell_text(v, &mut scratch));
        b.set(r, c, id);
    }
    b.finish_full(name, height, width)
}

/// Text of a cell. Borrows string cells directly; other types are formatted into
/// `scratch` (reused across calls, so no allocation per cell).
pub fn cell_text<'a>(c: &'a Data, scratch: &'a mut String) -> &'a str {
    match c {
        Data::Empty | Data::Error(_) => "",
        Data::String(s) | Data::DateTimeIso(s) | Data::DurationIso(s) => s,
        Data::Bool(b) => bool_text(*b),
        Data::Int(i) => fmt_into(scratch, format_args!("{i}")),
        Data::Float(f) => fmt_into(scratch, format_args!("{f}")),
        Data::DateTime(dt) => datetime_text(dt, scratch),
    }
}

/// Same formatting as [`cell_text`], for calamine's borrowed cell values.
fn data_ref_text<'a>(c: &'a DataRef<'_>, scratch: &'a mut String) -> &'a str {
    match c {
        DataRef::Empty | DataRef::Error(_) => "",
        DataRef::SharedString(s) => s,
        DataRef::String(s) | DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => s,
        DataRef::Bool(b) => bool_text(*b),
        DataRef::Int(i) => fmt_into(scratch, format_args!("{i}")),
        DataRef::Float(f) => fmt_into(scratch, format_args!("{f}")),
        DataRef::DateTime(dt) => datetime_text(dt, scratch),
    }
}

fn bool_text(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

fn fmt_into<'a>(scratch: &'a mut String, args: std::fmt::Arguments) -> &'a str {
    scratch.clear();
    let _ = scratch.write_fmt(args);
    scratch
}

fn datetime_text<'a>(dt: &calamine::ExcelDateTime, scratch: &'a mut String) -> &'a str {
    scratch.clear();
    let _ = match dt.as_datetime() {
        Some(d) if d.time() == chrono::NaiveTime::MIN => write!(scratch, "{}", d.format("%Y-%m-%d")),
        Some(d) => write!(scratch, "{}", d.format("%Y-%m-%d %H:%M:%S")),
        None => write!(scratch, "{}", dt.as_f64()),
    };
    scratch
}

/// Hasher for pointer-sized keys (addresses are already well spread; mix the bits once).
#[derive(Default)]
struct PtrHasher(u64);

impl Hasher for PtrHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
    fn write_usize(&mut self, i: usize) {
        self.0 = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(26);
    }
}

/// Grows a row-major id grid as cells arrive (in any order), tracking the bounds of
/// the non-empty cells like calamine's `Range::from_sparse`.
struct SheetBuilder {
    cells: Vec<u32>,
    /// Row stride of `cells` (absolute column count).
    stride: usize,
    text: String,
    ends: Vec<usize>,
    /// (min_row, min_col, max_row, max_col) of the cells set via `set_bounded`.
    bounds: Option<(usize, usize, usize, usize)>,
}

/// Don't trust a sheet's declared dimensions for more pre-allocation than this.
const MAX_PREALLOC_CELLS: usize = 64 << 20;
/// Excel's column limit; wider declared dimensions are ignored.
const MAX_COLS: usize = 16_384;

impl SheetBuilder {
    /// `hint` = expected (rows, cols) in absolute coordinates.
    fn new(hint: Option<(usize, usize)>) -> Self {
        let (rows, cols) = hint.unwrap_or((0, 0));
        let stride = if cols <= MAX_COLS { cols } else { 0 };
        let mut cells = Vec::new();
        if let Some(n) = rows.checked_mul(stride).filter(|&n| n <= MAX_PREALLOC_CELLS) {
            cells.reserve_exact(n);
        }
        Self {
            cells,
            stride,
            text: String::new(),
            ends: vec![0],
            bounds: None,
        }
    }

    fn push_text(&mut self, s: &str) -> u32 {
        if s.is_empty() {
            return EMPTY;
        }
        self.text.push_str(s);
        self.ends.push(self.text.len());
        (self.ends.len() - 1) as u32
    }

    fn set(&mut self, r: usize, c: usize, id: u32) {
        if c >= self.stride {
            self.widen(c + 1);
        }
        let i = r * self.stride + c;
        if i >= self.cells.len() {
            self.cells.resize((r + 1) * self.stride, EMPTY);
        }
        self.cells[i] = id;
    }

    /// `set` that also extends the tracked bounds (even for an empty-text id, e.g. an
    /// error cell, because calamine counts those as part of the range).
    fn set_bounded(&mut self, r: usize, c: usize, id: u32) {
        self.bounds = Some(match self.bounds {
            None => (r, c, r, c),
            Some((r0, c0, r1, c1)) => (r0.min(r), c0.min(c), r1.max(r), c1.max(c)),
        });
        self.set(r, c, id);
    }

    /// Re-lays out the grid with a larger row stride.
    fn widen(&mut self, stride: usize) {
        let rows = if self.stride == 0 { 0 } else { self.cells.len() / self.stride };
        let mut cells = vec![EMPTY; rows * stride];
        if self.stride > 0 {
            for (r, row) in self.cells.chunks_exact(self.stride).enumerate() {
                cells[r * stride..r * stride + self.stride].copy_from_slice(row);
            }
        }
        self.cells = cells;
        self.stride = stride;
    }

    /// Sheet covering exactly rows `0..height`, columns `0..width`.
    fn finish_full(self, name: String, height: usize, width: usize) -> Sheet {
        if height == 0 || width == 0 {
            return self.into_sheet(name, 0, 0, Vec::new());
        }
        self.crop(name, (0, 0, height - 1, width - 1))
    }

    /// Sheet covering the bounds of the non-empty cells.
    fn finish_cropped(self, name: String) -> Sheet {
        match self.bounds {
            Some(b) => self.crop(name, b),
            None => self.into_sheet(name, 0, 0, Vec::new()),
        }
    }

    fn crop(mut self, name: String, (r0, c0, r1, c1): (usize, usize, usize, usize)) -> Sheet {
        let (height, width) = (r1 - r0 + 1, c1 - c0 + 1);
        if c1 >= self.stride {
            self.widen(c1 + 1);
        }
        self.cells.resize((r1 + 1) * self.stride, EMPTY);
        let cells = if r0 == 0 && c0 == 0 && width == self.stride {
            // Common case: already the exact layout, no copy.
            let mut cells = std::mem::take(&mut self.cells);
            cells.shrink_to_fit();
            cells
        } else {
            let mut cells = Vec::with_capacity(height * width);
            for r in r0..=r1 {
                let start = r * self.stride + c0;
                cells.extend_from_slice(&self.cells[start..start + width]);
            }
            cells
        };
        self.into_sheet(name, height, width, cells)
    }

    fn into_sheet(mut self, name: String, height: usize, width: usize, cells: Vec<u32>) -> Sheet {
        self.text.shrink_to_fit();
        self.ends.shrink_to_fit();
        debug_assert_eq!(cells.len(), height * width);
        Sheet {
            name,
            width,
            cells,
            text: self.text,
            ends: self.ends,
        }
    }
}

/// Decode as UTF-8 (BOM aware), falling back to Windows-1250 like the original.
fn decode(bytes: &[u8]) -> String {
    if let Some((enc, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        let (text, _) = enc.decode_without_bom_handling(&bytes[bom_len..]);
        return text.into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::WINDOWS_1250.decode(bytes).0.into_owned(),
    }
}

/// Pick separator among `,` `;` `\t` by frequency in the first line (ties → `,`).
fn detect_separator(text: &str) -> u8 {
    let first = text.lines().next().unwrap_or_default();
    [b',', b';', b'\t']
        .into_iter()
        .rev()
        .max_by_key(|&c| first.bytes().filter(|&b| b == c).count())
        .unwrap_or(b',')
}

fn parse_csv(bytes: &[u8], name: String) -> Result<Sheet, String> {
    let text = decode(bytes);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(detect_separator(&text))
        .from_reader(text.as_bytes());

    let mut b = SheetBuilder::new(None);
    let (mut height, mut width) = (0, 0);
    let mut rec = csv::StringRecord::new();
    while rdr.read_record(&mut rec).map_err(|e| e.to_string())? {
        width = width.max(rec.len());
        for (c, v) in rec.iter().enumerate() {
            let id = b.push_text(v);
            if id != EMPTY {
                b.set(height, c, id);
            }
        }
        height += 1;
    }
    Ok(b.finish_full(name, height, width))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(s: &Sheet) -> Vec<Vec<String>> {
        s.rows()
            .take(s.height())
            .map(|r| r.iter().map(|&id| s.text(id).to_string()).collect())
            .collect()
    }

    #[test]
    fn csv_semicolon_and_padding() {
        let s = parse_csv(b"a;b;c\n1;2\n", "t".into()).unwrap();
        assert_eq!(grid(&s), vec![vec!["a", "b", "c"], vec!["1", "2", ""]]);
    }

    #[test]
    fn csv_comma_default() {
        let s = parse_csv(b"a\n1\n", "t".into()).unwrap();
        assert_eq!(grid(&s), vec![vec!["a"], vec!["1"]]);
    }

    #[test]
    fn csv_cp1250_fallback() {
        // "š" in Windows-1250 is 0x9A
        let s = parse_csv(b"name\n\x9Aa\n", "t".into()).unwrap();
        assert_eq!(grid(&s)[1][0], "ša");
    }

    #[test]
    fn csv_utf8_bom() {
        let s = parse_csv("\u{feff}x,y\n1,2\n".as_bytes(), "t".into()).unwrap();
        assert_eq!(grid(&s)[0], vec!["x", "y"]);
    }

    #[test]
    fn empty_csv() {
        let s = parse_csv(b"", "t".into()).unwrap();
        assert!(grid(&s).is_empty());
        assert_eq!(s.rows().count(), 0);
    }

    #[test]
    fn csv_trailing_empty_column_kept() {
        let s = parse_csv(b"a,b,\n1,,\n", "t".into()).unwrap();
        assert_eq!(grid(&s), vec![vec!["a", "b", ""], vec!["1", "", ""]]);
    }

    #[test]
    fn cell_formatting() {
        let mut sc = String::new();
        assert_eq!(cell_text(&Data::Float(1.0), &mut sc), "1");
        assert_eq!(cell_text(&Data::Float(1.5), &mut sc), "1.5");
        assert_eq!(cell_text(&Data::Int(-3), &mut sc), "-3");
        assert_eq!(cell_text(&Data::Bool(true), &mut sc), "True");
        assert_eq!(data_ref_text(&DataRef::Float(2.5), &mut sc), "2.5");
        assert_eq!(data_ref_text(&DataRef::SharedString("x"), &mut sc), "x");
    }

    #[test]
    fn builder_crops_to_non_empty_bounds_like_calamine() {
        // Cells at B3 and D4 (0-based (2,1) and (3,3)), arriving out of order and
        // beyond the declared width.
        let mut b = SheetBuilder::new(Some((2, 2)));
        let x = b.push_text("x");
        let y = b.push_text("y");
        b.set_bounded(3, 3, y);
        b.set_bounded(2, 1, x);
        let s = b.finish_cropped("s".into());
        assert_eq!((s.height(), s.width()), (2, 3));
        assert_eq!(grid(&s), vec![vec!["x", "", ""], vec!["", "", "y"]]);
    }

    #[test]
    fn builder_empty() {
        let s = SheetBuilder::new(Some((5, 5))).finish_cropped("s".into());
        assert_eq!((s.height(), s.width()), (0, 0));
        assert_eq!(s.rows().count(), 0);
    }

    #[test]
    fn sheet_from_calamine_range() {
        let mut r = calamine::Range::new((0, 0), (1, 1));
        r.set_value((0, 0), Data::String("h".into()));
        r.set_value((1, 1), Data::Float(3.0));
        let s = sheet_from_range("s".into(), &r);
        assert_eq!(grid(&s), vec![vec!["h", ""], vec!["", "3"]]);
    }

    #[test]
    fn supported_ext() {
        assert!(is_supported(Path::new("a.XLSX")));
        assert!(!is_supported(Path::new("a.txt")));
    }
}
