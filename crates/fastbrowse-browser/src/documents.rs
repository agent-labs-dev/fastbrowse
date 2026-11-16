//! Downloaded containers are read in the killable helper, never extracted onto the host's filesystem.

use std::io::{Cursor, Read};

use calamine::{Reader as _, Xlsx};
use quick_xml::{Reader, events::Event};
use zip::ZipArchive;

use crate::helper::{DOCX_TEXT, XLSX_TEXT, ZIP_TEXT};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(crate) fn parse(request: &str, raw: &[u8], max_bytes: usize) -> Result<String> {
    let mut budget = Budget { remaining: max_bytes, output: max_bytes };
    match request {
        XLSX_TEXT => spreadsheet(raw, &mut budget),
        DOCX_TEXT => document(raw, &mut budget),
        ZIP_TEXT => archive(raw, &mut budget),
        _ => Err("unknown document format".into()),
    }
}

#[derive(Debug)]
pub(crate) struct LimitExceeded;
impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("document contents exceed max_download_bytes")
    }
}
impl std::error::Error for LimitExceeded {}

struct Budget {
    remaining: usize,
    output: usize,
}

impl Budget {
    // Compressed downloads can expand far beyond their wire size. The caller's download limit applies to the
    // complete decompressed contents too; exceeding it rejects the document rather than returning truncated text.
    fn open<'a>(&mut self, raw: &'a [u8]) -> Result<ZipArchive<Cursor<&'a [u8]>>> {
        let mut archive = ZipArchive::new(Cursor::new(raw))?;
        for index in 0..archive.len() {
            let size = usize::try_from(archive.by_index_raw(index)?.size())?;
            self.remaining = self.remaining.checked_sub(size).ok_or(LimitExceeded)?;
        }
        Ok(archive)
    }

    fn append(&self, out: &mut String, text: &str) -> Result<()> {
        if text.len() > self.output.saturating_sub(out.len()) {
            return Err(LimitExceeded.into());
        }
        out.push_str(text);
        Ok(())
    }
}

fn spreadsheet(raw: &[u8], budget: &mut Budget) -> Result<String> {
    budget.open(raw)?;
    let mut workbook = Xlsx::new(Cursor::new(raw))?;
    let mut out = String::new();
    for name in workbook.sheet_names().to_owned() {
        let mut cells = workbook.worksheet_cells_reader(&name)?;
        budget.append(&mut out, &format!("[Worksheet: {name}]\n"))?;
        // Dense worksheet ranges allocate every coordinate between two cells. A sparse file can span the whole
        // worksheet, so stream its stored cells and retain their coordinates without materializing empty space.
        while let Some(cell) = cells.next_cell()? {
            let (row, column) = cell.get_position();
            budget.append(
                &mut out,
                &format!(
                    "row {}, column {}: {}\n",
                    row + 1,
                    column + 1,
                    serde_json::to_string(&calamine::Data::from(cell.get_value().clone()).to_string())?
                ),
            )?;
        }
        budget.append(&mut out, "\n")?;
    }
    Ok(out)
}

fn word_text(xml: &str) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    let mut out = String::new();
    let mut text = false;
    loop {
        match reader.read_event()? {
            Event::Start(tag) if tag.local_name().as_ref() == "t" => text = true,
            Event::Text(value) if text => out.push_str(&quick_xml::escape::unescape(&value)?),
            Event::CData(value) if text => out.push_str(&value),
            Event::GeneralRef(value) if text => {
                out.push_str(&quick_xml::escape::unescape(&format!("&{};", &*value))?);
            }
            Event::End(tag) => match tag.local_name().as_ref() {
                "t" => text = false,
                "p" | "tr" => out.push('\n'),
                "tc" => out.push('\t'),
                _ => {}
            },
            Event::Empty(tag) => match tag.local_name().as_ref() {
                "tab" => out.push('\t'),
                "br" | "cr" => out.push('\n'),
                _ => {}
            },
            Event::Eof => return Ok(out),
            _ => {}
        }
    }
}

fn document(raw: &[u8], budget: &mut Budget) -> Result<String> {
    let mut archive = budget.open(raw)?;
    // The main part must exist: an arbitrary ZIP of XML is not a Word document.
    archive.by_name("word/document.xml")?;
    let names = archive.file_names().collect::<std::result::Result<Vec<_>, _>>()?;
    let names: Vec<String> = names
        .into_iter()
        .filter(|name| name.starts_with("word/") && name.ends_with(".xml"))
        .map(|name| name.into_owned())
        .collect();
    let mut out = String::new();
    for name in names {
        let mut xml = String::new();
        archive.by_name(&name)?.read_to_string(&mut xml)?;
        let text = word_text(&xml)?;
        if !text.trim().is_empty() {
            budget.append(&mut out, &format!("[Document part: {name}]\n{text}\n"))?;
        }
    }
    Ok(out)
}

fn archive(raw: &[u8], budget: &mut Budget) -> Result<String> {
    let mut archive = budget.open(raw)?;
    let mut out = String::new();
    for index in 0..archive.len() {
        let raw_file = archive.by_index_raw(index)?;
        if raw_file.is_dir() {
            continue;
        }
        let name = raw_file.name()?.into_owned();
        drop(raw_file);
        budget.append(&mut out, &format!("[Archive member: {name}]\n"))?;
        let text = (|| -> Result<String> {
            let mut file = archive.by_index(index)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let extension = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
            match extension.as_str() {
                "xlsx" => spreadsheet(&bytes, budget),
                "docx" => document(&bytes, budget),
                "pdf" => pdf_extract::extract_text_from_mem(&bytes).map_err(Into::into),
                // ZIP has no member MIME type. Read declared text formats rather than guessing from binary bytes.
                "txt" | "csv" | "tsv" | "json" | "xml" | "md" | "html" => {
                    std::str::from_utf8(&bytes).map(str::to_owned).map_err(Into::into)
                }
                _ => Err("unsupported member format".into()),
            }
        })();
        match text {
            Ok(text) => budget.append(&mut out, &text)?,
            Err(error) if error.is::<LimitExceeded>() => return Err(error),
            Err(_) => budget.append(&mut out, "[Member contents were not read: unsupported or invalid document.]")?,
        }
        budget.append(&mut out, "\n\n")?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in files {
            writer
                .start_file(*name, SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored))
                .unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn compressed_archives_are_refused_before_expanding_past_the_download_limit() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("large.txt", SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated))
            .unwrap();
        writer.write_all(&vec![b'x'; 1024 * 1024]).unwrap();
        let raw = writer.finish().unwrap().into_inner();
        assert!(raw.len() < 64 * 1024);
        assert!(parse(ZIP_TEXT, &raw, 64 * 1024).unwrap_err().to_string().contains("max_download_bytes"));
    }

    #[test]
    fn a_corrupt_member_does_not_discard_readable_members() {
        let mut raw = zip(&[("bad.txt", b"FAIL"), ("good.txt", b"Useful evidence")]);
        let offset = raw.windows(4).position(|bytes| bytes == b"FAIL").unwrap();
        raw[offset] = b'X';
        let text = parse(ZIP_TEXT, &raw, usize::MAX).unwrap();
        assert!(text.contains("[Archive member: bad.txt]\n[Member contents were not read"), "{text}");
        assert!(text.contains("Useful evidence"), "{text}");
    }

    #[test]
    fn word_runs_keep_spacing_tables_and_parts() {
        let raw = zip(&[
            ("word/document.xml", br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t xml:space="preserve">A </w:t></w:r><w:r><w:t>&amp; B</w:t><w:tab/><w:t>Total</w:t><w:br/><w:t>42</w:t></w:r></w:p></w:body></w:document>"#),
            ("word/header1.xml", b"<hdr><p><r><t>Quarterly report</t></r></p></hdr>"),
        ]);
        let text = parse(DOCX_TEXT, &raw, usize::MAX).unwrap();
        assert!(text.contains("A & B\tTotal\n42"), "{text}");
        assert!(text.contains("Quarterly report"));
        assert!(parse(DOCX_TEXT, &zip(&[("unrelated.xml", b"<t>Not Word</t>")]), usize::MAX).is_err());
    }

    #[test]
    fn archive_members_keep_names_and_state_what_was_unread_without_writing_paths() {
        let raw = zip(&[("../report.csv", b"Item,Amount\nKettle,34.50"), ("image.bin", b"\0\xff")]);
        let text = parse(ZIP_TEXT, &raw, usize::MAX).unwrap();
        assert!(text.contains("[Archive member: ../report.csv]\nItem,Amount\nKettle,34.50"));
        assert!(text.contains("[Archive member: image.bin]\n[Member contents were not read"));
        assert!(parse(ZIP_TEXT, b"invalid", usize::MAX).is_err());
    }

    fn workbook(sheet: &[u8]) -> Vec<u8> {
        zip(&[
            ("_rels/.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#),
            ("[Content_Types].xml", br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#),
            ("xl/workbook.xml", br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Revenue" sheetId="1" r:id="rId1"/></sheets></workbook>"#),
            ("xl/_rels/workbook.xml.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#),
            ("xl/worksheets/sheet1.xml", sheet),
        ])
    }

    #[test]
    fn workbook_keeps_sheet_identity_and_cell_coordinates() {
        let raw = workbook(br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="2"><c r="B2" t="inlineStr"><is><t>Total</t></is></c><c r="D2"><v>42.5</v></c></row></sheetData></worksheet>"#);
        let text = parse(XLSX_TEXT, &raw, usize::MAX).unwrap();
        assert!(text.contains("[Worksheet: Revenue]"));
        assert!(text.contains("row 2, column 2: \"Total\""), "{text}");
        assert!(text.contains("row 2, column 4: \"42.5\""), "{text}");
        assert!(parse(XLSX_TEXT, b"invalid", usize::MAX).is_err());
    }

    #[test]
    fn a_sparse_workbook_does_not_allocate_the_rectangle_between_cells() {
        let raw = workbook(br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:XFD1048576"/><sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="1048576"><c r="XFD1048576"><v>2</v></c></row></sheetData></worksheet>"#);
        let text = parse(XLSX_TEXT, &raw, 64 * 1024).unwrap();
        assert_eq!(text, "[Worksheet: Revenue]\nrow 1, column 1: \"1\"\nrow 1048576, column 16384: \"2\"\n\n");
    }
}
