# xls2sql (Rust)

Rust port of the WPF `xls2sql` tool. Converts `.xls` / `.xlsx` / `.csv` into T-SQL
`INSERT` (and optionally `CREATE TABLE`) scripts. Same UI options and same output format.

## Build

Requires the MSVC Rust toolchain (pinned in `rust-toolchain.toml`) and VS Build Tools.

```
cargo build --release      # -> target/release/xls2sql.exe (single self-contained exe)
cargo test
```

## Layout

| File | Purpose |
|------|---------|
| `src/main.rs` | egui/eframe window, drag-and-drop, file dialog; load/generate/save run on background threads |
| `src/loader.rs` | Reads xls/xlsx (calamine) and csv (auto `,` `;` `\t`, UTF-8 or Windows-1250 fallback) into calamine `Range<Data>`; cells formatted lazily |
| `src/sql.rs` | SQL generation (port of `GenerateSQLQuery`), parallel over INSERT batches (rayon) + unit tests |
| `src/viewer.rs` | Virtualized read-only output view (only visible rows are laid out) |

## Performance

`mimalloc` global allocator; no per-cell allocations during generation; batches generated
in parallel; Save streams parts to disk. Manual benchmark (put a large file at
`test huge.xlsx`, git-ignored):

```
cargo test --release -- --ignored --nocapture huge_file
```

Preserved quirks from the C# version: `N'` prefix only from second column onward; with
"Prefer Nulls" the first column's empty value becomes the string `'NULL'`; empty headers are
skipped but values are still taken by position. "Save" writes `{table}.txt` next to the exe.

Cell formatting differences vs. .NET: numbers use Rust formatting (`1`, `1.5`, invariant `.`),
dates are `YYYY-MM-DD[ HH:MM:SS]`, booleans `True`/`False`.
