//! Downloaded containers are read in the killable helper, never extracted onto the host's filesystem.

use std::io::{Cursor, Read};

use calamine::{Reader as _, Xlsx};
use quick_xml::{Reader, events::Event};
use zip::ZipArchive;

use crate::helper::{DOCX_TEXT, XLSX_TEXT, ZIP_TEXT};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(crate) fn parse(request: &str, raw: &[u8]) -> Result<String> {
    match request {
        XLSX_TEXT => spreadsheet(raw),
        DOCX_TEXT => document(raw),
        ZIP_TEXT => archive(raw),
        _ => Err("unknown document format".into()),
    }
}

fn spreadsheet(raw: &[u8]) -> Result<String> {
    let mut workbook = Xlsx::new(Cursor::new(raw))?;
    let mut out = String::new();
    for name in workbook.sheet_names().to_owned() {
        let range = workbook.worksheet_range(&name)?;
        out.push_str(&format!("[Worksheet: {name}]\n"));
        if let Some((first_row, first_column)) = range.start() {
            for (row, values) in range.rows().enumerate() {
                // Coordinates retain the association of empty cells and distinguish sheets with the same labels.
                out.push_str(&format!("row {} (first column {}): ", first_row as usize + row + 1, first_column + 1));
                out.push_str(&serde_json::to_string(&values.iter().map(ToString::to_string).collect::<Vec<_>>())?);
                out.push('\n');
            }
        }
        out.push('\n');
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

fn document(raw: &[u8]) -> Result<String> {
    let mut archive = ZipArchive::new(Cursor::new(raw))?;
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
            out.push_str(&format!("[Document part: {name}]\n{text}\n"));
        }
    }
    Ok(out)
}

fn archive(raw: &[u8]) -> Result<String> {
    let mut archive = ZipArchive::new(Cursor::new(raw))?;
    let mut out = String::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        if file.is_dir() {
            continue;
        }
        let name = file.name()?.into_owned();
        out.push_str(&format!("[Archive member: {name}]\n"));
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let extension = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
        let text = match extension.as_str() {
            "xlsx" => spreadsheet(&bytes),
            "docx" => document(&bytes),
            "pdf" => pdf_extract::extract_text_from_mem(&bytes).map_err(Into::into),
            // ZIP has no member MIME type. Read the formats declared by their filename, never guess that a binary
            // member is prose because some of its bytes happen to decode as UTF-8.
            "txt" | "csv" | "tsv" | "json" | "xml" | "md" | "html" => {
                std::str::from_utf8(&bytes).map(str::to_owned).map_err(Into::into)
            }
            _ => Err("unsupported member format".into()),
        };
        match text {
            Ok(text) => out.push_str(&text),
            Err(_) => out.push_str("[Member contents were not read: unsupported or invalid document.]"),
        }
        out.push_str("\n\n");
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
            writer.start_file(*name, SimpleFileOptions::default()).unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn word_runs_keep_spacing_tables_and_parts() {
        let raw = zip(&[
            ("word/document.xml", br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t xml:space="preserve">A </w:t></w:r><w:r><w:t>&amp; B</w:t><w:tab/><w:t>Total</w:t><w:br/><w:t>42</w:t></w:r></w:p></w:body></w:document>"#),
            ("word/header1.xml", b"<hdr><p><r><t>Quarterly report</t></r></p></hdr>"),
        ]);
        let text = document(&raw).unwrap();
        assert!(text.contains("A & B\tTotal\n42"), "{text}");
        assert!(text.contains("Quarterly report"));
        assert!(document(&zip(&[("unrelated.xml", b"<t>Not Word</t>")])).is_err());
    }

    #[test]
    fn archive_members_keep_names_and_state_what_was_unread_without_writing_paths() {
        let raw = zip(&[("../report.csv", b"Item,Amount\nKettle,34.50"), ("image.bin", b"\0\xff")]);
        let text = archive(&raw).unwrap();
        assert!(text.contains("[Archive member: ../report.csv]\nItem,Amount\nKettle,34.50"));
        assert!(text.contains("[Archive member: image.bin]\n[Member contents were not read"));
        assert!(archive(b"invalid").is_err());
    }

    #[test]
    fn workbook_keeps_sheet_identity_coordinates_and_empty_cells() {
        let raw = zip(&[
            ("_rels/.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#),
            ("[Content_Types].xml", br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#),
            ("xl/workbook.xml", br#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Revenue" sheetId="1" r:id="rId1"/></sheets></workbook>"#),
            ("xl/_rels/workbook.xml.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#),
            ("xl/worksheets/sheet1.xml", br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="2"><c r="B2" t="inlineStr"><is><t>Total</t></is></c><c r="D2"><v>42.5</v></c></row></sheetData></worksheet>"#),
        ]);
        let text = spreadsheet(&raw).unwrap();
        assert!(text.contains("[Worksheet: Revenue]"));
        assert!(text.contains("row 2 (first column 2): [\"Total\",\"\",\"42.5\"]"), "{text}");
        assert!(spreadsheet(b"invalid").is_err());
    }
}
