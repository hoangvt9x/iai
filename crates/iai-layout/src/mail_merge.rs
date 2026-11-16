//! Data for filling templates: a CSV / Excel file read into a [`MergeTable`]
//! (the vector templates of the Pro pack fill their fields from it), and safe
//! output file names from a `{{field}}` pattern. Pure data: nothing about
//! egui, the job queue or PDF.
//!
//! A field `{{Field name}}` is matched against the data headers
//! case-insensitively and after trimming outer spaces (the letters must
//! match).

use std::collections::HashMap;
use std::path::Path;

/// A parsed data source: a header row plus zero or more data rows. Cells are
/// already stringified (Excel numbers/dates formatted for Vietnamese output).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MergeTable {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// One data row resolved to a header-keyed lookup, ready for substitution.
pub struct MergeRow {
    /// Normalised header -> cell value. See [`normalize_key`].
    map: HashMap<String, String>,
}

impl MergeRow {
    /// Look a placeholder name up against this row (trim + case-insensitive).
    /// `Some("")` means the field exists but is blank; `None` means no such
    /// column.
    pub fn get(&self, field: &str) -> Option<&str> {
        self.map.get(&normalize_key(field)).map(|s| s.as_str())
    }
}

impl MergeTable {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// True when `field` (trimmed, case-insensitive) is one of the headers.
    pub fn has_field(&self, field: &str) -> bool {
        let key = normalize_key(field);
        self.headers.iter().any(|h| normalize_key(h) == key)
    }

    /// Resolve one data row into a header-keyed lookup. Rows shorter than the
    /// header stretch read the missing trailing cells as empty.
    pub fn row(&self, index: usize) -> Option<MergeRow> {
        let cells = self.rows.get(index)?;
        let mut map = HashMap::with_capacity(self.headers.len());
        for (i, header) in self.headers.iter().enumerate() {
            let value = cells.get(i).cloned().unwrap_or_default();
            map.insert(normalize_key(header), value);
        }
        Some(MergeRow { map })
    }
}

/// Fold a placeholder or header name to its match key: outer whitespace trimmed
/// and lower-cased. Vietnamese diacritics are preserved (only case is folded).
pub fn normalize_key(s: &str) -> String {
    s.trim().to_lowercase()
}

// ---------------------------------------------------------------------------
// Reading data sources
// ---------------------------------------------------------------------------

/// Read a CSV or spreadsheet file into a [`MergeTable`], dispatching on the
/// file extension. `.csv`/`.tsv`/`.txt` go through the CSV reader; everything
/// else (`.xlsx`, `.xls`, `.xlsm`, `.ods`, ...) through calamine.
pub fn read_data_file(path: &Path) -> Result<MergeTable, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "csv" | "tsv" | "txt" => {
            let bytes = std::fs::read(path).map_err(|e| format!("Không đọc được tệp: {e}"))?;
            read_csv_bytes(&bytes)
        }
        _ => read_spreadsheet(path),
    }
}

/// Parse CSV bytes: strip a UTF-8 BOM, sniff the delimiter (`,`, `;` or tab)
/// from the first line, and read header + rows.
pub fn read_csv_bytes(bytes: &[u8]) -> Result<MergeTable, String> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let text = String::from_utf8_lossy(bytes);
    let delimiter = sniff_delimiter(&text);
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter)
        .from_reader(text.as_bytes());
    let rows = reader
        .records()
        .map(|record| {
            record
                .map(|r| r.iter().map(|c| c.to_string()).collect::<Vec<_>>())
                .map_err(|e| format!("Lỗi đọc CSV: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    table_from_rows(rows.into_iter())
}

/// Pick the delimiter with the most occurrences on the first non-empty line.
fn sniff_delimiter(text: &str) -> u8 {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let comma = first.matches(',').count();
    let semi = first.matches(';').count();
    let tab = first.matches('\t').count();
    if semi > comma && semi >= tab {
        b';'
    } else if tab > comma && tab > semi {
        b'\t'
    } else {
        b','
    }
}

/// Read the first worksheet of an Excel/ODS workbook into a [`MergeTable`].
fn read_spreadsheet(path: &Path) -> Result<MergeTable, String> {
    use calamine::{Reader, open_workbook_auto};
    let mut workbook =
        open_workbook_auto(path).map_err(|e| format!("Không mở được tệp bảng tính: {e}"))?;
    let range = workbook
        .worksheet_range_at(0)
        .ok_or_else(|| "Tệp không có trang tính nào".to_string())?
        .map_err(|e| format!("Không đọc được trang tính: {e}"))?;
    let rows = range
        .rows()
        .map(|row| row.iter().map(cell_to_string).collect::<Vec<_>>());
    table_from_rows(rows)
}

/// Stringify one spreadsheet cell for merge output. Numbers drop a redundant
/// `.0`; dates format as Vietnamese `dd/mm/yyyy` (with `HH:MM` only when a time
/// is present) so contract fields read naturally.
fn cell_to_string(cell: &calamine::Data) -> String {
    use calamine::Data;
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => format_number(*f),
        Data::DateTime(dt) => format_excel_datetime(dt),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("{e:?}"),
    }
}

fn format_number(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

fn format_excel_datetime(dt: &calamine::ExcelDateTime) -> String {
    match dt.as_datetime() {
        Some(ndt) if ndt.time() == chrono::NaiveTime::MIN => ndt.format("%d/%m/%Y").to_string(),
        Some(ndt) => ndt.format("%d/%m/%Y %H:%M").to_string(),
        None => format_number(dt.as_f64()),
    }
}

/// Build a table from raw string rows: the first row is the header (trailing
/// empty columns trimmed), the rest are data rows (fully blank rows skipped).
fn table_from_rows(mut rows: impl Iterator<Item = Vec<String>>) -> Result<MergeTable, String> {
    let mut headers = loop {
        match rows.next() {
            Some(row) if row.iter().any(|c| !c.trim().is_empty()) => break row,
            Some(_) => continue,
            None => return Err("Tệp dữ liệu trống — không có tiêu đề cột".to_string()),
        }
    };
    while headers.last().is_some_and(|c| c.trim().is_empty()) {
        headers.pop();
    }
    let width = headers.len();
    if width == 0 {
        return Err("Không tìm thấy tiêu đề cột nào".to_string());
    }
    let mut data = Vec::new();
    for mut row in rows {
        if row.iter().take(width).all(|c| c.trim().is_empty()) {
            continue;
        }
        row.truncate(width);
        while row.len() < width {
            row.push(String::new());
        }
        data.push(row);
    }
    Ok(MergeTable {
        headers,
        rows: data,
    })
}

// ---------------------------------------------------------------------------
// Placeholder scanning
// ---------------------------------------------------------------------------

/// One `{{field}}` occurrence in a string, as char indices into the source.
struct Placeholder {
    start: usize,
    end: usize,
    name: String,
}

/// Scan a `char` slice for `{{field}}` placeholders. A placeholder ends at the
/// first `}}`; a stray `{` or newline inside cancels it (so unbalanced braces in
/// prose are left alone).
fn scan_placeholders(chars: &[char]) -> Vec<Placeholder> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < chars.len() {
        if chars[i] == '{' && chars[i + 1] == '{' {
            let mut j = i + 2;
            let mut close = None;
            while j + 1 < chars.len() {
                if chars[j] == '}' && chars[j + 1] == '}' {
                    close = Some(j);
                    break;
                }
                if chars[j] == '{' || chars[j] == '\n' {
                    break;
                }
                j += 1;
            }
            if let Some(c) = close {
                let name: String = chars[i + 2..c].iter().collect();
                if !name.trim().is_empty() {
                    out.push(Placeholder {
                        start: i,
                        end: c + 2,
                        name: name.trim().to_string(),
                    });
                    i = c + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Output file names
// ---------------------------------------------------------------------------

/// Expand a file-name pattern (same `{{field}}` syntax; unknown fields become
/// empty) and sanitise it for the filesystem. Returns an empty string when the
/// pattern resolves to nothing usable — the caller supplies a fallback.
pub fn expand_filename(pattern: &str, row: &MergeRow) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let placeholders = scan_placeholders(&chars);
    let mut out = String::new();
    let mut cursor = 0;
    for ph in &placeholders {
        out.extend(&chars[cursor..ph.start]);
        if let Some(value) = row.get(&ph.name) {
            out.push_str(value);
        }
        cursor = ph.end;
    }
    out.extend(&chars[cursor..]);
    sanitize_filename(&out)
}

/// Make `name` safe as a single path component on Windows and Unix: replace
/// reserved characters, drop control chars, trim surrounding dots/spaces and
/// cap the length.
pub fn sanitize_filename(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => ' ',
            c => c,
        })
        .collect();
    s = s.trim().trim_matches('.').trim().to_string();
    // Collapse runs of whitespace introduced by the mapping above.
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > 120 {
        collapsed.chars().take(120).collect()
    } else {
        collapsed
    }
}

/// Make `base` unique against names already produced in this batch by appending
/// ` (2)`, ` (3)`, ... The comparison is case-insensitive to match Windows.
pub fn unique_stem(base: &str, used: &mut std::collections::HashSet<String>) -> String {
    let base = if base.is_empty() { "tai-lieu" } else { base };
    let mut candidate = base.to_string();
    let mut n = 2;
    while !used.insert(candidate.to_lowercase()) {
        candidate = format!("{base} ({n})");
        n += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> MergeTable {
        MergeTable {
            headers: vec!["Họ tên".into(), "Số tiền".into(), "Ngày".into()],
            rows: vec![
                vec!["Nguyễn Văn A".into(), "1500000".into(), "01/09/2026".into()],
                vec!["Trần Thị B".into(), "".into(), "".into()],
            ],
        }
    }

    #[test]
    fn csv_parses_headers_and_rows() {
        let csv = "Name,Amount\nAlice,100\nBob,200\n";
        let t = read_csv_bytes(csv.as_bytes()).unwrap();
        assert_eq!(t.headers, vec!["Name", "Amount"]);
        assert_eq!(t.row_count(), 2);
        assert_eq!(t.rows[1], vec!["Bob", "200"]);
    }

    #[test]
    fn csv_strips_bom_and_sniffs_semicolon() {
        let csv = "\u{FEFF}Ten;Tien\nA;1\nB;2\n";
        let t = read_csv_bytes(csv.as_bytes()).unwrap();
        assert_eq!(t.headers, vec!["Ten", "Tien"]);
        assert_eq!(t.rows[0], vec!["A", "1"]);
    }

    #[test]
    fn table_skips_blank_rows_and_pads_short_rows() {
        let rows = vec![
            vec!["A".to_string(), "B".to_string()],
            vec!["".to_string(), "".to_string()],
            vec!["x".to_string()],
        ];
        let t = table_from_rows(rows.into_iter()).unwrap();
        assert_eq!(t.row_count(), 1);
        assert_eq!(t.rows[0], vec!["x", ""]);
    }

    #[test]
    fn table_trims_trailing_empty_headers() {
        let rows = vec![
            vec!["A".into(), "B".into(), "".into(), "".into()],
            vec!["1".into(), "2".into(), "".into(), "".into()],
        ];
        let t = table_from_rows(rows.into_iter()).unwrap();
        assert_eq!(t.headers, vec!["A", "B"]);
        assert_eq!(t.rows[0], vec!["1", "2"]);
    }

    #[test]
    fn empty_source_is_an_error() {
        assert!(table_from_rows(std::iter::empty()).is_err());
    }

    #[test]
    fn row_lookup_is_trim_and_case_insensitive() {
        let t = table();
        let row = t.row(0).unwrap();
        // Trim + case fold only (diacritics must still match exactly).
        assert_eq!(row.get("  HỌ TÊN  "), Some("Nguyễn Văn A"));
        assert_eq!(row.get("SỐ TIỀN"), Some("1500000"));
        assert_eq!(row.get("ho ten"), None);
        assert_eq!(row.get("khong-co"), None);
    }

    #[test]
    fn filename_pattern_expands_and_sanitises() {
        let t = table();
        assert_eq!(
            expand_filename("HD {{Họ tên}} {{Ngày}}", &t.row(0).unwrap()),
            "HD Nguyễn Văn A 01_09_2026"
        );
        // Unknown field -> empty.
        assert_eq!(expand_filename("{{Ma}}-x", &t.row(0).unwrap()), "-x");
    }

    #[test]
    fn sanitize_strips_reserved_chars() {
        assert_eq!(sanitize_filename("a/b:c*?d"), "a_b_c__d");
        assert_eq!(sanitize_filename("  ..name..  "), "name");
    }

    #[test]
    fn unique_stem_disambiguates_case_insensitively() {
        let mut used = std::collections::HashSet::new();
        assert_eq!(unique_stem("An", &mut used), "An");
        assert_eq!(unique_stem("an", &mut used), "an (2)");
        assert_eq!(unique_stem("AN", &mut used), "AN (3)");
        assert_eq!(unique_stem("", &mut used), "tai-lieu");
    }

    #[test]
    fn reads_xlsx_fixture_with_date_and_number() {
        // Verifies the calamine path end to end: headers, an integer amount
        // (no trailing .0) and an Excel serial date formatted dd/mm/yyyy.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/mail_merge_sample.xlsx");
        let t = read_data_file(&path).unwrap();
        assert_eq!(t.headers, vec!["Họ tên", "Số tiền", "Ngày ký"]);
        assert_eq!(t.row_count(), 2);
        let row0 = t.row(0).unwrap();
        assert_eq!(row0.get("Họ tên"), Some("Nguyễn Văn A"));
        assert_eq!(row0.get("Số tiền"), Some("1500000"));
        assert_eq!(row0.get("Ngày ký"), Some("01/09/2026"));
        assert_eq!(t.row(1).unwrap().get("Ngày ký"), Some("25/12/2026"));
    }
}
