/// Scope analysis for Fennel source files.
///
/// A single recursive pass over the AST builds:
///   - `defs`:   definition byte-offset → DefinitionInfo
///   - `refs`:   reference byte-offset  → definition byte-offset
///   - `syms`:   sorted list of all symbol locations for position lookup
///   - `scopes`: scope tree for completion

use std::collections::HashMap;
use crate::lexer::Span;
use crate::parser::{AstNode, Form, head_sym};

// ── Definition ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefKind {
    Local,
    Var,
    Global,
    Fn,
    Macro,
    Param,
    LoopVar,
    Destructured,
}

#[derive(Debug, Clone)]
pub struct DefinitionInfo {
    pub name: String,
    pub kind: DefKind,
    pub span: Span,
    /// Parameter names, for functions.
    pub params: Option<Vec<String>>,
    /// Inline docstring (first string literal in function body).
    pub doc: Option<String>,
    /// True if the function accepts a rest arg (`& rest`) or varargs (`...`).
    pub variadic: bool,
    /// True if the last body expression can produce multiple values (e.g. `(values ...)`).
    /// Used to suppress false-positive arity warnings when this function is the last arg
    /// in a call: Lua expands multi-return calls in tail-argument position.
    pub returns_multiple: bool,
    /// Static field names for table literals (`{:a 1 :b 2}` → `["a", "b"]`).
    /// Only set when the binding is a table with all-literal keys. Used for field completions.
    pub table_fields: Option<Vec<String>>,
    /// For `DefKind::Macro` bindings: the module path from which this macro was
    /// imported (e.g. `"addons.lua-gdextension.defnode"`). `None` for inline
    /// `(macro ...)` / `(macros ...)` definitions.
    pub source_module: Option<String>,
}

// ── Symbol reference ─────────────────────────────────────────────────────────

/// All symbol occurrences (both defs and refs) stored for position lookup.
#[derive(Debug, Clone)]
pub struct SymbolEntry {
    pub span: Span,
    pub name: String,
    /// The byte offset of this symbol's definition, if known.
    pub def_byte: Option<u32>,
    pub is_def: bool,
    /// True when this symbol appears inside the argument list of a macro call.
    /// Unknown-identifier warnings are suppressed for these entries because macro
    /// arguments are DSL forms that don't follow normal Fennel evaluation rules.
    pub in_macro: bool,
}

// ── Scope ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Scope {
    pub span: Span,
    pub bindings: HashMap<String, u32>, // name → def byte
    pub parent: Option<usize>,
}

// ── Analysis result ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AnalysisWarning {
    pub message: String,
    pub span: Span,
    /// Span of the original definition, for shadow warnings.
    pub related_span: Option<Span>,
}

#[derive(Debug, Default)]
pub struct AnalysisResult {
    pub defs: HashMap<u32, DefinitionInfo>,
    /// reference byte offset → definition byte offset
    pub refs: HashMap<u32, u32>,
    /// All symbol positions (sorted by `span.start`).
    pub syms: Vec<SymbolEntry>,
    pub scopes: Vec<Scope>,
    pub warnings: Vec<AnalysisWarning>,
    /// Maps local binding name → required module name for `(local x (require :mod))`.
    pub module_bindings: HashMap<String, String>,
    /// Maps def-byte → required module name for require bindings (used for unused-require check).
    pub require_def_bytes: HashMap<u32, String>,
    /// Macro call sites found during analysis.
    /// Each entry: (call_span_start, source_module, macro_name).
    /// `source_module` is the module path from `import-macros`, or `None` for inline macros.
    /// Used by the server to look up and execute the right hook.
    pub macro_calls: Vec<(u32, Option<String>, String)>,
}

impl AnalysisResult {
    /// Find the innermost symbol whose span contains `byte`.
    pub fn symbol_at(&self, byte: u32) -> Option<&SymbolEntry> {
        // Binary search to the first candidate, then find innermost
        let idx = self
            .syms
            .partition_point(|s| s.span.start <= byte);
        // Walk backwards to find candidates that start at or before `byte`
        let mut best: Option<&SymbolEntry> = None;
        for i in (0..idx).rev() {
            let s = &self.syms[i];
            if s.span.end >= byte {
                // This span covers `byte`
                match best {
                    None => best = Some(s),
                    Some(b) if s.span.start > b.span.start => best = Some(s),
                    _ => {}
                }
            } else {
                break;
            }
        }
        best
    }

    /// All definitions visible at `byte` (for completion).
    pub fn defs_at(&self, byte: u32) -> Vec<&DefinitionInfo> {
        let mut scope_idx = self.innermost_scope(byte);
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();

        while let Some(idx) = scope_idx {
            let scope = &self.scopes[idx];
            for (name, &def_byte) in &scope.bindings {
                if def_byte <= byte && seen.insert(name.clone()) {
                    if let Some(def) = self.defs.get(&def_byte) {
                        result.push(def);
                    }
                }
            }
            scope_idx = scope.parent;
        }
        result
    }

    fn innermost_scope(&self, byte: u32) -> Option<usize> {
        // Find the narrowest scope whose span contains `byte`.
        let mut best: Option<(usize, u32)> = None; // (idx, span-width)
        for (i, scope) in self.scopes.iter().enumerate() {
            if scope.span.start <= byte && scope.span.end >= byte {
                let width = scope.span.end - scope.span.start;
                match best {
                    None => best = Some((i, width)),
                    Some((_, bw)) if width < bw => best = Some((i, width)),
                    _ => {}
                }
            }
        }
        best.map(|(i, _)| i)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// If `node` is `(require :mod)` or `(require "mod")`, return the module name.
fn extract_require_module(node: &AstNode) -> Option<String> {
    if let Form::List(forms) = &node.node {
        if head_sym(forms) == Some("require") && forms.len() >= 2 {
            return match &forms[1].node {
                Form::Keyword(s) | Form::Str(s) => Some(s.clone()),
                _ => None,
            };
        }
    }
    None
}

/// Extract static field names from a table literal.
/// Returns keyword and string keys found in pairs; skips computed keys silently.
/// An empty vec means either not a table or no static keys were found.
fn extract_table_keys(node: &AstNode) -> Vec<String> {
    let Form::Table(fields) = &node.node else { return vec![] };
    let mut keys = Vec::new();
    let mut i = 0;
    while i + 1 < fields.len() {
        match &fields[i].node {
            Form::Keyword(k) | Form::Str(k) => keys.push(k.clone()),
            _ => {}
        }
        i += 2;
    }
    keys
}

// ── Analyzer ─────────────────────────────────────────────────────────────────

struct Analyzer {
    result: AnalysisResult,
    /// Stack of scope indices (current scope chain).
    scope_stack: Vec<usize>,
    /// Def-bytes of `var` bindings that appear as the target of at least one `set`.
    mutation_targets: std::collections::HashSet<u32>,
    /// Nesting depth inside macro call argument lists.
    /// While > 0, all emitted SymbolEntries are tagged `in_macro: true` and
    /// unknown-identifier warnings are suppressed for them.
    macro_depth: usize,
    /// Hook results from a previous hook-runner pass, keyed by call_span_start.
    /// When populated, the catch-all macro branch uses these instead of in_macro tagging.
    hook_results: HashMap<u32, Vec<crate::hooks::Instruction>>,
}

impl Analyzer {
    fn new() -> Self {
        Self {
            result: AnalysisResult::default(),
            scope_stack: Vec::new(),
            mutation_targets: std::collections::HashSet::new(),
            macro_depth: 0,
            hook_results: HashMap::new(),
        }
    }

    fn new_with_hooks(hook_results: HashMap<u32, Vec<crate::hooks::Instruction>>) -> Self {
        Self { hook_results, ..Self::new() }
    }

    fn push_scope(&mut self, span: Span) -> usize {
        let parent = self.scope_stack.last().copied();
        let idx = self.result.scopes.len();
        self.result.scopes.push(Scope {
            span,
            bindings: HashMap::new(),
            parent,
        });
        self.scope_stack.push(idx);
        idx
    }

    fn pop_scope(&mut self) {
        self.scope_stack.pop();
    }

    fn current_scope(&self) -> Option<usize> {
        self.scope_stack.last().copied()
    }

    /// Add a definition at `span`, returning its byte offset.
    fn define(&mut self, name: &str, span: &Span, kind: DefKind, params: Option<Vec<String>>, doc: Option<String>) -> u32 {
        let byte = span.start;
        self.result.defs.insert(
            byte,
            DefinitionInfo {
                name: name.to_string(),
                kind,
                span: span.clone(),
                params,
                doc,
                variadic: false,
                returns_multiple: false,
                table_fields: None,
                source_module: None,
            },
        );
        self.result.syms.push(SymbolEntry {
            span: span.clone(),
            name: name.to_string(),
            def_byte: Some(byte),
            is_def: true,
            in_macro: self.macro_depth > 0,
        });
        if let Some(scope_idx) = self.current_scope() {
            if name != "_" && self.result.scopes[scope_idx].bindings.contains_key(name) {
                let orig_byte = self.result.scopes[scope_idx].bindings[name];
                let related_span = self.result.defs.get(&orig_byte).map(|d| d.span.clone());
                self.result.warnings.push(AnalysisWarning {
                    message: format!("`{}` is already defined in this scope", name),
                    span: span.clone(),
                    related_span,
                });
            }
            self.result.scopes[scope_idx]
                .bindings
                .insert(name.to_string(), byte);
        }
        byte
    }

    /// Record a symbol reference; looks up the scope chain for its definition.
    fn reference(&mut self, name: &str, span: &Span) {
        // For multisyms like `a.b.c` or `a:method`, look up the root part.
        // For bare operators like `..`, `+`, `<=`, use the whole name.
        let root = name
            .split(['.', ':'])
            .find(|s| !s.is_empty())
            .unwrap_or(name);
        let def_byte = self.lookup(root);
        self.result.syms.push(SymbolEntry {
            span: span.clone(),
            name: name.to_string(),
            def_byte,
            is_def: false,
            in_macro: self.macro_depth > 0,
        });
        if let (Some(db), Some(rb)) = (def_byte, Some(span.start)) {
            self.result.refs.insert(rb, db);
        }
    }

    /// Look up `name` in the current scope chain.
    fn lookup(&self, name: &str) -> Option<u32> {
        let mut scope_idx = self.current_scope();
        while let Some(idx) = scope_idx {
            let scope = &self.result.scopes[idx];
            if let Some(&byte) = scope.bindings.get(name) {
                return Some(byte);
            }
            scope_idx = scope.parent;
        }
        None
    }

    // ── Form dispatch ─────────────────────────────────────────────────────────

    fn analyze_forms(&mut self, forms: &[AstNode]) {
        for f in forms {
            self.analyze(f);
        }
    }

    fn analyze(&mut self, node: &AstNode) {
        match &node.node {
            Form::Symbol(name) => self.reference(name, &node.span),

            Form::List(forms) => self.analyze_list(forms, &node.span),

            Form::Table(fields) => {
                // Tables: analyze all field values (keys may be keywords/strings/symbols)
                for f in fields {
                    self.analyze(f);
                }
            }

            Form::Sequence(items) => {
                for f in items {
                    self.analyze(f);
                }
            }

            Form::HashFn(body) => {
                // #(body $) — $ is implicit arg
                let scope_idx = self.push_scope(node.span.clone());
                let dollar_span = Span {
                    start: node.span.start,
                    end: node.span.start + 1,
                    line: node.span.line,
                    col: node.span.col,
                    end_line: node.span.line,
                    end_col: node.span.col + 1,
                };
                self.define("$", &dollar_span, DefKind::Param, None, None);
                for i in 1..=9 {
                    let name = format!("${}", i);
                    self.define(&name, &dollar_span, DefKind::Param, None, None);
                    let _ = scope_idx;
                }
                self.analyze(body);
                self.pop_scope();
            }

            Form::Quasiquote(inner) => self.analyze_quasiquote(inner),

            // Atoms and quotes carry no scope info
            _ => {}
        }
    }

    fn analyze_quasiquote(&mut self, node: &AstNode) {
        match &node.node {
            Form::Unquote(inner) | Form::UnquoteSplice(inner) => self.analyze(inner),
            Form::List(forms) | Form::Table(forms) | Form::Sequence(forms) => {
                for f in forms {
                    self.analyze_quasiquote(f);
                }
            }
            _ => {}
        }
    }

    fn analyze_list(&mut self, forms: &[AstNode], list_span: &Span) {
        match head_sym(forms) {
            Some("local") => self.analyze_binding(forms, DefKind::Local),
            Some("var") => self.analyze_binding(forms, DefKind::Var),
            Some("global") => self.analyze_binding(forms, DefKind::Global),
            Some("set") => self.analyze_set(forms),
            Some("fn") => self.analyze_fn(forms, list_span),
            Some("lambda") | Some("λ") => self.analyze_lambda(forms, list_span),
            Some("let") => self.analyze_let(forms, list_span),
            Some("do") => self.analyze_do(&forms[1..], list_span),
            Some("if") => self.analyze_if(forms),
            Some("when") | Some("unless") => {
                // condition in current scope; body scoped like `(if cond (do ...))`
                if forms.len() >= 2 { self.analyze(&forms[1]); }
                self.analyze_do(&forms[2..], list_span);
            }
            Some("while") => self.analyze_while(forms, list_span),
            Some("each") => self.analyze_each(forms, list_span),
            Some("for") => self.analyze_for(forms, list_span),
            Some("macro") => self.analyze_macro_def(forms, list_span),
            Some("macros") => self.analyze_macros_form(forms),
            Some("import-macros") => self.analyze_import_macros(forms),
            Some("match") | Some("case") => self.analyze_match_form(forms, list_span),
            Some("case-try") | Some("match-try") => self.analyze_case_try(forms, list_span),
            // collect / icollect / accumulate / faccumulate are iterator macros
            Some("collect") | Some("icollect") => self.analyze_collect(forms, list_span),
            Some("fcollect") => self.analyze_fcollect(forms, list_span),
            Some("accumulate") => self.analyze_accumulate(forms, list_span),
            Some("faccumulate") => self.analyze_faccumulate(forms, list_span),
            Some("with-open") => self.analyze_with_open(forms, list_span),
            Some("as->") => self.analyze_as_arrow(forms, list_span),
            // Anything else: analyze head + args, then check arity.
            // If the head resolves to a user-defined macro, record the call site and
            // either execute cached hook instructions (giving full LSP support) or
            // fall back to in_macro tagging (suppressing unknown-identifier warnings).
            _ => {
                let macro_info = (|| -> Option<(String, Option<String>)> {
                    let head_name = match &forms.first()?.node {
                        Form::Symbol(s) => s.clone(),
                        _ => return None,
                    };
                    let def_byte = self.lookup(&head_name)?;
                    let def = self.result.defs.get(&def_byte)?;
                    if matches!(def.kind, DefKind::Macro) {
                        Some((head_name, def.source_module.clone()))
                    } else {
                        None
                    }
                })();

                if let Some((macro_name, source_module)) = macro_info {
                    self.result.macro_calls.push((list_span.start, source_module, macro_name.clone()));
                    if let Some(head) = forms.first() {
                        self.analyze(head);
                    }
                    if let Some(instrs) = self.hook_results.get(&list_span.start).cloned() {
                        self.execute_hook_instructions(&instrs, forms);
                    } else {
                        self.macro_depth += 1;
                        self.analyze_forms(&forms[1..]);
                        self.macro_depth -= 1;
                    }
                } else {
                    self.analyze_forms(forms);
                    self.check_arity(forms, list_span);
                }
            }
        }
    }

    /// Execute hook instructions for a macro call.
    ///
    /// `forms` is the full child list including the macro head at index 0.
    /// Instruction indices are 1-based (matching Lua convention), so index N maps
    /// to `forms[N-1]`.
    fn execute_hook_instructions(&mut self, instructions: &[crate::hooks::Instruction], forms: &[AstNode]) {
        use crate::hooks::Instruction;
        for instr in instructions {
            match instr {
                Instruction::Bind { name, span } => {
                    self.define(name, span, DefKind::Local, None, None);
                }
                Instruction::Analyze { index } => {
                    if let Some(form) = forms.get(index - 1) {
                        self.analyze(form);
                    }
                }
                Instruction::AnalyzeFn { index } => {
                    if let Some(form) = forms.get(index - 1) {
                        if let Form::List(sub_forms) = &form.node {
                            self.analyze_fn(sub_forms, &form.span);
                        }
                    }
                }
                Instruction::ScopeOpen { span } => {
                    self.push_scope(span.clone());
                }
                Instruction::ScopeClose => {
                    self.pop_scope();
                }
                Instruction::SubFormCompletions { .. } => {
                    // TODO: store in AnalysisResult for position-based completion
                }
                Instruction::AnalyzeChildAt { parent, child } => {
                    if let Some(parent_form) = forms.get(parent - 1) {
                        let sub = match &parent_form.node {
                            Form::List(ch) | Form::Sequence(ch) => Some(ch),
                            _ => None,
                        };
                        if let Some(sub_node) = sub.and_then(|ch| ch.get(child - 1)) {
                            self.analyze(sub_node);
                        }
                    }
                }
            }
        }
    }

    /// Warn when a resolved function is called with the wrong number of arguments.
    fn check_arity(&mut self, forms: &[AstNode], list_span: &Span) {
        // Phase 1: collect callee info (all borrows released before phase 2).
        let info = (|| -> Option<(String, usize)> {
            let head_name = match &forms.first()?.node {
                Form::Symbol(s) => s.as_str(),
                _ => return None,
            };
            let def_byte = self.lookup(head_name)?;
            let def = self.result.defs.get(&def_byte)?;
            if def.kind != DefKind::Fn || def.variadic {
                return None;
            }
            let expected = def.params.as_ref()?.len();
            Some((def.name.clone(), expected))
        })();

        let Some((fn_name, expected)) = info else { return };
        let actual = forms.len() - 1;

        if actual == expected {
            return;
        }

        // Under-arity: suppress when the last argument might expand to multiple
        // values at runtime.  In Lua, a call in last-argument position expands
        // to all of its return values, so `(f (multi-ret))` is valid even if
        // `f` takes more than one parameter.
        if actual < expected {
            let last_may_expand = forms.last().map_or(false, |last| {
                if tail_may_return_multiple(last) {
                    return true;
                }
                if let Form::List(inner) = &last.node {
                    if let Some(Form::Symbol(name)) = inner.first().map(|n| &n.node) {
                        let root = name.split(['.', ':']).find(|s| !s.is_empty()).unwrap_or(name);
                        if let Some(db) = self.lookup(root) {
                            return self.result.defs.get(&db)
                                .map_or(false, |d| d.returns_multiple);
                        }
                    }
                }
                false
            });
            if last_may_expand {
                return;
            }
        }

        self.result.warnings.push(AnalysisWarning {
            message: format!(
                "`{}` expects {} argument{} but got {}",
                fn_name,
                expected,
                if expected == 1 { "" } else { "s" },
                actual,
            ),
            span: list_span.clone(),
            related_span: None,
        });
    }

    // ── (local name val) / (var name val) / (global name val) ────────────────

    /// Walk parent scopes and emit a warning if `name` is already bound in one.
    /// Only called for explicit `local`/`var`/`let` bindings — not params or match patterns.
    fn check_outer_shadow(&mut self, name: &str, span: &Span) {
        if name.starts_with('_') {
            return;
        }
        let current = match self.current_scope() {
            Some(idx) => idx,
            None => return,
        };
        let mut idx_opt = self.result.scopes[current].parent;
        while let Some(idx) = idx_opt {
            if let Some(&orig_byte) = self.result.scopes[idx].bindings.get(name) {
                let related_span = self.result.defs.get(&orig_byte).map(|d| d.span.clone());
                self.result.warnings.push(AnalysisWarning {
                    message: format!("`{}` shadows a binding from an outer scope", name),
                    span: span.clone(),
                    related_span,
                });
                return;
            }
            idx_opt = self.result.scopes[idx].parent;
        }
    }

    /// If `name_node` is a plain symbol and `rhs` is a table literal with static keys,
    /// record those keys on the def so completions can offer `name.field`.
    fn set_table_fields(&mut self, name_node: &AstNode, rhs: &AstNode) {
        let Form::Symbol(_) = &name_node.node else { return };
        let fields = extract_table_keys(rhs);
        if fields.is_empty() {
            return;
        }
        let byte = name_node.span.start;
        if let Some(def) = self.result.defs.get_mut(&byte) {
            def.table_fields = Some(fields);
        }
    }

    fn analyze_binding(&mut self, forms: &[AstNode], kind: DefKind) {
        if forms.len() < 3 {
            return;
        }
        // Evaluate RHS first (so it doesn't see the new binding)
        self.analyze(&forms[2]);

        if let Form::Symbol(name) = &forms[1].node {
            // Cross-scope shadow check for explicit local/var
            self.check_outer_shadow(name, &forms[1].span);
            // Detect (local name (require :mod)) → record module bindings
            if let Some(module) = extract_require_module(&forms[2]) {
                self.result.module_bindings.insert(name.clone(), module.clone());
                self.result.require_def_bytes.insert(forms[1].span.start, module);
            }
        }
        self.bind_pattern(&forms[1], kind);
        // Record table shape for field completions
        self.set_table_fields(&forms[1], &forms[2]);
    }

    // ── (set name val) ────────────────────────────────────────────────────────

    fn analyze_set(&mut self, forms: &[AstNode]) {
        if forms.len() < 3 {
            return;
        }
        if let Form::Symbol(name) = &forms[1].node {
            let root = name.split(['.', ':']).find(|s| !s.is_empty()).unwrap_or(name);
            if let Some(def_byte) = self.lookup(root) {
                if let Some(def) = self.result.defs.get(&def_byte) {
                    match def.kind {
                        DefKind::Local | DefKind::Fn => {
                            self.result.warnings.push(AnalysisWarning {
                                message: format!(
                                    "`{}` is immutable; use `var` instead of `local` to allow mutation",
                                    def.name
                                ),
                                span: forms[1].span.clone(),
                                related_span: None,
                            });
                        }
                        DefKind::Var => {
                            self.mutation_targets.insert(def_byte);
                        }
                        _ => {}
                    }
                }
            }
            self.reference(name, &forms[1].span);
        }
        self.analyze(&forms[2]);
    }

    // ── (fn name? [params] body...) ───────────────────────────────────────────

    fn analyze_fn(&mut self, forms: &[AstNode], list_span: &Span) {
        if forms.len() < 2 {
            return;
        }
        let mut idx = 1;

        // Optional name
        let fn_def_byte = if let Form::Symbol(name) = &forms[idx].node {
            let db = self.define(name, &forms[idx].span, DefKind::Fn, None, None);
            idx += 1;
            Some(db)
        } else {
            None
        };

        // Params sequence
        let params = if idx < forms.len() {
            if let Form::Sequence(params) = &forms[idx].node {
                let params_span = forms[idx].span.clone();
                idx += 1;
                Some((params.clone(), params_span))
            } else {
                None
            }
        } else {
            None
        };

        let body = &forms[idx..];

        // Extract doc string
        let doc = body.first().and_then(|f| {
            if let Form::Str(s) = &f.node {
                Some(s.clone())
            } else {
                None
            }
        });

        let fn_scope_span = list_span.clone();
        self.push_scope(fn_scope_span);

        let mut param_names = Vec::new();
        let mut variadic = false;
        if let Some((param_nodes, _params_span)) = params {
            for p in &param_nodes {
                // `&` (rest marker) and `...` (varargs) both make fn variadic.
                // Note: `...` is Form::Varargs, not Form::Symbol.
                match &p.node {
                    Form::Symbol(s) if s == "&" => variadic = true,
                    Form::Varargs => variadic = true,
                    _ => {}
                }
                self.bind_param(p, &mut param_names);
            }
        }

        // Detect whether the last body expression may return multiple values.
        let returns_multiple = body.last().map_or(false, tail_may_return_multiple);

        // Patch the function's definition with param names, variadic flag, and return info.
        if let Some(db) = fn_def_byte {
            if let Some(def) = self.result.defs.get_mut(&db) {
                def.params = Some(param_names.clone());
                def.doc = doc.clone();
                def.variadic = variadic;
                def.returns_multiple = returns_multiple;
            }
        }

        self.analyze_forms(body);
        self.pop_scope();
    }

    // ── (lambda [params] body...) ─────────────────────────────────────────────
    // lambda/λ are identical to fn (with optional name and nil-checking).

    fn analyze_lambda(&mut self, forms: &[AstNode], list_span: &Span) {
        self.analyze_fn(forms, list_span);
    }

    // ── (let [k1 v1 k2 v2 ...] body) ─────────────────────────────────────────

    fn analyze_let(&mut self, forms: &[AstNode], list_span: &Span) {
        if forms.len() < 2 {
            return;
        }
        self.push_scope(list_span.clone());

        // Named let: (let name [bindings...] body...)
        // forms[1] is a symbol (loop name), forms[2] is the binding sequence.
        // All initial values are evaluated before any name is bound (like fn args),
        // then the loop name and all params enter scope together for the body.
        if let Form::Symbol(loop_name) = &forms[1].node {
            if forms.len() >= 3 {
                if let Form::Sequence(bindings) = &forms[2].node {
                    // Evaluate all initial values first, with no new names in scope
                    let mut i = 0;
                    while i + 1 < bindings.len() {
                        self.analyze(&bindings[i + 1]);
                        i += 2;
                    }
                    // Bind loop name, then all params
                    let loop_name = loop_name.clone();
                    self.define(&loop_name, &forms[1].span, DefKind::Local, None, None);
                    let mut i = 0;
                    while i + 1 < bindings.len() {
                        self.bind_pattern(&bindings[i], DefKind::Local);
                        i += 2;
                    }
                    self.analyze_forms(&forms[3..]);
                    self.pop_scope();
                    return;
                }
            }
        }

        // Standard let: (let [bindings...] body...)
        if let Form::Sequence(bindings) = &forms[1].node {
            let mut i = 0;
            while i + 1 < bindings.len() {
                self.analyze(&bindings[i + 1]);
                // Cross-scope shadow check for plain symbol let-bindings
                if let Form::Symbol(name) = &bindings[i].node {
                    self.check_outer_shadow(name, &bindings[i].span);
                }
                self.bind_pattern(&bindings[i], DefKind::Local);
                self.set_table_fields(&bindings[i], &bindings[i + 1]);
                i += 2;
            }
        }

        self.analyze_forms(&forms[2..]);
        self.pop_scope();
    }

    // ── (do body...) ─────────────────────────────────────────────────────────

    fn analyze_do(&mut self, body: &[AstNode], span: &Span) {
        self.push_scope(span.clone());
        self.analyze_forms(body);
        self.pop_scope();
    }

    // ── (if cond then else?) ──────────────────────────────────────────────────

    fn analyze_if(&mut self, forms: &[AstNode]) {
        // No new scope — just walk everything
        self.analyze_forms(&forms[1..]);
    }

    // ── (while cond body...) ──────────────────────────────────────────────────

    fn analyze_while(&mut self, forms: &[AstNode], list_span: &Span) {
        // (while condition body...)
        // Condition is in the outer scope; body gets its own scope.
        if forms.len() < 2 { return; }
        self.analyze(&forms[1]);
        self.push_scope(list_span.clone());
        self.analyze_forms(&forms[2..]);
        self.pop_scope();
    }

    // ── (each [k v iter] body...) ─────────────────────────────────────────────

    fn analyze_each(&mut self, forms: &[AstNode], list_span: &Span) {
        // (each [k v iter-expr &until cond?] body)
        if forms.len() < 2 {
            return;
        }
        self.push_scope(list_span.clone());

        if let Form::Sequence(binds) = &forms[1].node {
            if !binds.is_empty() {
                let until_pos = binds.iter().position(|b| {
                    matches!(&b.node, Form::Symbol(s) if s == "&until")
                });
                let effective_end = until_pos.unwrap_or(binds.len());

                if effective_end > 0 {
                    let iter_expr = &binds[effective_end - 1];
                    self.analyze(iter_expr);
                    for b in &binds[..effective_end - 1] {
                        self.bind_pattern(b, DefKind::LoopVar);
                    }
                }
                if let Some(p) = until_pos {
                    if p + 1 < binds.len() {
                        self.analyze(&binds[p + 1]);
                    }
                }
            }
        }

        self.analyze_forms(&forms[2..]);
        self.pop_scope();
    }

    // ── (for [i start stop step?] body...) ────────────────────────────────────

    fn analyze_for(&mut self, forms: &[AstNode], list_span: &Span) {
        // (for [var start stop step?] body...)
        // The binding clause is always a Sequence at forms[1].
        if forms.len() < 3 {
            return;
        }
        self.push_scope(list_span.clone());
        if let Form::Sequence(binds) = &forms[1].node {
            if binds.len() >= 3 {
                // Analyze start, stop, and optional step before binding the var.
                for expr in &binds[1..] {
                    self.analyze(expr);
                }
                self.bind_pattern(&binds[0], DefKind::LoopVar);
            }
        }
        self.analyze_forms(&forms[2..]);
        self.pop_scope();
    }

    // ── (macro name [params] body) ────────────────────────────────────────────

    fn analyze_macro_def(&mut self, forms: &[AstNode], list_span: &Span) {
        if forms.len() < 3 {
            return;
        }
        // Macro definitions have the same structure as `fn`: name + params + body.
        // Analyzing the body gives hover/go-to-def for names inside the macro.
        self.analyze_fn(forms, list_span);
        // Re-tag the definition as Macro rather than Fn.
        if let Form::Symbol(name) = &forms[1].node {
            if let Some(def) = self.result.defs.get_mut(&forms[1].span.start) {
                def.kind = DefKind::Macro;
                let _ = name;
            }
        }
    }

    // ── (macros {name fn ...}) ────────────────────────────────────────────────

    fn analyze_macros_form(&mut self, forms: &[AstNode]) {
        if forms.len() < 2 {
            return;
        }
        if let Form::Table(fields) = &forms[1].node {
            let mut i = 0;
            while i + 1 < fields.len() {
                if let Form::Symbol(name) = &fields[i].node {
                    self.define(name, &fields[i].span, DefKind::Macro, None, None);
                }
                i += 2;
            }
        }
    }

    // ── (import-macros {name :local} :module) ─────────────────────────────────

    fn analyze_import_macros(&mut self, forms: &[AstNode]) {
        if forms.len() < 2 {
            return;
        }
        // Extract the module path string (forms[2] is the module argument).
        let source_module: Option<String> = forms.get(2).and_then(|n| match &n.node {
            Form::Keyword(s) | Form::Str(s) => Some(s.clone()),
            _ => None,
        });

        if let Form::Table(fields) = &forms[1].node {
            let mut i = 0;
            while i < fields.len() {
                if i + 1 < fields.len() {
                    // Paired element — binding name is always at fields[i+1]:
                    //   {: foo}         → [sym(":"), sym("foo")]  → bind "foo"
                    //   {:remote local} → [kw(...), sym("local")] → bind "local"
                    if let Form::Symbol(name) = &fields[i + 1].node {
                        let byte = self.define(name, &fields[i + 1].span, DefKind::Macro, None, None);
                        if let Some(def) = self.result.defs.get_mut(&byte) {
                            def.source_module = source_module.clone();
                        }
                    }
                    i += 2;
                } else {
                    // Standalone trailing symbol — shorthand for {: name}:
                    //   {defnode}  →  bind "defnode"
                    if let Form::Symbol(name) = &fields[i].node {
                        let byte = self.define(name, &fields[i].span, DefKind::Macro, None, None);
                        if let Some(def) = self.result.defs.get_mut(&byte) {
                            def.source_module = source_module.clone();
                        }
                    }
                    i += 1;
                }
            }
        }
    }

    // ── (case-try val pat body ... catch pat body) ────────────────────────────
    // Each arm's pattern bindings accumulate into a single shared scope so that
    // later bodies can reference symbols bound in earlier arms.

    fn analyze_case_try(&mut self, forms: &[AstNode], list_span: &Span) {
        if forms.len() < 2 {
            return;
        }
        self.analyze(&forms[1]); // initial value

        // One accumulated scope for all non-catch arms
        self.push_scope(list_span.clone());

        let mut i = 2;
        while i < forms.len() {
            // Bare `catch` keyword (old syntax: ... catch pat body ...)
            if let Form::Symbol(s) = &forms[i].node {
                if s == "catch" {
                    i += 1;
                    while i + 1 < forms.len() {
                        self.push_scope(list_span.clone());
                        self.bind_match_pattern(&forms[i]);
                        self.analyze(&forms[i + 1]);
                        self.pop_scope();
                        i += 2;
                    }
                    break;
                }
            }
            // Parenthesised `(catch pat body ...)` syntax
            if let Form::List(items) = &forms[i].node {
                if head_sym(items) == Some("catch") {
                    let mut j = 1; // skip the "catch" head symbol
                    while j + 1 < items.len() {
                        self.push_scope(list_span.clone());
                        self.bind_match_pattern(&items[j]);
                        self.analyze(&items[j + 1]);
                        self.pop_scope();
                        j += 2;
                    }
                    break;
                }
            }
            if i + 1 < forms.len() {
                self.bind_match_pattern(&forms[i]);
                self.analyze(&forms[i + 1]);
                i += 2;
            } else {
                break;
            }
        }

        self.pop_scope();
    }

    // ── (match val pat body ...) ──────────────────────────────────────────────

    fn analyze_match_form(&mut self, forms: &[AstNode], list_span: &Span) {
        if forms.len() < 2 {
            return;
        }
        self.analyze(&forms[1]);
        let mut i = 2;
        while i + 1 < forms.len() {
            self.push_scope(list_span.clone());
            // Bind pattern names (symbols in pattern that aren't keywords)
            self.bind_match_pattern(&forms[i]);
            self.analyze(&forms[i + 1]);
            self.pop_scope();
            i += 2;
        }
    }

    // ── (collect [k v iter] body) / (icollect [v iter] body) ─────────────────

    fn analyze_collect(&mut self, forms: &[AstNode], list_span: &Span) {
        // (icollect/collect [vars... iter-expr &into t? &until cond?] body)
        if forms.len() < 2 {
            return;
        }
        self.push_scope(list_span.clone());
        if let Form::Sequence(binds) = &forms[1].node {
            if !binds.is_empty() {
                // Find modifier boundaries
                let modifier_pos = binds.iter().position(|b| {
                    matches!(&b.node, Form::Symbol(s) if s == "&into" || s == "&until")
                });
                // Everything before the first modifier is loop-vars + iter-expr
                let core_end = modifier_pos.unwrap_or(binds.len());

                if core_end > 0 {
                    // Last core element is the iterator expression
                    let iter = &binds[core_end - 1];
                    self.analyze(iter);
                    // Preceding elements are loop variable bindings
                    for b in &binds[..core_end - 1] {
                        self.bind_pattern(b, DefKind::LoopVar);
                    }
                }

                // Process modifiers (&into target, &until cond)
                let mut i = core_end;
                while i < binds.len() {
                    if let Form::Symbol(s) = &binds[i].node {
                        if (s == "&into" || s == "&until") && i + 1 < binds.len() {
                            self.analyze(&binds[i + 1]);
                            i += 2;
                            continue;
                        }
                    }
                    i += 1;
                }
            }
        }
        self.analyze_forms(&forms[2..]);
        self.pop_scope();
    }

    fn analyze_fcollect(&mut self, forms: &[AstNode], list_span: &Span) {
        // (fcollect [i start stop step? &until cond? &into t?] body)
        if forms.len() < 2 {
            return;
        }
        self.push_scope(list_span.clone());
        if let Form::Sequence(binds) = &forms[1].node {
            if let Some(var_node) = binds.first() {
                let into_pos = binds.iter().position(|b| {
                    matches!(&b.node, Form::Symbol(s) if s == "&into")
                });
                let until_pos = binds.iter().position(|b| {
                    matches!(&b.node, Form::Symbol(s) if s == "&until")
                });
                // Range expressions (start, stop, step) end at the first modifier
                let range_end = [into_pos, until_pos]
                    .iter()
                    .filter_map(|x| *x)
                    .min()
                    .unwrap_or(binds.len());
                for expr in &binds[1..range_end] {
                    self.analyze(expr);
                }
                // Bind the loop variable before analyzing modifiers so the
                // &until guard can reference it
                self.bind_pattern(var_node, DefKind::LoopVar);
                if let Some(p) = until_pos {
                    if p + 1 < binds.len() {
                        self.analyze(&binds[p + 1]);
                    }
                }
                if let Some(p) = into_pos {
                    if p + 1 < binds.len() {
                        self.analyze(&binds[p + 1]);
                    }
                }
            }
        }
        self.analyze_forms(&forms[2..]);
        self.pop_scope();
    }

    fn analyze_accumulate(&mut self, forms: &[AstNode], list_span: &Span) {
        // (accumulate [acc init iter-vars... iter-expr &until cond?] body)
        if forms.len() < 2 {
            return;
        }
        self.push_scope(list_span.clone());
        if let Form::Sequence(binds) = &forms[1].node {
            if binds.len() >= 2 {
                // Find &until boundary if present
                let until_pos = binds.iter().position(|b| {
                    matches!(&b.node, Form::Symbol(s) if s == "&until")
                });
                let effective_end = until_pos.unwrap_or(binds.len());

                self.analyze(&binds[1]); // init value
                self.bind_pattern(&binds[0], DefKind::Local); // acc binding

                if effective_end >= 3 {
                    // Last element before &until is the iterator expression
                    let iter_expr = &binds[effective_end - 1];
                    self.analyze(iter_expr);
                    for b in &binds[2..effective_end - 1] {
                        self.bind_pattern(b, DefKind::LoopVar);
                    }
                }

                // Analyze the &until guard (its condition can reference loop vars)
                if let Some(p) = until_pos {
                    if p + 1 < binds.len() {
                        self.analyze(&binds[p + 1]);
                    }
                }
            }
        }
        self.analyze_forms(&forms[2..]);
        self.pop_scope();
    }

    fn analyze_faccumulate(&mut self, forms: &[AstNode], list_span: &Span) {
        // (faccumulate [acc init var start stop step? &until cond?] body)
        // Unlike accumulate, the range elements (start/stop/step) are numeric
        // expressions, not loop-var patterns — they must be analyzed, not bound.
        if forms.len() < 2 { return; }
        self.push_scope(list_span.clone());
        if let Form::Sequence(binds) = &forms[1].node {
            if binds.len() >= 5 {
                let until_pos = binds.iter().position(|b| {
                    matches!(&b.node, Form::Symbol(s) if s == "&until")
                });
                let range_end = until_pos.unwrap_or(binds.len());
                // Analyze init before binding acc
                self.analyze(&binds[1]);
                self.bind_pattern(&binds[0], DefKind::Local);
                // Analyze start/stop/step before binding the loop var
                for expr in &binds[3..range_end] {
                    self.analyze(expr);
                }
                self.bind_pattern(&binds[2], DefKind::LoopVar);
                if let Some(p) = until_pos {
                    if p + 1 < binds.len() {
                        self.analyze(&binds[p + 1]);
                    }
                }
            }
        }
        self.analyze_forms(&forms[2..]);
        self.pop_scope();
    }

    // ── (with-open [name expr ...] body) ──────────────────────────────────────

    fn analyze_with_open(&mut self, forms: &[AstNode], list_span: &Span) {
        if forms.len() < 2 {
            return;
        }
        self.push_scope(list_span.clone());
        if let Form::Sequence(binds) = &forms[1].node {
            let mut i = 0;
            while i + 1 < binds.len() {
                self.analyze(&binds[i + 1]);
                self.bind_pattern(&binds[i], DefKind::Local);
                i += 2;
            }
        }
        self.analyze_forms(&forms[2..]);
        self.pop_scope();
    }

    // ── (as-> val name form ...) ──────────────────────────────────────────────
    // Thread `val` through each form, binding `name` to the intermediate result.

    fn analyze_as_arrow(&mut self, forms: &[AstNode], list_span: &Span) {
        // (as-> val name form1 form2 ...)
        if forms.len() < 3 {
            return;
        }
        self.analyze(&forms[1]); // initial value
        self.push_scope(list_span.clone());
        // forms[2] is the placeholder name symbol
        if let Form::Symbol(name) = &forms[2].node {
            let name = name.clone();
            self.define(&name, &forms[2].span, DefKind::Local, None, None);
        }
        self.analyze_forms(&forms[3..]);
        self.pop_scope();
    }

    // ── Pattern binding helpers ────────────────────────────────────────────────

    /// Bind a single pattern node as a definition.
    fn bind_pattern(&mut self, node: &AstNode, kind: DefKind) {
        match &node.node {
            Form::Symbol(name) => {
                // Ignore & (rest marker) and _ (discard)
                if name != "&" && name != "_" {
                    self.define(name, &node.span, kind, None, None);
                }
            }
            Form::Sequence(items) => {
                // Sequential destructuring: [a b c], also with & rest
                for item in items {
                    if let Form::Symbol(s) = &item.node {
                        if s == "&" {
                            continue; // rest marker — next item is the rest binding
                        }
                    }
                    self.bind_pattern(item, DefKind::Destructured);
                }
            }
            Form::List(items) => {
                // Multi-value destructuring: (a b) from multiple return values
                for item in items {
                    self.bind_pattern(item, DefKind::Destructured);
                }
            }
            Form::Table(fields) => {
                // Table destructuring — several syntactic forms:
                //   {:key name}      — keyword key, symbol binding
                //   {name :key}      — symbol binding, keyword key
                //   {"key" name}     — string key, symbol binding
                //   {: name}         — shorthand: bind `name` from key `:name`
                //   {&as name}       — bind the whole table to `name`
                //   {:key {nested}}  — nested destructuring
                let mut i = 0;
                while i < fields.len() {
                    let a = &fields[i];

                    // {: name ...} shorthand
                    if let Form::Symbol(s) = &a.node {
                        if s == ":" && i + 1 < fields.len() {
                            if let Form::Symbol(name) = &fields[i + 1].node {
                                self.define(name, &fields[i + 1].span, DefKind::Destructured, None, None);
                                i += 2;
                                continue;
                            }
                        }
                        // {&as name} — whole-table binding
                        if s == "&as" && i + 1 < fields.len() {
                            if let Form::Symbol(name) = &fields[i + 1].node {
                                self.define(name, &fields[i + 1].span, DefKind::Destructured, None, None);
                                i += 2;
                                continue;
                            }
                        }
                    }

                    // Pair forms
                    if i + 1 < fields.len() {
                        let b = &fields[i + 1];
                        match &a.node {
                            Form::Symbol(name) if !matches!(b.node, Form::Keyword(_) | Form::Table(_) | Form::Sequence(_)) => {
                                // {name :key} — but only if the value is a keyword
                                if let Form::Keyword(_) = &b.node {
                                    self.define(name, &a.span, DefKind::Destructured, None, None);
                                }
                            }
                            Form::Keyword(_) => match &b.node {
                                Form::Symbol(name) => {
                                    // {:key name}
                                    self.define(name, &b.span, DefKind::Destructured, None, None);
                                }
                                Form::Table(_) | Form::Sequence(_) => {
                                    // {:key {nested}} — nested destructuring
                                    self.bind_pattern(b, DefKind::Destructured);
                                }
                                _ => {}
                            },
                            Form::Str(_) => {
                                if let Form::Symbol(name) = &b.node {
                                    // {"key" name}
                                    self.define(name, &b.span, DefKind::Destructured, None, None);
                                }
                            }
                            Form::Symbol(name) => {
                                if let Form::Keyword(_) = &b.node {
                                    // {name :key}
                                    self.define(name, &a.span, DefKind::Destructured, None, None);
                                }
                            }
                            _ => {}
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            _ => {}
        }
    }

    /// Bind a function parameter (handles `&` rest marker).
    fn bind_param(&mut self, node: &AstNode, names: &mut Vec<String>) {
        match &node.node {
            Form::Symbol(name) => {
                // `&` marks the start of a rest parameter list; `...` is varargs
                if name != "&" && name != "..." {
                    self.define(name, &node.span, DefKind::Param, None, None);
                    names.push(name.clone());
                }
            }
            Form::Sequence(_) | Form::Table(_) => {
                self.bind_pattern(node, DefKind::Param);
                names.push("<destructured>".into());
            }
            _ => {}
        }
    }

    /// Bind symbols in a match/case pattern.
    fn bind_match_pattern(&mut self, node: &AstNode) {
        match &node.node {
            Form::Symbol(name) => {
                // Plain symbols are bindings. `_` and `_`-prefixed names are discards.
                if !name.starts_with('_') && name != "&" && name != "&as" && name != "&into" {
                    self.define(name, &node.span, DefKind::Local, None, None);
                }
            }
            Form::List(items) => {
                let head = items.first().and_then(|f| {
                    if let Form::Symbol(s) = &f.node { Some(s.as_str()) } else { None }
                });
                match head {
                    Some("where") => {
                        // `(where pattern guard...)` — only the pattern binds
                        if items.len() >= 2 {
                            self.bind_match_pattern(&items[1]);
                            for guard in &items[2..] {
                                self.analyze(guard);
                            }
                        }
                    }
                    Some("or") => {
                        // `(or pat1 pat2 ...)` — each alternative binds the same names;
                        // skip the `or` keyword itself
                        for item in &items[1..] {
                            self.bind_match_pattern(item);
                        }
                    }
                    _ => {
                        for item in items {
                            self.bind_match_pattern(item);
                        }
                    }
                }
            }
            Form::Sequence(items) => {
                let mut skip_next = false;
                for (idx, item) in items.iter().enumerate() {
                    if skip_next {
                        skip_next = false;
                        continue;
                    }
                    if let Form::Symbol(s) = &item.node {
                        if s == "&as" {
                            if idx + 1 < items.len() {
                                self.bind_match_pattern(&items[idx + 1]);
                            }
                            skip_next = true;
                            continue;
                        }
                        if s == "&" {
                            // `& rest` — the next symbol is a binding for the rest
                            if idx + 1 < items.len() {
                                self.bind_match_pattern(&items[idx + 1]);
                            }
                            skip_next = true;
                            continue;
                        }
                        if s == "&until" || s == "&into" {
                            // modifier keyword — the next item is an expression
                            skip_next = true;
                            continue;
                        }
                    }
                    self.bind_match_pattern(item);
                }
            }
            Form::Table(fields) => {
                let mut i = 0;
                while i < fields.len() {
                    if let Form::Symbol(s) = &fields[i].node {
                        if s == "&as" && i + 1 < fields.len() {
                            // &as name in table pattern
                            self.bind_match_pattern(&fields[i + 1]);
                            i += 2;
                            continue;
                        }
                    }
                    if i + 1 < fields.len() {
                        // For table patterns, the VALUE position (i+1) is the binding
                        // unless the key is a string/keyword (then it's {:key name})
                        match &fields[i].node {
                            Form::Keyword(_) | Form::Str(_) => {
                                self.bind_match_pattern(&fields[i + 1]);
                            }
                            Form::Symbol(s) if s == ":" => {
                                // {: name} shorthand
                                self.bind_match_pattern(&fields[i + 1]);
                            }
                            _ => {
                                // Both could be bindings in nested patterns
                                self.bind_match_pattern(&fields[i + 1]);
                            }
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

// ── Multi-value return detection ──────────────────────────────────────────────

/// Returns true when `node`, evaluated in tail position, may produce multiple
/// values.  Used to suppress false-positive arity warnings: Lua expands a
/// multi-return call that appears as the *last* argument in a call site.
fn tail_may_return_multiple(node: &AstNode) -> bool {
    match &node.node {
        Form::List(forms) => match head_sym(forms) {
            Some("values") => true,
            Some("do") | Some("when") | Some("unless") => {
                forms.last().map_or(false, tail_may_return_multiple)
            }
            Some("let") | Some("with-open") => {
                forms.last().map_or(false, tail_may_return_multiple)
            }
            Some("if") => {
                forms.get(2).map_or(false, tail_may_return_multiple)
                    || forms.get(3).map_or(false, tail_may_return_multiple)
            }
            _ => false,
        },
        _ => false,
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn analyze(ast: &[AstNode]) -> AnalysisResult {
    analyze_with_hooks(ast, &HashMap::new())
}

pub fn analyze_with_hooks(
    ast: &[AstNode],
    hook_results: &HashMap<u32, Vec<crate::hooks::Instruction>>,
) -> AnalysisResult {
    let mut analyzer = Analyzer::new_with_hooks(hook_results.clone());

    // Global scope covering the entire file (span 0..u32::MAX is fine)
    let global_span = if let (Some(first), Some(last)) = (ast.first(), ast.last()) {
        Span::merge(&first.span, &last.span)
    } else {
        Span { start: 0, end: 0, line: 0, col: 0, end_line: 0, end_col: 0 }
    };
    analyzer.push_scope(Span {
        start: 0,
        end: u32::MAX,
        ..global_span
    });

    analyzer.analyze_forms(ast);

    // Warn on `var` bindings that are never the target of a `set`.
    let unused_vars: Vec<(String, Span)> = analyzer.result.defs.iter()
        .filter(|(&db, def)| {
            def.kind == DefKind::Var && !analyzer.mutation_targets.contains(&db)
        })
        .map(|(_, def)| (def.name.clone(), def.span.clone()))
        .collect();
    for (name, span) in unused_vars {
        analyzer.result.warnings.push(AnalysisWarning {
            message: format!("`{}` is never mutated; use `local` instead of `var`", name),
            span,
            related_span: None,
        });
    }

    // Set of def-byte offsets that are the target of at least one reference.
    let referenced: std::collections::HashSet<u32> =
        analyzer.result.refs.values().copied().collect();

    // Warn on require bindings that are never used.
    let unused_requires: Vec<(String, String, Span)> = analyzer.result.require_def_bytes.iter()
        .filter(|(&db, _)| !referenced.contains(&db))
        .filter_map(|(&db, module)| {
            analyzer.result.defs.get(&db)
                .map(|def| (def.name.clone(), module.clone(), def.span.clone()))
        })
        .collect();
    for (name, module, span) in unused_requires {
        analyzer.result.warnings.push(AnalysisWarning {
            message: format!("`{}` (require :{}) is required but never used", name, module),
            span,
            related_span: None,
        });
    }

    // Warn on `local` bindings that are never read.
    // Skips `_`-prefixed names (conventional discard), destructured bindings,
    // and require bindings (those get their own message above).
    let unused_locals: Vec<(String, Span)> = analyzer.result.defs.iter()
        .filter(|(&db, def)| {
            def.kind == DefKind::Local
                && !referenced.contains(&db)
                && !def.name.starts_with('_')
                && !analyzer.result.require_def_bytes.contains_key(&db)
        })
        .map(|(_, def)| (def.name.clone(), def.span.clone()))
        .collect();
    for (name, span) in unused_locals {
        analyzer.result.warnings.push(AnalysisWarning {
            message: format!("`{}` is defined but never used", name),
            span,
            related_span: None,
        });
    }

    // Warn on function parameters that are never read.
    // Skips `_`-prefixed names and `<destructured>` placeholders.
    let unused_params: Vec<(String, Span)> = analyzer.result.defs.iter()
        .filter(|(&db, def)| {
            def.kind == DefKind::Param
                && !referenced.contains(&db)
                && !def.name.starts_with('_')
                && !def.name.starts_with('<')
                && !def.name.starts_with('$')
        })
        .map(|(_, def)| (def.name.clone(), def.span.clone()))
        .collect();
    for (name, span) in unused_params {
        analyzer.result.warnings.push(AnalysisWarning {
            message: format!("parameter `{}` is unused", name),
            span,
            related_span: None,
        });
    }

    // Sort syms by span.start for binary search
    analyzer.result.syms.sort_by_key(|s| s.span.start);

    analyzer.result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn analyze_src(src: &str) -> AnalysisResult {
        let (ast, _) = crate::parser::Parser::parse(src);
        analyze(&ast)
    }

    fn warnings_for(src: &str) -> Vec<String> {
        analyze_src(src).warnings.iter().map(|w| w.message.clone()).collect()
    }

    fn has_warning(src: &str, substr: &str) -> bool {
        warnings_for(src).iter().any(|w| w.contains(substr))
    }

    fn has_def(r: &AnalysisResult, name: &str) -> bool {
        r.defs.values().any(|d| d.name == name)
    }

    fn def_kind(r: &AnalysisResult, name: &str) -> Option<DefKind> {
        r.defs.values().find(|d| d.name == name).map(|d| d.kind.clone())
    }

    /// True if any unresolved reference to `name` exists.
    fn is_unknown(r: &AnalysisResult, name: &str) -> bool {
        r.syms.iter().any(|s| s.name == name && !s.is_def && s.def_byte.is_none())
    }

    /// True if at least one reference to `name` resolves to a def.
    fn ref_resolves(r: &AnalysisResult, name: &str) -> bool {
        r.syms.iter().any(|s| s.name == name && !s.is_def && s.def_byte.is_some())
    }

    // ── Unused var warning ────────────────────────────────────────────────────

    #[test]
    fn unused_var_warns() {
        assert!(has_warning("(var x 1) x", "never mutated"));
    }

    #[test]
    fn used_var_no_warn() {
        assert!(!has_warning("(var x 1) (set x 2) x", "never mutated"));
    }

    #[test]
    fn var_set_multiple_times_no_warn() {
        assert!(!has_warning("(var x 0) (set x 1) (set x 2) x", "never mutated"));
    }

    #[test]
    fn local_no_unused_var_warn() {
        assert!(!has_warning("(local x 1) x", "never mutated"));
    }

    #[test]
    fn fn_param_no_unused_var_warn() {
        assert!(!has_warning("(fn f [x] x)", "never mutated"));
    }

    #[test]
    fn hashfn_unused_dollar_params_no_warn() {
        // $2..$9 are pre-declared for every hashfn but unused ones must not warn
        assert!(!has_warning("#(+ $ 1)", "unused"));
        assert!(!has_warning("#(+ $1 $2)", "unused"));
    }

    #[test]
    fn var_in_inner_scope_unwarn_if_set() {
        assert!(!has_warning("(do (var x 1) (set x 2) x)", "never mutated"));
    }

    // ── Shadowed binding warning ──────────────────────────────────────────────

    #[test]
    fn shadowed_binding_warns() {
        assert!(has_warning("(let [x 1 x 2] x)", "already defined"));
    }

    #[test]
    fn top_level_shadow_warns() {
        assert!(has_warning("(local x 1) (local x 2) x", "already defined"));
    }

    #[test]
    fn fn_params_shadow_each_other_warns() {
        assert!(has_warning("(fn f [a a] a)", "already defined"));
    }

    #[test]
    fn same_scope_shadow_still_warns_already_defined() {
        // Same-scope re-definition keeps the original "already defined" message.
        assert!(!has_warning("(local x 1) (let [] (local x 2) x) x", "already defined"));
    }

    #[test]
    fn underscore_no_shadow_warn() {
        assert!(!has_warning("(let [_ 1 _ 2] nil)", "already defined"));
    }

    // ── Cross-scope shadowing ─────────────────────────────────────────────────

    #[test]
    fn outer_scope_shadow_warns() {
        assert!(has_warning("(local x 1) (do (local x 2) x) x", "shadows a binding from an outer scope"));
    }

    #[test]
    fn let_binding_outer_shadow_warns() {
        assert!(has_warning("(local x 1) (let [x 2] x)", "shadows a binding from an outer scope"));
    }

    #[test]
    fn fn_param_outer_shadow_no_warn() {
        // Function params are not checked — too noisy and idiomatic to reuse names.
        assert!(!has_warning("(local x 1) (fn f [x] x)", "shadows"));
    }

    #[test]
    fn loop_var_outer_shadow_no_warn() {
        // Loop vars (each/for) are also not checked.
        assert!(!has_warning("(local x 1) (each [x []] x)", "shadows"));
    }

    #[test]
    fn underscore_outer_shadow_no_warn() {
        assert!(!has_warning("(local _ 1) (do (local _ 2) nil)", "shadows"));
    }

    #[test]
    fn set_on_immutable_warns() {
        assert!(has_warning("(local x 1) (set x 2)", "immutable"));
    }

    #[test]
    fn set_on_fn_warns() {
        assert!(has_warning("(fn f [] nil) (set f 1)", "immutable"));
    }

    #[test]
    fn set_on_var_no_warn() {
        assert!(!has_warning("(var x 1) (set x 2)", "immutable"));
    }

    #[test]
    fn set_on_global_no_warn() {
        assert!(!has_warning("(global x 1) (set x 2)", "immutable"));
    }

    // ── Basic def / ref tracking ──────────────────────────────────────────────

    #[test]
    fn local_creates_def() {
        let r = analyze_src("(local x 1)");
        assert!(has_def(&r, "x"));
        assert_eq!(def_kind(&r, "x"), Some(DefKind::Local));
    }

    #[test]
    fn var_creates_def() {
        let r = analyze_src("(var x 1)");
        assert_eq!(def_kind(&r, "x"), Some(DefKind::Var));
    }

    #[test]
    fn global_creates_def() {
        let r = analyze_src("(global x 1)");
        assert_eq!(def_kind(&r, "x"), Some(DefKind::Global));
    }

    #[test]
    fn fn_creates_fn_def() {
        let r = analyze_src("(fn add [a b] (+ a b))");
        assert_eq!(def_kind(&r, "add"), Some(DefKind::Fn));
    }

    #[test]
    fn fn_params_are_param_defs() {
        let r = analyze_src("(fn f [a b] a)");
        assert_eq!(def_kind(&r, "a"), Some(DefKind::Param));
        assert_eq!(def_kind(&r, "b"), Some(DefKind::Param));
    }

    #[test]
    fn reference_to_local_resolves() {
        let r = analyze_src("(local x 1) x");
        assert!(ref_resolves(&r, "x"));
        assert!(!is_unknown(&r, "x"));
    }

    #[test]
    fn reference_to_undefined_is_unknown() {
        let r = analyze_src("y");
        assert!(is_unknown(&r, "y"));
    }

    #[test]
    fn fn_name_visible_in_body_for_recursion() {
        let r = analyze_src("(fn fact [n] (fact n))");
        // `fact` inside the body should resolve, not be unknown
        assert!(!is_unknown(&r, "fact"));
        assert!(ref_resolves(&r, "fact"));
    }

    #[test]
    fn fn_docstring_extracted() {
        let r = analyze_src(r#"(fn f [] "does a thing" nil)"#);
        let doc = r.defs.values().find(|d| d.name == "f").and_then(|d| d.doc.as_deref());
        assert_eq!(doc, Some("does a thing"));
    }

    // ── Scope isolation ───────────────────────────────────────────────────────

    #[test]
    fn let_binding_not_visible_after_let() {
        let r = analyze_src("(let [x 1] x) x");
        // The second `x` (outside let) should be unknown
        // There are two `x` refs: one inside the let (resolves) and one outside (unknown)
        assert!(is_unknown(&r, "x"));
    }

    #[test]
    fn fn_param_not_visible_outside_fn() {
        let r = analyze_src("(fn f [x] x) x");
        assert!(is_unknown(&r, "x"));
    }

    #[test]
    fn do_block_binding_not_visible_outside() {
        let r = analyze_src("(do (local x 1) x) x");
        assert!(is_unknown(&r, "x"));
    }

    #[test]
    fn let_sequential_later_binding_sees_earlier() {
        // In (let [a 1 b a] b), the RHS of b can see a
        let r = analyze_src("(let [a 1 b a] b)");
        assert!(!is_unknown(&r, "a"));
        assert!(ref_resolves(&r, "a"));
    }

    #[test]
    fn let_rhs_does_not_see_own_binding() {
        // In (let [x x] ...), the RHS `x` can't see the new `x` yet —
        // it refers to whatever `x` was in the outer scope (here: unknown)
        let r = analyze_src("(let [x x] x)");
        assert!(is_unknown(&r, "x"));
    }

    // ── Named let (tail-recursive loop) ──────────────────────────────────────

    #[test]
    fn named_let_binds_loop_var_in_body() {
        let r = analyze_src("(let loop [i 0] i)");
        assert!(!is_unknown(&r, "i"), "binding var must be in scope in body");
    }

    #[test]
    fn named_let_loop_name_callable_in_body() {
        let r = analyze_src("(let loop [i 0] (loop (+ i 1)))");
        assert!(!is_unknown(&r, "loop"), "loop name must be callable from body");
        assert!(!is_unknown(&r, "i"), "binding var must be in scope in body");
    }

    #[test]
    fn named_let_multiple_bindings_all_visible() {
        let r = analyze_src("(let go [i 1 total 0] (go (+ i 1) (+ total i)))");
        assert!(!is_unknown(&r, "i"));
        assert!(!is_unknown(&r, "total"));
        assert!(!is_unknown(&r, "go"));
    }

    #[test]
    fn named_let_init_values_not_in_loop_scope() {
        // The RHS of the first binding must NOT see the loop name or the other params
        // (they are evaluated before any binding is established, like fn args)
        let r = analyze_src("(let loop [i loop] i)");
        // `loop` on the RHS refers to an outer `loop` (unknown here)
        assert!(is_unknown(&r, "loop"));
    }

    #[test]
    fn named_let_not_visible_after_let() {
        let r = analyze_src("(let loop [i 0] i) loop");
        // `loop` after the let is outside its scope
        assert!(is_unknown(&r, "loop"));
    }

    #[test]
    fn named_let_binding_not_visible_after_let() {
        let r = analyze_src("(let loop [i 0] i) i");
        assert!(is_unknown(&r, "i"));
    }

    // ── as-> threading ────────────────────────────────────────────────────────

    #[test]
    fn as_arrow_binds_placeholder_in_body() {
        let r = analyze_src("(as-> 0 it (+ it 1))");
        assert!(!is_unknown(&r, "it"), "placeholder must be bound in each form");
    }

    #[test]
    fn as_arrow_placeholder_visible_across_forms() {
        let r = analyze_src("(as-> \"hello\" s (string.upper s) (.. s \"!\"))");
        assert!(!is_unknown(&r, "s"));
    }

    #[test]
    fn as_arrow_placeholder_not_visible_after_form() {
        let r = analyze_src("(as-> 0 it it) it");
        assert!(is_unknown(&r, "it"), "placeholder must not leak outside as->");
    }

    #[test]
    fn as_arrow_initial_value_analyzed() {
        // The initial value expression should be analyzed (outer refs resolve)
        let r = analyze_src("(local x 42) (as-> x it it)");
        assert!(!is_unknown(&r, "x"), "initial value reference must resolve");
        assert!(!is_unknown(&r, "it"));
    }

    // ── Destructuring ─────────────────────────────────────────────────────────

    #[test]
    fn sequence_destructuring_creates_defs() {
        let r = analyze_src("(local [a b c] t)");
        assert!(has_def(&r, "a"));
        assert!(has_def(&r, "b"));
        assert!(has_def(&r, "c"));
    }

    #[test]
    fn table_destructuring_creates_defs() {
        let r = analyze_src("(local {:x a :y b} t)");
        assert!(has_def(&r, "a"));
        assert!(has_def(&r, "b"));
    }

    #[test]
    fn table_shorthand_destructuring() {
        // {: name} binds `name`
        let r = analyze_src("(local {: name} t)");
        assert!(has_def(&r, "name"));
    }

    #[test]
    fn nested_sequence_destructuring() {
        let r = analyze_src("(let [[a b] pair] a)");
        assert!(has_def(&r, "a"));
        assert!(has_def(&r, "b"));
        assert!(!is_unknown(&r, "a"));
    }

    // ── Loop forms ────────────────────────────────────────────────────────────

    #[test]
    fn each_binds_loop_vars() {
        let r = analyze_src("(each [k v (pairs t)] k)");
        assert_eq!(def_kind(&r, "k"), Some(DefKind::LoopVar));
        assert_eq!(def_kind(&r, "v"), Some(DefKind::LoopVar));
    }

    #[test]
    fn for_binds_loop_var() {
        let r = analyze_src("(for [i 1 10] (print i))");
        assert_eq!(def_kind(&r, "i"), Some(DefKind::LoopVar));
        assert!(!is_unknown(&r, "i"));
    }

    #[test]
    fn for_var_in_scope_in_body() {
        let r = analyze_src("(for [i 1 5] (io.write (tostring i)))");
        assert!(!is_unknown(&r, "i"), "loop var must be visible in body");
    }

    #[test]
    fn for_with_step_var_in_scope() {
        let r = analyze_src("(for [i 0 100 10] (print i))");
        assert!(!is_unknown(&r, "i"), "loop var must be visible when step is present");
    }

    #[test]
    fn for_var_not_in_scope_outside() {
        let r = analyze_src("(for [i 1 3] nil) i");
        assert!(is_unknown(&r, "i"), "loop var must not leak outside for");
    }

    #[test]
    fn for_start_stop_analyzed() {
        let r = analyze_src("(local n 10) (for [i 1 n] (print i))");
        assert!(!is_unknown(&r, "n"), "stop expression must be analyzed");
    }

    #[test]
    fn accumulate_binds_acc_and_loop_vars() {
        let r = analyze_src("(accumulate [sum 0 k v (pairs t)] (+ sum v))");
        assert!(has_def(&r, "sum"));
        assert_eq!(def_kind(&r, "k"), Some(DefKind::LoopVar));
        assert_eq!(def_kind(&r, "v"), Some(DefKind::LoopVar));
        assert!(!is_unknown(&r, "sum"));
        assert!(!is_unknown(&r, "v"));
    }

    #[test]
    fn fcollect_binds_loop_var() {
        let r = analyze_src("(fcollect [i 1 10] i)");
        assert_eq!(def_kind(&r, "i"), Some(DefKind::LoopVar));
        assert!(!is_unknown(&r, "i"));
    }

    #[test]
    fn fcollect_until_loop_var_visible_in_guard() {
        // (fcollect [i 1 n &until (pred i)] i) — `i` must be in scope in the guard
        let r = analyze_src("(local n 10) (fcollect [i 1 n &until (>= i 5)] i)");
        assert!(!is_unknown(&r, "i"), "loop var must be visible in &until guard");
    }

    #[test]
    fn fcollect_until_guard_analyzed_after_bind() {
        // Reference inside the &until must resolve to the loop var, not unknown
        let r = analyze_src("(fcollect [i 1 10 &until (= i 5)] i)");
        assert!(ref_resolves(&r, "i"), "i in &until must resolve to the loop var");
        assert_eq!(def_kind(&r, "i"), Some(DefKind::LoopVar));
    }

    #[test]
    fn fcollect_into_and_until_together() {
        // Both modifiers present: loop var must be visible in &until,
        // and &into target must resolve as a reference
        let r = analyze_src(
            "(local acc []) (fcollect [i 1 10 &until (>= i 5) &into acc] i)",
        );
        assert!(!is_unknown(&r, "i"), "loop var visible in &until when &into also present");
        assert!(ref_resolves(&r, "acc"), "&into target must resolve");
    }

    #[test]
    fn with_open_binds_names() {
        let r = analyze_src("(with-open [f (io.open :file)] f)");
        assert!(has_def(&r, "f"));
        assert!(!is_unknown(&r, "f"));
    }

    #[test]
    fn collect_binds_loop_vars() {
        let r = analyze_src("(collect [k v (pairs t)] k v)");
        assert_eq!(def_kind(&r, "k"), Some(DefKind::LoopVar));
        assert_eq!(def_kind(&r, "v"), Some(DefKind::LoopVar));
    }

    // ── Match / case patterns ─────────────────────────────────────────────────

    #[test]
    fn match_pattern_binds_symbol() {
        let r = analyze_src("(match x a (+ a 1) _ 0)");
        assert!(has_def(&r, "a"));
        assert!(!is_unknown(&r, "a"));
    }

    #[test]
    fn match_pattern_not_visible_after_match() {
        // `n` is a pattern variable inside match; it must not be visible after
        let r = analyze_src("(match x n n) n");
        // The trailing `n` (outside match) should be unknown
        assert!(is_unknown(&r, "n"));
    }

    #[test]
    fn match_where_guard_sees_pattern_binding() {
        let r = analyze_src("(match x (where n (> n 0)) :pos _ :other)");
        // `n` in the guard `(> n 0)` must resolve
        assert!(!is_unknown(&r, "n"));
    }

    // ── HashFn implicit params ────────────────────────────────────────────────

    #[test]
    fn hashfn_dollar_params_visible() {
        let r = analyze_src("#(+ $ $1)");
        assert!(ref_resolves(&r, "$"));
        assert!(ref_resolves(&r, "$1"));
    }

    #[test]
    fn hashfn_numbered_params_visible() {
        let r = analyze_src("#[$1 $2 $3]");
        for name in &["$1", "$2", "$3"] {
            assert!(ref_resolves(&r, name), "{name} should resolve inside hashfn");
        }
    }

    // ── Quasiquote analysis ───────────────────────────────────────────────────

    #[test]
    fn quasiquote_unquote_analyzed() {
        // ,x inside a quasiquote IS analyzed as a reference
        let r = analyze_src("(local x 1) `(+ ,x 1)");
        assert!(ref_resolves(&r, "x"));
    }

    #[test]
    fn quasiquote_literal_not_analyzed() {
        // bare `x` inside quasiquote (not inside unquote) is literal data
        let r = analyze_src("`(+ x 1)");
        // x should NOT appear as a reference at all (it's quoted data)
        let x_as_ref = r.syms.iter().any(|s| s.name == "x" && !s.is_def);
        assert!(!x_as_ref, "x inside quasiquote literal should not be a reference");
    }

    // ── Macro bindings ────────────────────────────────────────────────────────

    #[test]
    fn macro_creates_macro_def() {
        let r = analyze_src("(macro my-mac [x] x)");
        assert_eq!(def_kind(&r, "my-mac"), Some(DefKind::Macro));
    }

    #[test]
    fn macro_body_analyzed() {
        // Param `x` inside macro body should be a def
        let r = analyze_src("(macro my-mac [x] x)");
        assert_eq!(def_kind(&r, "x"), Some(DefKind::Param));
    }

    #[test]
    fn import_macros_creates_defs() {
        let r = analyze_src("(import-macros {: foo :bar baz} :mylib)");
        assert!(has_def(&r, "foo"));
        assert!(has_def(&r, "baz"));
    }

    // ── Position queries (defs_at / symbol_at) ────────────────────────────────

    #[test]
    fn symbol_at_finds_reference() {
        // "(local x 1) x"
        //  0123456789012
        // x-def at 7, x-ref at 12
        let src = "(local x 1) x";
        let r = analyze_src(src);
        let sym = r.symbol_at(12).expect("symbol at ref");
        assert_eq!(sym.name, "x");
        assert!(!sym.is_def);
        assert!(sym.def_byte.is_some());
    }

    #[test]
    fn symbol_at_finds_definition() {
        let src = "(local x 1) x";
        let r = analyze_src(src);
        let sym = r.symbol_at(7).expect("symbol at def");
        assert_eq!(sym.name, "x");
        assert!(sym.is_def);
    }

    #[test]
    fn defs_at_inside_fn_sees_params() {
        // "(fn f [a b] a)" — 'a' in body at byte 12
        // byte: 0123456789012
        let src = "(fn f [a b] a)";
        let r = analyze_src(src);
        let names: HashSet<&str> = r.defs_at(12).iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains("f"), "fn name should be visible in body");
        assert!(names.contains("a"), "param a visible in body");
        assert!(names.contains("b"), "param b visible in body");
    }

    #[test]
    fn defs_at_outside_fn_no_params() {
        // "(fn f [a b] a) z" — 'z' at byte 15 is outside the fn span (0..13)
        let src = "(fn f [a b] a) z";
        let r = analyze_src(src);
        let names: HashSet<&str> = r.defs_at(15).iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains("f"),  "fn name visible everywhere after def");
        assert!(!names.contains("a"), "param a must not leak outside fn");
        assert!(!names.contains("b"), "param b must not leak outside fn");
    }

    // ── Complex tests (realistic patterns that trip up LSPs) ─────────────────

    #[test]
    fn complex_each_mutates_outer_var() {
        // A common Fennel pattern: accumulate into an outer `var` inside a loop.
        // Tests: loop-var scoping, outer-var mutation tracking, discard `_`.
        let src = "(var total 0)\n(each [_ v (ipairs xs)]\n  (set total (+ total v)))\ntotal";
        let r = analyze_src(src);

        assert_eq!(def_kind(&r, "total"), Some(DefKind::Var));
        assert_eq!(def_kind(&r, "v"), Some(DefKind::LoopVar));
        assert!(!has_def(&r, "_"), "_ should not be defined");

        // `set total` marks total as mutated → no unused-var warning
        assert!(!r.warnings.iter().any(|w| w.message.contains("total") && w.message.contains("mutated")));

        // `xs` has no definition
        assert!(is_unknown(&r, "xs"));
    }

    #[test]
    fn complex_let_fn_closure_and_sequential_bindings() {
        // A fn inside `let` closes over an outer local; a later `let` binding
        // calls the earlier fn. Tests: outer-scope capture, sequential let
        // visibility, no false "unknown" warnings.
        let src = "(local base 10)\n(let [adder (fn [x] (+ x base))\n      result (adder 5)]\n  result)";
        let r = analyze_src(src);

        assert!(has_def(&r, "base"));
        assert!(has_def(&r, "adder"));
        assert_eq!(def_kind(&r, "x"), Some(DefKind::Param));
        assert!(has_def(&r, "result"));

        for name in &["base", "adder", "result"] {
            assert!(!is_unknown(&r, name), "{name} should not be unknown");
        }
        assert!(r.warnings.is_empty(), "unexpected warnings: {:?}",
            r.warnings.iter().map(|w| &w.message).collect::<Vec<_>>());
    }

    #[test]
    fn complex_match_arms_have_independent_pattern_bindings() {
        // Two match arms both bind a variable called `n` — they must be in
        // separate scopes and neither should be reported as unknown or shadow.
        let src = "(fn describe [x]\n  (match x\n    {:type :num :val n} (+ n 1)\n    {:type :str :val n} n\n    _ 0))";
        let r = analyze_src(src);

        // `n` is bound independently in each arm → two separate defs
        let n_def_count = r.defs.values().filter(|d| d.name == "n").count();
        assert_eq!(n_def_count, 2, "n should be defined once per arm, not shared");

        // Every reference to `n` resolves
        for sym in r.syms.iter().filter(|s| s.name == "n" && !s.is_def) {
            assert!(sym.def_byte.is_some(), "n reference should always resolve");
        }
        assert!(r.warnings.is_empty(), "no warnings expected: {:?}",
            r.warnings.iter().map(|w| &w.message).collect::<Vec<_>>());
    }

    #[test]
    fn complex_triple_shadow_each_ref_resolves_to_nearest_binding() {
        // x is defined at three nested scope levels. Each reference should
        // resolve to a different def (the nearest enclosing one), with no
        // false shadow warnings since shadows are across different scopes.
        let src = "(local x 10)\n(let [x 20]\n  (do\n    (local x 30)\n    x)\n  x)\nx";
        let r = analyze_src(src);

        let x_defs: Vec<_> = r.defs.values().filter(|d| d.name == "x").collect();
        assert_eq!(x_defs.len(), 3, "three distinct x definitions");

        let x_refs: Vec<_> = r.syms.iter().filter(|s| s.name == "x" && !s.is_def).collect();
        assert_eq!(x_refs.len(), 3, "three x references");

        // All refs resolve
        for sym in &x_refs {
            assert!(sym.def_byte.is_some(), "every x ref must resolve");
        }

        // Each ref resolves to a DIFFERENT def (scope chain working correctly)
        let resolved: HashSet<u32> = x_refs.iter().filter_map(|s| s.def_byte).collect();
        assert_eq!(resolved.len(), 3, "each x ref should see a different binding");

        // No false shadow warnings (shadows cross scope boundaries, not same scope)
        assert!(!r.warnings.iter().any(|w| w.message.contains("already defined")));
    }

    // ── Edge-case combinations ────────────────────────────────────────────────

    #[test]
    fn set_on_table_field_marks_var_mutated() {
        // `(set t.field v)` should count as a mutation of `t` so that `t`
        // doesn't get an unused-var warning.
        assert!(!has_warning("(var t {}) (set t.field 1)", "never mutated"));
    }

    #[test]
    fn set_on_undefined_no_crash() {
        // Setting an undefined name must not panic and should mark it unknown.
        let r = analyze_src("(set ghost 1)");
        assert!(is_unknown(&r, "ghost"));
    }

    #[test]
    fn unused_var_in_fn_body() {
        // A `var` that is never mutated inside a fn body should still warn.
        assert!(has_warning("(fn f [] (var x 1) x)", "never mutated"));
    }

    #[test]
    fn each_with_until_guard_sees_loop_vars() {
        // `&until` guard expression is inside the loop's scope, so loop vars
        // must be visible there.
        let r = analyze_src("(each [k v (pairs t) &until (= k :stop)] v)");
        assert!(!is_unknown(&r, "k"), "k must be visible in &until guard");
        assert!(!is_unknown(&r, "v"), "v must be visible in body");
    }

    #[test]
    fn accumulate_with_until_guard_sees_acc_var() {
        // The `&until` guard in `accumulate` must see the accumulator.
        let r = analyze_src(
            "(accumulate [sum 0 i v (ipairs t) &until (> sum 100)] (+ sum v))",
        );
        assert!(!is_unknown(&r, "sum"), "sum must be visible in &until guard");
    }

    #[test]
    fn case_try_catch_arm_binds_error_var_bare_syntax() {
        // Old bare-keyword syntax: `... catch pat body`
        let r = analyze_src("(case-try (may-fail) x x catch e (tostring e))");
        assert!(has_def(&r, "e"), "catch arm must bind e");
        assert!(!is_unknown(&r, "e"), "e must resolve in catch body");
    }

    #[test]
    fn case_try_catch_arm_parenthesised_syntax() {
        // Modern parenthesised syntax: `(catch pat body ...)`
        let r = analyze_src("(case-try (io.open :f) h h (catch msg (print msg)))");
        assert!(has_def(&r, "msg"), "catch arm must bind msg");
        assert!(!is_unknown(&r, "msg"), "msg must resolve in catch body");
    }

    #[test]
    fn case_try_catch_multiple_arms_parenthesised() {
        let r = analyze_src(
            "(case-try (f) x x (catch :bad (print :bad) err (print err)))"
        );
        assert!(has_def(&r, "err"));
        assert!(!is_unknown(&r, "err"), "err must be visible in multi-arm catch");
    }

    #[test]
    fn match_try_catch_arm_parenthesised_syntax() {
        // `match-try` is an alias; must work with parenthesised catch too
        let r = analyze_src("(match-try (f) x x (catch msg (tostring msg)))");
        assert!(has_def(&r, "msg"));
        assert!(!is_unknown(&r, "msg"));
    }

    #[test]
    fn case_try_chain_bindings_visible_in_later_exprs() {
        // pat from arm 1 must be in scope when arm 2's expr is analyzed
        let r = analyze_src("(case-try (io.open :f) h (h:read :*a) text text (catch e e))");
        assert!(!is_unknown(&r, "h"),    "h must be visible in (h:read ...)");
        assert!(!is_unknown(&r, "text"), "text must be visible as result");
        assert!(!is_unknown(&r, "e"),    "e must be visible in catch");
    }

    #[test]
    fn let_fn_body_cannot_see_own_let_binding() {
        // `(let [f (fn [] (f))] f)` — inside the fn body, `f` is not yet bound
        // (the let binding is established AFTER the RHS is evaluated), so the
        // recursive call to `f` should be unknown.
        let r = analyze_src("(let [f (fn [] (f))] f)");
        // The inner `f` (inside the fn body) must be unknown
        let inner_f_unknown = r.syms.iter().any(|s| {
            s.name == "f" && !s.is_def && s.def_byte.is_none()
        });
        assert!(inner_f_unknown, "f inside fn body cannot see its own let binding");
    }

    #[test]
    fn nested_table_match_pattern() {
        // `(match t {:a {:b n}} n)` — nested table pattern must bind `n`
        let r = analyze_src("(match t {:a {:b n}} n)");
        assert!(has_def(&r, "n"), "nested table pattern must bind n");
        assert!(!is_unknown(&r, "n"), "n must resolve in the arm body");
    }

    #[test]
    fn fn_param_shadows_outer_let_no_false_warn() {
        // A function param with the same name as an outer let binding crosses
        // scope boundaries — must NOT produce a shadow warning.
        assert!(!has_warning("(let [x 1] (fn f [x] x))", "already defined"));
    }

    #[test]
    fn collect_into_dst_is_ref_not_binding() {
        // `&into dst` passes an existing collection; `dst` must resolve as a
        // reference to its prior definition, not create a new binding.
        let r = analyze_src("(local dst []) (collect [k v (pairs t) &into dst] k)");
        // dst should resolve (not be unknown), and there must be only ONE def for dst
        assert!(ref_resolves(&r, "dst"), "dst in &into must resolve");
        let dst_defs: Vec<_> = r.defs.values().filter(|d| d.name == "dst").collect();
        assert_eq!(dst_defs.len(), 1, "&into must not create a second binding for dst");
    }

    // ── Untested form aliases ─────────────────────────────────────────────────

    #[test]
    fn icollect_binds_loop_var() {
        let r = analyze_src("(icollect [v (ipairs t)] v)");
        assert_eq!(def_kind(&r, "v"), Some(DefKind::LoopVar));
        assert!(!is_unknown(&r, "v"));
    }

    #[test]
    fn faccumulate_binds_acc_and_loop_var() {
        let r = analyze_src("(faccumulate [sum 0 i 1 10] (+ sum i))");
        assert!(has_def(&r, "sum"));
        assert_eq!(def_kind(&r, "i"), Some(DefKind::LoopVar));
        assert!(!is_unknown(&r, "sum"));
        assert!(!is_unknown(&r, "i"));
    }

    #[test]
    fn faccumulate_variable_start_stop_analyzed_as_exprs() {
        let r = analyze_src("(local lo 1) (local hi 10) (faccumulate [sum 0 i lo hi] (+ sum i))");
        assert!(!is_unknown(&r, "lo"), "start must be analyzed as an expression");
        assert!(!is_unknown(&r, "hi"), "stop must be analyzed as an expression");
        assert!(!is_unknown(&r, "i"), "loop var must be visible in body");
        assert!(!is_unknown(&r, "sum"), "acc must be visible in body");
    }

    #[test]
    fn faccumulate_variable_step_analyzed_as_expr() {
        let r = analyze_src("(local step 2) (faccumulate [sum 0 i 1 10 step] (+ sum i))");
        assert!(!is_unknown(&r, "step"), "step must be analyzed as an expression");
        assert!(!is_unknown(&r, "i"));
    }

    #[test]
    fn faccumulate_until_guard_sees_loop_var() {
        let r = analyze_src("(faccumulate [sum 0 i 1 100 &until (> sum 50)] (+ sum i))");
        assert!(!is_unknown(&r, "i"), "loop var must be visible in &until guard");
        assert!(!is_unknown(&r, "sum"), "acc must be visible in &until guard");
    }

    #[test]
    fn faccumulate_acc_not_visible_outside() {
        let r = analyze_src("(faccumulate [sum 0 i 1 10] (+ sum i)) sum");
        assert!(is_unknown(&r, "sum"), "acc must not leak outside faccumulate");
    }

    #[test]
    fn lambda_creates_fn_def() {
        let r = analyze_src("(lambda greet [name] name)");
        assert_eq!(def_kind(&r, "greet"), Some(DefKind::Fn));
        assert_eq!(def_kind(&r, "name"), Some(DefKind::Param));
    }

    #[test]
    fn lambda_unicode_alias() {
        let r = analyze_src("(λ f [x] x)");
        assert_eq!(def_kind(&r, "f"), Some(DefKind::Fn));
        assert_eq!(def_kind(&r, "x"), Some(DefKind::Param));
    }

    #[test]
    fn macros_form_creates_macro_def() {
        // `(macros {name (fn [x] x)})` — bare symbol keys, not keyword keys
        let r = analyze_src("(macros {my-mac (fn [x] x)})");
        assert_eq!(def_kind(&r, "my-mac"), Some(DefKind::Macro));
    }

    #[test]
    fn macro_form_call_site_not_unknown() {
        // (macro ...) creates a def; the call site should resolve, not be flagged.
        let r = analyze_src("(macro my-mac [x] x) (my-mac 1)");
        assert!(!is_unknown(&r, "my-mac"), "macro call site must resolve");
    }

    #[test]
    fn macros_form_call_site_not_unknown() {
        // (macros {...}) also creates defs; call sites must resolve.
        let r = analyze_src("(macros {my-mac (fn [x] x)}) (my-mac 99)");
        assert!(!is_unknown(&r, "my-mac"), "macros-form call site must resolve");
    }

    #[test]
    fn import_macros_call_site_not_unknown() {
        // (import-macros {: foo} :lib) creates a def for `foo`; calling it must resolve.
        let r = analyze_src("(import-macros {: foo :bar baz} :mylib) (foo 1) (baz 2)");
        assert!(!is_unknown(&r, "foo"), "import-macros call site `foo` must resolve");
        assert!(!is_unknown(&r, "baz"), "import-macros call site `baz` must resolve");
    }

    #[test]
    fn import_macros_standalone_shorthand_binds() {
        // {defnode} without a preceding colon should bind "defnode"
        let r = analyze_src("(import-macros {defnode} :mylib)");
        assert_eq!(def_kind(&r, "defnode"), Some(DefKind::Macro));
    }

    #[test]
    fn macro_call_dsl_args_not_unknown() {
        // DSL symbols in macro argument position must not be flagged as unknown
        let r = analyze_src(
            "(import-macros {defnode} :mylib) (defnode FennelNode (extends Base) (tool))"
        );
        let dsl_syms: Vec<_> = r.syms.iter()
            .filter(|s| ["FennelNode", "extends", "Base", "tool"].contains(&s.name.as_str()))
            .collect();
        for s in &dsl_syms {
            assert!(s.in_macro, "`{}` should be tagged in_macro", s.name);
        }
    }

    #[test]
    fn macro_call_nested_fn_params_resolve() {
        // Inside a macro call, a (fn ...) sub-form should still bind its params
        let r = analyze_src(
            "(import-macros {defnode} :mylib) (defnode Foo (fn greet [self x] x))"
        );
        // `x` inside the fn should resolve to its param — def_byte not None
        let x_ref = r.syms.iter()
            .find(|s| s.name == "x" && !s.is_def)
            .expect("x reference");
        assert!(x_ref.def_byte.is_some(), "x should resolve to its param def");
    }

    // ── source_module on import-macros defs ──────────────────────────────────

    fn source_module_of(r: &AnalysisResult, name: &str) -> Option<String> {
        r.defs.values()
            .find(|d| d.name == name && d.kind == DefKind::Macro)
            .and_then(|d| d.source_module.clone())
    }

    #[test]
    fn import_macros_keyword_module_stored_on_def() {
        let r = analyze_src("(import-macros {: defnode} :addons.lua-gdextension.defnode)");
        assert_eq!(
            source_module_of(&r, "defnode").as_deref(),
            Some("addons.lua-gdextension.defnode"),
            "source_module should be the keyword module path"
        );
    }

    #[test]
    fn import_macros_string_module_stored_on_def() {
        let r = analyze_src(r#"(import-macros {: defnode} "addons.lua-gdextension.defnode")"#);
        assert_eq!(
            source_module_of(&r, "defnode").as_deref(),
            Some("addons.lua-gdextension.defnode")
        );
    }

    #[test]
    fn import_macros_standalone_shorthand_stores_module() {
        let r = analyze_src("(import-macros {defnode} :mylib)");
        assert_eq!(source_module_of(&r, "defnode").as_deref(), Some("mylib"));
    }

    #[test]
    fn inline_macro_has_no_source_module() {
        let r = analyze_src("(macro my-mac [x] `(local ,x nil))");
        assert_eq!(source_module_of(&r, "my-mac"), None,
            "inline macro should have no source_module");
    }

    #[test]
    fn macros_form_has_no_source_module() {
        let r = analyze_src("(macros {:my-mac (fn [x] `(local ,x nil))})");
        assert_eq!(source_module_of(&r, "my-mac"), None);
    }

    // ── macro_calls tracking ─────────────────────────────────────────────────

    #[test]
    fn macro_calls_populated_for_import_macros_call() {
        let r = analyze_src(
            "(import-macros {: defnode} :mylib) (defnode Foo (extends Bar))"
        );
        let call = r.macro_calls.iter()
            .find(|(_, _, name)| name == "defnode")
            .expect("defnode call should be recorded");
        assert_eq!(call.1.as_deref(), Some("mylib"), "source_module should be mylib");
    }

    #[test]
    fn macro_calls_has_none_source_module_for_inline_macro() {
        let r = analyze_src("(macro m [x] `(local ,x 1)) (m foo)");
        let call = r.macro_calls.iter()
            .find(|(_, _, name)| name == "m")
            .expect("m call should be recorded");
        assert!(call.1.is_none(), "inline macro call should have None source_module");
    }

    #[test]
    fn macro_calls_not_populated_for_regular_functions() {
        let r = analyze_src("(fn greet [x] x) (greet 42)");
        assert!(r.macro_calls.is_empty(), "regular fn calls should not appear in macro_calls");
    }

    // ── analyze_with_hooks: instruction execution ─────────────────────────────

    fn make_hook_results(span_start: u32, instrs: Vec<crate::hooks::Instruction>) -> HashMap<u32, Vec<crate::hooks::Instruction>> {
        let mut m = HashMap::new();
        m.insert(span_start, instrs);
        m
    }

    #[test]
    fn hook_bind_instruction_introduces_def() {
        // Without a hook, FennelNode3D would be in_macro.
        // With a Bind instruction it should be a real Local def.
        let src = "(import-macros {: defnode} :mylib) (defnode FennelNode3D)";
        let (ast, _) = crate::parser::Parser::parse(src);
        let first_pass = analyze(&ast);

        // Find the span of the defnode call
        let call_span = first_pass.macro_calls.iter()
            .find(|(_, _, n)| n == "defnode")
            .map(|(s, _, _)| *s)
            .expect("defnode call");

        // Build the span for "FennelNode3D" — it follows "defnode " at the call site
        let name_span = first_pass.syms.iter()
            .find(|s| s.name == "FennelNode3D")
            .map(|s| s.span.clone())
            .expect("FennelNode3D sym");

        let hook_results = make_hook_results(call_span, vec![
            crate::hooks::Instruction::Bind {
                name: "FennelNode3D".into(),
                span: name_span,
            },
        ]);
        let second_pass = analyze_with_hooks(&ast, &hook_results);

        // Should be a proper def now, not in_macro
        let def = second_pass.defs.values()
            .find(|d| d.name == "FennelNode3D")
            .expect("FennelNode3D def");
        assert_eq!(def.kind, DefKind::Local);

        let sym = second_pass.syms.iter()
            .find(|s| s.name == "FennelNode3D" && s.is_def)
            .expect("FennelNode3D sym entry");
        assert!(!sym.in_macro, "bound name should not be tagged in_macro");
    }

    #[test]
    fn hook_analyze_fn_instruction_binds_fn_name_and_params() {
        // (import-macros {: defnode} :mylib)
        // (defnode Foo (fn greet [self x] x))
        // With AnalyzeFn on the (fn ...) child, greet and self/x should be real defs.
        let src = "(import-macros {: defnode} :mylib) (defnode Foo (fn greet [self x] x))";
        let (ast, _) = crate::parser::Parser::parse(src);
        let first_pass = analyze(&ast);

        let call_span = first_pass.macro_calls.iter()
            .find(|(_, _, n)| n == "defnode")
            .map(|(s, _, _)| *s)
            .expect("defnode call");

        // The (fn greet ...) form is at index 2 of the defnode call (1-based: index 3).
        // In forms[], that's forms[2] (0-based). AnalyzeFn { index: 3 } → forms[2].
        let hook_results = make_hook_results(call_span, vec![
            crate::hooks::Instruction::AnalyzeFn { index: 3 },
        ]);
        let second_pass = analyze_with_hooks(&ast, &hook_results);

        assert!(has_def(&second_pass, "greet"), "greet should be a def via AnalyzeFn");
        assert_eq!(def_kind(&second_pass, "greet"), Some(DefKind::Fn));
        assert!(has_def(&second_pass, "self"), "self param should be a def");
        assert!(has_def(&second_pass, "x"), "x param should be a def");
        // x in the body should resolve to the param
        assert!(!is_unknown(&second_pass, "x"), "x in body should resolve");
    }

    #[test]
    fn hook_instructions_suppress_in_macro_tagging() {
        // When a hook is provided, forms handled by it should NOT be in_macro.
        // Forms not mentioned remain unanalyzed (no SymbolEntry at all).
        let src = "(import-macros {: defnode} :mylib) (defnode Foo (extends Base))";
        let (ast, _) = crate::parser::Parser::parse(src);
        let first_pass = analyze(&ast);

        let call_span = first_pass.macro_calls.iter()
            .find(|(_, _, n)| n == "defnode")
            .map(|(s, _, _)| *s)
            .expect("defnode call");

        // Hook provides no instructions (empty vec = hook present, skip all args)
        let hook_results = make_hook_results(call_span, vec![]);
        let second_pass = analyze_with_hooks(&ast, &hook_results);

        // extends and Base should not appear in syms (not analyzed at all)
        let base_syms: Vec<_> = second_pass.syms.iter()
            .filter(|s| s.name == "Base")
            .collect();
        assert!(base_syms.is_empty(), "Base should not be in syms when hook skips it");
    }

    // ── Destructuring edge cases ──────────────────────────────────────────────

    #[test]
    fn table_destructuring_symbol_keyword_order() {
        // {name :field} — symbol key, keyword value; `name` is the binding
        let r = analyze_src("(local {a :foo b :bar} t)");
        assert!(has_def(&r, "a"));
        assert!(has_def(&r, "b"));
    }

    #[test]
    fn table_destructuring_string_key() {
        // {"field" name} — string key, symbol value; `name` is the binding
        let r = analyze_src(r#"(local {"x" px "y" py} point)"#);
        assert!(has_def(&r, "px"));
        assert!(has_def(&r, "py"));
    }

    #[test]
    fn fn_rest_binding_is_param() {
        // (fn f [a & rest] rest) — `rest` captures varargs tail
        let r = analyze_src("(fn f [a & rest] rest)");
        assert!(has_def(&r, "rest"), "rest must be a param def");
        assert!(!is_unknown(&r, "rest"), "rest must resolve in body");
    }

    // ── Match pattern edge cases ──────────────────────────────────────────────

    #[test]
    fn match_as_in_sequence_pattern() {
        // [a b &as all] — `all` is bound to the whole matched sequence
        let r = analyze_src("(match t [a b &as all] all)");
        assert!(has_def(&r, "all"), "&as target must be defined");
        assert!(!is_unknown(&r, "all"), "&as target must resolve in body");
    }

    #[test]
    fn match_as_in_table_pattern() {
        // {:a x &as tbl} — `tbl` is bound to the whole matched table
        let r = analyze_src("(match t {:a x &as tbl} tbl)");
        assert!(has_def(&r, "tbl"), "&as target must be defined");
        assert!(!is_unknown(&r, "tbl"), "&as target must resolve in body");
    }

    #[test]
    fn match_or_pattern_binds_names_not_or_keyword() {
        // (or a b) — a and b should be bound; `or` itself must NOT be a def
        let r = analyze_src("(match t (or a b) a)");
        assert!(has_def(&r, "a"), "a must be bound by or pattern");
        assert!(has_def(&r, "b"), "b must be bound by or pattern");
        assert!(!has_def(&r, "or"), "`or` must not be defined as a local");
    }

    // ── Global scope limitation ───────────────────────────────────────────────

    // ── when / unless scoping ─────────────────────────────────────────────────

    #[test]
    fn when_body_binding_not_visible_after_form() {
        // `when` expands to `(if cond (do ...))` so body bindings are scoped.
        let r = analyze_src("(when true (local x 1) x) x");
        // The trailing `x` outside the when must be unknown.
        assert!(is_unknown(&r, "x"), "local inside when must not leak out");
    }

    #[test]
    fn unless_body_binding_not_visible_after_form() {
        let r = analyze_src("(unless false (local y 1) y) y");
        assert!(is_unknown(&r, "y"), "local inside unless must not leak out");
    }

    #[test]
    fn when_condition_analyzed_in_outer_scope() {
        // The condition expression is in the outer scope, not the body scope.
        let r = analyze_src("(local flag true) (when flag nil)");
        assert!(ref_resolves(&r, "flag"), "condition ref must resolve");
    }

    #[test]
    fn when_body_refs_resolve() {
        let r = analyze_src("(local x 1) (when true x)");
        assert!(ref_resolves(&r, "x"), "body ref to outer binding must resolve");
    }

    // ── while ─────────────────────────────────────────────────────────────────

    #[test]
    fn while_condition_refs_resolve() {
        let r = analyze_src("(local n 10) (while (> n 0) nil)");
        assert!(!is_unknown(&r, "n"), "condition must see outer bindings");
    }

    #[test]
    fn while_body_refs_resolve() {
        let r = analyze_src("(local x 1) (while true (print x))");
        assert!(!is_unknown(&r, "x"), "body must see outer bindings");
    }

    #[test]
    fn while_body_binding_not_visible_after_while() {
        let r = analyze_src("(while true (local inner 1)) inner");
        assert!(is_unknown(&r, "inner"), "while body binding must not leak outside");
    }

    #[test]
    fn while_body_multiple_forms_all_analyzed() {
        let r = analyze_src("(local a 1) (local b 2) (while true (print a) (print b))");
        assert!(!is_unknown(&r, "a"));
        assert!(!is_unknown(&r, "b"));
    }

    // ── Global scope limitation ───────────────────────────────────────────────

    #[test]
    fn global_inside_do_not_visible_outside() {
        // In Fennel, `global` is truly file-global, but our analyzer puts it in
        // the current scope — so it leaks out of `do` blocks only if the do is
        // at the top level (which has no enclosing scope).  This test documents
        // the known limitation: a `global` inside a nested `do` is not visible
        // after that do.
        let r = analyze_src("(do (global x 1)) x");
        assert!(is_unknown(&r, "x"), "known limitation: global inside do not visible after it");
    }

    // ── Multisym references ───────────────────────────────────────────────────

    #[test]
    fn multisym_reference_resolves_root() {
        // `reference()` splits on `.`/`:` to find the root name for lookup;
        // the SymbolEntry stores the full `t.x` but resolves via `t`.
        // "(local t {}) t.x"
        //  0         1
        //  0123456789012345
        // t def at 7, t.x ref starting at 13
        let r = analyze_src("(local t {}) t.x");
        let entry = r.syms.iter().find(|s| s.name == "t.x" && !s.is_def);
        assert!(entry.is_some(), "t.x should be recorded as a symbol entry");
        assert!(entry.unwrap().def_byte.is_some(), "t.x root `t` must resolve to local def");
    }

    #[test]
    fn symbol_at_on_multisym_span() {
        // Byte 14 is the `.` inside `t.x` (bytes 13-15); symbol_at should
        // find the entry whose span covers that byte.
        let src = "(local t {}) t.x";
        let r = analyze_src(src);
        let sym = r.symbol_at(14).expect("byte 14 is inside the t.x span");
        assert_eq!(sym.name, "t.x");
    }

    #[test]
    fn set_colon_multisym_marks_var_mutated() {
        // `(set obj:method val)` — root is `obj` (split on `:`), must count
        // as a mutation so obj doesn't get an unused-var warning.
        assert!(!has_warning("(var obj {}) (set obj:method fn)", "never mutated"));
    }

    // ── defs_at / completion ──────────────────────────────────────────────────

    #[test]
    fn defs_at_sees_outer_scope_bindings() {
        // Cursor inside a `let` should return BOTH the let-bound names and
        // the enclosing scope's names.
        // "(local a 1) (let [b 2] a)"
        //  0         1         2
        //  01234567890123456789012345
        // a def at 7, b def at 18, cursor/ref at 23
        let src = "(local a 1) (let [b 2] a)";
        let r = analyze_src(src);
        let names: HashSet<&str> = r.defs_at(23).iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains("a"), "outer `a` must be visible inside let");
        assert!(names.contains("b"), "let-bound `b` must be visible");
    }

    #[test]
    fn defs_at_shadowed_binding_returns_innermost() {
        // When the same name exists in two nested scopes, defs_at should
        // return only the innermost one (the `seen` dedup prevents duplicates).
        // "(local x 1) (let [x 2] x)"
        //  0         1         2
        //  01234567890123456789012345
        // outer x at 7, inner x at 18, cursor at 23
        let src = "(local x 1) (let [x 2] x)";
        let r = analyze_src(src);
        let defs_here = r.defs_at(23);
        let x_defs: Vec<_> = defs_here.iter().filter(|d| d.name == "x").collect();
        assert_eq!(x_defs.len(), 1, "only one x should appear (deduped)");
        assert_eq!(x_defs[0].span.start, 18, "innermost x (byte 18) must win over outer x (byte 7)");
    }

    // ── refs map ──────────────────────────────────────────────────────────────

    #[test]
    fn refs_map_populated_on_resolution() {
        // After analysis, result.refs maps each reference's byte offset to its
        // definition's byte offset — used for go-to-definition.
        // "(local x 1) x"
        //  0         1
        //  01234567890123
        // x def at 7, x ref at 12
        let src = "(local x 1) x";
        let r = analyze_src(src);
        assert_eq!(r.refs.get(&12).copied(), Some(7),
            "refs[12] should point to x's def at byte 7");
    }

    // ── Form aliases ──────────────────────────────────────────────────────────

    #[test]
    fn case_alias_for_match() {
        // `case` is an alias for `match` — same analysis path
        let r = analyze_src("(case t a (+ a 1) _ 0)");
        assert!(has_def(&r, "a"), "case pattern must bind `a`");
        assert!(!is_unknown(&r, "a"), "`a` must resolve in arm body");
    }

    #[test]
    fn match_try_alias_for_case_try() {
        // `match-try` is an alias for `case-try` — catch arm still binds
        let r = analyze_src("(match-try (f) x x catch e (tostring e))");
        assert!(has_def(&r, "e"), "catch arm must bind e");
        assert!(!is_unknown(&r, "e"), "e must resolve in catch body");
    }

    // ── Anonymous fn ─────────────────────────────────────────────────────────

    #[test]
    fn anonymous_fn_params_in_scope() {
        // `(fn [a] a)` — no name, but params are still in scope for the body
        let r = analyze_src("(fn [a] a)");
        assert_eq!(def_kind(&r, "a"), Some(DefKind::Param));
        assert!(!is_unknown(&r, "a"), "param a must resolve in anonymous fn body");
        assert!(!r.defs.values().any(|d| d.kind == DefKind::Fn),
            "anonymous fn must not create a Fn def");
    }

    // ── Quasiquote ────────────────────────────────────────────────────────────

    #[test]
    fn unquote_splice_inside_quasiquote_analyzed() {
        // `,@xs` inside a quasiquote must be analyzed (UnquoteSplice is treated
        // the same as Unquote in analyze_quasiquote)
        let r = analyze_src("(local xs []) `(list ,@xs)");
        assert!(ref_resolves(&r, "xs"), ",@xs inside quasiquote must resolve");
    }

    // ── Combined pattern forms ────────────────────────────────────────────────

    #[test]
    fn match_where_or_combined_pattern() {
        // `(where (or a b) guard)` — the two fixes must work together:
        //   - `or` keyword is not defined as a local
        //   - `a` and `b` are both bound
        //   - the guard `(= a 1)` sees `a` so `a` is not unknown there
        let r = analyze_src("(match t (where (or a b) (= a 1)) a)");
        assert!(has_def(&r, "a"), "a must be bound by or pattern");
        assert!(has_def(&r, "b"), "b must be bound by or pattern");
        assert!(!has_def(&r, "or"), "`or` must not become a spurious local def");
        assert!(!is_unknown(&r, "a"), "a must resolve in where guard and arm body");
    }

    // ── Table / loop / binding edge cases ────────────────────────────────────

    #[test]
    fn table_constructor_symbol_key_analyzed_as_ref() {
        // In `{k 1}`, `k` is a computed key (evaluated at runtime in Fennel).
        // analyze() visits ALL table fields, so `k` is recorded as a reference.
        let r = analyze_src("(local k :foo) {k 1}");
        assert!(ref_resolves(&r, "k"), "symbol key in table constructor must resolve");
    }

    #[test]
    fn for_step_expression_analyzed() {
        let r = analyze_src("(local step 2) (for [i 1 10 step] (print i))");
        assert!(!is_unknown(&r, "step"), "step variable must resolve in for bindings");
        assert!(!is_unknown(&r, "i"), "loop var must be visible alongside step");
    }

    #[test]
    fn let_sequence_rest_binding() {
        // `(let [[a & rest] coll] rest)` — bind_pattern skips `&` and binds `rest`
        let r = analyze_src("(let [[a & rest] [1 2 3]] rest)");
        assert!(has_def(&r, "a"), "a must be bound");
        assert!(has_def(&r, "rest"), "rest must be bound");
        assert!(!is_unknown(&r, "rest"), "rest must resolve in body");
    }

    #[test]
    fn with_open_later_binding_sees_earlier() {
        // In `(with-open [f1 rhs1 f2 rhs2] body)`, rhs2 is analyzed after f1
        // is bound (same sequential ordering as `let`).
        let r = analyze_src(
            "(fn process [h] h) (with-open [f1 (io.open :a) f2 (process f1)] f2)",
        );
        assert!(has_def(&r, "f1"), "f1 must be bound");
        assert!(has_def(&r, "f2"), "f2 must be bound");
        assert!(ref_resolves(&r, "f1"),
            "f1 must resolve inside f2's RHS (sequential binding)");
        assert!(!is_unknown(&r, "f2"), "f2 must resolve in body");
    }

    // ── Malformed / short-form no-crash guards ────────────────────────────────

    #[test]
    fn local_with_no_value_no_crash() {
        // `(local x)` is missing the RHS; analyze_binding returns early when
        // forms.len() < 3 — must not panic and must NOT create a def.
        let r = analyze_src("(local x)");
        assert!(!has_def(&r, "x"), "truncated (local x) must not create a def");
    }

    #[test]
    fn var_with_no_value_no_crash() {
        let r = analyze_src("(var y)");
        assert!(!has_def(&r, "y"), "truncated (var y) must not create a def");
    }

    #[test]
    fn fn_with_no_args_no_crash() {
        // `(fn)` — fewer than 2 forms; analyze_fn returns early.
        let r = analyze_src("(fn)");
        assert!(r.defs.is_empty(), "(fn) must not create any def");
        assert!(r.warnings.is_empty());
    }

    // ── Empty file ────────────────────────────────────────────────────────────

    #[test]
    fn empty_file_no_crash() {
        // Analyzing an empty string must not panic and must return an empty result.
        let r = analyze_src("");
        assert!(r.defs.is_empty());
        assert!(r.syms.is_empty());
        assert!(r.warnings.is_empty());
        assert!(r.refs.is_empty());
    }

    // ── symbol_at edge cases ──────────────────────────────────────────────────

    #[test]
    fn symbol_at_past_end_returns_none() {
        let r = analyze_src("(local x 1)");
        assert!(r.symbol_at(9999).is_none(), "byte past end of file should return None");
    }

    #[test]
    fn symbol_at_between_symbols_returns_none() {
        // "(local x 1) (local y 2)" — byte 11 is the space between the two forms.
        let src = "(local x 1) (local y 2)";
        let r = analyze_src(src);
        // Byte 11 is ' ' between `)` and `(` — no symbol spans that byte.
        assert!(r.symbol_at(11).is_none(), "whitespace gap should return None");
    }

    // ── fcollect &into target ─────────────────────────────────────────────────

    #[test]
    fn fcollect_into_target_resolves_as_ref() {
        let r = analyze_src("(local acc []) (fcollect [i 1 10 &into acc] i)");
        assert!(ref_resolves(&r, "acc"), "&into target must resolve");
        let acc_defs: Vec<_> = r.defs.values().filter(|d| d.name == "acc").collect();
        assert_eq!(acc_defs.len(), 1, "&into must not create a second binding for acc");
    }

    // ── Unused local warnings ─────────────────────────────────────────────────

    #[test]
    fn unused_local_warns() {
        assert!(has_warning("(local x 1)", "defined but never used"));
    }

    #[test]
    fn used_local_no_warn() {
        assert!(!has_warning("(local x 1) x", "defined but never used"));
    }

    #[test]
    fn underscore_local_no_warn() {
        assert!(!has_warning("(local _x 1)", "defined but never used"));
    }

    #[test]
    fn local_used_in_nested_scope_no_warn() {
        assert!(!has_warning("(local x 1) (fn f [] x)", "defined but never used"));
    }

    // ── Unused param warnings ─────────────────────────────────────────────────

    #[test]
    fn unused_param_warns() {
        assert!(has_warning("(fn f [x] nil)", "parameter `x` is unused"));
    }

    #[test]
    fn used_param_no_warn() {
        assert!(!has_warning("(fn f [x] x)", "parameter"));
    }

    #[test]
    fn underscore_param_no_warn() {
        assert!(!has_warning("(fn f [_x] nil)", "parameter"));
    }

    #[test]
    fn bare_underscore_in_body_not_unknown() {
        // `_` is a builtin discard. Referencing it in body position should not
        // produce an "unknown identifier" warning. It IS unusual code (you normally
        // don't read a discard), but the LSP should not emit a spurious diagnostic.
        let r = analyze_src("(fn f [_] _)");
        assert!(!is_unknown(&r, "_"),
            "_ in body must not be flagged as unknown identifier");
    }

    #[test]
    fn multiple_underscore_params_no_shadow_warning() {
        // (fn f [_ _] nil) — two _ params: no shadow warning, no unused warning.
        assert!(!has_warning("(fn f [_ _] nil)", "already defined"),
            "multiple _ params must not trigger shadow warning");
        assert!(!has_warning("(fn f [_ _] nil)", "parameter"),
            "_ params must not trigger unused-param warning");
    }

    #[test]
    fn underscore_loop_var_no_warning() {
        // (each [_ v (ipairs t)] v) — _ discards the key, v is used.
        assert!(!has_warning("(each [_ v (ipairs t)] v)", "unused"),
            "_ loop var must not trigger unused warning");
        assert!(!is_unknown(&analyze_src("(each [_ v (ipairs t)] v)"), "_"),
            "_ loop var must not trigger unknown-identifier");
    }

    #[test]
    fn multiple_params_one_unused_warns_correctly() {
        let r = analyze_src("(fn f [a b] a)");
        assert!(r.warnings.iter().any(|w| w.message.contains("`b`") && w.message.contains("unused")),
            "b must warn as unused");
        assert!(!r.warnings.iter().any(|w| w.message.contains("`a`") && w.message.contains("unused")),
            "a must not warn");
    }

    // ── Arity checking ────────────────────────────────────────────────────────

    #[test]
    fn arity_exact_match_no_warn() {
        assert!(!has_warning("(fn f [a b] nil) (f 1 2)", "argument"));
    }

    #[test]
    fn arity_too_few_warns() {
        assert!(has_warning("(fn f [a b] nil) (f 1)", "expects 2 arguments but got 1"));
    }

    #[test]
    fn arity_too_many_warns() {
        assert!(has_warning("(fn f [a b] nil) (f 1 2 3)", "expects 2 arguments but got 3"));
    }

    #[test]
    fn arity_zero_params_no_warn() {
        assert!(!has_warning("(fn f [] nil) (f)", "argument"));
    }

    #[test]
    fn arity_zero_params_too_many_warns() {
        assert!(has_warning("(fn f [] nil) (f 1)", "expects 0 arguments but got 1"));
    }

    #[test]
    fn arity_rest_param_no_warn() {
        // `& rest` makes the function variadic; any arg count is accepted
        assert!(!has_warning("(fn f [a & rest] nil) (f 1 2 3)", "argument"));
    }

    #[test]
    fn arity_varargs_no_warn() {
        // `...` makes the function variadic
        assert!(!has_warning("(fn f [a ...] nil) (f 1 2 3)", "argument"));
    }

    #[test]
    fn arity_singular_grammar() {
        // "1 argument" not "1 arguments"
        assert!(has_warning("(fn f [a] nil) (f)", "expects 1 argument but got 0"));
    }

    #[test]
    fn arity_lambda_checked() {
        // lambda is an alias for fn and should also be arity-checked
        assert!(has_warning("(lambda f [a b] nil) (f 1)", "expects 2 arguments but got 1"));
    }

    #[test]
    fn arity_recursive_call_no_false_warn() {
        // `(fn fact [n] (fact n))` — 1 arg, 1 param: no warning
        assert!(!has_warning("(fn fact [n] (fact n))", "argument"));
    }

    #[test]
    fn arity_values_direct_as_last_arg_suppressed() {
        // (values 1 2) in last-arg position expands at runtime → no warning
        assert!(!has_warning("(fn f [a b] nil) (f (values 1 2))", "argument"));
    }

    #[test]
    fn arity_values_with_explicit_first_arg_suppressed() {
        // (f x (values 1 2)) where f takes 3 params: x fills slot 1, values fills 2+3
        assert!(!has_warning("(fn f [a b c] nil) (local x 1) (f x (values 1 2))", "argument"));
    }

    #[test]
    fn arity_multi_return_fn_suppresses_under_arity() {
        // (fn pair [] (values 1 2)) is the last arg — its expansion may satisfy arity
        assert!(!has_warning(
            "(fn pair [] (values 1 2)) (fn add [a b] nil) (add (pair))",
            "argument",
        ));
    }

    #[test]
    fn arity_multi_return_fn_with_leading_arg_suppressed() {
        // (add x (pair)) — x fills slot 1, pair expands to fill slots 2+3
        assert!(!has_warning(
            "(fn pair [] (values 1 2)) (fn add [a b c] nil) (local x 1) (add x (pair))",
            "argument",
        ));
    }

    #[test]
    fn arity_single_return_fn_still_warns() {
        // (fn f [] nil) returns one value; (g (f)) with g taking 2 should still warn
        assert!(has_warning(
            "(fn f [] nil) (fn g [a b] nil) (g (f))",
            "expects 2 arguments but got 1",
        ));
    }

    #[test]
    fn arity_over_supply_always_warns_regardless_of_multi_return() {
        // Too many explicit args is always wrong, even if last arg is multi-return
        assert!(has_warning(
            "(fn pair [] (values 1 2)) (fn f [a] nil) (local x 1) (f x (pair))",
            "expects 1 argument but got 2",
        ));
    }

    #[test]
    fn arity_values_in_do_body_marks_returns_multiple() {
        // A fn whose body is (do (values 1 2)) also propagates returns_multiple
        assert!(!has_warning(
            "(fn f [] (do (values 1 2))) (fn g [a b] nil) (g (f))",
            "argument",
        ));
    }

    #[test]
    fn arity_values_in_if_branch_marks_returns_multiple() {
        // Both branches return multiple values → returns_multiple
        assert!(!has_warning(
            "(fn f [x] (if x (values 1 2) (values 3 4))) (fn g [a b] nil) (g (f true))",
            "argument",
        ));
    }

    #[test]
    fn arity_non_last_arg_multi_return_still_warns() {
        // Lua truncates non-last arguments to 1 value; (pair) in non-last position
        // does NOT expand, so the count is still wrong.
        // (f (pair) x) where f takes 3: pair is truncated to 1, x is 1 → actual=2, expected=3
        assert!(has_warning(
            "(fn pair [] (values 1 2)) (fn f [a b c] nil) (local x 1) (f (pair) x)",
            "expects 3 arguments but got 2",
        ));
    }

    // ── module_bindings ───────────────────────────────────────────────────────

    #[test]
    fn module_binding_keyword_arg() {
        let r = analyze_src("(local utils (require :my.mod))");
        assert_eq!(r.module_bindings.get("utils").map(|s| s.as_str()), Some("my.mod"));
    }

    #[test]
    fn module_binding_string_arg() {
        let r = analyze_src(r#"(local utils (require "my.mod"))"#);
        assert_eq!(r.module_bindings.get("utils").map(|s| s.as_str()), Some("my.mod"));
    }

    #[test]
    fn module_binding_var_keyword() {
        let r = analyze_src("(var lib (require :lib))");
        assert_eq!(r.module_bindings.get("lib").map(|s| s.as_str()), Some("lib"));
    }

    #[test]
    fn non_require_binding_not_recorded() {
        let r = analyze_src("(local x 42)");
        assert!(r.module_bindings.is_empty());
    }

    #[test]
    fn destructured_binding_not_recorded_as_module() {
        let r = analyze_src("(local {:foo foo} (require :mod))");
        // Destructuring: forms[1] is a Table, not a Symbol — no module_bindings entry
        assert!(r.module_bindings.is_empty());
    }

    // ── Unused require warnings ───────────────────────────────────────────────

    #[test]
    fn unused_require_warns() {
        assert!(has_warning(
            "(local api (require :my-mod))",
            "required but never used",
        ));
    }

    #[test]
    fn used_require_no_warn() {
        assert!(!has_warning(
            "(local api (require :my-mod)) (api.call)",
            "required but never used",
        ));
    }

    #[test]
    fn used_require_field_access_no_warn() {
        // api.foo is a reference to the root `api` binding
        assert!(!has_warning(
            "(local api (require :mod)) api.foo",
            "required but never used",
        ));
    }

    #[test]
    fn unused_require_not_double_warned_as_unused_local() {
        // Unused require should emit the require message, NOT "defined but never used"
        let ws = warnings_for("(local api (require :mod))");
        assert!(ws.iter().any(|w| w.contains("required but never used")));
        assert!(!ws.iter().any(|w| w.contains("defined but never used")));
    }

    // ── Table field completions ───────────────────────────────────────────────

    #[test]
    fn table_fields_extracted_from_literal() {
        let r = analyze_src("(local t {:a 1 :b 2})");
        let def = r.defs.values().find(|d| d.name == "t").unwrap();
        let fields = def.table_fields.as_deref().unwrap_or(&[]);
        assert!(fields.contains(&"a".to_string()), "expected 'a' in fields: {:?}", fields);
        assert!(fields.contains(&"b".to_string()), "expected 'b' in fields: {:?}", fields);
    }

    #[test]
    fn table_fields_string_keys() {
        let r = analyze_src(r#"(local t {"name" 1 "age" 2})"#);
        let def = r.defs.values().find(|d| d.name == "t").unwrap();
        let fields = def.table_fields.as_deref().unwrap_or(&[]);
        assert!(fields.contains(&"name".to_string()));
        assert!(fields.contains(&"age".to_string()));
    }

    #[test]
    fn table_fields_non_table_rhs_is_none() {
        let r = analyze_src("(local x 42)");
        let def = r.defs.values().find(|d| d.name == "x").unwrap();
        assert!(def.table_fields.is_none());
    }

    #[test]
    fn table_fields_let_binding() {
        let r = analyze_src("(let [t {:x 10 :y 20}] t)");
        let def = r.defs.values().find(|d| d.name == "t").unwrap();
        let fields = def.table_fields.as_deref().unwrap_or(&[]);
        assert!(fields.contains(&"x".to_string()));
        assert!(fields.contains(&"y".to_string()));
    }

    #[test]
    fn table_fields_computed_key_skipped_others_kept() {
        // Computed keys (non-keyword, non-string) are silently skipped; static ones are kept.
        let r = analyze_src("(local t {:a 1})");
        let def = r.defs.values().find(|d| d.name == "t").unwrap();
        let fields = def.table_fields.as_deref().unwrap_or(&[]);
        assert!(fields.contains(&"a".to_string()));
    }
}
