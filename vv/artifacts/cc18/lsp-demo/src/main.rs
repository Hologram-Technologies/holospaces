//! `lsp-demo` — a minimal but real Language Server (the `CC-18` authority).
//!
//! It speaks the Language Server Protocol over the standard stdio base-protocol
//! transport (the `Content-Length` framing of the LSP base protocol) and uses
//! `lsp-types` — rust-analyzer's own LSP type crate, the spec's authoritative
//! Rust embodiment — for its advertised `ServerCapabilities` and every message
//! body. It is single-threaded and does real, document-based language
//! intelligence over the opened source file: hover (the symbol under the
//! cursor), completion (the document's identifiers + keywords), go-to-definition
//! (the symbol's first occurrence), and diagnostics (a `TODO` marker). It runs
//! in the devcontainer OS; the workbench speaks LSP to it (ADR-012/015).

use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Read, StdinLock, Write};

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionResponse, Diagnostic,
    DiagnosticSeverity, Hover, HoverContents, HoverProviderCapability, Location, MarkupContent,
    MarkupKind, OneOf, Position, PublishDiagnosticsParams, Range, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use serde_json::{json, Value};

fn main() {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // The currently-open document (uri, text) — the source the workbench opened.
    let mut doc: Option<(Url, String)> = None;
    let mut shutting_down = false;

    while let Some(msg) = read_message(&mut reader) {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        match method {
            "initialize" => {
                let caps = ServerCapabilities {
                    text_document_sync: Some(TextDocumentSyncCapability::Kind(
                        TextDocumentSyncKind::FULL,
                    )),
                    hover_provider: Some(HoverProviderCapability::Simple(true)),
                    completion_provider: Some(CompletionOptions::default()),
                    definition_provider: Some(OneOf::Left(true)),
                    ..Default::default()
                };
                respond(
                    &mut out,
                    id,
                    json!({
                        "capabilities": caps,
                        "serverInfo": { "name": "lsp-demo", "version": "0.1.0" }
                    }),
                );
            }
            "initialized" => {} // notification — no reply
            "textDocument/didOpen" => {
                if let Some(td) = msg.pointer("/params/textDocument") {
                    if let (Some(uri), Some(text)) = (
                        td.get("uri").and_then(Value::as_str),
                        td.get("text").and_then(Value::as_str),
                    ) {
                        if let Ok(url) = Url::parse(uri) {
                            // Real diagnostics: flag a TODO in the opened source.
                            publish_diagnostics(&mut out, &url, text);
                            doc = Some((url, text.to_owned()));
                        }
                    }
                }
            }
            "textDocument/hover" => {
                let result = doc
                    .as_ref()
                    .and_then(|(_, text)| word_at(text, position(&msg)))
                    .map(|(word, line)| {
                        let hover = Hover {
                            contents: HoverContents::Markup(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: format!("`{word}` — symbol on line `{line}`"),
                            }),
                            range: None,
                        };
                        serde_json::to_value(hover).unwrap()
                    })
                    .unwrap_or(Value::Null);
                respond(&mut out, id, result);
            }
            "textDocument/completion" => {
                let items: Vec<CompletionItem> = doc
                    .as_ref()
                    .map(|(_, text)| completions(text))
                    .unwrap_or_default();
                respond(
                    &mut out,
                    id,
                    serde_json::to_value(CompletionResponse::Array(items)).unwrap(),
                );
            }
            "textDocument/definition" => {
                let result = doc
                    .as_ref()
                    .and_then(|(uri, text)| {
                        let (word, _) = word_at(text, position(&msg))?;
                        let range = first_occurrence(text, &word)?;
                        Some(serde_json::to_value(Location { uri: uri.clone(), range }).unwrap())
                    })
                    .unwrap_or(Value::Null);
                respond(&mut out, id, result);
            }
            "shutdown" => {
                shutting_down = true;
                respond(&mut out, id, Value::Null);
            }
            "exit" => break,
            _ => {
                if id.is_some() {
                    respond_error(&mut out, id, -32601, "method not found");
                }
            }
        }
    }
    let _ = shutting_down; // a clean shutdown→exit handshake was honoured
}

// `read_exact`/`read_line` come from `Read`/`BufRead`, imported above.

/// Read one LSP base-protocol message (`Content-Length` header + JSON body).
/// Returns `None` at end of stream.
fn read_message(reader: &mut BufReader<StdinLock<'static>>) -> Option<Value> {
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // EOF
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break; // end of headers
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().ok()?;
        }
    }
    if content_length == 0 {
        return None;
    }
    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// Write one LSP base-protocol message.
fn write_message<W: Write>(out: &mut W, value: &Value) {
    let body = serde_json::to_vec(value).expect("serialize");
    let _ = write!(out, "Content-Length: {}\r\n\r\n", body.len());
    let _ = out.write_all(&body);
    let _ = out.flush();
}

fn respond<W: Write>(out: &mut W, id: Option<Value>, result: Value) {
    if let Some(id) = id {
        write_message(out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }
}

fn respond_error<W: Write>(out: &mut W, id: Option<Value>, code: i64, message: &str) {
    if let Some(id) = id {
        write_message(
            out,
            &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        );
    }
}

fn publish_diagnostics<W: Write>(out: &mut W, uri: &Url, text: &str) {
    let mut diags = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        if let Some(col) = line.find("TODO") {
            let start = Position::new(line_no as u32, col as u32);
            let end = Position::new(line_no as u32, (col + 4) as u32);
            diags.push(Diagnostic {
                range: Range::new(start, end),
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("lsp-demo".to_owned()),
                message: "TODO found".to_owned(),
                ..Default::default()
            });
        }
    }
    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics: diags,
        version: None,
    };
    write_message(
        out,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": params
        }),
    );
}

/// The `Position` of a `textDocument/*` request.
fn position(msg: &Value) -> Position {
    let line = msg
        .pointer("/params/position/line")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let character = msg
        .pointer("/params/position/character")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    Position::new(line, character)
}

/// The identifier word under `pos`, plus its 0-based line number.
fn word_at(text: &str, pos: Position) -> Option<(String, u32)> {
    let line = text.lines().nth(pos.line as usize)?;
    let bytes = line.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let c = (pos.character as usize).min(bytes.len());
    let mut start = c;
    while start > 0 && is_word(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = c;
    while end < bytes.len() && is_word(bytes[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }
    Some((line[start..end].to_owned(), pos.line))
}

/// The document's distinct identifiers + a few language keywords, as completions.
fn completions(text: &str) -> Vec<CompletionItem> {
    let mut idents: BTreeSet<String> = BTreeSet::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            if cur.len() > 1 && cur.as_bytes()[0].is_ascii_alphabetic() {
                idents.insert(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    if cur.len() > 1 {
        idents.insert(cur);
    }
    let mut items: Vec<CompletionItem> = idents
        .into_iter()
        .map(|label| CompletionItem {
            kind: Some(CompletionItemKind::TEXT),
            label,
            ..Default::default()
        })
        .collect();
    for kw in ["fn", "let", "return"] {
        items.push(CompletionItem {
            kind: Some(CompletionItemKind::KEYWORD),
            label: kw.to_owned(),
            ..Default::default()
        });
    }
    items
}

/// The range of the first occurrence of `word` in `text` (go-to-definition).
fn first_occurrence(text: &str, word: &str) -> Option<Range> {
    for (line_no, line) in text.lines().enumerate() {
        if let Some(col) = line.find(word) {
            let start = Position::new(line_no as u32, col as u32);
            let end = Position::new(line_no as u32, (col + word.len()) as u32);
            return Some(Range::new(start, end));
        }
    }
    None
}
