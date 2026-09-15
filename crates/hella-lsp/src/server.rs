//! The LSP server: lifecycle, request/notification dispatch, and document
//! analysis plumbing. Uses `lsp-server`'s stdio `Connection` (the same
//! foundation as rust-analyzer).

use std::collections::HashMap;

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{Completion, DocumentSymbolRequest, Formatting, GotoDefinition, HoverRequest, RangeFormatting, Request as _, SignatureHelpRequest};
use lsp_types::{
    CompletionParams, CompletionResponse, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DocumentFormattingParams, DocumentRangeFormattingParams,
    DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, HoverParams, InitializeResult, OneOf, Position,
    PublishDiagnosticsParams, ServerCapabilities, ServerInfo, SignatureHelp,
    SignatureHelpOptions, SignatureHelpParams, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions,
};

use crate::analysis::Analysis;
use crate::diagnostics::diagnostics;
use crate::document::{position_to_offset, DocumentManager};
use crate::progress::{self, ProgressState};

const SERVER_NAME: &str = "hella-lsp";
const SERVER_VERSION: &str = "0.1.0";

/// Server state: open documents plus a cached symbol analysis per URI.
#[derive(Default)]
struct State {
    docs: DocumentManager,
    analysis: HashMap<String, Analysis>,
    /// Workspace roots (from `workspaceFolders`, falling back to `rootUri`).
    workspace_roots: Vec<std::path::PathBuf>,
    /// Server-initiated progress token lifecycle (`$/progress`).
    progress: ProgressState,
    /// Counter for server→client request ids (`hella/progress/<n>`).
    next_request_id: u64,
    /// Client advertised `completionItem/snippetSupport`: enables call
    /// snippets and `end`-closing block templates in completion.
    snippet_support: bool,
}

/// Blocking entry point: runs the LSP loop until `exit`.
pub fn run() -> i32 {
    let (connection, io_threads) = Connection::stdio();

    // ── Initialize handshake ─────────────────────────────────────────
    let (initialize_id, init_params) = match connection.initialize_start() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{SERVER_NAME}: initialize failed: {e}");
            return 1;
        }
    };
    let init: lsp_types::InitializeParams =
        serde_json::from_value(init_params).unwrap_or_default();

    let mut state = State::default();
    state.workspace_roots = workspace_roots(&init);
    state.snippet_support = init
        .capabilities
        .text_document
        .as_ref()
        .and_then(|t| t.completion.as_ref())
        .and_then(|c| c.completion_item.as_ref())
        .and_then(|i| i.snippet_support)
        .unwrap_or(false);

    let result = InitializeResult {
        capabilities: server_capabilities(),
        server_info: Some(ServerInfo {
            name: SERVER_NAME.into(),
            version: Some(SERVER_VERSION.into()),
        }),
    };
    if let Err(e) = connection
        .initialize_finish(initialize_id, serde_json::to_value(result).unwrap())
    {
        eprintln!("{SERVER_NAME}: initialize_finish failed: {e}");
        return 1;
    }

    // NOTE: `initialize_finish` already consumed the client's `initialized`
    // notification, so the progress handshake starts here rather than in
    // the notification dispatcher below.
    request_startup_progress(&connection, &mut state);

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if req.method == "shutdown" {
                    let resp = Response::new_ok(req.id.clone(), ());
                    let _ = connection.sender.send(resp.into());
                    continue;
                }
                if let Err(e) = handle_request(&connection, &mut state, req) {
                    eprintln!("{SERVER_NAME}: request handler error: {e}");
                }
            }
            Message::Notification(not) => {
                if not.method == "exit" {
                    break;
                }
                handle_notification(&connection, &mut state, not);
            }
            Message::Response(resp) => {
                handle_response(&connection, &mut state, resp);
            }
        }
    }

    // Drop the connection (and its writer-side channel sender) *before*
    // joining the IO threads: the writer thread only terminates once every
    // sender clone is gone, so keeping `connection` alive here would deadlock
    // the join and leave the process running after `exit`. Buffered messages
    // (e.g. final publishDiagnostics) are still flushed by the writer.
    drop(connection);
    let _ = io_threads.join();
    0
}

/// Advertised LSP capabilities.
fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(TextDocumentSyncKind::INCREMENTAL),
            will_save: None,
            will_save_wait_until: None,
            save: None,
        })),
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        completion_provider: Some(lsp_types::CompletionOptions {
            trigger_characters: Some(vec![".".into(), "_".into(), "$".into()]),
            ..Default::default()
        }),
        document_symbol_provider: Some(OneOf::Left(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        document_range_formatting_provider: Some(OneOf::Left(true)),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".into(), ",".into(), ")".into()]),
            retrigger_characters: Some(vec![",".into(), ")".into()]),
            work_done_progress_options: Default::default(),
        }),
        ..Default::default()
    }
}

fn handle_request(
    connection: &Connection,
    state: &mut State,
    req: Request,
) -> Result<(), Box<dyn std::error::Error>> {
    match req.method.as_str() {
        HoverRequest::METHOD => {
            let (id, params) = match extract::<HoverParams>(req, HoverRequest::METHOD) {
                Ok(v) => v,
                Err((id, msg)) => {
                    send_err(connection, id, msg);
                    return Ok(());
                }
            };
            let result = state
                .hover(&params)
                .map(|h| serde_json::to_value(h))
                .transpose()?
                .unwrap_or(serde_json::Value::Null);
            send_ok(connection, id, result);
        }
        GotoDefinition::METHOD => {
            let (id, params) = match extract::<GotoDefinitionParams>(req, GotoDefinition::METHOD) {
                Ok(v) => v,
                Err((id, msg)) => {
                    send_err(connection, id, msg);
                    return Ok(());
                }
            };
            let result = state
                .definition(&params)
                .map(|d| serde_json::to_value(d))
                .transpose()?
                .unwrap_or(serde_json::Value::Null);
            send_ok(connection, id, result);
        }
        Completion::METHOD => {
            let (id, params) = match extract::<CompletionParams>(req, Completion::METHOD) {
                Ok(v) => v,
                Err((id, msg)) => {
                    send_err(connection, id, msg);
                    return Ok(());
                }
            };
            let result = state
                .completions(&params)
                .map(|c| serde_json::to_value(c))
                .transpose()?
                .unwrap_or(serde_json::Value::Null);
            send_ok(connection, id, result);
        }
        DocumentSymbolRequest::METHOD => {
            let (id, params) = match extract::<DocumentSymbolParams>(req, DocumentSymbolRequest::METHOD)
            {
                Ok(v) => v,
                Err((id, msg)) => {
                    send_err(connection, id, msg);
                    return Ok(());
                }
            };
            let result = state
                .document_symbols(&params)
                .map(|d| serde_json::to_value(d))
                .transpose()?
                .unwrap_or(serde_json::Value::Null);
            send_ok(connection, id, result);
        }
        Formatting::METHOD => {
            let (id, params) = match extract::<DocumentFormattingParams>(req, Formatting::METHOD) {
                Ok(v) => v,
                Err((id, msg)) => {
                    send_err(connection, id, msg);
                    return Ok(());
                }
            };
            let result = state
                .format_document(&params)
                .map(|e| serde_json::to_value(e))
                .transpose()?
                .unwrap_or(serde_json::Value::Null);
            send_ok(connection, id, result);
        }
        RangeFormatting::METHOD => {
            let (id, params) =
                match extract::<DocumentRangeFormattingParams>(req, RangeFormatting::METHOD) {
                    Ok(v) => v,
                    Err((id, msg)) => {
                        send_err(connection, id, msg);
                        return Ok(());
                    }
                };
            let result = state
                .format_range(&params)
                .map(|e| serde_json::to_value(e))
                .transpose()?
                .unwrap_or(serde_json::Value::Null);
            send_ok(connection, id, result);
        }
        SignatureHelpRequest::METHOD => {
            let (id, params) =
                match extract::<SignatureHelpParams>(req, SignatureHelpRequest::METHOD) {
                    Ok(v) => v,
                    Err((id, msg)) => {
                        send_err(connection, id, msg);
                        return Ok(());
                    }
                };
            let result = state
                .signature_help(&params)
                .map(|s| serde_json::to_value(s))
                .transpose()?
                .unwrap_or(serde_json::Value::Null);
            send_ok(connection, id, result);
        }
        _ => {
            let resp = Response::new_err(
                req.id,
                lsp_server::ErrorCode::MethodNotFound as i32,
                format!("method not found: {}", req.method),
            );
            let _ = connection.sender.send(resp.into());
        }
    }
    Ok(())
}

/// Deserialize request params without consuming the request, so a malformed
/// payload can still be answered with `InvalidParams` (same `id`) instead
/// of hanging the client with silence.
fn extract<P: serde::de::DeserializeOwned>(
    req: Request,
    method: &str,
) -> Result<(RequestId, P), (RequestId, String)> {
    if req.method != method {
        let id = req.id.clone();
        return Err((id, format!("method mismatch: expected {method}")));
    }
    let id = req.id.clone();
    match serde_json::from_value(req.params) {
        Ok(params) => Ok((id, params)),
        Err(e) => Err((id, format!("invalid params for {method}: {e}"))),
    }
}

fn send_err(connection: &Connection, id: RequestId, msg: String) {
    let resp = Response::new_err(id, lsp_server::ErrorCode::InvalidParams as i32, msg);
    let _ = connection.sender.send(resp.into());
}

fn send_ok(connection: &Connection, id: RequestId, result: serde_json::Value) {
    let resp = Response::new_ok(id, result);
    let _ = connection.sender.send(resp.into());
}

/// Workspace roots for progress reporting: `workspaceFolders` first,
/// falling back to `rootUri`. Non-file URIs are skipped.
#[allow(deprecated)] // `rootUri` fallback for older clients
fn workspace_roots(init: &lsp_types::InitializeParams) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    if let Some(folders) = &init.workspace_folders {
        for f in folders {
            if let Some(p) = crate::document::uri_to_path(&f.uri) {
                roots.push(p);
            }
        }
    }
    if roots.is_empty() {
        if let Some(root) = &init.root_uri {
            if let Some(p) = crate::document::uri_to_path(root) {
                roots.push(p);
            }
        }
    }
    roots
}

/// Ask the client to create our progress token once, right after the
/// `initialized` handshake. The indexing report itself runs when the client
/// acknowledges the token (see `handle_response`).
fn request_startup_progress(connection: &Connection, state: &mut State) {
    if !matches!(state.progress, ProgressState::Idle) {
        return;
    }
    state.next_request_id += 1;
    let id = RequestId::from(format!("hella/progress/{}", state.next_request_id));
    let req = progress::create_request(id.clone());
    if connection.sender.send(req.into()).is_err() {
        return;
    }
    state.progress = ProgressState::Creating(id);
}

/// Complete the `window/workDoneProgress/create` handshake: on success run
/// the (fast, bounded) workspace scan wrapped in begin/report/end so the
/// editor shows a loading indicator that dismisses itself; on rejection
/// stay silent for the rest of the session.
fn handle_response(connection: &Connection, state: &mut State, resp: Response) {
    let creating_id = match &state.progress {
        ProgressState::Creating(id) => id.clone(),
        _ => return,
    };
    if resp.id != creating_id {
        return;
    }
    if resp.response_result.is_err() {
        state.progress = ProgressState::Unsupported;
        return;
    }
    state.progress = ProgressState::Ready;
    run_indexing_progress(connection, state);
}

/// `Indexing Hella workspace` progress around the startup `.hll` scan.
fn run_indexing_progress(connection: &Connection, state: &State) {
    if !state.progress.is_ready() {
        return;
    }
    let _ = connection.sender.send(
        progress::begin(
            "Indexing Hella workspace",
            Some("Scanning for Hella files"),
            Some(0),
        )
        .into(),
    );
    let count = progress::count_hella_files(&state.workspace_roots);
    let files = if count == 1 {
        "1 Hella file".to_string()
    } else {
        format!("{count} Hella files")
    };
    let _ = connection.sender.send(
        progress::report(Some(&format!("Found {files}")), Some(100)).into(),
    );
    let _ = connection.sender.send(
        progress::end(Some(&format!("Indexed {files}"))).into(),
    );
}

fn handle_notification(connection: &Connection, state: &mut State, not: Notification) {
    match not.method.as_str() {
        DidOpenTextDocument::METHOD => {
            let params: DidOpenTextDocumentParams = match serde_json::from_value(not.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{SERVER_NAME}: bad didOpen: {e}");
                    return;
                }
            };
            state.docs.open(
                params.text_document.uri.clone(),
                params.text_document.version,
                params.text_document.text,
            );
            state.analyze(&params.text_document.uri);
            publish(connection, state, &params.text_document.uri);
        }
        DidChangeTextDocument::METHOD => {
            let params: DidChangeTextDocumentParams = match serde_json::from_value(not.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{SERVER_NAME}: bad didChange: {e}");
                    return;
                }
            };
            state.docs.change(
                &params.text_document.uri,
                params.text_document.version,
                &params.content_changes,
            );
            state.analyze(&params.text_document.uri);
            publish(connection, state, &params.text_document.uri);
        }
        DidCloseTextDocument::METHOD => {
            let params: DidCloseTextDocumentParams = match serde_json::from_value(not.params) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{SERVER_NAME}: bad didClose: {e}");
                    return;
                }
            };
            // Clear any outstanding diagnostics.
            let clear = PublishDiagnosticsParams::new(
                params.text_document.uri.clone(),
                vec![],
                None,
            );
            let not = Notification::new(PublishDiagnostics::METHOD.to_string(), clear);
            let _ = connection.sender.send(not.into());
            state.docs.close(&params.text_document.uri);
            state.analysis.remove(params.text_document.uri.as_str());
        }
        // Ignored notifications (initialized, change configuration, etc.).
        _ => {}
    }
}

/// Run diagnostics for a document and publish them.
fn publish(connection: &Connection, state: &State, uri: &lsp_types::Uri) {
    let Some(doc) = state.docs.get(uri) else {
        return;
    };
    // Project-model context for import resolution + the `main`-file gate.
    // `None` (non-file URIs) keeps the legacy single-file behavior.
    let path = crate::document::uri_to_path(uri);
    let diags = diagnostics(&doc.text, path.as_deref());
    let params = PublishDiagnosticsParams::new(uri.clone(), diags, Some(doc.version));
    let not = Notification::new(PublishDiagnostics::METHOD.to_string(), params);
    let _ = connection.sender.send(not.into());
}
/// Best-effort re-parse for mid-typing buffers: closes unclosed `(` so a
/// half-typed call still yields top-level signatures. Returns `None` when
/// even the repaired buffer does not parse.
fn tolerant_parse(text: &str) -> Option<hella_compiler::ast::Program> {
    let mut depth = 0u32;
    let mut in_str = false;
    let mut escape = false;
    for b in text.bytes() {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' || b == b'\'' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' | b'\'' => in_str = true,
            b'(' => depth += 1,
            b')' if depth > 0 => depth -= 1,
            _ => {}
        }
    }
    let mut repaired = text.to_string();
    for _ in 0..depth.min(16) {
        repaired.push(')');
    }
    let out = hella_compiler::lexer::lex(&repaired);
    hella_compiler::parse::parse(out.tokens, repaired).ok()
}

impl State {
    /// Re-run analysis for a document, replacing the cached symbol table.
    fn analyze(&mut self, uri: &lsp_types::Uri) {
        let Some(doc) = self.docs.get(uri).cloned() else {
            return;
        };
        let mut analysis = Analysis::default();
        let out = hella_compiler::lexer::lex(&doc.text);
        if let Ok(prog) = hella_compiler::parse::parse(out.tokens, doc.text.clone()) {
            analysis = Analysis::from_program(&prog);
        }
        self.analysis.insert(uri.as_str().to_string(), analysis);
    }

    fn hover(&self, params: &HoverParams) -> Option<lsp_types::Hover> {
        let (doc, offset) = self.doc_at(
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        )?;
        let a = self.analysis.get(doc.uri.as_str())?;
        a.hover(&doc.text, offset)
    }

    fn definition(&self, params: &GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
        let (doc, offset) = self.doc_at(
            &params.text_document_position_params.text_document.uri,
            &params.text_document_position_params.position,
        )?;
        let a = self.analysis.get(doc.uri.as_str())?;
        a.definition(&doc.text, &doc.uri, offset)
            .map(GotoDefinitionResponse::Scalar)
    }

    fn completions(&self, params: &CompletionParams) -> Option<CompletionResponse> {
        let uri = &params.text_document_position.text_document.uri;
        let (doc, offset) = self.doc_at(
            uri,
            &params.text_document_position.position,
        )?;
        // Completion parses error-tolerantly on its own (mid-typing
        // buffers with an incomplete `receiver.|` rarely parse cleanly),
        // so it does not use the cached analysis. The document path drives
        // import-path completion and loading imported names.
        let path = crate::document::uri_to_path(uri);
        let items =
            Analysis::complete_with_path(&doc.text, path.as_deref(), offset, self.snippet_support);
        Some(CompletionResponse::Array(items))
    }

    fn document_symbols(&self, params: &DocumentSymbolParams) -> Option<DocumentSymbolResponse> {
        let doc = self.docs.get(&params.text_document.uri)?;
        let a = self.analysis.get(doc.uri.as_str())?;
        let symbols = a.document_symbols(&doc.text);
        Some(DocumentSymbolResponse::Nested(symbols))
    }

    fn format_document(&self, params: &DocumentFormattingParams) -> Option<Vec<lsp_types::TextEdit>> {
        let doc = self.docs.get(&params.text_document.uri)?;
        crate::formatting::format_document(&doc.text)
    }

    fn format_range(&self, params: &DocumentRangeFormattingParams) -> Option<Vec<lsp_types::TextEdit>> {
        let doc = self.docs.get(&params.text_document.uri)?;
        crate::formatting::format_range(&doc.text, &params.range)
    }

    fn signature_help(&self, params: &SignatureHelpParams) -> Option<SignatureHelp> {
        let uri = &params.text_document_position_params.text_document.uri;
        let (doc, offset) = self.doc_at(
            uri,
            &params.text_document_position_params.position,
        )?;
        // Prefer the cached analysis; fall back to a tolerant re-parse so
        // mid-typing buffers (unclosed parens/blocks) still resolve.
        if let Some(a) = self.analysis.get(doc.uri.as_str()) {
            if let Some(h) = crate::signatures::help(a, &doc.text, offset) {
                return Some(h);
            }
        }
        let out = hella_compiler::lexer::lex(&doc.text);
        let prog = hella_compiler::parse::parse(out.tokens, doc.text.clone())
            .ok()
            .or_else(|| tolerant_parse(&doc.text))?;
        let a = Analysis::from_program(&prog);
        crate::signatures::help(&a, &doc.text, offset)
    }

    fn doc_at(
        &self,
        uri: &lsp_types::Uri,
        pos: &Position,
    ) -> Option<(crate::document::Document, usize)> {
        let doc = self.docs.get(uri)?.clone();
        let offset = position_to_offset(&doc.text, pos);
        Some((doc, offset))
    }
}
