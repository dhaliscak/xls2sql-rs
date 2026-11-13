//! T-SQL script generation. Port of `GenerateSQLQuery` and helpers from the WPF version.
//!
//! Performance notes:
//! - Cells are written straight into the output buffer (no per-cell `String`s).
//! - INSERT batches are independent, so groups of batches are built in parallel
//!   with rayon and returned as ordered parts (kept as parts for display, streamed
//!   to disk for Save). Every part ends at a line end, so the viewer can index lines
//!   without ever joining the parts into one big string.

use crate::loader::Sheet;
use rayon::prelude::*;
use std::fmt::Write;

/// Optional auto-id column prepended in CREATE TABLE.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FirstColumn {
    #[default]
    None,
    IntIdentity,
    NewSequentialId,
}

impl FirstColumn {
    pub const ALL: [FirstColumn; 3] = [
        FirstColumn::None,
        FirstColumn::IntIdentity,
        FirstColumn::NewSequentialId,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FirstColumn::None => "None",
            FirstColumn::IntIdentity => "Id (int, IDENTITY 1,1)",
            FirstColumn::NewSequentialId => "Id (NewSequentialId())",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SqlSettings {
    pub database_name: String,
    pub table_name: String,
    pub create_table: bool,
    /// Rows per INSERT batch (>= 1).
    pub separator: usize,
    pub prefer_nulls: bool,
    pub trim_white_spaces: bool,
    pub first_column: FirstColumn,
}

impl Default for SqlSettings {
    fn default() -> Self {
        Self {
            database_name: "databasename".into(),
            table_name: "tablename".into(),
            create_table: true,
            separator: 1000,
            prefer_nulls: true,
            trim_white_spaces: true,
            first_column: FirstColumn::None,
        }
    }
}

const BATCH_SEPARATOR: &str = ";\r\n\r\n";

/// Whole script as one string.
#[cfg(test)]
pub fn generate_sql_query(s: &SqlSettings, sheet: &Sheet) -> String {
    let parts = generate_sql_parts(s, sheet);
    let mut out = String::with_capacity(parts.iter().map(String::len).sum());
    for p in &parts {
        out.push_str(p);
    }
    out
}

/// Script as ordered parts whose concatenation is the full script.
pub fn generate_sql_parts(s: &SqlSettings, sheet: &Sheet) -> Vec<String> {
    let separator = s.separator.max(1);

    let mut head = String::new();
    let _ = write!(head, "USE {};\r\n", s.database_name);
    head.push_str("SET ANSI_NULLS ON;\r\n");
    head.push_str("SET QUOTED_IDENTIFIER ON;\r\n");
    head.push_str("\r\n");

    let headers = get_column_names(sheet, s.trim_white_spaces);
    let column_names = generate_column_names(&headers);
    let header_count = headers.len();

    if s.create_table {
        let _ = write!(
            head,
            "CREATE TABLE {} ({});\r\n\r\n",
            s.table_name,
            generate_column_names_for_create_table(&headers, s.first_column)
        );
    }

    let insert_header = format!("INSERT INTO {} ({}) \r\nVALUES ", s.table_name, column_names);

    let data_rows: Vec<&[u32]> = sheet.rows().skip(1).collect();
    if data_rows.is_empty() {
        head.push_str(&insert_header);
        head.push(';');
        return vec![head];
    }

    // Group whole batches into work units: enough units to keep all cores busy,
    // few enough that per-unit overhead stays negligible.
    let total_batches = data_rows.len().div_ceil(separator);
    let target_units = rayon::current_num_threads() * 4;
    let batches_per_unit = total_batches.div_ceil(target_units).max(1);
    let rows_per_unit = batches_per_unit * separator;

    let col_count = header_count.min(sheet.width());
    let unit_count = data_rows.len().div_ceil(rows_per_unit);
    let units: Vec<String> = data_rows
        .par_chunks(rows_per_unit)
        .enumerate()
        .map(|(unit_idx, rows)| {
            let last_unit = unit_idx + 1 == unit_count;
            let batch_count = rows.len().div_ceil(separator);
            let mut out = String::with_capacity(estimate_size(sheet, rows, col_count));
            for (i, batch) in rows.chunks(separator).enumerate() {
                out.push_str(&insert_header);
                for (r, row) in batch.iter().enumerate() {
                    if r > 0 {
                        out.push_str(", ");
                    }
                    write_row(&mut out, sheet, &row[..col_count], s);
                }
                // The separator closes its batch (rather than opening the next one), so
                // every part ends with a complete line.
                if last_unit && i + 1 == batch_count {
                    out.push(';');
                } else {
                    out.push_str(BATCH_SEPARATOR);
                }
            }
            out
        })
        .collect();

    let mut parts = Vec::with_capacity(units.len() + 1);
    parts.push(head);
    parts.extend(units);
    parts
}

/// Rough output size for a unit, from a small sample, to avoid repeated reallocation.
fn estimate_size(sheet: &Sheet, rows: &[&[u32]], cols: usize) -> usize {
    const SAMPLE: usize = 16;
    let sampled = rows.len().min(SAMPLE).max(1);
    let bytes: usize = rows
        .iter()
        .take(SAMPLE)
        .map(|r| {
            r[..cols]
                .iter()
                .map(|&c| sheet.text(c).len() + 6)
                .sum::<usize>()
                + 4
        })
        .sum();
    bytes / sampled * rows.len() * 11 / 10
}

/// Writes `('v0', N'v1', NULL, ...)`.
///
/// Matches the C# output: `N'` prefix only from the second column onward, and with
/// "Prefer Nulls" an empty (or literal `NULL`) value becomes `NULL` — except in the
/// first column, where it stays quoted `'NULL'`, because the original only replaced
/// `N'NULL'`.
fn write_row(out: &mut String, sheet: &Sheet, cells: &[u32], s: &SqlSettings) {
    out.push('(');
    if cells.is_empty() {
        out.push_str("''");
    }
    for (j, &cell) in cells.iter().enumerate() {
        let mut v = sheet.text(cell);
        if s.trim_white_spaces {
            v = v.trim();
        }
        let is_null = s.prefer_nulls && (v.is_empty() || v == "NULL");
        if j > 0 {
            out.push_str(", ");
            if is_null {
                out.push_str("NULL");
                continue;
            }
            out.push('N');
        }
        out.push('\'');
        if is_null {
            out.push_str("NULL");
        } else {
            push_escaped(out, v);
        }
        out.push('\'');
    }
    out.push(')');
}

#[inline]
fn push_escaped(out: &mut String, v: &str) {
    let mut rest = v;
    while let Some(p) = rest.find('\'') {
        out.push_str(&rest[..=p]);
        out.push('\'');
        rest = &rest[p + 1..];
    }
    out.push_str(rest);
}

/// First row is the header row; tabs/newlines removed; empty headers excluded.
pub fn get_column_names(sheet: &Sheet, trim_white_spaces: bool) -> Vec<String> {
    let Some(first) = sheet.rows().next() else {
        return Vec::new();
    };
    first
        .iter()
        .map(|&c| {
            let h = sheet.text(c);
            let h = if trim_white_spaces { h.trim() } else { h };
            h.chars()
                .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
                .collect::<String>()
        })
        .filter(|h| !h.is_empty())
        .collect()
}

pub fn generate_column_names(headers: &[String]) -> String {
    if headers.is_empty() {
        return String::new();
    }
    format!("[{}]", headers.join("], ["))
}

pub fn generate_column_names_for_create_table(headers: &[String], first: FirstColumn) -> String {
    if headers.is_empty() {
        return String::new();
    }
    let mut sb = String::new();
    match first {
        FirstColumn::None => {}
        FirstColumn::IntIdentity => sb.push_str("Id INT IDENTITY (1, 1) NOT NULL, "),
        FirstColumn::NewSequentialId => {
            sb.push_str("Id UNIQUEIDENTIFIER DEFAULT NEWSEQUENTIALID() NOT NULL, ")
        }
    }
    let _ = write!(sb, "[{}] varchar(max) NULL", headers.join("] varchar(max) NULL, ["));
    sb
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet(rows: &[&[&str]]) -> Sheet {
        let rows: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().map(|c| c.to_string()).collect())
            .collect();
        Sheet::from_rows("Sheet1".into(), rows)
    }

    #[test]
    fn basic_output_matches_wpf_format() {
        let sh = sheet(&[&["Name", "Age"], &["Bob", "30"], &["O'Neil", ""]]);
        let s = SqlSettings {
            table_name: "People".into(),
            database_name: "Db".into(),
            ..Default::default()
        };
        assert_eq!(
            generate_sql_query(&s, &sh),
            "USE Db;\r\nSET ANSI_NULLS ON;\r\nSET QUOTED_IDENTIFIER ON;\r\n\r\n\
             CREATE TABLE People ([Name] varchar(max) NULL, [Age] varchar(max) NULL);\r\n\r\n\
             INSERT INTO People ([Name], [Age]) \r\nVALUES ('Bob', N'30'), ('O''Neil', NULL);"
        );
    }

    #[test]
    fn first_column_null_stays_quoted_quirk() {
        let sh = sheet(&[&["A", "B"], &["", "x"]]);
        let s = SqlSettings { create_table: false, ..Default::default() };
        assert!(generate_sql_query(&s, &sh).ends_with("VALUES ('NULL', N'x');"));
    }

    #[test]
    fn literal_null_text_becomes_null_like_original() {
        let sh = sheet(&[&["A", "B"], &["NULL", "NULL"]]);
        let s = SqlSettings { create_table: false, ..Default::default() };
        assert!(generate_sql_query(&s, &sh).ends_with("VALUES ('NULL', NULL);"));
    }

    #[test]
    fn no_prefer_nulls_keeps_empty_strings() {
        let sh = sheet(&[&["A", "B"], &["x", ""]]);
        let s = SqlSettings { create_table: false, prefer_nulls: false, ..Default::default() };
        assert!(generate_sql_query(&s, &sh).ends_with("VALUES ('x', N'');"));
    }

    #[test]
    fn trims_and_escapes_values() {
        let sh = sheet(&[&["A", "B"], &["  x ", " 'y' "]]);
        let s = SqlSettings { create_table: false, ..Default::default() };
        assert!(generate_sql_query(&s, &sh).ends_with("VALUES ('x', N'''y''');"));
    }

    #[test]
    fn batches_split_at_separator() {
        let sh = sheet(&[&["A"], &["1"], &["2"], &["3"]]);
        let s = SqlSettings { create_table: false, separator: 2, ..Default::default() };
        assert!(generate_sql_query(&s, &sh).ends_with(
            "INSERT INTO tablename ([A]) \r\nVALUES ('1'), ('2');\r\n\r\n\
             INSERT INTO tablename ([A]) \r\nVALUES ('3');"
        ));
    }

    #[test]
    fn parallel_units_join_identically_to_sequential() {
        // Many rows and a small separator force multiple work units.
        let mut rows: Vec<Vec<String>> = vec![vec!["A".into(), "B".into()]];
        for i in 0..10_007 {
            let b = if i % 3 == 0 { String::new() } else { format!("v'{i}") };
            rows.push(vec![i.to_string(), b]);
        }
        let sh = Sheet::from_rows("s".into(), rows.clone());
        let s = SqlSettings { create_table: false, separator: 7, ..Default::default() };

        let mut expected = String::from(
            "USE databasename;\r\nSET ANSI_NULLS ON;\r\nSET QUOTED_IDENTIFIER ON;\r\n\r\n",
        );
        for (i, r) in rows[1..].iter().enumerate() {
            if i % 7 == 0 {
                if i > 0 {
                    expected.push_str(";\r\n\r\n");
                }
                expected.push_str("INSERT INTO tablename ([A], [B]) \r\nVALUES ");
            } else {
                expected.push_str(", ");
            }
            let b = if r[1].is_empty() {
                "NULL".to_string()
            } else {
                format!("N'{}'", r[1].replace('\'', "''"))
            };
            expected.push_str(&format!("('{}', {})", r[0], b));
        }
        expected.push(';');
        assert_eq!(generate_sql_query(&s, &sh), expected);
    }

    #[test]
    fn parts_end_on_line_boundaries() {
        let mut rows: Vec<Vec<String>> = vec![vec!["A".into()]];
        rows.extend((0..5_000).map(|i| vec![i.to_string()]));
        let sh = Sheet::from_rows("s".into(), rows);
        let s = SqlSettings { separator: 3, ..Default::default() };
        let parts = generate_sql_parts(&s, &sh);
        assert!(parts.len() > 2);
        let (last, rest) = parts.split_last().unwrap();
        assert!(rest.iter().all(|p| p.ends_with('\n')));
        assert!(last.ends_with(");"));
    }

    #[test]
    fn no_data_rows() {
        let sh = sheet(&[&["A"]]);
        let s = SqlSettings { create_table: false, ..Default::default() };
        assert!(generate_sql_query(&s, &sh).ends_with("INSERT INTO tablename ([A]) \r\nVALUES ;"));
    }

    #[test]
    fn headers_trimmed_cleaned_and_empty_skipped() {
        let sh = sheet(&[&[" Na\nme ", "", "X\t"]]);
        assert_eq!(get_column_names(&sh, true), vec!["Name", "X"]);
    }

    #[test]
    fn first_column_variants() {
        let h = vec!["A".to_string()];
        assert_eq!(
            generate_column_names_for_create_table(&h, FirstColumn::IntIdentity),
            "Id INT IDENTITY (1, 1) NOT NULL, [A] varchar(max) NULL"
        );
        assert_eq!(
            generate_column_names_for_create_table(&h, FirstColumn::NewSequentialId),
            "Id UNIQUEIDENTIFIER DEFAULT NEWSEQUENTIALID() NOT NULL, [A] varchar(max) NULL"
        );
    }

    #[test]
    fn extra_cells_beyond_headers_ignored() {
        let sh = sheet(&[&["A", ""], &["1", "2"]]);
        let s = SqlSettings { create_table: false, ..Default::default() };
        assert!(generate_sql_query(&s, &sh).ends_with("VALUES ('1');"));
    }

    /// Manual perf check: `cargo test --release -- --ignored --nocapture huge_file`
    /// Set `XLS2SQL_DUMP=<path>` to also write the generated script.
    #[test]
    #[ignore]
    fn huge_file_timing() {
        let file = std::env::var("XLS2SQL_FILE").unwrap_or_else(|_| "test huge.xlsx".into());
        let path = std::path::Path::new(&file);
        let t = std::time::Instant::now();
        let sheets = crate::loader::load_file(path).unwrap();
        println!("load: {:?}", t.elapsed());
        for sh in &sheets {
            println!("sheet {}: {:?}", sh.name, (sh.height(), sh.width()));
        }
        let mut sql = String::new();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            sql = generate_sql_query(&SqlSettings::default(), &sheets[0]);
            println!("generate: {:?}, {} bytes", t.elapsed(), sql.len());
        }
        if let Ok(p) = std::env::var("XLS2SQL_DUMP") {
            std::fs::write(p, &sql).unwrap();
        }
    }
}
