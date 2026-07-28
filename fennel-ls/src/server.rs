/// tower-lsp backend — implements the LanguageServer trait and all LSP handlers.

use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};
use dashmap::DashMap;

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
    /// Extra globals from `.lsp.fnl` that suppress unknown-identifier warnings.
    /// Populated from `known_globals` plus roots inferred from `global_docs` keys.
    extra_globals: RwLock<Option<HashSet<String>>>,
    /// Per-symbol hover docs loaded from `global_docs` in `.lsp.fnl`.
    /// Keys are exact Fennel symbol names, e.g. `"MyLib.module.fn"`.
    global_docs: RwLock<Option<HashMap<String, GlobalDoc>>>,
    /// Whether `textDocument/formatting` is enabled (disabled via `--no-formatting`).
    formatting_enabled: bool,
    /// Caches the last full semantic token data per file (uri string → flat u32 vec).
    /// Used to compute deltas for `textDocument/semanticTokens/full/delta`.
    semantic_token_cache: DashMap<String, Vec<u32>>,
    /// Monotonic counter for unique semantic token result IDs.
    token_id_counter: AtomicU64,
    expander: crate::expander::MacroExpander,
    hook_runner: crate::hooks::HookRunner,
    warn_unhooked_macros: std::sync::atomic::AtomicBool,
}

impl Backend {
    pub fn new(client: Client, formatting_enabled: bool) -> Self {
        Self {
            client,
            workspace: Workspace::new(),
            workspace_root: OnceLock::new(),
            extra_globals: RwLock::new(None),
            global_docs: RwLock::new(None),
            formatting_enabled,
            semantic_token_cache: DashMap::new(),
            token_id_counter: AtomicU64::new(1),
            expander: crate::expander::MacroExpander::new(),
            hook_runner: crate::hooks::HookRunner::new(),
            warn_unhooked_macros: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Re-read `.lsp.fnl` from `root` and apply the configuration.
    /// Safe to call at any time — uses interior mutability (RwLock).
    fn apply_config(&self, root: &std::path::Path) {
        log::info!("apply_config: loading config from {}", root.display());
        let config = crate::config::Config::load(root);

        log::info!(
            "apply_config: platform={:?} known_globals={:?} global_docs={} entries",
            config.platform,
            config.known_globals,
            config.global_docs.as_ref().map_or(0, |d| d.len()),
        );

        let platform = config.platform.as_deref().and_then(platform_from_str);
        if let Some(p) = platform {
            self.workspace.configure_platform(p);
        }

        let mut all_globals: HashSet<String> = config.known_globals
            .unwrap_or_default()
            .into_iter()
            .collect();
        if let Some(docs) = &config.global_docs {
            for key in docs.keys() {
                if let Some(root_name) = key.split(['.', ':']).find(|s| !s.is_empty()) {
                    all_globals.insert(root_name.to_string());
                }
            }
        }
        log::info!("apply_config: extra_globals = {:?}", all_globals);
        *self.extra_globals.write().unwrap() = if all_globals.is_empty() {
            None
        } else {
            Some(all_globals)
        };
        *self.global_docs.write().unwrap() = config.global_docs;

        self.warn_unhooked_macros.store(
            config.warn_unhooked_macros.unwrap_or(false),
            std::sync::atomic::Ordering::Relaxed,
        );

        // Pass `.lsp.fnl` source to the hook runner so it can extract `:macro-hooks`.
        if let Ok(src) = std::fs::read_to_string(root.join(".lsp.fnl")) {
            let search_path = root.to_string_lossy().into_owned();
            self.hook_runner.try_set_source(src, search_path);
        }
    }

    /// Run hooks for all macro calls in `uri` and return a map of
    /// call_span_start → instructions. Returns empty if no hooks are configured.
    async fn compute_hooks(
        &self,
        uri: &Url,
        version: i32,
    ) -> std::collections::HashMap<u32, Vec<crate::hooks::Instruction>> {
        use crate::hooks::SerialNode;

        // Collect macro call info from the current analysis.
        let macro_calls: Vec<(u32, Option<String>, String)> = self
            .workspace
            .with_file(uri, |f| f.analysis.macro_calls.clone())
            .unwrap_or_default();

        if macro_calls.is_empty() {
            return std::collections::HashMap::new();
        }

        let warn = self.warn_unhooked_macros.load(std::sync::atomic::Ordering::Relaxed);
        let mut results = std::collections::HashMap::new();
        let mut unhooked: Vec<(crate::lexer::Span, String)> = Vec::new();

        for (span_start, source_module, macro_name) in macro_calls {
            let call_node = self
                .workspace
                .with_file(uri, |f| f.find_list_at(span_start).map(SerialNode::from_ast))
                .flatten();
            let Some(call_node) = call_node else { continue };

            let module_str = source_module.as_deref().unwrap_or("");
            match self.hook_runner.run_hook(module_str, &macro_name, call_node).await {
                Some(instrs) => {
                    results.insert(span_start, instrs);
                }
                None if warn => {
                    // No hook registered — record the call site for the diagnostic.
                    let span = self
                        .workspace
                        .with_file(uri, |f| {
                            f.find_list_at(span_start).map(|n| n.span.clone())
                        })
                        .flatten();
                    if let Some(span) = span {
                        unhooked.push((span, macro_name));
                    }
                }
                None => {}
            }
        }

        if !unhooked.is_empty() {
            self.workspace.set_unhooked_macros(uri, unhooked);
        } else {
            self.workspace.set_unhooked_macros(uri, vec![]);
        }

        self.hook_runner.store_results(uri.to_string(), version, results.clone());
        results
    }

    /// Compile `text` with Fennel and merge any macro-introduced names into
    /// the file's scope, then re-publish diagnostics.
    async fn run_macro_expansion(&self, uri: Url, text: String) {
        if text.contains("import-macros") || text.contains("require-macros") {
            let search_path = self.workspace_root
                .get()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let names = self.expander.expand(&text, &search_path).await;
            if !names.is_empty() {
                self.workspace.set_macro_globals(&uri, names);
                self.publish_diagnostics(uri).await;
            }
        }
    }

    fn is_known_global(&self, name: &str) -> bool {
        known_global(name)
            || self.extra_globals.read().unwrap().as_ref().map_or(false, |set| set.contains(name))
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
                let related_information = warn.related_span.as_ref().map(|rspan| {
                    vec![DiagnosticRelatedInformation {
                        location: Location {
                            uri: uri.clone(),
                            range: text::span_to_range(&file.text, rspan),
                        },
                        message: "original definition here".into(),
                    }]
                });
                diags.push(Diagnostic {
                    range: text::span_to_range(&file.text, &warn.span),
                    severity: Some(DiagnosticSeverity::WARNING),
                    message: warn.message.clone(),
                    source: Some("fennel-ls".into()),
                    related_information,
                    code: warning_code(&warn.message),
                    tags: warning_tags(&warn.message),
                    ..Default::default()
                });
            }

            // Macro calls with no hook definition (optional, toggled by warn-unhooked-macros)
            for (span, macro_name) in &file.unhooked_macros {
                diags.push(Diagnostic {
                    range: text::span_to_range(&file.text, span),
                    severity: Some(DiagnosticSeverity::HINT),
                    message: format!(
                        "macro `{}` has no hook definition; \
                         add one under :macro-hooks in .lsp.fnl for better analysis",
                        macro_name
                    ),
                    source: Some("fennel-ls".into()),
                    code: Some(NumberOrString::String("unhooked-macro".into())),
                    ..Default::default()
                });
            }

            // Undefined symbols (not in scope and not a builtin)
            for sym in &file.analysis.syms {
                if sym.is_def {
                    continue;
                }
                if sym.def_byte.is_none() && !sym.in_macro && !builtins.is_known(&sym.name) {
                    let root = sym.name.split(['.', ':']).find(|s| !s.is_empty()).unwrap_or(&sym.name);
                    if !self.is_known_global(root) && !file.macro_globals.contains(root) {
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

    async fn open_or_change(&self, uri: Url, text: String, version: i32) {
        let root = self.workspace_root.get().map(|p| p.as_path());

        // Pass 1: sync analysis with any cached hook results from a prior run.
        let cached = self.hook_runner.cached_results(uri.as_str(), version)
            .unwrap_or_default();
        self.workspace.update(uri.clone(), text.clone(), version, root, &cached);
        self.publish_diagnostics(uri.clone()).await;

        // Run hooks and do a second analysis pass if we got new results.
        let hook_results = self.compute_hooks(&uri, version).await;
        if !hook_results.is_empty() {
            self.workspace.update(uri.clone(), text.clone(), version, root, &hook_results);
            self.publish_diagnostics(uri.clone()).await;
        }

        self.run_macro_expansion(uri, text).await;
    }
}

#[async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Store workspace root for require resolution and config loading
        log::info!(
            "initialize: rootUri={:?} workspaceFolders={:?}",
            params.root_uri,
            params.workspace_folders.as_ref().map(|fs| fs.iter().map(|f| f.uri.as_str()).collect::<Vec<_>>()),
        );
        let root = params.root_uri
            .as_ref()
            .and_then(|u| u.to_file_path().ok())
            .or_else(|| {
                params.workspace_folders.as_ref()
                    .and_then(|fs| fs.first())
                    .and_then(|f| f.uri.to_file_path().ok())
            });
        if let Some(path) = root {
            log::info!("initialize: resolved workspace root → {}", path.display());
            self.apply_config(&path);
            let _ = self.workspace_root.set(path);
        } else {
            log::warn!("initialize: no workspace root could be resolved — config will not be loaded");
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "fennel-ls".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        will_save: Some(true),
                        will_save_wait_until: Some(true),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                    }
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                declaration_provider: Some(DeclarationCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
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
                inline_value_provider: Some(OneOf::Left(true)),
                document_formatting_provider: if self.formatting_enabled {
                    Some(OneOf::Left(true))
                } else {
                    None
                },
                document_range_formatting_provider: if self.formatting_enabled {
                    Some(OneOf::Left(true))
                } else {
                    None
                },
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: Default::default(),
                }),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: None,
                    file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                        did_rename: Some(FileOperationRegistrationOptions {
                            filters: vec![FileOperationFilter {
                                scheme: Some("file".into()),
                                pattern: FileOperationPattern {
                                    glob: "**/*.fnl".into(),
                                    matches: None,
                                    options: None,
                                },
                            }],
                        }),
                        will_rename: Some(FileOperationRegistrationOptions {
                            filters: vec![FileOperationFilter {
                                scheme: Some("file".into()),
                                pattern: FileOperationPattern {
                                    glob: "**/*.fnl".into(),
                                    matches: None,
                                    options: None,
                                },
                            }],
                        }),
                        did_delete: Some(FileOperationRegistrationOptions {
                            filters: vec![FileOperationFilter {
                                scheme: Some("file".into()),
                                pattern: FileOperationPattern {
                                    glob: "**/*.fnl".into(),
                                    matches: None,
                                    options: None,
                                },
                            }],
                        }),
                        did_create: Some(FileOperationRegistrationOptions {
                            filters: vec![FileOperationFilter {
                                scheme: Some("file".into()),
                                pattern: FileOperationPattern {
                                    glob: "**/*.fnl".into(),
                                    matches: None,
                                    options: None,
                                },
                            }],
                        }),
                        will_create: None,
                        will_delete: None,
                    }),
                }),
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: "\n".into(),
                    more_trigger_character: None,
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
                            full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                            range: Some(true),
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

        // Register a watcher for all .fnl files so we're notified when
        // required-but-not-open dependencies change on disk.
        let registration = Registration {
            id: "fennel-ls-file-watcher".into(),
            method: "workspace/didChangeWatchedFiles".into(),
            register_options: serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                watchers: vec![FileSystemWatcher {
                    glob_pattern: GlobPattern::String("**/*.fnl".into()),
                    kind: None,
                }],
            }).ok(),
        };
        let _ = self.client.register_capability(vec![registration]).await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    // ── Text synchronization ──────────────────────────────────────────────────

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        let text = doc.text.clone();
        let uri = doc.uri.clone();
        self.open_or_change(uri, text, doc.version).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        let current = self.workspace.with_file(&uri, |f| f.text.clone());
        let text = apply_incremental_changes(
            current.unwrap_or_default(),
            params.content_changes,
        );
        self.open_or_change(uri, text, version).await;
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

    async fn did_change_configuration(&self, _params: DidChangeConfigurationParams) {
        let Some(root) = self.workspace_root.get() else { return };
        self.apply_config(root);
        // Re-publish diagnostics so global-suppression changes take effect immediately.
        let uris = self.workspace.all_open_uris();
        for uri in uris {
            self.publish_diagnostics(uri).await;
        }
        self.client
            .log_message(MessageType::INFO, "fennel-ls: configuration reloaded")
            .await;
    }

    // ── willSave / willSaveWaitUntil ──────────────────────────────────────────

    async fn will_save(&self, _params: WillSaveTextDocumentParams) {}

    async fn will_save_wait_until(
        &self,
        params: WillSaveTextDocumentParams,
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
                range: Range { start: Position { line: 0, character: 0 }, end },
                new_text: formatted,
            }])
        });
        Ok(edits.flatten())
    }

    // ── declaration ───────────────────────────────────────────────────────────

    async fn goto_declaration(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        // Fennel has no separate declaration; redirect to definition.
        self.goto_definition(params).await
    }

    // ── document link ─────────────────────────────────────────────────────────

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let uri = params.text_document.uri;
        let root = self.workspace_root.get().cloned();
        let links = self.workspace.with_file(&uri, |file| {
            let Some(root) = &root else { return vec![] };
            let mut links = Vec::new();
            for node in &file.ast {
                collect_require_links(node, &file.text, root, &mut links);
            }
            links
        });
        Ok(links)
    }

    // ── didChangeWatchedFiles ─────────────────────────────────────────────────

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let root = self.workspace_root.get().map(|p| p.as_path());
        for change in params.changes {
            let uri = change.uri;
            // Files open in the editor are managed by didOpen/didChange.
            // We only handle files that are required-but-not-open.
            let is_open = self.workspace.with_file(&uri, |_| ()).is_some();
            if is_open {
                // Invalidate the require-cache entry so the next re-analysis
                // of open files that depend on this one picks up fresh exports.
                if let Ok(path) = uri.to_file_path() {
                    self.workspace.invalidate_require_cache(&path);
                }
            } else {
                match change.typ {
                    FileChangeType::CHANGED | FileChangeType::CREATED => {
                        if let Ok(path) = uri.to_file_path() {
                            if let Ok(text) = std::fs::read_to_string(&path) {
                                self.workspace.update(uri.clone(), text, 0, root, &Default::default());
                                self.publish_diagnostics(uri).await;
                            }
                        }
                    }
                    FileChangeType::DELETED => {
                        self.workspace.remove(&uri);
                    }
                    _ => {}
                }
            }
        }
        // Re-publish all open files so cross-file deps pick up any cache invalidations.
        for uri in self.workspace.all_open_uris() {
            self.publish_diagnostics(uri).await;
        }
    }

    // ── code lens ─────────────────────────────────────────────────────────────

    async fn code_lens(
        &self,
        params: CodeLensParams,
    ) -> Result<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri;

        struct DefInfo { name: String, range: Range, same_file_refs: usize }

        let infos = self.workspace.with_file(&uri, |file| {
            file.analysis.defs.iter()
                .filter(|(_, def)| matches!(def.kind, DefKind::Fn))
                .map(|(&byte, def)| {
                    let same_file_refs = file.analysis.refs.values()
                        .filter(|&&b| b == byte)
                        .count();
                    DefInfo {
                        name: def.name.clone(),
                        range: text::span_to_range(&file.text, &def.span),
                        same_file_refs,
                    }
                })
                .collect::<Vec<_>>()
        });
        let Some(infos) = infos else { return Ok(None) };

        let lenses = infos.into_iter().map(|info| {
            let cross = self.workspace.cross_file_refs_of(&uri, &info.name).len();
            let count = info.same_file_refs + cross;
            CodeLens {
                range: info.range,
                command: Some(Command {
                    title: format!("{} reference{}", count, if count == 1 { "" } else { "s" }),
                    command: "".into(),
                    arguments: None,
                }),
                data: None,
            }
        }).collect();
        Ok(Some(lenses))
    }

    // ── workspace file events ─────────────────────────────────────────────────

    async fn did_create_files(&self, params: CreateFilesParams) {
        // New .fnl files may be required by open files — invalidate the cache
        // and re-analyze so require completions and diagnostics stay accurate.
        let root = self.workspace_root.get().map(|p| p.as_path());
        for file_create in params.files {
            if let Ok(uri) = file_create.uri.parse::<Url>() {
                if let Ok(path) = uri.to_file_path() {
                    self.workspace.invalidate_require_cache(&path);
                    // If the new file already has content (e.g. from a template), index it.
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        self.workspace.update(uri, text, 0, root, &Default::default());
                    }
                }
            }
        }
        for uri in self.workspace.all_open_uris() {
            self.publish_diagnostics(uri).await;
        }
    }

    async fn did_delete_files(&self, params: DeleteFilesParams) {
        for file_delete in params.files {
            if let Ok(uri) = file_delete.uri.parse::<Url>() {
                if let Ok(path) = uri.to_file_path() {
                    self.workspace.invalidate_require_cache(&path);
                }
                self.workspace.remove(&uri);
                self.semantic_token_cache.remove(&uri.to_string());
            }
        }
        for uri in self.workspace.all_open_uris() {
            self.publish_diagnostics(uri).await;
        }
    }

    async fn will_rename_files(
        &self,
        params: RenameFilesParams,
    ) -> Result<Option<WorkspaceEdit>> {
        let Some(root) = self.workspace_root.get() else { return Ok(None) };
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();

        for rename in &params.files {
            let old_uri = rename.old_uri.parse::<Url>().ok();
            let new_uri = rename.new_uri.parse::<Url>().ok();
            let (Some(old_uri), Some(new_uri)) = (old_uri, new_uri) else { continue };
            let Ok(old_path) = old_uri.to_file_path() else { continue };
            let Ok(new_path) = new_uri.to_file_path() else { continue };
            let Some(old_mod) = file_path_to_module(&old_path, root) else { continue };
            let Some(new_mod) = file_path_to_module(&new_path, root) else { continue };
            if old_mod == new_mod { continue }

            // Find all open files that require the old module name and
            // return edits to update the require string.
            for file_uri in self.workspace.all_open_uris() {
                let edits = self.workspace.with_file(&file_uri, |file| {
                    let mut edits = Vec::new();
                    for node in &file.ast {
                        collect_require_rename_edits(
                            node, &file.text, &old_mod, &new_mod, &mut edits,
                        );
                    }
                    edits
                });
                if let Some(edits) = edits {
                    if !edits.is_empty() {
                        changes.entry(file_uri).or_default().extend(edits);
                    }
                }
            }
        }

        if changes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }))
        }
    }

    async fn did_rename_files(&self, params: RenameFilesParams) {
        let root = self.workspace_root.get().map(|p| p.as_path());
        for rename in params.files {
            if let (Ok(old_uri), Ok(new_uri)) = (
                rename.old_uri.parse::<Url>(),
                rename.new_uri.parse::<Url>(),
            ) {
                // Move cached token data to new URI key.
                if let Some((_, data)) = self.semantic_token_cache.remove(&old_uri.to_string()) {
                    self.semantic_token_cache.insert(new_uri.to_string(), data);
                }
                self.workspace.remove(&old_uri);
                if let Ok(path) = new_uri.to_file_path() {
                    self.workspace.invalidate_require_cache(&path);
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        self.workspace.update(new_uri, text, 0, root, &Default::default());
                    }
                }
            }
        }
        for uri in self.workspace.all_open_uris() {
            self.publish_diagnostics(uri).await;
        }
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

            // Custom global docs from `.lsp.fnl`.
            // Try the full name first, then progressively strip trailing
            // members until a match is found or the chain is exhausted.
            // This lets a parent namespace entry serve as a fallback for
            // any child call with no specific entry.
            let gd_guard = self.global_docs.read().unwrap();
            if let Some(docs) = gd_guard.as_ref() {
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
                        return Some(GotoDefinitionResponse::Scalar(
                            exports.location_for_def(def),
                        ));
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
            let syms = doc_syms_from_nodes(&file.ast, &file.text, &file.analysis);
            DocumentSymbolResponse::Nested(syms)
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
            let gd_guard2 = self.global_docs.read().unwrap();
            if let Some(docs) = gd_guard2.as_ref() {
                for (name, doc) in docs {
                    if let Some(ref pfx) = multisym_prefix {
                        if !name.starts_with(pfx.as_str()) { continue; }
                    }
                    if seen.insert(name.clone()) {
                        // insert_text is the suffix after the already-typed prefix so
                        // editors don't double-insert the prefix (e.g. gd.gd.Node3D).
                        let insert_text = multisym_prefix.as_deref()
                            .and_then(|pfx| name.get(pfx.len()..))
                            .map(|s| s.to_string());
                        items.push(CompletionItem {
                            label: name.clone(),
                            insert_text,
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
                                insert_text: Some(name.clone()),
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

                // Table-literal field completions: `t.` where `t` is a local
                // bound to a table constructor with static keys.
                let pfx_root = pfx.split(['.', ':']).next().unwrap_or("");
                if !pfx_root.is_empty() {
                    let table_def = file.analysis.defs_at(byte)
                        .into_iter()
                        .find(|d| d.name == pfx_root)
                        .and_then(|d| d.table_fields.clone());
                    if let Some(fields) = table_def {
                        for field in fields {
                            let full_label = format!("{}.{}", pfx_root, field);
                            if seen.insert(full_label.clone()) {
                                items.push(CompletionItem {
                                    label: full_label,
                                    insert_text: Some(field.clone()),
                                    kind: Some(CompletionItemKind::FIELD),
                                    ..Default::default()
                                });
                            }
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

    // ── Prepare rename ────────────────────────────────────────────────────────

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = &params.text_document.uri;
        let pos = params.position;
        let result = self.workspace.with_file(uri, |file| {
            let byte = text::position_to_byte(&file.text, pos)? as u32;
            let sym = file.analysis.symbol_at(byte)?;
            // Only allow rename on symbols with a resolvable definition.
            let _ = if sym.is_def { Some(sym.span.start) } else { sym.def_byte }?;
            Some(PrepareRenameResponse::RangeWithPlaceholder {
                range: text::span_to_range(&file.text, &sym.span),
                placeholder: sym.name.split(['.', ':']).last().unwrap_or(&sym.name).to_string(),
            })
        });
        Ok(result.flatten())
    }

    // ── Selection range ───────────────────────────────────────────────────────

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let uri = &params.text_document.uri;
        let result = self.workspace.with_file(uri, |file| {
            params.positions.iter().map(|&pos| {
                let byte = text::position_to_byte(&file.text, pos)
                    .unwrap_or(0) as u32;

                // Collect all AST spans that contain this byte, innermost last.
                let mut spans: Vec<crate::lexer::Span> = Vec::new();
                for node in &file.ast {
                    collect_enclosing_spans(node, byte, &mut spans);
                }

                // Deduplicate and sort smallest-first (innermost first).
                spans.sort_by_key(|s| s.end - s.start);
                spans.dedup_by(|a, b| a.start == b.start && a.end == b.end);

                // Build chain from outermost inward so each node's parent is larger.
                let mut chain: Option<Box<SelectionRange>> = None;
                for span in spans.iter().rev() {
                    chain = Some(Box::new(SelectionRange {
                        range: text::span_to_range(&file.text, span),
                        parent: chain,
                    }));
                }

                chain.map(|c| *c).unwrap_or_else(|| SelectionRange {
                    range: Range {
                        start: Position { line: 0, character: 0 },
                        end: text::byte_to_position(&file.text, file.text.len()),
                    },
                    parent: None,
                })
            }).collect::<Vec<_>>()
        });
        Ok(result)
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

            // Remove unused local: delete the whole `(local name expr)` form.
            if diag.message.contains("defined but never used") {
                let action = self.workspace.with_file(uri, |file| {
                    let name_byte = text::position_to_byte(&file.text, diag.range.start)?;
                    let form_span = find_containing_local_form(&file.ast, name_byte as u32)?;
                    // Extend deletion to include the trailing newline if present.
                    let end = form_span.end as usize;
                    let end_with_nl = if file.text.as_bytes().get(end) == Some(&b'\n') {
                        end + 1
                    } else {
                        end
                    };
                    let mut changes = HashMap::new();
                    changes.insert(
                        file.uri.clone(),
                        vec![TextEdit {
                            range: Range {
                                start: text::byte_to_position(&file.text, form_span.start as usize),
                                end: text::byte_to_position(&file.text, end_with_nl),
                            },
                            new_text: String::new(),
                        }],
                    );
                    Some(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!("Remove unused `{}`", diag.message
                            .split('`').nth(1).unwrap_or("binding")),
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

            // `local → var` refactor: offer when cursor is on an immutable binding.
            if diag.message.contains("use `local` instead of `var`") {
                // Already handled by the var→local action above; nothing extra needed.
            }
        }

        // Refactor actions based on cursor position (not tied to a specific diagnostic).
        // `local → var`: offer when the selection is on a `local` binding name.
        {
            let action = self.workspace.with_file(uri, |file| {
                let name_byte = text::position_to_byte(&file.text, params.range.start)?;
                let local_byte = find_local_keyword_before(&file.text, name_byte)?;
                let local_pos = text::byte_to_position(&file.text, local_byte);
                let local_end = text::byte_to_position(&file.text, local_byte + 5);
                let mut changes = HashMap::new();
                changes.insert(
                    file.uri.clone(),
                    vec![TextEdit {
                        range: Range { start: local_pos, end: local_end },
                        new_text: "var".into(),
                    }],
                );
                Some(CodeActionOrCommand::CodeAction(CodeAction {
                    title: "Change `local` to `var`".into(),
                    kind: Some(CodeActionKind::REFACTOR),
                    diagnostics: None,
                    edit: Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                    ..Default::default()
                }))
            });
            if let Some(Some(a)) = action {
                actions.push(a);
            }
        }

        // Wrap in `(do ...)`: expand selection to complete top-level forms and wrap them.
        {
            let action = self.workspace.with_file(uri, |file| {
                let start_byte = text::position_to_byte(&file.text, params.range.start)?;
                let end_byte = text::position_to_byte(&file.text, params.range.end)
                    .unwrap_or(file.text.len());
                if start_byte >= end_byte { return None; }
                // Find AST nodes at any depth that overlap the selection range and
                // that are siblings (same parent list).
                let selected_text = &file.text[start_byte..end_byte];
                // Only offer when there's non-whitespace selected.
                if selected_text.trim().is_empty() { return None; }
                let wrapped = format!("(do\n  {})", selected_text.trim());
                let mut changes = HashMap::new();
                changes.insert(
                    file.uri.clone(),
                    vec![TextEdit {
                        range: params.range,
                        new_text: wrapped,
                    }],
                );
                Some(CodeActionOrCommand::CodeAction(CodeAction {
                    title: "Wrap in `(do ...)`".into(),
                    kind: Some(CodeActionKind::REFACTOR),
                    diagnostics: None,
                    edit: Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                    ..Default::default()
                }))
            });
            if let Some(Some(a)) = action {
                actions.push(a);
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

    // ── Inline values ─────────────────────────────────────────────────────────

    async fn inline_value(
        &self,
        params: InlineValueParams,
    ) -> Result<Option<Vec<InlineValue>>> {
        let uri = &params.text_document.uri;
        let stopped = params.context.stopped_location.end;
        let result = self.workspace.with_file(uri, |file| {
            let byte = match text::position_to_byte(&file.text, stopped) {
                Some(b) => b as u32,
                None => return vec![],
            };
            file.analysis
                .defs_at(byte)
                .into_iter()
                .map(|def| {
                    InlineValue::VariableLookup(InlineValueVariableLookup {
                        range: text::span_to_range(&file.text, &def.span),
                        variable_name: Some(def.name.clone()),
                        case_sensitive_lookup: true,
                    })
                })
                .collect()
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
            build_semantic_tokens(&file.analysis, &file.text)
        });
        let Some(tokens) = result else { return Ok(None) };
        let id = self.token_id_counter.fetch_add(1, Ordering::Relaxed).to_string();
        let flat = tokens_to_flat(&tokens);
        self.semantic_token_cache.insert(uri.to_string(), flat);
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: Some(id),
            data: tokens,
        })))
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        let uri = &params.text_document.uri;
        let new_tokens = self.workspace.with_file(uri, |file| {
            build_semantic_tokens(&file.analysis, &file.text)
        });
        let Some(new_tokens) = new_tokens else { return Ok(None) };
        let new_flat = tokens_to_flat(&new_tokens);
        let new_id = self.token_id_counter.fetch_add(1, Ordering::Relaxed).to_string();

        let old_flat = self.semantic_token_cache
            .get(&uri.to_string())
            .map(|e| e.clone())
            .unwrap_or_default();

        self.semantic_token_cache.insert(uri.to_string(), new_flat.clone());

        // Build a minimal single-edit delta by finding the changed region.
        let prefix = old_flat.iter().zip(new_flat.iter()).take_while(|(a, b)| a == b).count();
        let suffix = old_flat[prefix..].iter().rev()
            .zip(new_flat[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let old_mid = &old_flat[prefix..old_flat.len().saturating_sub(suffix)];
        let new_mid = &new_flat[prefix..new_flat.len().saturating_sub(suffix)];

        if old_mid.is_empty() && new_mid.is_empty() {
            // No change — return empty delta.
            return Ok(Some(SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta {
                result_id: Some(new_id),
                edits: vec![],
            })));
        }

        let new_tokens_mid = flat_to_tokens(new_mid);
        Ok(Some(SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta {
            result_id: Some(new_id),
            edits: vec![SemanticTokensEdit {
                start: prefix as u32,
                delete_count: old_mid.len() as u32,
                data: if new_tokens_mid.is_empty() { None } else { Some(new_tokens_mid) },
            }],
        })))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri = &params.text_document.uri;
        let range = params.range;
        let result = self.workspace.with_file(uri, |file| {
            let all = build_semantic_tokens(&file.analysis, &file.text);
            // Filter to tokens whose decoded line falls within the requested range.
            // Tokens are delta-encoded so we must track the running line/col.
            let mut line: u32 = 0;
            let mut col: u32 = 0;
            let filtered: Vec<SemanticToken> = all.into_iter().filter_map(|t| {
                line += t.delta_line;
                col = if t.delta_line == 0 { col + t.delta_start } else { t.delta_start };
                if line >= range.start.line && line <= range.end.line {
                    Some(t)
                } else {
                    None
                }
            }).collect();
            SemanticTokensRangeResult::Tokens(SemanticTokens { result_id: None, data: filtered })
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

    // ── Range formatting ──────────────────────────────────────────────────────

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        if !self.formatting_enabled {
            return Ok(None);
        }
        let uri = &params.text_document.uri;
        let req_range = params.range;
        let edits = self.workspace.with_file(uri, |file| {
            let start_byte = text::position_to_byte(&file.text, req_range.start)?;
            let end_byte = text::position_to_byte(&file.text, req_range.end)
                .unwrap_or(file.text.len());

            // Expand to cover complete top-level forms overlapping the selection.
            let overlapping: Vec<_> = file.ast.iter()
                .filter(|n| (n.span.start as usize) < end_byte && (n.span.end as usize) > start_byte)
                .collect();
            if overlapping.is_empty() {
                return Some(vec![]);
            }

            let region_start = overlapping.first().unwrap().span.start as usize;
            let region_end = (overlapping.last().unwrap().span.end as usize).min(file.text.len());
            let region_text = &file.text[region_start..region_end];

            let formatted = crate::fmt::format(region_text)?;
            if formatted == region_text {
                return Some(vec![]);
            }

            // Strip trailing newline when the region doesn't extend to end of file
            // to avoid inserting a blank line between forms.
            let new_text = if region_end < file.text.len() {
                formatted.trim_end_matches('\n').to_string()
            } else {
                formatted
            };

            Some(vec![TextEdit {
                range: Range {
                    start: text::byte_to_position(&file.text, region_start),
                    end: text::byte_to_position(&file.text, region_end),
                },
                new_text,
            }])
        });
        Ok(edits.flatten())
    }

    // ── On-type formatting ────────────────────────────────────────────────────

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        if params.ch != "\n" {
            return Ok(None);
        }
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let edits = self.workspace.with_file(&uri, |file| {
            let cursor_byte = text::position_to_byte(&file.text, pos).unwrap_or(0);
            let indent = compute_indent(&file.text, cursor_byte);
            let indent_str = " ".repeat(indent as usize);

            // Find how much leading whitespace is already on the new line.
            let line_start = text::position_to_byte(
                &file.text,
                Position { line: pos.line, character: 0 },
            )
            .unwrap_or(0);
            let existing_ws = file.text[line_start..]
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .count() as u32;

            vec![TextEdit {
                range: Range {
                    start: Position { line: pos.line, character: 0 },
                    end: Position { line: pos.line, character: existing_ws },
                },
                new_text: indent_str,
            }]
        });
        Ok(edits)
    }

    // ── Call hierarchy ────────────────────────────────────────────────────────

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let result = self.workspace.with_file(uri, |file| {
            let byte = text::position_to_byte(&file.text, pos)? as u32;
            let sym = file.analysis.symbol_at(byte)?;
            let def_byte = if sym.is_def { sym.span.start } else { sym.def_byte? };
            let def = file.analysis.defs.get(&def_byte)?;
            if def.kind != DefKind::Fn { return None; }

            let name_span = crate::lexer::Span {
                start: def_byte,
                end: def_byte + def.name.len() as u32,
                line: 0, col: 0, end_line: 0, end_col: 0,
            };
            Some(vec![CallHierarchyItem {
                name: def.name.clone(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                detail: def.params.as_ref().map(|p| format!("[{}]", p.join(" "))),
                uri: file.uri.clone(),
                range: text::span_to_range(&file.text, &def.span),
                selection_range: text::span_to_range(&file.text, &name_span),
                data: None,
            }])
        });
        Ok(result.flatten())
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let item = &params.item;
        let def_uri = &item.uri;
        let def_name = &item.name;
        // Resolve the definition byte from the selection_range start position.
        let def_byte_opt = self.workspace.with_file(def_uri, |file| {
            text::position_to_byte(&file.text, item.selection_range.start).map(|b| b as u32)
        });
        let Some(Some(def_byte)) = def_byte_opt else { return Ok(None) };

        // Collect callers: for each file, find syms referencing this def, then
        // group by the enclosing function definition in that file.
        let mut callers: Vec<CallHierarchyIncomingCall> = Vec::new();

        // Same-file callers
        self.workspace.with_file(def_uri, |file| {
            collect_incoming(file, def_byte, def_uri, &mut callers);
        });

        // Cross-file callers from other open files
        let cross = self.workspace.cross_file_refs_of(def_uri, def_name);
        for xref in &cross {
            self.workspace.with_file(&xref.uri, |file| {
                // Find the SymbolEntry for this cross-file ref
                let ref_byte = xref.span.start;
                if let Some(enclosing) = enclosing_fn_def(&file.analysis, &file.ast, ref_byte) {
                    let call_range = text::span_to_range(&file.text, &xref.span);
                    let name_span = crate::lexer::Span {
                        start: enclosing.span.start,
                        end: enclosing.span.start + enclosing.name.len() as u32,
                        line: 0, col: 0, end_line: 0, end_col: 0,
                    };
                    if let Some(existing) = callers.iter_mut().find(|c| {
                        c.from.uri == file.uri && c.from.name == enclosing.name
                    }) {
                        existing.from_ranges.push(call_range);
                    } else {
                        callers.push(CallHierarchyIncomingCall {
                            from: CallHierarchyItem {
                                name: enclosing.name.clone(),
                                kind: SymbolKind::FUNCTION,
                                tags: None,
                                detail: None,
                                uri: file.uri.clone(),
                                range: text::span_to_range(&file.text, &enclosing.span),
                                selection_range: text::span_to_range(&file.text, &name_span),
                                data: None,
                            },
                            from_ranges: vec![call_range],
                        });
                    }
                }
            });
        }

        Ok(if callers.is_empty() { None } else { Some(callers) })
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let item = &params.item;
        let uri = &item.uri;
        let result = self.workspace.with_file(uri, |file| {
            let def_byte = text::position_to_byte(&file.text, item.selection_range.start)? as u32;
            // Use the whole-form span (not just the name token) to bound the walk.
            let form_span = fn_form_span(&file.ast, def_byte)?;

            let mut calls: Vec<CallHierarchyOutgoingCall> = Vec::new();
            for node in &file.ast {
                collect_outgoing(node, &form_span, &file.analysis, &file.text, &file.uri, &mut calls);
            }
            Some(calls)
        });
        Ok(result.flatten().filter(|v: &Vec<_>| !v.is_empty()))
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
///
/// Positions and lengths are in UTF-16 code units as required by the LSP spec.
pub fn build_semantic_tokens(
    analysis: &crate::analyzer::AnalysisResult,
    text: &str,
) -> Vec<SemanticToken> {
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

        // Use byte_to_position so column is in UTF-16 code units, not bytes.
        let pos = text::byte_to_position(text, sym.span.start as usize);
        let line = pos.line;
        let start = pos.character;

        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 { start - prev_start } else { start };

        // Length in UTF-16 code units (name.len() would give bytes, wrong for non-ASCII).
        let length = sym.name.encode_utf16().count() as u32;

        data.push(SemanticToken {
            delta_line,
            delta_start,
            length,
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

// ── selectionRange helpers ────────────────────────────────────────────────────

/// Recursively collect every AST span that contains `byte` into `out`.
fn collect_enclosing_spans(node: &crate::parser::AstNode, byte: u32, out: &mut Vec<crate::lexer::Span>) {
    if node.span.start > byte || node.span.end < byte {
        return;
    }
    out.push(node.span.clone());
    match &node.node {
        Form::List(children) | Form::Sequence(children) | Form::Table(children) => {
            for child in children {
                collect_enclosing_spans(child, byte, out);
            }
        }
        Form::Quote(inner) | Form::Quasiquote(inner)
        | Form::Unquote(inner) | Form::UnquoteSplice(inner)
        | Form::HashFn(inner) => {
            collect_enclosing_spans(inner, byte, out);
        }
        _ => {}
    }
}

// ── documentSymbol helpers ────────────────────────────────────────────────────

fn doc_syms_from_nodes(
    nodes: &[crate::parser::AstNode],
    text: &str,
    analysis: &crate::analyzer::AnalysisResult,
) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    for node in nodes {
        doc_sym_from_node(node, text, analysis, &mut out);
    }
    out
}

/// Recursively extract `DocumentSymbol` entries from an AST node.
/// Named definitions (`fn`, `local`, `var`, `global`, `macro`) become symbols;
/// scoping forms (`let`, `do`, `if`, …) are walked for nested definitions.
fn doc_sym_from_node(
    node: &crate::parser::AstNode,
    text: &str,
    analysis: &crate::analyzer::AnalysisResult,
    out: &mut Vec<DocumentSymbol>,
) {
    let Form::List(forms) = &node.node else { return };

    match head_sym(forms) {
        Some("fn") | Some("lambda") | Some("λ") | Some("macro") => {
            if forms.len() < 2 { return; }
            if let Form::Symbol(name) = &forms[1].node {
                // Named function/macro: name at [1], params at [2], body at [3+]
                let name_node = &forms[1];
                let def = analysis.defs.get(&name_node.span.start);
                let kind = def.map(|d| def_kind_to_symbol_kind(&d.kind))
                    .unwrap_or(SymbolKind::FUNCTION);
                let detail = def.and_then(|d| d.params.as_ref())
                    .map(|p| format!("fn [{}]", p.join(" ")));
                let body = forms.get(3..).unwrap_or(&[]);
                let children = doc_syms_from_nodes(body, text, analysis);
                #[allow(deprecated)]
                out.push(DocumentSymbol {
                    name: name.clone(),
                    detail,
                    kind,
                    tags: None,
                    deprecated: None,
                    range: text::span_to_range(text, &node.span),
                    selection_range: text::span_to_range(text, &name_node.span),
                    children: if children.is_empty() { None } else { Some(children) },
                });
            } else {
                // Anonymous fn — recurse into body to surface any nested named defs
                let body = forms.get(2..).unwrap_or(&[]);
                for n in body { doc_sym_from_node(n, text, analysis, out); }
            }
        }
        Some("local") | Some("var") | Some("global") => {
            if forms.len() < 3 { return; }
            if let Form::Symbol(name) = &forms[1].node {
                let name_node = &forms[1];
                let def = analysis.defs.get(&name_node.span.start);
                let kind = def.map(|d| def_kind_to_symbol_kind(&d.kind))
                    .unwrap_or(SymbolKind::VARIABLE);
                // Check if the value is an fn — carry through its param detail
                let detail = def.and_then(|d| d.params.as_ref())
                    .map(|p| format!("fn [{}]", p.join(" ")));
                // Recurse into the RHS so anonymous fns inside expose their named inner defs
                let rhs = forms.get(2..).unwrap_or(&[]);
                let children = doc_syms_from_nodes(rhs, text, analysis);
                #[allow(deprecated)]
                out.push(DocumentSymbol {
                    name: name.clone(),
                    detail,
                    kind,
                    tags: None,
                    deprecated: None,
                    range: text::span_to_range(text, &node.span),
                    selection_range: text::span_to_range(text, &name_node.span),
                    children: if children.is_empty() { None } else { Some(children) },
                });
            } else {
                // Destructuring — no single name to attach; recurse into value
                if let Some(val) = forms.get(2) {
                    doc_sym_from_node(val, text, analysis, out);
                }
            }
        }
        // Scoping and control-flow forms: walk their bodies for nested defs
        Some("let") | Some("do") | Some("when") | Some("unless") | Some("if")
        | Some("each") | Some("for") | Some("while")
        | Some("accumulate") | Some("collect") | Some("icollect")
        | Some("fcollect") | Some("faccumulate")
        | Some("with-open") | Some("match") | Some("case")
        | Some("case-try") | Some("match-try") => {
            for child in forms.iter().skip(1) {
                doc_sym_from_node(child, text, analysis, out);
            }
        }
        _ => {}
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

/// Return a diagnostic code string for a warning message, or `None` for parse errors.
fn warning_code(msg: &str) -> Option<NumberOrString> {
    Some(NumberOrString::String(if msg.contains("already defined") || msg.contains("shadows a binding") {
        "shadow"
    } else if msg.contains("required but never used") {
        "unused-require"
    } else if msg.contains("never used") {
        "unused-local"
    } else if msg.contains("parameter") && msg.contains("unused") {
        "unused-param"
    } else if msg.contains("never mutated") {
        "never-mutated"
    } else if msg.contains("immutable") {
        "immutable"
    } else if msg.contains("expects") {
        "arity"
    } else if msg.contains("unknown identifier") {
        "unknown"
    } else {
        return None;
    }.into()))
}

/// Return `DiagnosticTag::UNNECESSARY` for warnings about dead/unused code.
fn warning_tags(msg: &str) -> Option<Vec<DiagnosticTag>> {
    if msg.contains("required but never used")
        || msg.contains("never used")
        || (msg.contains("parameter") && msg.contains("unused"))
    {
        Some(vec![DiagnosticTag::UNNECESSARY])
    } else {
        None
    }
}

/// Convert a slice of `SemanticToken` to a flat `Vec<u32>` (5 u32s per token).
fn tokens_to_flat(tokens: &[SemanticToken]) -> Vec<u32> {
    let mut out = Vec::with_capacity(tokens.len() * 5);
    for t in tokens {
        out.push(t.delta_line);
        out.push(t.delta_start);
        out.push(t.length);
        out.push(t.token_type);
        out.push(t.token_modifiers_bitset);
    }
    out
}

/// Reconstruct `SemanticToken`s from a flat u32 slice (inverse of `tokens_to_flat`).
fn flat_to_tokens(flat: &[u32]) -> Vec<SemanticToken> {
    flat.chunks_exact(5).map(|c| SemanticToken {
        delta_line: c[0],
        delta_start: c[1],
        length: c[2],
        token_type: c[3],
        token_modifiers_bitset: c[4],
    }).collect()
}

/// Walk the AST and collect `DocumentLink`s for every `(require :mod)` form
/// whose module resolves to an existing file.
fn collect_require_links(
    node: &crate::parser::AstNode,
    text: &str,
    root: &std::path::Path,
    out: &mut Vec<DocumentLink>,
) {
    use crate::parser::Form;
    if let Form::List(forms) = &node.node {
        if crate::parser::head_sym(forms) == Some("require") && forms.len() >= 2 {
            if let Form::Keyword(s) | Form::Str(s) = &forms[1].node {
                if let Some(path) = crate::workspace::resolve_require_path(s, root) {
                    if let Ok(target) = Url::from_file_path(&path) {
                        out.push(DocumentLink {
                            range: text::span_to_range(text, &forms[1].span),
                            target: Some(target),
                            tooltip: Some(path.display().to_string()),
                            data: None,
                        });
                    }
                }
            }
        }
        for child in forms {
            collect_require_links(child, text, root, out);
        }
    }
}

/// Walk the AST and collect `TextEdit`s that rename `(require :old_mod)` →
/// `(require :new_mod)`.  Preserves the original delimiter (`:` or `"`).
fn collect_require_rename_edits(
    node: &crate::parser::AstNode,
    text: &str,
    old_mod: &str,
    new_mod: &str,
    out: &mut Vec<TextEdit>,
) {
    use crate::parser::Form;
    if let Form::List(forms) = &node.node {
        if crate::parser::head_sym(forms) == Some("require") && forms.len() >= 2 {
            match &forms[1].node {
                Form::Keyword(s) | Form::Str(s) if s == old_mod => {
                    // Replace just the string content, keeping surrounding : or "".
                    let arg_range = text::span_to_range(text, &forms[1].span);
                    let raw = &text[forms[1].span.start as usize..forms[1].span.end as usize];
                    let prefix = &raw[..1]; // ":" or "\""
                    let suffix = if raw.starts_with('"') { "\"" } else { "" };
                    out.push(TextEdit {
                        range: arg_range,
                        new_text: format!("{prefix}{new_mod}{suffix}"),
                    });
                }
                _ => {}
            }
        }
        for child in forms {
            collect_require_rename_edits(child, text, old_mod, new_mod, out);
        }
    }
}

/// Compute the Fennel module name for an absolute file path relative to `root`.
/// `root/path/to/mod.fnl` → `"path.to.mod"`, `root/mod/init.fnl` → `"mod"`.
fn file_path_to_module(path: &std::path::Path, root: &std::path::Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let stem = rel.with_extension("");
    let module_path = if stem.file_name()?.to_str()? == "init" {
        stem.parent()?.to_path_buf()
    } else {
        stem
    };
    let s = module_path.to_str()?;
    Some(s.replace(std::path::MAIN_SEPARATOR, "."))
}

/// Compute the indentation column for a new line at `cursor_byte`.
/// Scans forward through `text[..cursor_byte]`, tracking paren nesting.
/// Returns the column of the innermost unclosed `(` + 1, or 0 if at top level.
fn compute_indent(text: &str, cursor_byte: usize) -> u32 {
    let before = &text[..cursor_byte.min(text.len())];
    let mut stack: Vec<u32> = vec![];
    let mut col: u32 = 0;
    let mut chars = before.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => col = 0,
            ';' => {
                while chars.peek().is_some_and(|&c| c != '\n') {
                    chars.next();
                }
            }
            '"' => {
                col += 1;
                loop {
                    match chars.next() {
                        None | Some('\n') => break,
                        Some('\\') => { chars.next(); col += 1; }
                        Some('"') => { col += 1; break; }
                        Some(_) => col += 1,
                    }
                }
            }
            '(' | '[' | '{' => { stack.push(col + 1); col += 1; }
            ')' | ']' | '}' => { stack.pop(); col += 1; }
            _ => col += 1,
        }
    }
    stack.last().copied().unwrap_or(0)
}

/// Find the byte offset of the `local` keyword immediately before `name_byte`.
/// Returns `None` if the text before the name doesn't end with `local`.
fn find_local_keyword_before(text: &str, name_byte: usize) -> Option<usize> {
    let prefix = text.get(..name_byte)?;
    let trimmed = prefix.trim_end_matches(|c: char| c.is_whitespace());
    if !trimmed.ends_with("local") {
        return None;
    }
    let local_start = trimmed.len() - 5;
    let ok = trimmed[..local_start].chars().last()
        .map_or(true, |c| !c.is_alphanumeric() && c != '_' && c != '-');
    if ok { Some(local_start) } else { None }
}

/// Find the span of the top-level `(local …)` or `(var …)` form that contains
/// the given binding-name byte. Returns `None` if not found.
fn find_containing_local_form(
    ast: &[crate::parser::AstNode],
    name_byte: u32,
) -> Option<crate::lexer::Span> {
    for node in ast {
        if let Form::List(forms) = &node.node {
            if let Some(head) = head_sym(forms) {
                if (head == "local" || head == "var" || head == "global") && forms.len() >= 2 {
                    let binding = &forms[1];
                    if binding.span.start <= name_byte && name_byte <= binding.span.end {
                        return Some(node.span.clone());
                    }
                }
            }
        }
    }
    None
}

/// Walk the AST to find the span of the `(fn name ...)` form whose name node
/// starts at `def_byte`. Returns the whole-form span, not just the name span.
fn fn_form_span(
    ast: &[crate::parser::AstNode],
    def_byte: u32,
) -> Option<crate::lexer::Span> {
    fn find(nodes: &[crate::parser::AstNode], def_byte: u32) -> Option<crate::lexer::Span> {
        for node in nodes {
            if let Form::List(forms) = &node.node {
                if matches!(head_sym(forms), Some("fn") | Some("lambda") | Some("λ")) {
                    if let Some(name_node) = forms.get(1) {
                        if name_node.span.start == def_byte {
                            return Some(node.span.clone());
                        }
                    }
                }
                if let Some(s) = find(forms, def_byte) {
                    return Some(s);
                }
            }
        }
        None
    }
    find(ast, def_byte)
}

/// Walk the AST to find the innermost `(fn name ...)` form whose span contains
/// `ref_byte`, then return the corresponding `DefinitionInfo`.
fn enclosing_fn_def<'a>(
    analysis: &'a crate::analyzer::AnalysisResult,
    ast: &[crate::parser::AstNode],
    ref_byte: u32,
) -> Option<&'a crate::analyzer::DefinitionInfo> {
    fn find_name_byte(nodes: &[crate::parser::AstNode], byte: u32, best: &mut Option<(u32, u32)>) {
        for node in nodes {
            if node.span.start > byte || node.span.end < byte {
                continue;
            }
            if let Form::List(forms) = &node.node {
                if matches!(head_sym(forms), Some("fn") | Some("lambda") | Some("λ")) {
                    if let Some(name_node) = forms.get(1) {
                        if let Form::Symbol(_) = &name_node.node {
                            // Prefer the innermost (largest start) enclosing fn.
                            let form_start = node.span.start;
                            match best {
                                None => *best = Some((name_node.span.start, form_start)),
                                Some((_, bs)) if form_start > *bs => {
                                    *best = Some((name_node.span.start, form_start));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                find_name_byte(forms, byte, best);
            }
        }
    }

    let mut best: Option<(u32, u32)> = None;
    find_name_byte(ast, ref_byte, &mut best);
    let (name_byte, _) = best?;
    analysis.defs.get(&name_byte)
}

/// Collect `CallHierarchyIncomingCall` entries for same-file references to
/// the definition at `def_byte` in `file`.
fn collect_incoming(
    file: &crate::workspace::AnalyzedFile,
    def_byte: u32,
    def_uri: &tower_lsp::lsp_types::Url,
    out: &mut Vec<CallHierarchyIncomingCall>,
) {
    for sym in &file.analysis.syms {
        if sym.is_def || sym.def_byte != Some(def_byte) {
            continue;
        }
        let ref_byte = sym.span.start;
        let Some(enclosing) = enclosing_fn_def(&file.analysis, &file.ast, ref_byte) else { continue };
        // Skip self-reference (the definition itself)
        if enclosing.span.start == def_byte { continue; }

        let call_range = text::span_to_range(&file.text, &sym.span);
        let name_span = crate::lexer::Span {
            start: enclosing.span.start,
            end: enclosing.span.start + enclosing.name.len() as u32,
            line: 0, col: 0, end_line: 0, end_col: 0,
        };
        if let Some(existing) = out.iter_mut().find(|c| {
            c.from.uri == *def_uri && c.from.name == enclosing.name
        }) {
            existing.from_ranges.push(call_range);
        } else {
            out.push(CallHierarchyIncomingCall {
                from: CallHierarchyItem {
                    name: enclosing.name.clone(),
                    kind: SymbolKind::FUNCTION,
                    tags: None,
                    detail: None,
                    uri: def_uri.clone(),
                    range: text::span_to_range(&file.text, &enclosing.span),
                    selection_range: text::span_to_range(&file.text, &name_span),
                    data: None,
                },
                from_ranges: vec![call_range],
            });
        }
    }
}

/// Recursively walk the AST within `fn_span`, collecting outgoing calls to
/// known `Fn`-kind definitions. Deduplicates by callee name.
fn collect_outgoing(
    node: &crate::parser::AstNode,
    fn_span: &crate::lexer::Span,
    analysis: &crate::analyzer::AnalysisResult,
    text: &str,
    caller_uri: &tower_lsp::lsp_types::Url,
    out: &mut Vec<CallHierarchyOutgoingCall>,
) {
    if node.span.start > fn_span.end || node.span.end < fn_span.start {
        return;
    }
    if let Form::List(forms) = &node.node {
        if let Some(head) = forms.first() {
            if let Form::Symbol(_) = &head.node {
                // Look up via refs map to find the callee definition.
                if let Some(def_byte) = analysis.refs.get(&head.span.start) {
                    if let Some(callee) = analysis.defs.get(def_byte) {
                        if callee.kind == DefKind::Fn && callee.span.start != fn_span.start {
                            let call_range = text::span_to_range(text, &head.span);
                            let callee_name_span = crate::lexer::Span {
                                start: *def_byte,
                                end: def_byte + callee.name.len() as u32,
                                line: 0, col: 0, end_line: 0, end_col: 0,
                            };
                            if let Some(existing) = out.iter_mut().find(|c| c.to.name == callee.name) {
                                existing.from_ranges.push(call_range);
                            } else {
                                out.push(CallHierarchyOutgoingCall {
                                    to: CallHierarchyItem {
                                        name: callee.name.clone(),
                                        kind: SymbolKind::FUNCTION,
                                        tags: None,
                                        detail: None,
                                        uri: caller_uri.clone(),
                                        range: text::span_to_range(text, &callee.span),
                                        selection_range: text::span_to_range(text, &callee_name_span),
                                        data: None,
                                    },
                                    from_ranges: vec![call_range],
                                });
                            }
                        }
                    }
                }
            }
            for child in forms {
                collect_outgoing(child, fn_span, analysis, text, caller_uri, out);
            }
        } else {
            for child in forms {
                collect_outgoing(child, fn_span, analysis, text, caller_uri, out);
            }
        }
    } else {
        let children: &[crate::parser::AstNode] = match &node.node {
            Form::Sequence(c) | Form::Table(c) => c,
            Form::Quote(i) | Form::Quasiquote(i)
            | Form::Unquote(i) | Form::UnquoteSplice(i)
            | Form::HashFn(i) => {
                collect_outgoing(i, fn_span, analysis, text, caller_uri, out);
                return;
            }
            _ => return,
        };
        for child in children {
            collect_outgoing(child, fn_span, analysis, text, caller_uri, out);
        }
    }
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
        DefinitionInfo { name: name.into(), kind, span: dummy_span(), params, doc, variadic: false, returns_multiple: false, table_fields: None, source_module: None }
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

    // ── find_local_keyword_before ─────────────────────────────────────────────

    #[test]
    fn find_local_before_finds_local() {
        // "(local x 1)": x is at byte 7
        assert_eq!(find_local_keyword_before("(local x 1)", 7), Some(1));
    }

    #[test]
    fn find_local_before_extra_whitespace() {
        // "(local  x 1)": x is at byte 8 due to extra space
        assert_eq!(find_local_keyword_before("(local  x 1)", 8), Some(1));
    }

    #[test]
    fn find_local_before_not_found_for_var() {
        assert_eq!(find_local_keyword_before("(var x 1)", 5), None);
    }

    #[test]
    fn find_local_before_requires_word_boundary() {
        // "notlocal" ends with "local" but is a single identifier — must not match
        assert_eq!(find_local_keyword_before("(notlocal x 1)", 10), None);
    }

    #[test]
    fn find_local_before_at_start_of_string() {
        assert_eq!(find_local_keyword_before("x", 0), None);
    }

    // ── find_containing_local_form ────────────────────────────────────────────

    #[test]
    fn containing_local_form_finds_local_binding() {
        // "(local x 42)": x is at byte 7
        let ast = parse_ast("(local x 42)");
        let span = find_containing_local_form(&ast, 7);
        assert!(span.is_some(), "should find the local form");
        assert_eq!(span.unwrap().start, 0, "form starts at opening paren");
    }

    #[test]
    fn containing_local_form_finds_var_binding() {
        // "(var y 1)": y is at byte 5
        let ast = parse_ast("(var y 1)");
        let span = find_containing_local_form(&ast, 5);
        assert!(span.is_some(), "should find the var form");
        assert_eq!(span.unwrap().start, 0);
    }

    #[test]
    fn containing_local_form_finds_global_binding() {
        // "(global z 99)": z is at byte 8
        let ast = parse_ast("(global z 99)");
        let span = find_containing_local_form(&ast, 8);
        assert!(span.is_some(), "should find the global form");
    }

    #[test]
    fn containing_local_form_returns_none_for_non_binding_form() {
        let ast = parse_ast("(+ 1 2)");
        assert!(find_containing_local_form(&ast, 1).is_none());
    }

    #[test]
    fn containing_local_form_returns_none_when_byte_not_on_name() {
        // "(local x 42)": byte 9 is inside "42", not the binding name
        let ast = parse_ast("(local x 42)");
        assert!(find_containing_local_form(&ast, 9).is_none(),
            "byte inside value expression should not match");
    }

    // ── enclosing_fn_def ──────────────────────────────────────────────────────

    #[test]
    fn enclosing_fn_def_byte_inside_fn_returns_def() {
        let src = "(fn greet [name] name)";
        let ast = parse_ast(src);
        let analysis = analyze(src);
        // A byte inside the body (past the param list) should be enclosed by greet
        let inside = src.find("name)").unwrap() as u32;
        let result = enclosing_fn_def(&analysis, &ast, inside);
        assert!(result.is_some(), "should find enclosing fn");
        assert_eq!(result.unwrap().name, "greet");
    }

    #[test]
    fn enclosing_fn_def_byte_outside_any_fn_returns_none() {
        let src = "(local x 1)";
        let ast = parse_ast(src);
        let analysis = analyze(src);
        let x_def = analysis.defs.values().find(|d| d.name == "x").unwrap();
        assert!(enclosing_fn_def(&analysis, &ast, x_def.span.start).is_none());
    }

    #[test]
    fn enclosing_fn_def_nested_fns_returns_innermost() {
        let src = "(fn outer [] (fn inner [] nil))";
        let ast = parse_ast(src);
        let analysis = analyze(src);
        // A byte inside "nil" (inside inner) should return inner, not outer
        let inside_nil = src.find("nil").unwrap() as u32 + 1;
        let result = enclosing_fn_def(&analysis, &ast, inside_nil);
        assert_eq!(result.unwrap().name, "inner",
            "innermost enclosing fn should be returned");
    }

    // ── collect_incoming (via Workspace) ──────────────────────────────────────

    fn ws_uri(src: &str) -> (Workspace, Url) {
        let ws = Workspace::new();
        let uri = Url::parse("file:///ch_test.fnl").unwrap();
        ws.update(uri.clone(), src.to_string(), 1, None, &Default::default());
        (ws, uri)
    }

    #[test]
    fn collect_incoming_finds_same_file_caller() {
        let (ws, uri) = ws_uri("(fn greet [name] nil)\n(fn main [] (greet \"world\"))");
        let callers = ws.with_file(&uri, |file| {
            let greet = file.analysis.defs.values().find(|d| d.name == "greet")?;
            let mut out = Vec::new();
            collect_incoming(file, greet.span.start, &uri, &mut out);
            Some(out.into_iter().map(|c| c.from.name).collect::<Vec<_>>())
        }).flatten().unwrap_or_default();
        assert!(callers.contains(&"main".to_string()),
            "should find main as caller; got: {callers:?}");
    }

    #[test]
    fn collect_incoming_empty_when_no_callers() {
        let (ws, uri) = ws_uri("(fn greet [name] nil)");
        let callers = ws.with_file(&uri, |file| {
            let greet = file.analysis.defs.values().find(|d| d.name == "greet")?;
            let mut out = Vec::new();
            collect_incoming(file, greet.span.start, &uri, &mut out);
            Some(out)
        }).flatten().unwrap_or_default();
        assert!(callers.is_empty(), "no callers expected");
    }

    #[test]
    fn collect_incoming_groups_multiple_calls_into_one_entry() {
        let (ws, uri) = ws_uri("(fn f [] nil)\n(fn caller [] (f) (f))");
        let entries = ws.with_file(&uri, |file| {
            let f_def = file.analysis.defs.values().find(|d| d.name == "f")?;
            let mut out = Vec::new();
            collect_incoming(file, f_def.span.start, &uri, &mut out);
            Some(out)
        }).flatten().unwrap_or_default();
        assert_eq!(entries.len(), 1, "two calls from same fn = one caller entry");
        assert_eq!(entries[0].from_ranges.len(), 2, "two call site ranges");
    }

    // ── collect_outgoing (via Workspace) ──────────────────────────────────────

    #[test]
    fn collect_outgoing_finds_user_fn_callee() {
        let (ws, uri) = ws_uri("(fn helper [] nil)\n(fn main [] (helper))");
        let callees = ws.with_file(&uri, |file| {
            let main = file.analysis.defs.values().find(|d| d.name == "main")?;
            let span = fn_form_span(&file.ast, main.span.start)?;
            let mut out = Vec::new();
            for node in &file.ast {
                collect_outgoing(node, &span, &file.analysis, &file.text, &file.uri, &mut out);
            }
            Some(out.into_iter().map(|c| c.to.name).collect::<Vec<_>>())
        }).flatten().unwrap_or_default();
        assert!(callees.contains(&"helper".to_string()),
            "should find helper as callee; got: {callees:?}");
    }

    #[test]
    fn collect_outgoing_skips_builtins() {
        let (ws, uri) = ws_uri("(fn greet [name] (print name))");
        let callees = ws.with_file(&uri, |file| {
            let greet = file.analysis.defs.values().find(|d| d.name == "greet")?;
            let span = fn_form_span(&file.ast, greet.span.start)?;
            let mut out = Vec::new();
            for node in &file.ast {
                collect_outgoing(node, &span, &file.analysis, &file.text, &file.uri, &mut out);
            }
            Some(out)
        }).flatten().unwrap_or_default();
        assert!(callees.is_empty(), "builtin calls should not appear as outgoing calls");
    }

    #[test]
    fn collect_outgoing_deduplicates_repeated_callee() {
        let (ws, uri) = ws_uri("(fn helper [] nil)\n(fn main [] (helper) (helper))");
        let out = ws.with_file(&uri, |file| {
            let main = file.analysis.defs.values().find(|d| d.name == "main")?;
            let span = fn_form_span(&file.ast, main.span.start)?;
            let mut out = Vec::new();
            for node in &file.ast {
                collect_outgoing(node, &span, &file.analysis, &file.text, &file.uri, &mut out);
            }
            Some(out)
        }).flatten().unwrap_or_default();
        assert_eq!(out.len(), 1, "two calls to helper = one outgoing entry; got: {}", out.len());
        assert_eq!(out[0].from_ranges.len(), 2, "must have two call site ranges");
    }

    // ── range formatting helpers ───────────────────────────────────────────────

    #[test]
    fn range_format_clean_form_produces_identical_output() {
        let (ws, uri) = ws_uri("(fn f [] nil)\n(fn g [] nil)\n");
        let already_clean = ws.with_file(&uri, |file| {
            let node = file.ast.first()?;
            let region = &file.text[node.span.start as usize..node.span.end as usize];
            let formatted = crate::fmt::format(region)?;
            // The formatter adds a trailing newline; strip it before comparing since
            // the range handler also strips it for non-end-of-file regions.
            Some(formatted.trim_end_matches('\n') == region)
        }).flatten().unwrap_or(false);
        assert!(already_clean, "clean form should round-trip unchanged through formatter");
    }

    #[test]
    fn range_format_messy_form_produces_different_output() {
        let (ws, uri) = ws_uri("(fn f [] nil)\n(fn g   [] nil)\n");
        let changed = ws.with_file(&uri, |file| {
            // The second form has extra spaces — it should format differently.
            assert_eq!(file.ast.len(), 2, "should parse two top-level forms");
            let second = &file.ast[1];
            let start = second.span.start as usize;
            let end = second.span.end as usize;
            let region = &file.text[start..end];
            let formatted = crate::fmt::format(region)?;
            Some(formatted.trim_end_matches('\n') != region)
        }).flatten().unwrap_or(false);
        assert!(changed, "messy form should produce different formatted output");
    }

    #[test]
    fn range_format_only_affects_selected_top_level_form() {
        let src = "(fn f [] nil)\n(fn g   [] nil)\n";
        let (ws, uri) = ws_uri(src);
        ws.with_file(&uri, |file| {
            // The second form starts after the first form + newline.
            let second = &file.ast[1];
            assert!(second.span.start as usize >= "(fn f [] nil)\n".len(),
                "second form must start after the first; span.start={}", second.span.start);
        });
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

    fn sem_tokens(src: &str) -> Vec<SemanticToken> {
        build_semantic_tokens(&analyze(src), src)
    }

    #[test]
    fn semantic_tokens_fn_def_produces_function_token() {
        let tokens = sem_tokens("(fn add [a b] (+ a b))");
        // `add` is a Fn definition → token_type 0 (function), modifier has definition bit
        let fn_tok = tokens.iter().find(|t| t.token_type == 0 && t.token_modifiers_bitset & 1 != 0);
        assert!(fn_tok.is_some(), "expected a function definition token: {tokens:?}");
    }

    #[test]
    fn semantic_tokens_param_produces_parameter_token() {
        let tokens = sem_tokens("(fn f [x] x)");
        let param_tok = tokens.iter().find(|t| t.token_type == 1);
        assert!(param_tok.is_some(), "expected a parameter token: {tokens:?}");
    }

    #[test]
    fn semantic_tokens_local_produces_variable_token() {
        let tokens = sem_tokens("(local x 1) x");
        let var_tok = tokens.iter().find(|t| t.token_type == 2);
        assert!(var_tok.is_some(), "expected a variable token: {tokens:?}");
    }

    #[test]
    fn semantic_tokens_unresolved_refs_skipped() {
        // `undefined` has no def_byte, so it should produce no token
        let tokens = sem_tokens("undefined");
        assert!(tokens.is_empty(), "unresolved ref should produce no token: {tokens:?}");
    }

    #[test]
    fn semantic_tokens_delta_encoding_is_cumulative() {
        // Two defs on different lines; delta_line of second must be 1, not line number
        let tokens = sem_tokens("(local a 1)\n(local b 2)");
        assert!(tokens.len() >= 2, "need at least two tokens");
        assert_eq!(tokens[0].delta_line, 0, "first token on line 0");
        assert_eq!(tokens[1].delta_line, 1, "second token one line below first");
    }

    // ── Semantic token positions with non-ASCII ───────────────────────────────

    #[test]
    fn semantic_tokens_ascii_col_correct() {
        // Sanity: pure ASCII (byte col == UTF-16 col).
        // "(local foo 1) foo"
        //  01234567890123456
        // foo def at byte 7, foo ref at byte 14 → delta = 7
        let tokens = sem_tokens("(local foo 1) foo");
        let var_toks: Vec<_> = tokens.iter().filter(|t| t.token_type == 2).collect();
        assert_eq!(var_toks.len(), 2, "expected 2 foo tokens: {tokens:?}");
        assert_eq!(var_toks[0].delta_start, 7, "foo def at UTF-16 col 7");
        assert_eq!(var_toks[1].delta_start, 7, "foo ref delta 7");
    }

    #[test]
    fn semantic_tokens_col_correct_after_bmp_non_ascii() {
        // "(local foo \"caf\u{E9}\") foo"
        // é = U+00E9: 2 UTF-8 bytes, 1 UTF-16 unit
        // foo def: byte 7 → UTF-16 col 7 (no non-ASCII before it)
        // foo ref: byte 20 → UTF-16 col 19 (é saves 1 unit vs bytes)
        // delta: 19 - 7 = 12
        let src = "(local foo \"caf\u{00E9}\") foo";
        let tokens = sem_tokens(src);
        let var_toks: Vec<_> = tokens.iter().filter(|t| t.token_type == 2).collect();
        assert_eq!(var_toks.len(), 2, "expected 2 foo tokens: {tokens:?}");
        assert_eq!(var_toks[0].delta_start, 7, "foo def at UTF-16 col 7");
        assert_eq!(var_toks[1].delta_start, 12, "foo ref delta 12 UTF-16 units");
    }

    #[test]
    fn semantic_tokens_col_correct_after_supplementary_non_ascii() {
        // "\u{1F389} (local foo 1) foo"
        // 🎉 = U+1F389: 4 UTF-8 bytes, 2 UTF-16 units
        //   bytes: 🎉=0-3, ' '=4, (=5, l=6, o=7, c=8, a=9, l=10, ' '=11, f=12
        // UTF-16: 🎉=2, ' '=1, (=1, l=1, o=1, c=1, a=1, l=1, ' '=1 → col 10 for f
        let src = "\u{1F389} (local foo 1) foo";
        let tokens = sem_tokens(src);
        let var_toks: Vec<_> = tokens.iter().filter(|t| t.token_type == 2).collect();
        assert_eq!(var_toks.len(), 2, "expected 2 foo tokens: {tokens:?}");
        assert_eq!(var_toks[0].delta_start, 10, "foo def at UTF-16 col 10");
    }

    #[test]
    fn semantic_tokens_length_correct_for_bmp_non_ascii_name() {
        // café: c(1)+a(1)+f(1)+é(2 bytes UTF-8, 1 unit UTF-16) = 5 bytes, 4 UTF-16 units
        let tokens = sem_tokens("(local caf\u{00E9} 1)");
        let var_tok = tokens.iter().find(|t| t.token_type == 2 && t.token_modifiers_bitset & 1 != 0);
        assert!(var_tok.is_some(), "expected variable definition token: {tokens:?}");
        assert_eq!(var_tok.unwrap().length, 4, "token length must be 4 UTF-16 units");
    }

    #[test]
    fn semantic_tokens_length_correct_for_supplementary_plane_name() {
        // a🎉b: a(1)+🎉(4 bytes, 2 UTF-16 units)+b(1) = 6 bytes, 4 UTF-16 units
        let tokens = sem_tokens("(local a\u{1F389}b 1)");
        let var_tok = tokens.iter().find(|t| t.token_type == 2 && t.token_modifiers_bitset & 1 != 0);
        assert!(var_tok.is_some(), "expected variable token: {tokens:?}");
        assert_eq!(var_tok.unwrap().length, 4, "token length must be 4 UTF-16 units");
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
        ws.update(uri.clone(), main_src.to_string(), 1, Some(root), &Default::default());
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
        ws.update(uri, "(fn greet [x] x) (local msg \"hi\")".to_string(), 1, None, &Default::default());
        let defs = ws.all_defs("gre");
        assert!(defs.iter().any(|(_, _, d)| d.name == "greet"), "greet not found: {defs:?}");
    }

    #[test]
    fn workspace_symbol_empty_query_returns_all() {
        let ws = Workspace::new();
        let uri = Url::parse("file:///main.fnl").unwrap();
        ws.update(uri, "(fn alpha []) (fn beta [])".to_string(), 1, None, &Default::default());
        let defs = ws.all_defs("");
        let names: Vec<_> = defs.iter().map(|(_, _, d)| d.name.as_str()).collect();
        assert!(names.contains(&"alpha") && names.contains(&"beta"), "{names:?}");
    }

    #[test]
    fn workspace_symbol_case_insensitive() {
        let ws = Workspace::new();
        let uri = Url::parse("file:///main.fnl").unwrap();
        ws.update(uri, "(fn Greet [x] x)".to_string(), 1, None, &Default::default());
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

    // ── collect_enclosing_spans ───────────────────────────────────────────────

    #[test]
    fn enclosing_spans_cursor_on_symbol() {
        // "(local x 1) x" — cursor on the trailing `x` at byte 12
        // That x has only its own span; no enclosing list.
        let src = "(local x 1) x";
        let ast = parse_ast(src);
        let mut spans = Vec::new();
        for node in &ast {
            collect_enclosing_spans(node, 12, &mut spans);
        }
        // The trailing `x` is a top-level atom — only one span
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].start, 12);
    }

    #[test]
    fn enclosing_spans_cursor_inside_list() {
        // "(fn f [a] a)" — cursor on `a` param at byte 7
        // Spans: the `a` symbol (7..7), the sequence `[a]` (6..8), the whole list (0..12)
        let src = "(fn f [a] a)";
        let ast = parse_ast(src);
        let mut spans = Vec::new();
        for node in &ast {
            collect_enclosing_spans(node, 7, &mut spans);
        }
        assert!(spans.len() >= 2, "should find at least the symbol and its enclosing list");
        // Sort by size: smallest first
        spans.sort_by_key(|s| s.end - s.start);
        // Innermost should be just the `a` symbol
        assert_eq!(spans[0].start, 7);
        // Outermost should be the whole fn form starting at 0
        assert_eq!(spans.last().unwrap().start, 0);
    }

    #[test]
    fn enclosing_spans_cursor_inside_each_form() {
        // "(fn f [] nil) (fn g [] nil)" — the two fns do not overlap.
        // A byte inside only the first form should find only that form's spans.
        let src = "(fn f [] nil) (fn g [] nil)";
        let ast = parse_ast(src);
        // byte 4 is 'f' — inside the first fn only
        let mut spans = Vec::new();
        for node in &ast {
            collect_enclosing_spans(node, 4, &mut spans);
        }
        // Should find: the Symbol 'f', the Sequence '[]', the whole first List
        assert!(!spans.is_empty(), "cursor inside first fn must find enclosing spans");
        // All found spans must start at or before byte 4
        for s in &spans {
            assert!(s.start <= 4, "span starts after cursor: {:?}", s);
        }
    }

    // ── doc_syms_from_nodes ───────────────────────────────────────────────────

    fn analyze_and_doc_syms(src: &str) -> Vec<DocumentSymbol> {
        let (ast, _) = crate::parser::Parser::parse(src);
        let analysis = crate::analyzer::analyze(&ast);
        doc_syms_from_nodes(&ast, src, &analysis)
    }

    #[test]
    fn doc_syms_named_fn_at_top_level() {
        let syms = analyze_and_doc_syms("(fn greet [name] name)");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "greet");
        assert_eq!(syms[0].kind, SymbolKind::FUNCTION);
        assert_eq!(syms[0].detail.as_deref(), Some("fn [name]"));
    }

    #[test]
    fn doc_syms_local_binding() {
        let syms = analyze_and_doc_syms("(local x 42)");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "x");
        assert_eq!(syms[0].kind, SymbolKind::VARIABLE);
    }

    #[test]
    fn doc_syms_multiple_top_level() {
        let syms = analyze_and_doc_syms("(fn f [] nil) (local x 1) (fn g [a] a)");
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"f"));
        assert!(names.contains(&"x"));
        assert!(names.contains(&"g"));
        assert_eq!(syms.len(), 3);
    }

    #[test]
    fn doc_syms_nested_fn_is_child() {
        // An inner named fn in the body should be a child, not a sibling.
        let src = "(fn outer [] (fn inner [] nil))";
        let syms = analyze_and_doc_syms(src);
        assert_eq!(syms.len(), 1, "only outer at top level");
        assert_eq!(syms[0].name, "outer");
        let children = syms[0].children.as_ref().expect("outer must have children");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "inner");
    }

    #[test]
    fn doc_syms_anonymous_fn_body_surfaced() {
        // (local f (fn [] (fn inner [] nil))) — inner fn inside anonymous fn
        // should appear as a child of f.
        let src = "(local f (fn [] (fn inner [] nil)))";
        let syms = analyze_and_doc_syms(src);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "f");
        let children = syms[0].children.as_ref().expect("f must have children");
        assert_eq!(children[0].name, "inner");
    }

    #[test]
    fn doc_syms_selection_range_is_name_span() {
        // selection_range should point to just the name identifier, not the whole form
        let src = "(fn greet [name] name)";
        // "greet" starts at byte 4
        let syms = analyze_and_doc_syms(src);
        assert_eq!(syms[0].selection_range.start.character, 4,
            "selection_range must start at the name token, not the opening paren");
        // range should start at 0 (the opening paren of the whole form)
        assert_eq!(syms[0].range.start.character, 0);
    }

    #[test]
    fn doc_syms_no_symbols_for_anonymous_forms() {
        // Bare expressions and anonymous fns don't produce symbols
        let syms = analyze_and_doc_syms("(+ 1 2) (fn [x] x)");
        assert!(syms.is_empty(), "no named definitions → no symbols");
    }

    // ── compute_indent tests ──────────────────────────────────────────────────

    #[test]
    fn compute_indent_top_level_returns_zero() {
        // No open parens → top-level, indent 0
        assert_eq!(compute_indent("(fn foo [])\n", 12), 0);
    }

    #[test]
    fn compute_indent_inside_one_level() {
        // Cursor is right after the newline in "(fn foo []\n"
        // The opening ( is at col 0, so indent = 1
        let src = "(fn foo []\n";
        assert_eq!(compute_indent(src, src.len()), 1);
    }

    #[test]
    fn compute_indent_inside_nested_form() {
        // "(let [x 1]\n" — [x 1] is closed before the newline, so innermost
        // unclosed paren is (let at col 0 → indent 1
        let src = "(let [x 1]\n";
        assert_eq!(compute_indent(src, src.len()), 1);
    }

    #[test]
    fn compute_indent_inside_open_bracket() {
        // "(let [x\n" — [ at col 5 is unclosed, so indent = 6
        let src = "(let [x\n";
        assert_eq!(compute_indent(src, src.len()), 6);
    }

    #[test]
    fn compute_indent_ignores_parens_in_strings() {
        // The ( inside the string should not open a nesting level
        let src = "(local s \"(not a paren\"\n";
        assert_eq!(compute_indent(src, src.len()), 1);
    }

    #[test]
    fn compute_indent_ignores_parens_in_comments() {
        // The ( after ; is a comment, not a real paren
        let src = "; (fake\n(fn foo []\n";
        assert_eq!(compute_indent(src, src.len()), 1);
    }

    #[test]
    fn compute_indent_closed_form_returns_outer_indent() {
        // "(do (+ 1 2)\n" — inner ( was closed, outer ( at col 0 → indent 1
        let src = "(do (+ 1 2)\n";
        assert_eq!(compute_indent(src, src.len()), 1);
    }

    // ── related_information tests ─────────────────────────────────────────────

    #[test]
    fn shadow_warning_has_related_span() {
        let result = analyze("(local x 1)\n(local x 2)\n");
        let shadow = result.warnings.iter().find(|w| w.message.contains("already defined"));
        assert!(shadow.is_some(), "should have a shadow warning");
        assert!(shadow.unwrap().related_span.is_some(), "shadow warning must carry related_span");
    }

    #[test]
    fn non_shadow_warnings_have_no_related_span() {
        let result = analyze("(fn foo [x] x)\n(foo 1 2)\n");
        let arity = result.warnings.iter().find(|w| w.message.contains("expects"));
        assert!(arity.is_some(), "should have an arity warning");
        assert!(arity.unwrap().related_span.is_none(), "arity warning must not carry related_span");
    }

    // ── warning_code / warning_tags tests ─────────────────────────────────────

    #[test]
    fn warning_code_shadow() {
        assert_eq!(warning_code("already defined"), Some(NumberOrString::String("shadow".into())));
    }

    #[test]
    fn warning_code_unused_local() {
        assert_eq!(warning_code("never used"), Some(NumberOrString::String("unused-local".into())));
    }

    #[test]
    fn warning_code_arity() {
        assert_eq!(warning_code("expects 2 arguments but got 3"), Some(NumberOrString::String("arity".into())));
    }

    #[test]
    fn warning_code_unknown() {
        assert_eq!(warning_code("unknown identifier `foo`"), Some(NumberOrString::String("unknown".into())));
    }

    #[test]
    fn warning_tags_unused_local_is_unnecessary() {
        assert_eq!(warning_tags("never used"), Some(vec![DiagnosticTag::UNNECESSARY]));
    }

    #[test]
    fn warning_tags_unused_param_is_unnecessary() {
        assert_eq!(warning_tags("parameter `x` is unused"), Some(vec![DiagnosticTag::UNNECESSARY]));
    }

    #[test]
    fn warning_tags_non_unused_is_none() {
        assert_eq!(warning_tags("already defined"), None);
        assert_eq!(warning_tags("expects 1 argument"), None);
    }

    // ── tokens_to_flat / flat_to_tokens tests ────────────────────────────────

    #[test]
    fn tokens_roundtrip_through_flat() {
        let tokens = vec![
            SemanticToken { delta_line: 0, delta_start: 3, length: 4, token_type: 0, token_modifiers_bitset: 1 },
            SemanticToken { delta_line: 1, delta_start: 0, length: 2, token_type: 2, token_modifiers_bitset: 0 },
        ];
        let flat = tokens_to_flat(&tokens);
        assert_eq!(flat.len(), 10);
        assert_eq!(flat_to_tokens(&flat), tokens);
    }

    #[test]
    fn flat_to_tokens_ignores_incomplete_tail() {
        let flat = vec![0u32, 1, 2, 3]; // only 4 values, not a multiple of 5
        assert!(flat_to_tokens(&flat).is_empty());
    }

    // ── file_path_to_module tests ─────────────────────────────────────────────

    #[test]
    fn file_path_to_module_simple() {
        use std::path::PathBuf;
        let root = PathBuf::from("/project");
        assert_eq!(
            file_path_to_module(&PathBuf::from("/project/utils.fnl"), &root),
            Some("utils".into())
        );
    }

    #[test]
    fn file_path_to_module_nested() {
        use std::path::PathBuf;
        let root = PathBuf::from("/project");
        assert_eq!(
            file_path_to_module(&PathBuf::from("/project/lib/math.fnl"), &root),
            Some("lib.math".into())
        );
    }

    #[test]
    fn file_path_to_module_init() {
        use std::path::PathBuf;
        let root = PathBuf::from("/project");
        assert_eq!(
            file_path_to_module(&PathBuf::from("/project/mymod/init.fnl"), &root),
            Some("mymod".into())
        );
    }

    #[test]
    fn file_path_to_module_outside_root_returns_none() {
        use std::path::PathBuf;
        let root = PathBuf::from("/project");
        assert_eq!(file_path_to_module(&PathBuf::from("/other/utils.fnl"), &root), None);
    }

    // ── inline_value ──────────────────────────────────────────────────────────

    fn inline_value_names_at(src: &str, line: u32, col: u32) -> Vec<String> {
        let (ast, _) = crate::parser::Parser::parse(src);
        let analysis = crate::analyzer::analyze(&ast);
        let pos = Position { line, character: col };
        let Some(byte) = text::position_to_byte(src, pos) else { return vec![] };
        let mut names: Vec<String> = analysis
            .defs_at(byte as u32)
            .into_iter()
            .map(|d| d.name.clone())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn inline_value_no_locals_at_top_level() {
        // Stopped on a bare expression with no user-defined bindings
        let names = inline_value_names_at("(+ 1 2)", 0, 1);
        assert!(names.is_empty());
    }

    #[test]
    fn inline_value_single_local_in_scope() {
        // Stopped on line 1 — `x` was defined on line 0 and is visible
        let names = inline_value_names_at("(local x 1)\nx", 1, 0);
        assert_eq!(names, vec!["x"]);
    }

    #[test]
    fn inline_value_multiple_locals_in_scope() {
        let src = "(local a 1)\n(local b 2)\nb";
        let names = inline_value_names_at(src, 2, 0);
        assert!(names.contains(&"a".to_string()));
        assert!(names.contains(&"b".to_string()));
    }

    #[test]
    fn inline_value_defined_after_cursor_not_visible() {
        // Stopped on `x` reference — `y` is defined later and must not appear
        let src = "(local x 1)\nx\n(local y 2)";
        let names = inline_value_names_at(src, 1, 0);
        assert!(names.contains(&"x".to_string()));
        assert!(!names.contains(&"y".to_string()));
    }

    #[test]
    fn inline_value_fn_params_visible_inside_body() {
        // Stopped on the body expression — both params should appear
        let src = "(fn f [a b]\n  (+ a b))";
        let names = inline_value_names_at(src, 1, 2);
        assert!(names.contains(&"a".to_string()));
        assert!(names.contains(&"b".to_string()));
    }

    #[test]
    fn inline_value_fn_name_visible_inside_body() {
        // The fn name itself is in scope for recursion
        let src = "(fn fact [n]\n  n)";
        let names = inline_value_names_at(src, 1, 2);
        assert!(names.contains(&"fact".to_string()));
        assert!(names.contains(&"n".to_string()));
    }

    #[test]
    fn inline_value_nested_scope_sees_outer_bindings() {
        // Inside the let body, both the outer local and the let binding are visible
        let src = "(local x 1)\n(let [y 2]\n  y)";
        let names = inline_value_names_at(src, 2, 2);
        assert!(names.contains(&"x".to_string()));
        assert!(names.contains(&"y".to_string()));
    }

    #[test]
    fn inline_value_let_binding_not_visible_after_scope_ends() {
        // After the let block, `y` goes out of scope
        let src = "(let [y 2] y)\n(+ 1 2)";
        let names = inline_value_names_at(src, 1, 1);
        assert!(!names.contains(&"y".to_string()));
    }

    #[test]
    fn inline_value_var_visible() {
        let src = "(var x 0)\nx";
        let names = inline_value_names_at(src, 1, 0);
        assert!(names.contains(&"x".to_string()));
    }

    #[test]
    fn inline_value_shadow_shows_only_one_entry_per_name() {
        // When `x` is shadowed in an inner scope, defs_at returns only one `x`
        let src = "(local x 1)\n(let [x 2]\n  x)";
        let names = inline_value_names_at(src, 2, 2);
        assert_eq!(names.iter().filter(|n| *n == "x").count(), 1);
    }

    #[test]
    fn inline_value_params_not_visible_outside_fn() {
        // `a` and `b` are fn params — they must not leak to the outer scope
        let src = "(fn f [a b] a)\n(+ 1 2)";
        let names = inline_value_names_at(src, 1, 1);
        assert!(!names.contains(&"a".to_string()));
        assert!(!names.contains(&"b".to_string()));
    }

    #[test]
    fn inline_value_multiple_fns_only_current_params_visible() {
        // Stopped inside `g` — only `g`'s param `c` is visible, not `f`'s `a`/`b`
        let src = "(fn f [a b] a)\n(fn g [c]\n  c)";
        let names = inline_value_names_at(src, 2, 2);
        assert!(names.contains(&"c".to_string()));
        assert!(!names.contains(&"a".to_string()));
        assert!(!names.contains(&"b".to_string()));
    }

    #[test]
    fn inline_value_outer_local_visible_inside_fn() {
        // A module-level local is visible inside a fn defined after it
        let src = "(local config 42)\n(fn run []\n  config)";
        let names = inline_value_names_at(src, 2, 2);
        assert!(names.contains(&"config".to_string()));
    }

    #[test]
    fn inline_value_invalid_position_returns_empty() {
        // Line 99 doesn't exist — should return empty, not panic
        let names = inline_value_names_at("(local x 1)", 99, 0);
        assert!(names.is_empty());
    }
}
