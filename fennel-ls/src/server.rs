/// tower-lsp backend — implements the LanguageServer trait and all LSP handlers.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use async_trait::async_trait;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analyzer::DefKind;
use crate::docs;
use crate::docs::Platform;
use crate::parser::{head_sym, Form};
use crate::text;
use crate::workspace::Workspace;

pub struct Backend {
    pub client: Client,
    pub workspace: Workspace,
    workspace_root: OnceLock<std::path::PathBuf>,
    /// Extra globals from `.fennel-ls.toml` that suppress unknown-identifier warnings.
    extra_globals: OnceLock<HashSet<String>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            workspace: Workspace::new(),
            workspace_root: OnceLock::new(),
            extra_globals: OnceLock::new(),
        }
    }

    fn is_known_global(&self, name: &str) -> bool {
        known_global(name)
            || self.extra_globals.get().map_or(false, |set| set.contains(name))
    }

    async fn publish_diagnostics(&self, uri: Url) {
        let builtins = self.workspace.builtins();
        let diags = self.workspace.with_file(&uri, |file| {
            let mut diags = Vec::new();

            // Parse errors
            for err in &file.parse_errors {
                diags.push(Diagnostic {
                    range: text::span_to_range(&file.text, &err.span),
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: err.message.clone(),
                    source: Some("fennel-ls".into()),
                    ..Default::default()
                });
            }

            // Semantic warnings (e.g. set on immutable binding, unused, arity)
            for warn in &file.analysis.warnings {
                diags.push(Diagnostic {
                    range: text::span_to_range(&file.text, &warn.span),
                    severity: Some(DiagnosticSeverity::WARNING),
                    message: warn.message.clone(),
                    source: Some("fennel-ls".into()),
                    ..Default::default()
                });
            }

            // Undefined symbols (not in scope and not a builtin)
            for sym in &file.analysis.syms {
                if sym.is_def {
                    continue;
                }
                if sym.def_byte.is_none() && !builtins.is_known(&sym.name) {
                    let root = sym.name.split(['.', ':']).find(|s| !s.is_empty()).unwrap_or(&sym.name);
                    if !self.is_known_global(root) {
                        diags.push(Diagnostic {
                            range: text::span_to_range(&file.text, &sym.span),
                            severity: Some(DiagnosticSeverity::WARNING),
                            message: format!("unknown identifier `{}`", sym.name),
                            source: Some("fennel-ls".into()),
                            ..Default::default()
                        });
                    }
                }
            }

            diags
        });

        if let Some(diags) = diags {
            self.client
                .publish_diagnostics(uri, diags, None)
                .await;
        }
    }
}

#[async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Store workspace root for require resolution and config loading
        let root = params.root_uri
            .as_ref()
            .and_then(|u| u.to_file_path().ok())
            .or_else(|| {
                params.workspace_folders.as_ref()
                    .and_then(|fs| fs.first())
                    .and_then(|f| f.uri.to_file_path().ok())
            });
        if let Some(path) = root {
            let config = crate::config::Config::load(&path);

            let platform = config.platform.as_deref().and_then(platform_from_str);
            if let Some(p) = platform {
                self.workspace.configure_platform(p);
            }

            if let Some(globals) = config.known_globals {
                let _ = self.extra_globals.set(globals.into_iter().collect());
            }

            let _ = self.workspace_root.set(path);
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "fennel-ls".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec![
                        "(".into(), "[".into(), "{".into(),
                        ".".into(), ":".into(),
                    ]),
                    ..Default::default()
                }),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: vec![
                                    SemanticTokenType::FUNCTION,   // 0
                                    SemanticTokenType::PARAMETER,  // 1
                                    SemanticTokenType::VARIABLE,   // 2
                                    SemanticTokenType::MACRO,      // 3
                                ],
                                token_modifiers: vec![
                                    SemanticTokenModifier::DEFINITION, // bit 0
                                    SemanticTokenModifier::READONLY,   // bit 1
                                ],
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: None,
                            work_done_progress_options: Default::default(),
                        },
                    ),
                ),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "fennel-ls initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    // ── Text synchronization ──────────────────────────────────────────────────

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        self.workspace
            .update(doc.uri.clone(), doc.text, doc.version);
        self.publish_diagnostics(doc.uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;

        let current = self.workspace.with_file(&uri, |f| f.text.clone());
        let text = apply_incremental_changes(
            current.unwrap_or_default(),
            params.content_changes,
        );

        self.workspace.update(uri.clone(), text, version);
        self.publish_diagnostics(uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.publish_diagnostics(params.text_document.uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.workspace.remove(&params.text_document.uri);
        self.client
            .publish_diagnostics(params.text_document.uri, vec![], None)
            .await;
    }

    // ── Hover ─────────────────────────────────────────────────────────────────

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let result = self.workspace.with_file(uri, |file| {
            let byte = match text::position_to_byte(&file.text, pos) {
                Some(b) => b as u32,
                None => return None,
            };

            let sym = file.analysis.symbol_at(byte)?;
            let name = sym.name.clone();
            let span = sym.span.clone();
            let range = text::span_to_range(&file.text, &span);

            // User-defined definition
            if let Some(def_byte) = sym.def_byte {
                if let Some(def) = file.analysis.defs.get(&def_byte) {
                    let md = format_definition(def);
                    return Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: md,
                        }),
                        range: Some(range),
                    });
                }
            }

            // Built-in doc (multisym stripping handled inside get())
            if let Some(doc) = self.workspace.builtins().get(&name) {
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!(
                            "```fennel\n{}\n```\n\n{}",
                            doc.signature, doc.doc
                        ),
                    }),
                    range: Some(range),
                });
            }

            None
        });

        Ok(result.flatten())
    }

    // ── Go to definition ──────────────────────────────────────────────────────

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        // Symbol go-to-def (existing)
        let result = self.workspace.with_file(uri, |file| {
            let byte = text::position_to_byte(&file.text, pos)? as u32;
            let sym = file.analysis.symbol_at(byte)?;
            let def_byte = sym.def_byte?;
            let def = file.analysis.defs.get(&def_byte)?;

            Some(GotoDefinitionResponse::Scalar(Location {
                uri: file.uri.clone(),
                range: text::span_to_range(&file.text, &def.span),
            }))
        });

        if let Some(r) = result.flatten() {
            return Ok(Some(r));
        }

        // Require string go-to-def: cursor on the module arg of (require :mod)
        if let Some(root) = self.workspace_root.get() {
            let require_result = self.workspace.with_file(uri, |file| {
                let byte = text::position_to_byte(&file.text, pos)? as u32;
                let module = require_module_at(byte, &file.ast)?;
                let path = resolve_require(&module, root)?;
                let file_uri = Url::from_file_path(&path).ok()?;
                Some(GotoDefinitionResponse::Scalar(Location {
                    uri: file_uri,
                    range: Range::default(),
                }))
            });
            if let Some(r) = require_result.flatten() {
                return Ok(Some(r));
            }
        }

        Ok(None)
    }

    // ── Find references ───────────────────────────────────────────────────────

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let result = self.workspace.with_file(uri, |file| {
            let byte = text::position_to_byte(&file.text, pos)? as u32;
            let sym = file.analysis.symbol_at(byte)?;

            // Find the definition byte (either this is a def, or follow the ref)
            let target_def = if sym.is_def {
                sym.span.start
            } else {
                sym.def_byte?
            };

            let mut locs: Vec<Location> = file
                .analysis
                .syms
                .iter()
                .filter(|s| {
                    (s.is_def && s.span.start == target_def)
                        || (!s.is_def && s.def_byte == Some(target_def))
                })
                .map(|s| Location {
                    uri: file.uri.clone(),
                    range: text::span_to_range(&file.text, &s.span),
                })
                .collect();

            if params.context.include_declaration {
                // Already included above
            }

            locs.sort_by_key(|l| (l.range.start.line, l.range.start.character));
            Some(locs)
        });

        Ok(result.flatten())
    }

    // ── Document highlight ─────────────────────────────────────────────────────

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let result = self.workspace.with_file(uri, |file| {
            let byte = text::position_to_byte(&file.text, pos)? as u32;
            let sym = file.analysis.symbol_at(byte)?;

            let target_def = if sym.is_def {
                sym.span.start
            } else {
                sym.def_byte?
            };

            let highlights: Vec<DocumentHighlight> = file
                .analysis
                .syms
                .iter()
                .filter(|s| {
                    (s.is_def && s.span.start == target_def)
                        || (!s.is_def && s.def_byte == Some(target_def))
                })
                .map(|s| DocumentHighlight {
                    range: text::span_to_range(&file.text, &s.span),
                    kind: if s.is_def {
                        Some(DocumentHighlightKind::WRITE)
                    } else {
                        Some(DocumentHighlightKind::READ)
                    },
                })
                .collect();

            Some(highlights)
        });

        Ok(result.flatten())
    }

    // ── Document symbols ──────────────────────────────────────────────────────

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;

        let result = self.workspace.with_file(uri, |file| {
            let syms: Vec<SymbolInformation> = file
                .analysis
                .defs
                .values()
                .map(|def| {
                    #[allow(deprecated)]
                    SymbolInformation {
                        name: def.name.clone(),
                        kind: def_kind_to_symbol_kind(&def.kind),
                        tags: None,
                        deprecated: None,
                        location: Location {
                            uri: file.uri.clone(),
                            range: text::span_to_range(&file.text, &def.span),
                        },
                        container_name: None,
                    }
                })
                .collect();

            DocumentSymbolResponse::Flat(syms)
        });

        Ok(result)
    }

    // ── Completion ────────────────────────────────────────────────────────────

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let result = self.workspace.with_file(uri, |file| {
            let byte = text::position_to_byte(&file.text, pos).unwrap_or(0) as u32;

            let mut items: Vec<CompletionItem> = Vec::new();
            let mut seen = std::collections::HashSet::new();

            // Scope-local definitions
            for def in file.analysis.defs_at(byte) {
                if seen.insert(def.name.clone()) {
                    items.push(CompletionItem {
                        label: def.name.clone(),
                        kind: Some(match def.kind {
                            DefKind::Fn | DefKind::Macro => CompletionItemKind::FUNCTION,
                            DefKind::Param => CompletionItemKind::VARIABLE,
                            DefKind::LoopVar | DefKind::Destructured => {
                                CompletionItemKind::VARIABLE
                            }
                            _ => CompletionItemKind::VARIABLE,
                        }),
                        detail: def.params.as_ref().map(|p| {
                            format!("fn [{}]", p.join(" "))
                        }),
                        documentation: def.doc.as_ref().map(|d| {
                            Documentation::MarkupContent(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: d.clone(),
                            })
                        }),
                        ..Default::default()
                    });
                }
            }

            // Built-in symbols
            for (name, doc) in self.workspace.builtins().iter() {
                if seen.insert(name.to_string()) {
                    items.push(CompletionItem {
                        label: name.to_string(),
                        kind: Some(match doc.kind {
                            docs::BuiltinKind::Function => CompletionItemKind::FUNCTION,
                            docs::BuiltinKind::Macro | docs::BuiltinKind::SpecialForm => {
                                CompletionItemKind::KEYWORD
                            }
                            docs::BuiltinKind::Value => CompletionItemKind::MODULE,
                        }),
                        detail: Some(doc.signature.to_string()),
                        documentation: Some(Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: doc.doc.to_string(),
                        })),
                        ..Default::default()
                    });
                }
            }

            items.sort_by(|a, b| a.label.cmp(&b.label));
            CompletionResponse::Array(items)
        });

        Ok(result)
    }

    // ── Rename ────────────────────────────────────────────────────────────────

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let new_name = &params.new_name;

        let result = self.workspace.with_file(uri, |file| {
            let byte = text::position_to_byte(&file.text, pos)? as u32;
            let sym = file.analysis.symbol_at(byte)?;

            let target_def = if sym.is_def {
                sym.span.start
            } else {
                sym.def_byte?
            };

            let edits: Vec<TextEdit> = file
                .analysis
                .syms
                .iter()
                .filter(|s| {
                    (s.is_def && s.span.start == target_def)
                        || (!s.is_def && s.def_byte == Some(target_def))
                })
                .map(|s| TextEdit {
                    range: text::span_to_range(&file.text, &s.span),
                    new_text: new_name.clone(),
                })
                .collect();

            let mut changes = HashMap::new();
            changes.insert(file.uri.clone(), edits);

            Some(WorkspaceEdit {
                changes: Some(changes),
                ..Default::default()
            })
        });

        Ok(result.flatten())
    }

    // ── Code actions ──────────────────────────────────────────────────────────

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = &params.text_document.uri;
        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        for diag in &params.context.diagnostics {
            // `var → local` quickfix for "never mutated" warnings
            if diag.message.contains("never mutated") {
                let action = self.workspace.with_file(uri, |file| {
                    let name_byte = text::position_to_byte(&file.text, diag.range.start)?;
                    let var_byte = find_var_keyword_before(&file.text, name_byte)?;
                    let var_pos = text::byte_to_position(&file.text, var_byte);
                    let var_end = text::byte_to_position(&file.text, var_byte + 3);
                    let mut changes = HashMap::new();
                    changes.insert(
                        file.uri.clone(),
                        vec![TextEdit {
                            range: Range { start: var_pos, end: var_end },
                            new_text: "local".into(),
                        }],
                    );
                    Some(CodeActionOrCommand::CodeAction(CodeAction {
                        title: "Change `var` to `local`".into(),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: Some(vec![diag.clone()]),
                        edit: Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                        ..Default::default()
                    }))
                });
                if let Some(Some(a)) = action {
                    actions.push(a);
                }
            }

            // Unknown-identifier stub: offer to insert `(local name nil)` above
            if let Some(name) = diag.message
                .strip_prefix("unknown identifier `")
                .and_then(|s| s.strip_suffix('`'))
            {
                let insert_pos = Position {
                    line: diag.range.start.line,
                    character: 0,
                };
                let mut changes = HashMap::new();
                changes.insert(
                    uri.clone(),
                    vec![TextEdit {
                        range: Range { start: insert_pos, end: insert_pos },
                        new_text: format!("(local {} nil)\n", name),
                    }],
                );
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Add `(local {} nil)`", name),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diag.clone()]),
                    edit: Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                    ..Default::default()
                }));
            }
        }

        Ok(if actions.is_empty() { None } else { Some(actions) })
    }

    // ── Semantic tokens ───────────────────────────────────────────────────────

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;
        let result = self.workspace.with_file(uri, |file| {
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: build_semantic_tokens(&file.analysis),
            })
        });
        Ok(result)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Map a platform string from config to the Platform enum.
fn platform_from_str(s: &str) -> Option<Platform> {
    match s {
        "lua51" => Some(Platform::Lua51),
        "lua52" => Some(Platform::Lua52),
        "lua53" => Some(Platform::Lua53),
        "lua54" => Some(Platform::Lua54),
        "luajit" => Some(Platform::LuaJIT),
        "luau" => Some(Platform::Luau),
        _ => None,
    }
}

/// Apply a list of (possibly incremental) LSP content-change events to `text`
/// in sequence. Each change's positions are relative to the text after all
/// previous changes have been applied.
pub fn apply_incremental_changes(
    mut text: String,
    changes: Vec<TextDocumentContentChangeEvent>,
) -> String {
    for change in changes {
        if let Some(range) = change.range {
            let start = text::position_to_byte(&text, range.start).unwrap_or(0);
            let end = text::position_to_byte(&text, range.end).unwrap_or(text.len());
            text.replace_range(start..end, &change.text);
        } else {
            // No range = full replacement
            text = change.text;
        }
    }
    text
}

/// Build the delta-encoded semantic token data from an analysis result.
/// Token types (index into legend):
///   0 = function, 1 = parameter, 2 = variable, 3 = macro
/// Token modifier bits:
///   0 = definition, 1 = readonly
pub fn build_semantic_tokens(analysis: &crate::analyzer::AnalysisResult) -> Vec<SemanticToken> {
    let mut data: Vec<SemanticToken> = Vec::new();
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;

    for sym in &analysis.syms {
        let def = if sym.is_def {
            analysis.defs.get(&sym.span.start)
        } else {
            sym.def_byte.and_then(|b| analysis.defs.get(&b))
        };

        let Some(def) = def else { continue };

        let token_type: u32 = match def.kind {
            DefKind::Fn => 0,
            DefKind::Param => 1,
            DefKind::Macro => 3,
            _ => 2, // variable
        };
        let mut token_modifiers: u32 = 0;
        if sym.is_def {
            token_modifiers |= 1; // definition
        }
        if matches!(def.kind, DefKind::Local | DefKind::Fn | DefKind::Param | DefKind::Macro) {
            token_modifiers |= 2; // readonly
        }

        let line = sym.span.line as u32;
        let start = sym.span.col as u32;

        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 { start - prev_start } else { start };

        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: sym.name.len() as u32,
            token_type,
            token_modifiers_bitset: token_modifiers,
        });

        prev_line = line;
        prev_start = start;
    }

    data
}

fn format_definition(def: &crate::analyzer::DefinitionInfo) -> String {
    use crate::analyzer::DefKind;
    let kind_str = match def.kind {
        DefKind::Fn | DefKind::Macro => "fn",
        DefKind::Local => "local",
        DefKind::Var => "var",
        DefKind::Global => "global",
        DefKind::Param => "param",
        DefKind::LoopVar => "loop-var",
        DefKind::Destructured => "let",
    };

    let sig = if let Some(params) = &def.params {
        format!(
            "```fennel\n({} {} [{}])\n```",
            kind_str,
            def.name,
            params.join(" ")
        )
    } else {
        format!("```fennel\n({} {})\n```", kind_str, def.name)
    };

    if let Some(doc) = &def.doc {
        format!("{}\n\n{}", sig, doc)
    } else {
        sig
    }
}

fn def_kind_to_symbol_kind(kind: &DefKind) -> SymbolKind {
    match kind {
        DefKind::Fn => SymbolKind::FUNCTION,
        DefKind::Macro => SymbolKind::OPERATOR,
        DefKind::Local | DefKind::Var | DefKind::Destructured => SymbolKind::VARIABLE,
        DefKind::Global => SymbolKind::VARIABLE,
        DefKind::Param => SymbolKind::VARIABLE,
        DefKind::LoopVar => SymbolKind::VARIABLE,
    }
}

/// Find the byte offset of the `var` keyword immediately before `name_byte`.
/// Returns `None` if the text before the name doesn't end with `var`.
fn find_var_keyword_before(text: &str, name_byte: usize) -> Option<usize> {
    let prefix = text.get(..name_byte)?;
    let trimmed = prefix.trim_end_matches(|c: char| c.is_whitespace());
    if !trimmed.ends_with("var") {
        return None;
    }
    let var_start = trimmed.len() - 3;
    // `var` must be preceded by a non-identifier character (e.g. `(` or space)
    let ok = trimmed[..var_start].chars().last()
        .map_or(true, |c| !c.is_alphanumeric() && c != '_' && c != '-');
    if ok { Some(var_start) } else { None }
}

/// If `byte` falls inside the module-name argument of a `(require …)` form,
/// return that module name string.
fn require_module_at(byte: u32, ast: &[crate::parser::AstNode]) -> Option<String> {
    for node in ast {
        if let Form::List(forms) = &node.node {
            if head_sym(forms) == Some("require") && forms.len() >= 2 {
                let arg = &forms[1];
                if arg.span.start <= byte && byte <= arg.span.end {
                    return match &arg.node {
                        Form::Keyword(s) | Form::Str(s) => Some(s.clone()),
                        _ => None,
                    };
                }
            }
        }
    }
    None
}

/// Resolve a Fennel module name (e.g. `"my.util"`) to a `.fnl` file path
/// relative to `root`, following Fennel's standard search convention.
fn resolve_require(module: &str, root: &std::path::Path) -> Option<std::path::PathBuf> {
    let rel = module.replace('.', "/");
    for suffix in &[".fnl", "/init.fnl"] {
        let candidate = root.join(format!("{rel}{suffix}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Common Lua globals not in our doc table that we shouldn't warn about.
pub fn known_global(name: &str) -> bool {
    matches!(
        name,
        "_G" | "_VERSION"
            | "arg"
            | "bit"
            | "bit32"
            | "debug"
            | "jit"
            | "package"
            | "utf8"
            | "ffi"
            | "love"
            | "vim"
            | "hs"
            | "mp"
            // Fennel runtime symbols
            | "fennel"
            | "___repl___"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::{DefKind, DefinitionInfo};
    use crate::lexer::Span;

    fn dummy_span() -> Span {
        Span { start: 0, end: 1, line: 0, col: 0, end_line: 0, end_col: 1 }
    }

    fn make_def(
        name: &str,
        kind: DefKind,
        params: Option<Vec<String>>,
        doc: Option<String>,
    ) -> DefinitionInfo {
        DefinitionInfo { name: name.into(), kind, span: dummy_span(), params, doc, variadic: false }
    }

    // ── format_definition ─────────────────────────────────────────────────────

    #[test]
    fn format_def_fn_with_params_and_doc() {
        let def = make_def(
            "add", DefKind::Fn,
            Some(vec!["a".into(), "b".into()]),
            Some("Add two numbers.".into()),
        );
        let s = format_definition(&def);
        assert!(s.contains("(fn add [a b])"), "signature missing: {s}");
        assert!(s.contains("Add two numbers."), "doc missing: {s}");
        assert!(s.contains("```fennel"), "no fennel code block: {s}");
        assert!(s.contains("\n\n"), "doc separator must be present: {s}");
    }

    #[test]
    fn format_def_fn_with_params_no_doc() {
        let def = make_def("f", DefKind::Fn, Some(vec!["x".into()]), None);
        let s = format_definition(&def);
        assert!(s.contains("(fn f [x])"), "signature: {s}");
        assert!(!s.contains("\n\n"), "no doc separator without doc: {s}");
    }

    #[test]
    fn format_def_fn_empty_params() {
        let def = make_def("thunk", DefKind::Fn, Some(vec![]), None);
        let s = format_definition(&def);
        assert!(s.contains("(fn thunk [])"), "empty param list: {s}");
    }

    #[test]
    fn format_def_no_params_variant() {
        let def = make_def("x", DefKind::Local, None, None);
        let s = format_definition(&def);
        assert!(s.contains("(local x)"), "no-param form: {s}");
        assert!(!s.contains('['), "no bracket when params is None: {s}");
    }

    #[test]
    fn format_def_macro_uses_fn_prefix() {
        let def = make_def("my-mac", DefKind::Macro, Some(vec!["form".into()]), None);
        let s = format_definition(&def);
        assert!(s.contains("(fn my-mac [form])"), "macro signature: {s}");
    }

    #[test]
    fn format_def_kind_str_variants() {
        let cases = [
            (DefKind::Local,       "local"),
            (DefKind::Var,         "var"),
            (DefKind::Global,      "global"),
            (DefKind::Param,       "param"),
            (DefKind::LoopVar,     "loop-var"),
            (DefKind::Destructured,"let"),
        ];
        for (kind, expected) in cases {
            let def = make_def("x", kind, None, None);
            let s = format_definition(&def);
            assert!(s.contains(expected), "kind_str for {expected}: {s}");
        }
    }

    #[test]
    fn format_def_output_is_fennel_markdown_block() {
        let def = make_def("f", DefKind::Local, None, None);
        let s = format_definition(&def);
        assert!(s.starts_with("```fennel\n"), "must open with ```fennel: {s}");
        assert!(s.contains("\n```"), "must close code block: {s}");
    }

    #[test]
    fn format_def_fn_with_doc_and_no_params() {
        let def = make_def("anon", DefKind::Fn, None, Some("docs".into()));
        let s = format_definition(&def);
        assert!(s.contains("(fn anon)"), "anonymous fn: {s}");
        assert!(s.contains("docs"), "doc present: {s}");
    }

    // ── def_kind_to_symbol_kind ───────────────────────────────────────────────

    #[test]
    fn symbol_kind_fn_is_function() {
        assert_eq!(def_kind_to_symbol_kind(&DefKind::Fn), SymbolKind::FUNCTION);
    }

    #[test]
    fn symbol_kind_macro_is_operator() {
        assert_eq!(def_kind_to_symbol_kind(&DefKind::Macro), SymbolKind::OPERATOR);
    }

    #[test]
    fn symbol_kind_local_var_destructured_are_variable() {
        for kind in [DefKind::Local, DefKind::Var, DefKind::Destructured] {
            assert_eq!(
                def_kind_to_symbol_kind(&kind), SymbolKind::VARIABLE,
                "{kind:?} should be VARIABLE"
            );
        }
    }

    #[test]
    fn symbol_kind_global_param_loopvar_are_variable() {
        for kind in [DefKind::Global, DefKind::Param, DefKind::LoopVar] {
            assert_eq!(
                def_kind_to_symbol_kind(&kind), SymbolKind::VARIABLE,
                "{kind:?} should be VARIABLE"
            );
        }
    }

    // ── known_global ──────────────────────────────────────────────────────────

    #[test]
    fn known_global_project_specific_frameworks() {
        assert!(known_global("love"), "LÖVE game framework");
        assert!(known_global("vim"),  "Neovim Lua API");
        assert!(known_global("hs"),   "Hammerspoon");
        assert!(known_global("mp"),   "mpv scripting");
    }

    #[test]
    fn known_global_lua54_builtins_covered_by_docs_too() {
        assert!(known_global("_G"));
        assert!(known_global("_VERSION"));
        assert!(known_global("arg"));
        assert!(known_global("debug"));
        assert!(known_global("package"));
        assert!(known_global("fennel"));
        assert!(known_global("___repl___"));
        assert!(known_global("utf8"));
    }

    #[test]
    fn known_global_platform_specific_not_in_lua54_default() {
        assert!(known_global("bit"));
        assert!(known_global("bit32"));
        assert!(known_global("jit"));
        assert!(known_global("ffi"));
    }

    #[test]
    fn known_global_unknown_names_false() {
        assert!(!known_global("my_custom_lib"));
        assert!(!known_global("undefined_xyz"));
        assert!(!known_global(""));
        assert!(!known_global("Love")); // case-sensitive
    }

    #[test]
    fn as_arrow_is_known_builtin() {
        // `as->` is a standard Fennel threading macro; using it must not
        // produce an "unknown identifier" diagnostic.
        let ws = crate::workspace::Workspace::default();
        let builtins = ws.builtins();
        assert!(builtins.is_known("as->"),  "as-> missing from builtin set");
        // Confirm the peer threading macros are still present
        assert!(builtins.is_known("->"),    "-> must be known");
        assert!(builtins.is_known("->>"),   "->> must be known");
        assert!(builtins.is_known("-?>"),   "-?> must be known");
        assert!(builtins.is_known("-?>>"),  "-?>> must be known");
    }

    // ── find_var_keyword_before ───────────────────────────────────────────────

    #[test]
    fn find_var_before_finds_var() {
        let text = "(var x 1)";
        assert_eq!(find_var_keyword_before(text, 5), Some(1));
    }

    #[test]
    fn find_var_before_with_extra_whitespace() {
        let text = "(var  x 1)";
        assert_eq!(find_var_keyword_before(text, 6), Some(1));
    }

    #[test]
    fn find_var_before_not_found_for_local() {
        let text = "(local x 1)";
        assert_eq!(find_var_keyword_before(text, 7), None);
    }

    #[test]
    fn find_var_before_not_found_when_identifier() {
        let text = "myvar x";
        assert_eq!(find_var_keyword_before(text, 6), None);
    }

    #[test]
    fn find_var_before_at_start_of_string() {
        assert_eq!(find_var_keyword_before("x", 0), None);
    }

    // ── require_module_at ─────────────────────────────────────────────────────

    fn parse_ast(src: &str) -> Vec<crate::parser::AstNode> {
        crate::parser::Parser::parse(src).0
    }

    #[test]
    fn require_module_at_keyword_arg() {
        let src = "(require :my.mod)";
        let ast = parse_ast(src);
        assert_eq!(require_module_at(10, &ast), Some("my.mod".into()));
    }

    #[test]
    fn require_module_at_string_arg() {
        let src = r#"(require "my.mod")"#;
        let ast = parse_ast(src);
        assert_eq!(require_module_at(10, &ast), Some("my.mod".into()));
    }

    #[test]
    fn require_module_at_wrong_byte_returns_none() {
        let src = "(require :my.mod)";
        let ast = parse_ast(src);
        assert_eq!(require_module_at(0, &ast), None);
    }

    #[test]
    fn require_module_at_not_a_require_returns_none() {
        let src = "(print :hello)";
        let ast = parse_ast(src);
        assert_eq!(require_module_at(8, &ast), None);
    }

    // ── apply_incremental_changes ─────────────────────────────────────────────

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn change(start: Position, end: Position, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(Range { start, end }),
            range_length: None,
            text: text.to_string(),
        }
    }

    fn full_change(text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.to_string(),
        }
    }

    #[test]
    fn incremental_single_char_insert() {
        let result = apply_incremental_changes(
            "hello world".into(),
            vec![change(pos(0, 5), pos(0, 5), ",")],
        );
        assert_eq!(result, "hello, world");
    }

    #[test]
    fn incremental_replace_word() {
        let result = apply_incremental_changes(
            "hello world".into(),
            vec![change(pos(0, 6), pos(0, 11), "Rust")],
        );
        assert_eq!(result, "hello Rust");
    }

    #[test]
    fn incremental_delete_range() {
        let result = apply_incremental_changes(
            "hello world".into(),
            vec![change(pos(0, 5), pos(0, 11), "")],
        );
        assert_eq!(result, "hello");
    }

    #[test]
    fn incremental_multiline_edit() {
        let result = apply_incremental_changes(
            "line1\nline2\nline3".into(),
            vec![change(pos(1, 0), pos(1, 5), "replaced")],
        );
        assert_eq!(result, "line1\nreplaced\nline3");
    }

    #[test]
    fn incremental_sequential_edits_applied_in_order() {
        // First insert a comma, then replace "world" in the result
        let result = apply_incremental_changes(
            "hello world".into(),
            vec![
                change(pos(0, 5), pos(0, 5), ","),   // "hello, world"
                change(pos(0, 7), pos(0, 12), "Rust"), // "hello, Rust"
            ],
        );
        assert_eq!(result, "hello, Rust");
    }

    #[test]
    fn incremental_full_replacement_when_no_range() {
        let result = apply_incremental_changes(
            "old content".into(),
            vec![full_change("new content")],
        );
        assert_eq!(result, "new content");
    }

    // ── build_semantic_tokens ─────────────────────────────────────────────────

    fn analyze(src: &str) -> crate::analyzer::AnalysisResult {
        let (ast, _) = crate::parser::Parser::parse(src);
        crate::analyzer::analyze(&ast)
    }

    #[test]
    fn semantic_tokens_fn_def_produces_function_token() {
        let analysis = analyze("(fn add [a b] (+ a b))");
        let tokens = build_semantic_tokens(&analysis);
        // `add` is a Fn definition → token_type 0 (function), modifier has definition bit
        let fn_tok = tokens.iter().find(|t| t.token_type == 0 && t.token_modifiers_bitset & 1 != 0);
        assert!(fn_tok.is_some(), "expected a function definition token: {tokens:?}");
    }

    #[test]
    fn semantic_tokens_param_produces_parameter_token() {
        let analysis = analyze("(fn f [x] x)");
        let tokens = build_semantic_tokens(&analysis);
        let param_tok = tokens.iter().find(|t| t.token_type == 1);
        assert!(param_tok.is_some(), "expected a parameter token: {tokens:?}");
    }

    #[test]
    fn semantic_tokens_local_produces_variable_token() {
        let analysis = analyze("(local x 1) x");
        let tokens = build_semantic_tokens(&analysis);
        let var_tok = tokens.iter().find(|t| t.token_type == 2);
        assert!(var_tok.is_some(), "expected a variable token: {tokens:?}");
    }

    #[test]
    fn semantic_tokens_unresolved_refs_skipped() {
        // `undefined` has no def_byte, so it should produce no token
        let analysis = analyze("undefined");
        let tokens = build_semantic_tokens(&analysis);
        assert!(tokens.is_empty(), "unresolved ref should produce no token: {tokens:?}");
    }

    #[test]
    fn semantic_tokens_delta_encoding_is_cumulative() {
        // Two defs on different lines; delta_line of second must be 1, not line number
        let analysis = analyze("(local a 1)\n(local b 2)");
        let tokens = build_semantic_tokens(&analysis);
        assert!(tokens.len() >= 2, "need at least two tokens");
        assert_eq!(tokens[0].delta_line, 0, "first token on line 0");
        assert_eq!(tokens[1].delta_line, 1, "second token one line below first");
    }

    // ── platform_from_str ─────────────────────────────────────────────────────

    #[test]
    fn platform_from_str_all_variants() {
        assert!(matches!(platform_from_str("lua51"), Some(Platform::Lua51)));
        assert!(matches!(platform_from_str("lua52"), Some(Platform::Lua52)));
        assert!(matches!(platform_from_str("lua53"), Some(Platform::Lua53)));
        assert!(matches!(platform_from_str("lua54"), Some(Platform::Lua54)));
        assert!(matches!(platform_from_str("luajit"), Some(Platform::LuaJIT)));
        assert!(matches!(platform_from_str("luau"), Some(Platform::Luau)));
        assert!(platform_from_str("lua99").is_none());
        assert!(platform_from_str("").is_none());
    }
}
