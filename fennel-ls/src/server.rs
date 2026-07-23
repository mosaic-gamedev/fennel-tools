/// tower-lsp backend — implements the LanguageServer trait and all LSP handlers.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use async_trait::async_trait;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::analyzer::DefKind;
use crate::config::GlobalDoc;
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
    /// Populated from `known_globals` plus roots inferred from `global_docs` keys.
    extra_globals: OnceLock<HashSet<String>>,
    /// Per-symbol hover docs loaded from `global_docs` (inline or via `include`).
    /// Keys are exact Fennel symbol names, e.g. `"MyLib.module.fn"`.
    global_docs: OnceLock<HashMap<String, GlobalDoc>>,
    /// Whether `textDocument/formatting` is enabled (disabled via `--no-formatting`).
    formatting_enabled: bool,
}

impl Backend {
    pub fn new(client: Client, formatting_enabled: bool) -> Self {
        Self {
            client,
            workspace: Workspace::new(),
            workspace_root: OnceLock::new(),
            extra_globals: OnceLock::new(),
            global_docs: OnceLock::new(),
            formatting_enabled,
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

            // Build extra_globals: explicit known_globals + roots inferred from
            // global_docs keys so callers don't have to list them twice.
            let mut all_globals: HashSet<String> = config.known_globals
                .unwrap_or_default()
                .into_iter()
                .collect();
            if let Some(docs) = &config.global_docs {
                for key in docs.keys() {
                    if let Some(root) = key.split(['.', ':']).find(|s| !s.is_empty()) {
                        all_globals.insert(root.to_string());
                    }
                }
            }
            if !all_globals.is_empty() {
                let _ = self.extra_globals.set(all_globals);
            }
            if let Some(docs) = config.global_docs {
                let _ = self.global_docs.set(docs);
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
                workspace_symbol_provider: Some(OneOf::Left(true)),
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
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".into(), " ".into()]),
                    retrigger_characters: None,
                    work_done_progress_options: Default::default(),
                }),
                inlay_hint_provider: Some(OneOf::Left(true)),
                document_formatting_provider: if self.formatting_enabled {
                    Some(OneOf::Left(true))
                } else {
                    None
                },
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
        self.workspace.update(
            doc.uri.clone(),
            doc.text,
            doc.version,
            self.workspace_root.get().map(|p| p.as_path()),
        );
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

        self.workspace.update(
            uri.clone(),
            text,
            version,
            self.workspace_root.get().map(|p| p.as_path()),
        );
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

            // Cross-file hover: dotted multisym where root is a module binding.
            // E.g. `utils.helper` → look up `helper` in the required `utils` module.
            // Checked before the local def so module members take priority over
            // the bare binding info for the root name.
            if let Some((mod_root, member)) = split_multisym(&name) {
                let member_root = member.split(['.', ':']).next().unwrap_or(member);
                if let Some(def) = file.modules.get(mod_root)
                    .and_then(|ex| ex.defs.get(member_root))
                {
                    return Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: format_definition(def),
                        }),
                        range: Some(range),
                    });
                }
            }

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

            // Custom global docs from `.fennel-ls.toml` / included files.
            // Try the full name first, then progressively strip trailing
            // members until a match is found or the chain is exhausted.
            // This lets a parent namespace entry serve as a fallback for
            // any child call with no specific entry.
            if let Some(docs) = self.global_docs.get() {
                let mut key: &str = &name;
                loop {
                    if let Some(doc) = docs.get(key) {
                        let value = match &doc.doc {
                            Some(d) => format!("```fennel\n{}\n```\n\n{}", doc.signature, d),
                            None    => format!("```fennel\n{}\n```", doc.signature),
                        };
                        return Some(Hover {
                            contents: HoverContents::Markup(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value,
                            }),
                            range: Some(range),
                        });
                    }
                    match key.rfind('.') {
                        Some(i) => key = &key[..i],
                        None    => break,
                    }
                }
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

        // Symbol go-to-def: cross-file module members take priority over root binding.
        let result = self.workspace.with_file(uri, |file| {
            let byte = text::position_to_byte(&file.text, pos)? as u32;
            let sym = file.analysis.symbol_at(byte)?;

            // Cross-file: `utils.helper` → jump to `helper` in utils.fnl
            if let Some((mod_root, member)) = split_multisym(&sym.name) {
                let member_root = member.split(['.', ':']).next().unwrap_or(member);
                if let Some(exports) = file.modules.get(mod_root) {
                    if let Some(def) = exports.defs.get(member_root) {
                        return Some(GotoDefinitionResponse::Scalar(Location {
                            uri: exports.uri.clone(),
                            range: crate::text::span_to_range(&exports.text, &def.span),
                        }));
                    }
                }
            }

            // Local def
            let def_byte = sym.def_byte?;
            let def = file.analysis.defs.get(&def_byte)?;
            Some(GotoDefinitionResponse::Scalar(Location {
                uri: file.uri.clone(),
                range: crate::text::span_to_range(&file.text, &def.span),
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
                let path = crate::workspace::resolve_require_path(&module, root)?;
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

        // Collect same-file refs AND derive the cross-file key in one pass.
        let result = self.workspace.with_file(uri, |file| {
            let byte = text::position_to_byte(&file.text, pos)? as u32;
            let sym = file.analysis.symbol_at(byte)?;

            // For cross-file multisyms (e.g. `utils.greet`), def_byte is None
            // because the symbol isn't defined locally.  Don't bail out early —
            // just skip the same-file search for those.
            let target_def: Option<u32> = if sym.is_def {
                Some(sym.span.start)
            } else {
                sym.def_byte
            };

            let same_file: Vec<Location> = match target_def {
                Some(def_start) => file
                    .analysis
                    .syms
                    .iter()
                    .filter(|s| {
                        (s.is_def && s.span.start == def_start)
                            || (!s.is_def && s.def_byte == Some(def_start))
                    })
                    .map(|s| Location {
                        uri: file.uri.clone(),
                        range: text::span_to_range(&file.text, &s.span),
                    })
                    .collect(),
                None => vec![],
            };

            // Determine (def_uri, def_name) for cross-file lookup.
            // If the cursor is on a def, the def is in this file.
            // If the cursor is on a cross-file ref (e.g. `utils.greet`), resolve
            // through file.modules to the exporting file.
            let cross_key: Option<(Url, String)> = if sym.is_def {
                Some((file.uri.clone(), sym.name.clone()))
            } else if let Some(sep) = sym.name.find(['.', ':']) {
                let root = &sym.name[..sep];
                let member = &sym.name[sep + 1..];
                let member_root = member.split(['.', ':']).next().unwrap_or(member);
                file.modules
                    .get(root)
                    .map(|ex| (ex.uri.clone(), member_root.to_string()))
            } else {
                None
            };

            if same_file.is_empty() && cross_key.is_none() {
                return None;
            }
            Some((same_file, cross_key))
        });

        let (mut locs, cross_key) = match result.flatten() {
            Some(v) => v,
            None => return Ok(None),
        };

        // Append cross-file refs from all open files.
        if let Some((def_uri, def_name)) = cross_key {
            for xref in self.workspace.cross_file_refs_of(&def_uri, &def_name) {
                locs.push(Location {
                    uri: xref.uri,
                    range: text::span_to_range(&xref.text, &xref.span),
                });
            }
        }

        locs.sort_by_key(|l| {
            (l.uri.to_string(), l.range.start.line, l.range.start.character)
        });
        Ok(if locs.is_empty() { None } else { Some(locs) })
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

    // ── Workspace symbols ─────────────────────────────────────────────────────

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let defs = self.workspace.all_defs(&params.query);
        if defs.is_empty() {
            return Ok(None);
        }
        let syms = defs
            .into_iter()
            .map(|(uri, file_text, def)| {
                #[allow(deprecated)]
                SymbolInformation {
                    name: def.name.clone(),
                    kind: def_kind_to_symbol_kind(&def.kind),
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri,
                        range: text::span_to_range(&file_text, &def.span),
                    },
                    container_name: None,
                }
            })
            .collect();
        Ok(Some(syms))
    }

    // ── Completion ────────────────────────────────────────────────────────────

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let result = self.workspace.with_file(uri, |file| {
            let byte = text::position_to_byte(&file.text, pos).unwrap_or(0) as u32;
            let multisym_prefix = multisym_prefix_at(&file.text, byte as usize);

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

            // Custom global docs, filtered to the current multisym prefix if any.
            if let Some(docs) = self.global_docs.get() {
                for (name, doc) in docs {
                    if let Some(ref pfx) = multisym_prefix {
                        if !name.starts_with(pfx.as_str()) { continue; }
                    }
                    if seen.insert(name.clone()) {
                        items.push(CompletionItem {
                            label: name.clone(),
                            kind: Some(CompletionItemKind::FUNCTION),
                            detail: Some(doc.signature.clone()),
                            documentation: doc.doc.as_ref().map(|d| {
                                Documentation::MarkupContent(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value: d.clone(),
                                })
                            }),
                            ..Default::default()
                        });
                    }
                }
            }

            // Module binding completions: when prefix is `binding.`, offer exports.
            // E.g. `(local utils (require :utils))` → typing `utils.` offers
            // all top-level defs from utils.fnl with labels like `utils.helper`.
            if let Some(ref pfx) = multisym_prefix {
                for (binding, exports) in &file.modules {
                    let expected_pfx = format!("{}.", binding);
                    if !pfx.starts_with(&expected_pfx) { continue; }
                    for (name, def) in &exports.defs {
                        let full_label = format!("{}.{}", binding, name);
                        if seen.insert(full_label.clone()) {
                            items.push(CompletionItem {
                                label: full_label,
                                kind: Some(match def.kind {
                                    DefKind::Fn | DefKind::Macro => CompletionItemKind::FUNCTION,
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

            let target_def: Option<u32> = if sym.is_def {
                Some(sym.span.start)
            } else {
                sym.def_byte
            };

            let same_file_edits: Vec<TextEdit> = match target_def {
                Some(def_start) => file
                    .analysis
                    .syms
                    .iter()
                    .filter(|s| {
                        (s.is_def && s.span.start == def_start)
                            || (!s.is_def && s.def_byte == Some(def_start))
                    })
                    .map(|s| TextEdit {
                        range: text::span_to_range(&file.text, &s.span),
                        new_text: new_name.clone(),
                    })
                    .collect(),
                None => vec![],
            };

            // Same cross-file key logic as references().
            let cross_key: Option<(Url, String)> = if sym.is_def {
                Some((file.uri.clone(), sym.name.clone()))
            } else if let Some(sep) = sym.name.find(['.', ':']) {
                let root = &sym.name[..sep];
                let member = &sym.name[sep + 1..];
                let member_root = member.split(['.', ':']).next().unwrap_or(member);
                file.modules
                    .get(root)
                    .map(|ex| (ex.uri.clone(), member_root.to_string()))
            } else {
                None
            };

            Some((file.uri.clone(), same_file_edits, cross_key))
        });

        let (file_uri, same_file_edits, cross_key) = match result.flatten() {
            Some(v) => v,
            None => return Ok(None),
        };

        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        changes.insert(file_uri, same_file_edits);

        // Cross-file: for each ref `binding_prefix.old_name`, rename the member.
        if let Some((def_uri, def_name)) = cross_key {
            for xref in self.workspace.cross_file_refs_of(&def_uri, &def_name) {
                // xref.sym_name is like "utils.greet" — preserve the prefix+sep.
                let new_sym = if let Some(sep_idx) = xref.sym_name.find(['.', ':']) {
                    let sep_char = &xref.sym_name[sep_idx..=sep_idx];
                    let prefix = &xref.sym_name[..sep_idx];
                    format!("{}{}{}", prefix, sep_char, new_name)
                } else {
                    new_name.clone()
                };
                let edit = TextEdit {
                    range: text::span_to_range(&xref.text, &xref.span),
                    new_text: new_sym,
                };
                changes.entry(xref.uri).or_default().push(edit);
            }
        }

        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
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

    // ── Folding ranges ────────────────────────────────────────────────────────

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        let uri = &params.text_document.uri;
        let result = self.workspace.with_file(uri, |file| {
            let mut ranges = Vec::new();
            for node in &file.ast {
                collect_folds(node, &file.text, &mut ranges);
            }
            ranges
        });
        Ok(result)
    }

    // ── Signature help ────────────────────────────────────────────────────────

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let result = self.workspace.with_file(uri, |file| {
            let byte = text::position_to_byte(&file.text, pos)? as u32;
            let (head_byte, arg_index) = enclosing_call(&file.ast, byte)?;
            let sym = file.analysis.symbol_at(head_byte)?;
            let def = file.analysis.defs.get(&sym.def_byte?)?;
            let params_list = def.params.as_ref()?;
            let label = format!("({} {})", def.name, params_list.join(" "));
            let parameters: Vec<ParameterInformation> = params_list
                .iter()
                .map(|p| ParameterInformation {
                    label: ParameterLabel::Simple(p.clone()),
                    documentation: None,
                })
                .collect();
            let active = arg_index.min(parameters.len().saturating_sub(1)) as u32;
            Some(SignatureHelp {
                signatures: vec![SignatureInformation {
                    label,
                    documentation: def.doc.as_ref().map(|d| {
                        Documentation::MarkupContent(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: d.clone(),
                        })
                    }),
                    parameters: Some(parameters),
                    active_parameter: Some(active),
                }],
                active_signature: Some(0),
                active_parameter: Some(active),
            })
        });
        Ok(result.flatten())
    }

    // ── Inlay hints ───────────────────────────────────────────────────────────

    async fn inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let uri = &params.text_document.uri;
        let result = self.workspace.with_file(uri, |file| {
            let mut hints = Vec::new();
            for node in &file.ast {
                collect_inlay_hints(node, &file.text, &file.analysis, &mut hints);
            }
            hints
        });
        Ok(result)
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

    // ── Formatting ────────────────────────────────────────────────────────────

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        if !self.formatting_enabled {
            return Ok(None);
        }
        let uri = &params.text_document.uri;
        let edits = self.workspace.with_file(uri, |file| {
            let formatted = crate::fmt::format(&file.text)?;
            if formatted == file.text {
                return Some(vec![]);
            }
            let end = text::byte_to_position(&file.text, file.text.len());
            Some(vec![TextEdit {
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end,
                },
                new_text: formatted,
            }])
        });
        Ok(edits.flatten())
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

/// Walk the AST and emit a FoldingRange for every multi-line list/sequence/table.
fn collect_folds(node: &crate::parser::AstNode, text: &str, out: &mut Vec<FoldingRange>) {
    let children: &[crate::parser::AstNode] = match &node.node {
        Form::List(c) | Form::Sequence(c) | Form::Table(c) => c,
        Form::Quote(inner) | Form::Quasiquote(inner)
        | Form::Unquote(inner) | Form::UnquoteSplice(inner)
        | Form::HashFn(inner) => {
            collect_folds(inner, text, out);
            return;
        }
        _ => return,
    };
    let start_line = text::byte_to_position(text, node.span.start as usize).line;
    let end_line   = text::byte_to_position(text, node.span.end   as usize).line;
    if end_line > start_line {
        out.push(FoldingRange {
            start_line,
            end_line,
            start_character: None,
            end_character: None,
            kind: Some(FoldingRangeKind::Region),
            collapsed_text: None,
        });
    }
    for child in children {
        collect_folds(child, text, out);
    }
}

/// Return the byte offset of the head symbol and the 0-based active-parameter
/// index for the innermost function-call list that contains `byte`.
fn enclosing_call(
    ast: &[crate::parser::AstNode],
    byte: u32,
) -> Option<(u32, usize)> {
    fn walk(
        node: &crate::parser::AstNode,
        byte: u32,
        best: &mut Option<(u32, usize, u32)>, // (head_byte, arg_idx, span_width)
    ) {
        if node.span.start > byte || node.span.end < byte {
            return;
        }
        if let Form::List(children) = &node.node {
            if let Some(head) = children.first() {
                if matches!(&head.node, Form::Symbol(_)) {
                    let arg_index = children[1..]
                        .iter()
                        .take_while(|c| c.span.end < byte)
                        .count();
                    let width = node.span.end - node.span.start;
                    match best {
                        None => *best = Some((head.span.start, arg_index, width)),
                        Some((_, _, bw)) if width < *bw => {
                            *best = Some((head.span.start, arg_index, width));
                        }
                        _ => {}
                    }
                }
            }
            for child in children {
                walk(child, byte, best);
            }
        }
        match &node.node {
            Form::Sequence(c) | Form::Table(c) => {
                for child in c { walk(child, byte, best); }
            }
            Form::Quote(i) | Form::Quasiquote(i)
            | Form::Unquote(i) | Form::UnquoteSplice(i)
            | Form::HashFn(i) => walk(i, byte, best),
            _ => {}
        }
    }
    let mut best = None;
    for node in ast {
        walk(node, byte, &mut best);
    }
    best.map(|(hb, ai, _)| (hb, ai))
}

/// Walk the AST and emit an inlay hint for each argument of every call to a
/// locally-defined function whose params are known.
fn collect_inlay_hints(
    node: &crate::parser::AstNode,
    text: &str,
    analysis: &crate::analyzer::AnalysisResult,
    out: &mut Vec<InlayHint>,
) {
    let children: &[crate::parser::AstNode] = match &node.node {
        Form::List(c) => c,
        Form::Sequence(c) | Form::Table(c) => {
            for child in c { collect_inlay_hints(child, text, analysis, out); }
            return;
        }
        Form::Quote(i) | Form::Quasiquote(i)
        | Form::Unquote(i) | Form::UnquoteSplice(i)
        | Form::HashFn(i) => {
            collect_inlay_hints(i, text, analysis, out);
            return;
        }
        _ => return,
    };
    // Try to resolve the head to a function with known params
    if let Some(head) = children.first() {
        if let Some(sym) = analysis.symbol_at(head.span.start) {
            if let Some(def_byte) = sym.def_byte {
                if let Some(def) = analysis.defs.get(&def_byte) {
                    if let Some(params) = &def.params {
                        for (i, arg) in children[1..].iter().enumerate() {
                            if let Some(param) = params.get(i) {
                                // Skip rest param and underscore params
                                if param.starts_with('&') || param.starts_with('_') {
                                    continue;
                                }
                                let pos = text::byte_to_position(text, arg.span.start as usize);
                                out.push(InlayHint {
                                    position: pos,
                                    label: InlayHintLabel::String(format!("{}:", param)),
                                    kind: Some(InlayHintKind::PARAMETER),
                                    text_edits: None,
                                    tooltip: None,
                                    padding_left: None,
                                    padding_right: Some(true),
                                    data: None,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    for child in children {
        collect_inlay_hints(child, text, analysis, out);
    }
}

/// Extract a multisym namespace prefix from the text immediately before `byte`.
/// e.g. if text before cursor is `Lib.module.` → returns `Some("Lib.module.")`
fn multisym_prefix_at(text: &str, byte: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = byte;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
            || b == b'?' || b == b'!' || b == b'.' || b == b':'
        {
            i -= 1;
        } else {
            break;
        }
    }
    let token = &text[i..byte];
    let last_sep = token.rfind(|c| c == '.' || c == ':')?;
    Some(token[..=last_sep].to_string())
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

/// Split `name` on the first `.` or `:` separator.
/// Returns `(root, rest)` or `None` if there is no separator.
fn split_multisym(name: &str) -> Option<(&str, &str)> {
    name.find(['.', ':']).map(|i| (&name[..i], &name[i + 1..]))
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

    // ── Completion (defs_at) ──────────────────────────────────────────────────

    fn completion_names_at(src: &str, byte: u32) -> Vec<String> {
        let analysis = analyze(src);
        analysis.defs_at(byte).into_iter().map(|d| d.name.clone()).collect()
    }

    #[test]
    fn completion_includes_locals_in_scope() {
        let src = "(local foo 1) (local bar 2) |";
        //                                             ^ byte = src.len()-1
        let byte = src.len() as u32 - 1;
        let names = completion_names_at(src, byte);
        assert!(names.contains(&"foo".to_string()), "foo must appear in completions");
        assert!(names.contains(&"bar".to_string()), "bar must appear in completions");
    }

    #[test]
    fn completion_includes_fn_params_inside_fn() {
        // Cursor is inside the fn body (after the opening of print call)
        let src = "(fn greet [name age] (print name))";
        // byte inside the body — just before the closing paren
        let byte = src.len() as u32 - 2;
        let names = completion_names_at(src, byte);
        assert!(names.contains(&"name".to_string()), "param `name` must be offered");
        assert!(names.contains(&"age".to_string()),  "param `age` must be offered");
    }

    #[test]
    fn completion_excludes_locals_not_yet_in_scope() {
        // `bar` is defined after the cursor position
        let src = "(local foo 1) | (local bar 2)";
        let byte = src.find('|').unwrap() as u32;
        let src = src.replace('|', " ");
        let names = completion_names_at(&src, byte);
        assert!(names.contains(&"foo".to_string()), "foo is before cursor — must appear");
        assert!(!names.contains(&"bar".to_string()), "bar is after cursor — must not appear");
    }

    #[test]
    fn completion_excludes_params_outside_fn() {
        let src = "(fn f [x] x) |";
        let byte = src.len() as u32 - 1;
        let names = completion_names_at(src, byte);
        assert!(!names.contains(&"x".to_string()), "param must not leak outside fn");
    }

    #[test]
    fn completion_includes_loop_var_inside_for() {
        let src = "(for [i 1 10] |)";
        let byte = src.find('|').unwrap() as u32;
        let src = src.replace('|', "i");
        let names = completion_names_at(&src, byte);
        assert!(names.contains(&"i".to_string()), "loop var must be offered inside for body");
    }

    #[test]
    fn completion_no_duplicates() {
        let src = "(local x 1) (local x 2) |";
        let byte = src.len() as u32 - 1;
        let names = completion_names_at(src, byte);
        let x_count = names.iter().filter(|n| n.as_str() == "x").count();
        assert_eq!(x_count, 1, "shadowed name must appear exactly once");
    }

    // ── multisym_prefix_at ────────────────────────────────────────────────────

    #[test]
    fn multisym_prefix_single_dot() {
        assert_eq!(multisym_prefix_at("(Lib.", 5), Some("Lib.".into()));
    }

    #[test]
    fn multisym_prefix_nested_dot() {
        assert_eq!(multisym_prefix_at("(Lib.mod.", 9), Some("Lib.mod.".into()));
    }

    #[test]
    fn multisym_prefix_colon_method() {
        assert_eq!(multisym_prefix_at("(obj:", 5), Some("obj:".into()));
    }

    #[test]
    fn multisym_prefix_partial_name_after_dot() {
        // "Lib.fo" — cursor mid-word after dot, prefix is still "Lib."
        assert_eq!(multisym_prefix_at("(Lib.fo", 7), Some("Lib.".into()));
    }

    #[test]
    fn multisym_prefix_no_separator_returns_none() {
        assert_eq!(multisym_prefix_at("(foo", 4), None);
    }

    #[test]
    fn multisym_prefix_empty_returns_none() {
        assert_eq!(multisym_prefix_at("", 0), None);
    }

    // ── collect_folds ─────────────────────────────────────────────────────────

    fn folds_for(src: &str) -> Vec<FoldingRange> {
        let (ast, _) = crate::parser::Parser::parse(src);
        let mut ranges = Vec::new();
        for node in &ast {
            collect_folds(node, src, &mut ranges);
        }
        ranges
    }

    #[test]
    fn folding_multiline_list_produces_fold() {
        let src = "(fn foo []\n  (+ 1 2))";
        let ranges = folds_for(src);
        assert_eq!(ranges.len(), 1, "one multiline list → one fold");
        assert_eq!(ranges[0].start_line, 0);
        assert_eq!(ranges[0].end_line, 1);
    }

    #[test]
    fn folding_single_line_no_fold() {
        let src = "(fn foo [] (+ 1 2))";
        assert!(folds_for(src).is_empty(), "single-line list must not fold");
    }

    #[test]
    fn folding_nested_multiline_both_fold() {
        let src = "(fn foo []\n  (let [x 1]\n    x))";
        let ranges = folds_for(src);
        assert_eq!(ranges.len(), 2, "outer fn and inner let both span multiple lines");
    }

    #[test]
    fn folding_sequence_folds_when_multiline() {
        let src = "(local x [\n  1\n  2])";
        let ranges = folds_for(src);
        assert!(ranges.iter().any(|r| r.start_line < r.end_line),
            "multiline sequence must produce a fold");
    }

    // ── enclosing_call ────────────────────────────────────────────────────────

    #[test]
    fn enclosing_call_finds_head_byte() {
        let src = "(foo 1 2)";
        let ast = parse_ast(src);
        // cursor anywhere inside the call
        let (head_byte, _) = enclosing_call(&ast, 5).expect("must find enclosing call");
        // head `foo` starts at byte 1
        assert_eq!(head_byte, 1);
    }

    #[test]
    fn enclosing_call_arg_index_first_arg() {
        let src = "(foo aaa bbb)";
        let ast = parse_ast(src);
        // cursor on `aaa` (byte 5)
        let (_, arg_idx) = enclosing_call(&ast, 5).unwrap();
        assert_eq!(arg_idx, 0, "cursor on first arg → active param 0");
    }

    #[test]
    fn enclosing_call_arg_index_second_arg() {
        let src = "(foo aaa bbb)";
        let ast = parse_ast(src);
        // cursor on `bbb` (byte 9)
        let (_, arg_idx) = enclosing_call(&ast, 9).unwrap();
        assert_eq!(arg_idx, 1, "cursor on second arg → active param 1");
    }

    #[test]
    fn enclosing_call_innermost_wins() {
        let src = "(outer (inner 1))";
        let ast = parse_ast(src);
        // cursor on `1` — should resolve to `inner`, not `outer`
        let (head_byte, _) = enclosing_call(&ast, 14).unwrap();
        // `inner` starts at byte 8
        assert_eq!(head_byte, 8);
    }

    #[test]
    fn enclosing_call_bare_symbol_returns_none() {
        let ast = parse_ast("foo");
        assert!(enclosing_call(&ast, 1).is_none());
    }

    // ── collect_inlay_hints ───────────────────────────────────────────────────

    fn inlay_hint_labels(src: &str) -> Vec<String> {
        let (ast, _) = crate::parser::Parser::parse(src);
        let analysis = crate::analyzer::analyze(&ast);
        let mut hints = Vec::new();
        for node in &ast {
            collect_inlay_hints(node, src, &analysis, &mut hints);
        }
        hints.into_iter().map(|h| match h.label {
            InlayHintLabel::String(s) => s,
            _ => String::new(),
        }).collect()
    }

    #[test]
    fn inlay_hints_emitted_for_known_fn() {
        let labels = inlay_hint_labels("(fn add [a b] (+ a b)) (add 1 2)");
        assert!(labels.contains(&"a:".to_string()));
        assert!(labels.contains(&"b:".to_string()));
    }

    #[test]
    fn inlay_hints_skip_rest_param() {
        let labels = inlay_hint_labels("(fn f [a & rest] a) (f 1 2 3)");
        assert!(labels.contains(&"a:".to_string()), "first named param gets a hint");
        assert!(!labels.iter().any(|l| l.contains('&')), "rest param must not emit a hint");
    }

    #[test]
    fn inlay_hints_skip_underscore_param() {
        let labels = inlay_hint_labels("(fn f [_ignored b] b) (f 1 2)");
        assert!(!labels.iter().any(|l| l.starts_with('_')), "underscore param must not emit hint");
        assert!(labels.contains(&"b:".to_string()), "named param after underscore still hints");
    }

    #[test]
    fn inlay_hints_unknown_fn_no_hints() {
        // Call to an unresolved function — no params known, no hints
        let labels = inlay_hint_labels("(unknown-fn 1 2 3)");
        assert!(labels.is_empty());
    }

    #[test]
    fn inlay_hints_nested_calls_both_hinted() {
        let src = "(fn add [a b] (+ a b)) (fn neg [x] (- x)) (add (neg 1) 2)";
        let labels = inlay_hint_labels(src);
        assert!(labels.contains(&"a:".to_string()));
        assert!(labels.contains(&"b:".to_string()));
        assert!(labels.contains(&"x:".to_string()));
    }

    // ── Cross-file require resolution (hover / goto-def / completion) ─────────
    //
    // These tests exercise the full pipeline: tempdir module file on disk →
    // workspace.update() with root → handler logic pulling from file.modules.

    /// Build a Workspace with `main_src` open as `file:///main.fnl`, resolving
    /// requires against `root` (a tempdir path that may contain module files).
    fn ws_with_require(root: &std::path::Path, main_src: &str) -> (Workspace, Url) {
        let ws = Workspace::new();
        let uri = Url::parse("file:///main.fnl").unwrap();
        ws.update(uri.clone(), main_src.to_string(), 1, Some(root));
        (ws, uri)
    }

    /// Return the hover markdown for the symbol at `(line, col)` in `uri`,
    /// using only the cross-file module-member path (not builtins / global_docs).
    fn module_hover_at(ws: &Workspace, uri: &Url, line: u32, col: u32) -> Option<String> {
        ws.with_file(uri, |file| {
            let byte = crate::text::position_to_byte(
                &file.text,
                tower_lsp::lsp_types::Position { line, character: col },
            )? as u32;
            let sym = file.analysis.symbol_at(byte)?;
            let (mod_root, member) = split_multisym(&sym.name)?;
            let member_root = member.split(['.', ':']).next().unwrap_or(member);
            let def = file.modules.get(mod_root)?.defs.get(member_root)?;
            Some(format_definition(def))
        }).flatten()
    }

    /// Goto-def: return `(target_uri_filename, start_col)` for module member.
    fn module_goto_def_at(ws: &Workspace, uri: &Url, line: u32, col: u32) -> Option<(String, u32)> {
        ws.with_file(uri, |file| {
            let byte = crate::text::position_to_byte(
                &file.text,
                tower_lsp::lsp_types::Position { line, character: col },
            )? as u32;
            let sym = file.analysis.symbol_at(byte)?;
            let (mod_root, member) = split_multisym(&sym.name)?;
            let member_root = member.split(['.', ':']).next().unwrap_or(member);
            let exports = file.modules.get(mod_root)?;
            let def = exports.defs.get(member_root)?;
            let range = crate::text::span_to_range(&exports.text, &def.span);
            let filename = exports.uri.path().split('/').last().unwrap_or("").to_string();
            Some((filename, range.start.character))
        }).flatten()
    }

    /// Completion: labels from module exports for a given multisym prefix.
    fn module_completion_labels(ws: &Workspace, uri: &Url, line: u32, col: u32) -> Vec<String> {
        ws.with_file(uri, |file| {
            let byte = crate::text::position_to_byte(
                &file.text,
                tower_lsp::lsp_types::Position { line, character: col },
            ).unwrap_or(0) as u32;
            let pfx = multisym_prefix_at(&file.text, byte as usize)?;
            let mut labels = Vec::new();
            for (binding, exports) in &file.modules {
                let expected = format!("{}.", binding);
                if !pfx.starts_with(&expected) { continue; }
                for name in exports.defs.keys() {
                    labels.push(format!("{}.{}", binding, name));
                }
            }
            labels.sort();
            Some(labels)
        }).flatten().unwrap_or_default()
    }

    #[test]
    fn module_hover_shows_member_signature_and_doc() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("utils.fnl"),
            r#"(fn greet [name] "Say hello." (.. "Hello, " name))"#).unwrap();
        // line 0: "(local utils (require :utils))"
        // line 1: "(utils.greet "world")"
        //          0123456789012 — "utils.greet" starts at col 1
        let src = "(local utils (require :utils))\n(utils.greet \"world\")";
        let (ws, uri) = ws_with_require(dir.path(), src);
        let hover = module_hover_at(&ws, &uri, 1, 5).unwrap();
        assert!(hover.contains("fn greet [name]"), "signature should be in hover: {hover}");
        assert!(hover.contains("Say hello."), "docstring should be in hover: {hover}");
    }

    #[test]
    fn module_hover_none_for_non_module_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let src = "(local x 42) x";
        let (ws, uri) = ws_with_require(dir.path(), src);
        // `x` has no `.` separator — split_multisym returns None
        let hover = module_hover_at(&ws, &uri, 0, 13);
        assert!(hover.is_none());
    }

    #[test]
    fn module_goto_def_jumps_to_module_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("geo.fnl"),
            "(fn make-vec [x y] {:x x :y y})").unwrap();
        // line 0: "(local geo (require :geo))"
        // line 1: "(geo.make-vec 1 2)"
        //          01234567890123
        let src = "(local geo (require :geo))\n(geo.make-vec 1 2)";
        let (ws, uri) = ws_with_require(dir.path(), src);
        let (filename, _col) = module_goto_def_at(&ws, &uri, 1, 5).unwrap();
        assert_eq!(filename, "geo.fnl", "should jump to geo.fnl, got: {filename}");
    }

    #[test]
    fn module_goto_def_col_matches_def_position() {
        let dir = tempfile::tempdir().unwrap();
        // `helper` starts at col 4: "(fn helper [x] x)"
        //                             0123456789
        std::fs::write(dir.path().join("lib.fnl"), "(fn helper [x] x)").unwrap();
        let src = "(local lib (require :lib))\n(lib.helper 42)";
        let (ws, uri) = ws_with_require(dir.path(), src);
        let (_file, col) = module_goto_def_at(&ws, &uri, 1, 5).unwrap();
        assert_eq!(col, 4, "helper def starts at col 4 in lib.fnl");
    }

    #[test]
    fn module_completion_offers_all_exports() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("math.fnl"),
            "(fn add [a b] (+ a b)) (fn sub [a b] (- a b))").unwrap();
        // Cursor is right after "math." on line 1
        // "(local math (require :math))\n(math."
        //  0         1         2         0123456
        let src = "(local math (require :math))\n(math.";
        let (ws, uri) = ws_with_require(dir.path(), src);
        let byte = src.len() as u32;
        let labels = module_completion_labels(&ws, &uri, 1, 6);
        assert!(labels.contains(&"math.add".to_string()), "add should be offered: {labels:?}");
        assert!(labels.contains(&"math.sub".to_string()), "sub should be offered: {labels:?}");
    }

    #[test]
    fn module_completion_empty_without_prefix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("utils.fnl"), "(fn foo [] nil)").unwrap();
        let src = "(local utils (require :utils))\n";
        let (ws, uri) = ws_with_require(dir.path(), src);
        // No "." in the text before cursor → no module completions
        let labels = module_completion_labels(&ws, &uri, 1, 0);
        assert!(labels.is_empty(), "no prefix → no module completions");
    }

    #[test]
    fn module_diagnostics_no_unknown_identifier_for_member() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("utils.fnl"), "(fn helper [x] x)").unwrap();
        let src = "(local utils (require :utils))\n(utils.helper 1)";
        let (ws, uri) = ws_with_require(dir.path(), src);
        let warnings: Vec<String> = ws.with_file(&uri, |f| {
            f.analysis.warnings.iter().map(|w| w.message.clone()).collect()
        }).unwrap_or_default();
        assert!(
            !warnings.iter().any(|w| w.contains("utils")),
            "no unknown-identifier warning for utils or utils.helper: {warnings:?}"
        );
    }

    // ── workspace/symbol (all_defs) ───────────────────────────────────────────

    #[test]
    fn workspace_symbol_finds_def_in_open_file() {
        let ws = Workspace::new();
        let uri = Url::parse("file:///main.fnl").unwrap();
        ws.update(uri, "(fn greet [x] x) (local msg \"hi\")".to_string(), 1, None);
        let defs = ws.all_defs("gre");
        assert!(defs.iter().any(|(_, _, d)| d.name == "greet"), "greet not found: {defs:?}");
    }

    #[test]
    fn workspace_symbol_empty_query_returns_all() {
        let ws = Workspace::new();
        let uri = Url::parse("file:///main.fnl").unwrap();
        ws.update(uri, "(fn alpha []) (fn beta [])".to_string(), 1, None);
        let defs = ws.all_defs("");
        let names: Vec<_> = defs.iter().map(|(_, _, d)| d.name.as_str()).collect();
        assert!(names.contains(&"alpha") && names.contains(&"beta"), "{names:?}");
    }

    #[test]
    fn workspace_symbol_case_insensitive() {
        let ws = Workspace::new();
        let uri = Url::parse("file:///main.fnl").unwrap();
        ws.update(uri, "(fn Greet [x] x)".to_string(), 1, None);
        let defs = ws.all_defs("greet");
        assert!(defs.iter().any(|(_, _, d)| d.name == "Greet"), "case-insensitive miss: {defs:?}");
    }

    #[test]
    fn workspace_symbol_searches_require_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("utils.fnl"), "(fn helper [] nil)").unwrap();
        let (ws, _) = ws_with_require(dir.path(), "(local utils (require :utils))");
        let defs = ws.all_defs("helper");
        assert!(defs.iter().any(|(_, _, d)| d.name == "helper"), "module def not found: {defs:?}");
    }

    // ── cross-file references ─────────────────────────────────────────────────

    #[test]
    fn cross_file_refs_finds_uses_in_consumer_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("utils.fnl"), "(fn greet [] nil)").unwrap();
        let consumer = "(local utils (require :utils))\n(utils.greet)\n(utils.greet)";
        let (ws, _) = ws_with_require(dir.path(), consumer);
        let def_uri = Url::from_file_path(dir.path().join("utils.fnl")).unwrap();
        let refs = ws.cross_file_refs_of(&def_uri, "greet");
        assert_eq!(refs.len(), 2, "expected 2 cross-file refs: {refs:?}");
    }

    #[test]
    fn cross_file_refs_sym_name_is_qualified() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("utils.fnl"), "(fn greet [] nil)").unwrap();
        let consumer = "(local utils (require :utils))\n(utils.greet)";
        let (ws, _) = ws_with_require(dir.path(), consumer);
        let def_uri = Url::from_file_path(dir.path().join("utils.fnl")).unwrap();
        let refs = ws.cross_file_refs_of(&def_uri, "greet");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].sym_name, "utils.greet");
    }

    #[test]
    fn cross_file_refs_no_match_for_other_member() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("utils.fnl"), "(fn greet [] nil) (fn bye [] nil)").unwrap();
        let consumer = "(local utils (require :utils))\n(utils.greet)";
        let (ws, _) = ws_with_require(dir.path(), consumer);
        let def_uri = Url::from_file_path(dir.path().join("utils.fnl")).unwrap();
        let refs = ws.cross_file_refs_of(&def_uri, "bye");
        assert!(refs.is_empty(), "bye is not called in consumer: {refs:?}");
    }

    // ── cross-file rename ─────────────────────────────────────────────────────

    #[test]
    fn cross_file_rename_generates_qualified_new_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("utils.fnl"), "(fn greet [] nil)").unwrap();
        let consumer = "(local utils (require :utils))\n(utils.greet)";
        let (ws, _) = ws_with_require(dir.path(), consumer);
        let def_uri = Url::from_file_path(dir.path().join("utils.fnl")).unwrap();
        let refs = ws.cross_file_refs_of(&def_uri, "greet");
        assert_eq!(refs.len(), 1);
        // Simulate rename edit: prefix + sep + new_name
        let new_name = "hello";
        let new_sym = {
            let sym = &refs[0].sym_name;
            let sep_idx = sym.find(['.', ':']).unwrap();
            let sep_char = &sym[sep_idx..=sep_idx];
            let prefix = &sym[..sep_idx];
            format!("{}{}{}", prefix, sep_char, new_name)
        };
        assert_eq!(new_sym, "utils.hello");
    }

    #[test]
    fn cross_file_rename_preserves_colon_separator() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("obj.fnl"), "(fn method [] nil)").unwrap();
        // Use colon syntax for method call
        let consumer = "(local obj (require :obj))\n(obj:method)";
        let (ws, _) = ws_with_require(dir.path(), consumer);
        let def_uri = Url::from_file_path(dir.path().join("obj.fnl")).unwrap();
        let refs = ws.cross_file_refs_of(&def_uri, "method");
        assert_eq!(refs.len(), 1, "should find the method ref: {refs:?}");
        let sym = &refs[0].sym_name;
        let sep_idx = sym.find(['.', ':']).unwrap();
        assert_eq!(&sym[sep_idx..=sep_idx], ":", "separator should be :");
        let new_sym = format!("{}:{}", &sym[..sep_idx], "newMethod");
        assert_eq!(new_sym, "obj:newMethod");
    }

    // ── split_multisym ────────────────────────────────────────────────────────

    #[test]
    fn split_multisym_dot() {
        assert_eq!(split_multisym("utils.helper"), Some(("utils", "helper")));
    }

    #[test]
    fn split_multisym_colon() {
        assert_eq!(split_multisym("obj:method"), Some(("obj", "method")));
    }

    #[test]
    fn split_multisym_no_separator() {
        assert_eq!(split_multisym("plain"), None);
    }

    #[test]
    fn split_multisym_nested_dot() {
        assert_eq!(split_multisym("a.b.c"), Some(("a", "b.c")));
    }
}
