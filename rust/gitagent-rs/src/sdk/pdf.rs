//! Dependency-free PDF writer + the builtin `pdf` tool.
//!
//! Produces real multi-page PDFs using the base-14 Helvetica fonts (no font
//! embedding needed), with headings (#/##/###), bullets, word-wrap, and page
//! breaks. Text is encoded Latin-1 (non-representable chars become '?').

use crate::pi::message::ToolResult;
use crate::pi::tool::{AgentTool, ExecutionMode};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

const PAGE_W: f32 = 612.0;
const PAGE_H: f32 = 792.0;
const MARGIN: f32 = 56.0;

#[derive(Clone, Copy, PartialEq)]
enum Style {
    H1,
    H2,
    H3,
    Body,
    Bullet,
    Gap,
}

impl Style {
    fn size(self) -> f32 {
        match self {
            Style::H1 => 20.0,
            Style::H2 => 16.0,
            Style::H3 => 13.0,
            _ => 11.0,
        }
    }
    fn leading(self) -> f32 {
        match self {
            Style::H1 => 28.0,
            Style::H2 => 23.0,
            Style::H3 => 19.0,
            Style::Gap => 8.0,
            _ => 15.5,
        }
    }
    fn bold(self) -> bool {
        matches!(self, Style::H1 | Style::H2 | Style::H3)
    }
    fn wrap_cols(self) -> usize {
        // Approximate chars per line for Helvetica at each size within margins.
        match self {
            Style::H1 => 44,
            Style::H2 => 58,
            Style::H3 => 72,
            _ => 92,
        }
    }
}

/// Strip light markdown inline syntax (keep the text).
fn clean_inline(s: &str) -> String {
    s.replace("**", "").replace("__", "").replace('`', "")
}

/// Escape PDF string syntax and coerce to Latin-1.
fn pdf_escape(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        match ch {
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 256 => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

fn wrap(text: &str, cols: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > cols {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Parse content (light markdown) into styled, wrapped lines.
fn layout(content: &str) -> Vec<(Style, String)> {
    let mut out = Vec::new();
    for raw in content.lines() {
        let line = raw.trim_end();
        let (style, text) = if let Some(t) = line.strip_prefix("### ") {
            (Style::H3, t.to_string())
        } else if let Some(t) = line.strip_prefix("## ") {
            (Style::H2, t.to_string())
        } else if let Some(t) = line.strip_prefix("# ") {
            (Style::H1, t.to_string())
        } else if let Some(t) = line.trim_start().strip_prefix("- ").or_else(|| line.trim_start().strip_prefix("* ")) {
            (Style::Bullet, format!("\u{2022} {t}"))
        } else if line.trim().is_empty() {
            out.push((Style::Gap, String::new()));
            continue;
        } else {
            (Style::Body, line.to_string())
        };
        for wrapped in wrap(&clean_inline(&text), style.wrap_cols()) {
            out.push((style, wrapped));
        }
    }
    out
}

/// Build the PDF bytes from styled lines.
pub fn build_pdf(title: Option<&str>, content: &str) -> Vec<u8> {
    let mut lines = Vec::new();
    if let Some(t) = title {
        lines.push((Style::H1, t.to_string()));
        lines.push((Style::Gap, String::new()));
    }
    lines.extend(layout(content));

    // Paginate into content streams.
    let mut pages: Vec<String> = Vec::new();
    let mut stream = String::new();
    let mut y = PAGE_H - MARGIN;
    for (style, text) in &lines {
        if y - style.leading() < MARGIN {
            pages.push(std::mem::take(&mut stream));
            y = PAGE_H - MARGIN;
        }
        y -= style.leading();
        if *style != Style::Gap && !text.is_empty() {
            let font = if style.bold() { "F2" } else { "F1" };
            stream.push_str(&format!(
                "BT /{font} {} Tf 1 0 0 1 {MARGIN} {y:.1} Tm ({}) Tj ET\n",
                style.size(),
                pdf_escape(text)
            ));
        }
    }
    pages.push(stream);

    // Assemble objects: 1 catalog, 2 pages, 3 F1, 4 F2, then (content, page)*.
    let npages = pages.len();
    let kids: Vec<String> = (0..npages).map(|i| format!("{} 0 R", 6 + i * 2)).collect();
    let mut objects: Vec<String> = vec![
        "<</Type/Catalog/Pages 2 0 R>>".into(),
        format!("<</Type/Pages/Kids[{}]/Count {npages}>>", kids.join(" ")),
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>".into(),
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Bold/Encoding/WinAnsiEncoding>>".into(),
    ];
    for p in &pages {
        objects.push(format!("<</Length {}>>\nstream\n{p}endstream", p.len()));
        let content_id = objects.len(); // 1-based id of the stream just pushed
        objects.push(format!(
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 {PAGE_W} {PAGE_H}]/Resources<</Font<</F1 3 0 R/F2 4 0 R>>>>/Contents {content_id} 0 R>>"
        ));
    }

    // Serialize with a correct xref table.
    let mut out: Vec<u8> = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{obj}\nendobj\n", i + 1).as_bytes());
    }
    let xref_at = out.len();
    let mut xref = format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for off in &offsets {
        xref.push_str(&format!("{off:010} 00000 n \n"));
    }
    out.extend_from_slice(xref.as_bytes());
    out.extend_from_slice(
        format!(
            "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

// ── the builtin tool ────────────────────────────────────────────────────────

pub struct PdfTool {
    pub cwd: PathBuf,
}

fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
}

#[async_trait]
impl AgentTool for PdfTool {
    fn name(&self) -> &str {
        "pdf"
    }
    fn description(&self) -> &str {
        "Create a PDF document. Give it text or light markdown (#/##/### headings, - bullets, blank lines) and it writes a real multi-page PDF to `path`."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {
            "path": { "type": "string", "description": "Output file path, e.g. report.pdf" },
            "title": { "type": "string", "description": "Optional document title (rendered as a heading)" },
            "content": { "type": "string", "description": "The document text (light markdown supported)" }
        }, "required": ["path", "content"] })
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }
    async fn execute(&self, _id: &str, args: Value, _cancel: &CancellationToken) -> anyhow::Result<ToolResult> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or_default();
        let content = args.get("content").and_then(Value::as_str).unwrap_or_default();
        let title = args.get("title").and_then(Value::as_str);
        if path.is_empty() || content.is_empty() {
            return Ok(ToolResult::text("Error: 'path' and 'content' are required"));
        }
        let mut abs = resolve(&self.cwd, path);
        if abs.extension().and_then(|e| e.to_str()) != Some("pdf") {
            abs.set_extension("pdf");
        }
        let bytes = build_pdf(title, content);
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let n = bytes.len();
        tokio::fs::write(&abs, bytes).await?;
        Ok(ToolResult::text(format!("Wrote PDF ({n} bytes) to {}", abs.display())))
    }
}
