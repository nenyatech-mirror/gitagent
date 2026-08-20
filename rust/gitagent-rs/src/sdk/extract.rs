//! Document text extraction — lets the `read` tool and the file viewer handle
//! binary document formats:
//!   .docx / .xlsx / .pptx  — native (ZIP + XML strip, no external tools)
//!   .pdf                   — best-effort (deflate streams + Tj/TJ text ops)
//!   .doc / .rtf / .odt     — via macOS `textutil` when available
//! Returns None when no readable text could be recovered.

use std::io::Read;
use std::path::Path;

pub const DOC_EXTS: &[&str] = &["docx", "xlsx", "pptx", "pdf", "doc", "rtf", "odt"];

pub fn is_document(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| DOC_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Extract readable text from a document file. `bytes` are the file contents.
pub fn extract_text(path: &Path, bytes: &[u8]) -> Option<String> {
    let ext = path.extension().and_then(|e| e.to_str())?.to_lowercase();
    let text = match ext.as_str() {
        "docx" => zip_xml_text(bytes, &["word/document.xml"])?,
        "pptx" => {
            let names = zip_names(bytes, "ppt/slides/slide");
            zip_xml_text(bytes, &names.iter().map(String::as_str).collect::<Vec<_>>())?
        }
        "xlsx" => zip_xml_text(bytes, &["xl/sharedStrings.xml"])?,
        "pdf" => pdf_text(bytes)?,
        "doc" | "rtf" | "odt" => textutil(path)?,
        _ => return None,
    };
    let clean = tidy(&text);
    (!clean.trim().is_empty()).then_some(clean)
}

// ── OOXML (zip + xml) ───────────────────────────────────────────────────────

fn zip_archive(bytes: &[u8]) -> Option<zip::ZipArchive<std::io::Cursor<&[u8]>>> {
    zip::ZipArchive::new(std::io::Cursor::new(bytes)).ok()
}

/// Names of zip entries starting with a prefix (sorted) — e.g. slide xml files.
fn zip_names(bytes: &[u8], prefix: &str) -> Vec<String> {
    let Some(ar) = zip_archive(bytes) else { return Vec::new() };
    let mut names: Vec<String> = ar
        .file_names()
        .filter(|n| n.starts_with(prefix) && n.ends_with(".xml"))
        .map(String::from)
        .collect();
    names.sort();
    names
}

fn zip_xml_text(bytes: &[u8], entries: &[&str]) -> Option<String> {
    let mut ar = zip_archive(bytes)?;
    let mut out = String::new();
    for name in entries {
        let Ok(mut f) = ar.by_name(name) else { continue };
        let mut xml = String::new();
        if f.read_to_string(&mut xml).is_ok() {
            out.push_str(&xml_to_text(&xml));
            out.push('\n');
        }
    }
    (!out.trim().is_empty()).then_some(out)
}

/// Strip XML to text, keeping paragraph/row structure as newlines.
fn xml_to_text(xml: &str) -> String {
    let x = xml
        .replace("</w:p>", "\n")
        .replace("<w:br/>", "\n")
        .replace("<w:tab/>", "\t")
        .replace("</a:p>", "\n")
        .replace("</t>", "\n") // xlsx shared strings: one per line
        .replace("</si>", "");
    let mut out = String::new();
    let mut in_tag = false;
    for c in x.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

// ── PDF (best-effort) ───────────────────────────────────────────────────────

/// Extract text-op strings from a PDF: inflate its streams, then read `(...)`
/// strings inside BT…ET blocks. Works well for simple/text PDFs (incl. ours);
/// CID-encoded fonts may come out garbled — callers should sanity-check.
fn pdf_text(bytes: &[u8]) -> Option<String> {
    let mut out = String::new();
    collect_pdf_ops(bytes, &mut out); // uncompressed content streams
    let mut i = 0;
    while let Some(s) = find(bytes, b"stream", i) {
        let mut ds = s + 6;
        if bytes.get(ds) == Some(&b'\r') {
            ds += 1;
        }
        if bytes.get(ds) == Some(&b'\n') {
            ds += 1;
        }
        let Some(e) = find(bytes, b"endstream", ds) else { break };
        let mut inflated = Vec::new();
        if flate2::read::ZlibDecoder::new(&bytes[ds..e]).read_to_end(&mut inflated).is_ok() {
            collect_pdf_ops(&inflated, &mut out);
        }
        i = e + 9;
    }
    // Sanity: require a reasonable proportion of printable ASCII.
    let printable = out.chars().filter(|c| c.is_ascii_graphic() || *c == ' ' || *c == '\n').count();
    (!out.trim().is_empty() && printable * 10 >= out.chars().count() * 7).then_some(out)
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

/// Pull `(...)`-string arguments of text operators out of a content stream.
fn collect_pdf_ops(data: &[u8], out: &mut String) {
    let mut i = 0;
    let mut wrote = false;
    while i < data.len() {
        match data[i] {
            b'(' => {
                let mut s = String::new();
                i += 1;
                let mut depth = 1;
                while i < data.len() && depth > 0 {
                    match data[i] {
                        b'\\' if i + 1 < data.len() => {
                            let c = data[i + 1];
                            match c {
                                b'n' => s.push('\n'),
                                b't' => s.push('\t'),
                                b'(' | b')' | b'\\' => s.push(c as char),
                                _ => {}
                            }
                            i += 2;
                            continue;
                        }
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        c => s.push(c as char),
                    }
                    i += 1;
                }
                // Keep only strings that are followed (nearby) by a text op.
                let tail: &[u8] = &data[i.min(data.len())..(i + 12).min(data.len())];
                let is_text_op = find(tail, b"Tj", 0).is_some() || find(tail, b"TJ", 0).is_some() || find(tail, b"'", 0).is_some();
                if is_text_op && !s.trim().is_empty() {
                    out.push_str(&s);
                    out.push(' ');
                    wrote = true;
                }
            }
            b'E' if data.get(i + 1) == Some(&b'T') && wrote => {
                out.push('\n');
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
}

// ── legacy formats via macOS textutil ───────────────────────────────────────

fn textutil(path: &Path) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("textutil")
        .args(["-convert", "txt", "-stdout"])
        .arg(path)
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Collapse runs of blank lines and trailing spaces.
fn tidy(s: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in s.lines() {
        let t = line.trim_end();
        if t.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(t);
        out.push('\n');
    }
    out
}
