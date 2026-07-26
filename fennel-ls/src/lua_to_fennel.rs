use full_moon::{
    ast::{
        self, Block, Call, Expression, FunctionArgs, FunctionBody, FunctionName, LastStmt,
        Parameter, Prefix, Stmt, Suffix, Var,
    },
    node::Node,
    tokenizer::{TokenReference, TokenType},
};

/// Sourcemap: `source_map[i]` = Lua line (1-indexed) for Fennel output line `i+1`.
pub struct TranspileOutput {
    pub fennel: String,
    pub source_map: Vec<u32>,
}

/// Transpile Lua source to Fennel with a line-level source map.
pub fn transpile(lua: &str) -> Result<TranspileOutput, String> {
    let ast = full_moon::parse(lua)
        .map_err(|errs| errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; "))?;
    let mut ctx = Ctx::default();
    ctx.emit_block(ast.nodes());
    Ok(ctx.finish())
}

#[derive(Default)]
struct Ctx {
    fennel: String,
    source_map: Vec<u32>,
}

impl Ctx {
    fn push_form(&mut self, form: String, lua_line: u32) {
        self.fennel.push_str(&form);
        self.fennel.push('\n');
        self.source_map.push(lua_line);
    }

    fn finish(self) -> TranspileOutput {
        TranspileOutput { fennel: self.fennel, source_map: self.source_map }
    }

    fn emit_block(&mut self, block: &Block) {
        for stmt in block.stmts() {
            let line = node_line(stmt);
            if let Some(f) = self.stmt(stmt) {
                self.push_form(f, line);
            }
        }
        if let Some(last) = block.last_stmt() {
            let line = node_line(last);
            if let Some(f) = self.last_stmt(last) {
                self.push_form(f, line);
            }
        }
    }

    fn inline_block(&self, block: &Block) -> String {
        let mut parts: Vec<String> = block.stmts().filter_map(|s| self.stmt(s)).collect();
        if let Some(f) = block.last_stmt().and_then(|s| self.last_stmt(s)) {
            parts.push(f);
        }
        parts.join(" ")
    }

    fn stmt(&self, stmt: &Stmt) -> Option<String> {
        Some(match stmt {
            Stmt::LocalAssignment(a) => self.local_assign(a),
            Stmt::LocalFunction(f) => self.local_fn(f),
            Stmt::FunctionDeclaration(f) => self.fn_decl(f),
            Stmt::Assignment(a) => self.assign(a),
            Stmt::Do(d) => format!("(do {})", self.inline_block(d.block())),
            Stmt::FunctionCall(c) => self.call_stmt(c),
            Stmt::If(i) => self.if_expr(i),
            Stmt::While(w) => {
                format!("(while {} {})", self.expr(w.condition()), self.inline_block(w.block()))
            }
            Stmt::NumericFor(f) => self.num_for(f),
            Stmt::GenericFor(f) => self.gen_for(f),
            Stmt::Repeat(r) => self.repeat(r),
            _ => return None,
        })
    }

    fn last_stmt(&self, stmt: &LastStmt) -> Option<String> {
        Some(match stmt {
            LastStmt::Return(r) => multi_val(r.returns().iter().map(|e| self.expr(e)).collect()),
            LastStmt::Break(_) => r#"(lua "break")"#.to_string(),
            _ => return None,
        })
    }

    fn local_assign(&self, a: &ast::LocalAssignment) -> String {
        let names: Vec<String> = a.names().iter().map(tok_str).collect();
        let exprs: Vec<String> = a.expressions().iter().map(|e| self.expr(e)).collect();
        let lhs = if names.len() == 1 {
            names[0].clone()
        } else {
            format!("[{}]", names.join(" "))
        };
        let rhs = if exprs.is_empty() { "nil".to_string() } else { multi_val(exprs) };
        format!("(local {} {})", lhs, rhs)
    }

    fn local_fn(&self, f: &ast::LocalFunction) -> String {
        let name = tok_str(f.name());
        let (params, body) = self.fn_body(f.body());
        format!("(fn {} [{}] {})", name, params, body)
    }

    fn fn_decl(&self, f: &ast::FunctionDeclaration) -> String {
        let name = fname_str(f.name());
        let (params, body) = self.fn_body(f.body());
        format!("(fn {} [{}] {})", name, params, body)
    }

    fn fn_body(&self, body: &FunctionBody) -> (String, String) {
        let params: Vec<String> = body
            .parameters()
            .iter()
            .map(|p| match p {
                Parameter::Ellipsis(_) => "...".to_string(),
                Parameter::Name(n) => tok_str(n),
                _ => "_".to_string(),
            })
            .collect();
        (params.join(" "), self.inline_block(body.block()))
    }

    fn assign(&self, a: &ast::Assignment) -> String {
        let vars: Vec<String> = a.variables().iter().map(|v| self.var(v)).collect();
        let exprs: Vec<String> = a.expressions().iter().map(|e| self.expr(e)).collect();
        let lhs = if vars.len() == 1 { vars[0].clone() } else { format!("[{}]", vars.join(" ")) };
        format!("(set {} {})", lhs, multi_val(exprs))
    }

    fn if_expr(&self, i: &ast::If) -> String {
        let cond = self.expr(i.condition());
        let then = self.inline_block(i.block());
        let mut parts = vec![cond, then];

        if let Some(elseifs) = i.else_if() {
            for ei in elseifs {
                parts.push(self.expr(ei.condition()));
                parts.push(self.inline_block(ei.block()));
            }
        }

        if let Some(else_blk) = i.else_block() {
            parts.push(self.inline_block(else_blk));
            format!("(if {})", parts.join(" "))
        } else if parts.len() == 2 {
            format!("(when {})", parts.join(" "))
        } else {
            format!("(if {})", parts.join(" "))
        }
    }

    fn num_for(&self, f: &ast::NumericFor) -> String {
        let var = tok_str(f.index_variable());
        let start = self.expr(f.start());
        let limit = self.expr(f.end());
        let step = f.step().map(|s| format!(" {}", self.expr(s))).unwrap_or_default();
        format!("(for [{} {} {}{}] {})", var, start, limit, step, self.inline_block(f.block()))
    }

    fn gen_for(&self, f: &ast::GenericFor) -> String {
        let names: Vec<String> = f.names().iter().map(tok_str).collect();
        let exprs: Vec<String> = f.expressions().iter().map(|e| self.expr(e)).collect();
        format!(
            "(each [{} {}] {})",
            names.join(" "),
            multi_val(exprs),
            self.inline_block(f.block())
        )
    }

    fn repeat(&self, r: &ast::Repeat) -> String {
        let body = self.inline_block(r.block());
        let cond = self.expr(r.until());
        format!("(while true {} (when {} (lua \"break\")))", body, cond)
    }

    fn call_stmt(&self, c: &ast::FunctionCall) -> String {
        self.build_chain(c.prefix(), c.suffixes())
    }

    fn expr(&self, e: &Expression) -> String {
        match e {
            Expression::Number(n) => tok_str(n),
            Expression::String(s) => str_tok(s),
            Expression::Symbol(s) => tok_str(s),
            Expression::Var(v) => self.var(v),
            Expression::FunctionCall(c) => self.build_chain(c.prefix(), c.suffixes()),
            Expression::Function(f) => {
                let (params, body) = self.fn_body(f.body());
                format!("(fn [{}] {})", params, body)
            }
            Expression::TableConstructor(t) => self.table_ctor(t),
            Expression::BinaryOperator { lhs, binop, rhs } => {
                format!("({} {} {})", binop_str(binop), self.expr(lhs), self.expr(rhs))
            }
            Expression::UnaryOperator { unop, expression } => {
                format!("({} {})", unop_str(unop), self.expr(expression))
            }
            Expression::Parentheses { expression, .. } => self.expr(expression),
            _ => "; unsupported-expr".to_string(),
        }
    }

    fn var(&self, v: &Var) -> String {
        match v {
            Var::Name(n) => tok_str(n),
            Var::Expression(e) => self.build_chain(e.prefix(), e.suffixes()),
            _ => "; unsupported-var".to_string(),
        }
    }

    fn build_chain<'a>(
        &self,
        prefix: &Prefix,
        suffixes: impl Iterator<Item = &'a Suffix>,
    ) -> String {
        let mut base = match prefix {
            Prefix::Name(n) => tok_str(n),
            Prefix::Expression(e) => self.expr(e),
            _ => return "; unsupported-prefix".to_string(),
        };
        for suf in suffixes {
            base = self.apply_suffix(base, suf);
        }
        base
    }

    fn apply_suffix(&self, base: String, suf: &Suffix) -> String {
        match suf {
            Suffix::Index(idx) => match idx {
                ast::Index::Dot { name, .. } => format!("{}.{}", base, tok_str(name)),
                ast::Index::Brackets { expression, .. } => {
                    format!("(. {} {})", base, self.expr(expression))
                }
                _ => base,
            },
            Suffix::Call(call) => match call {
                Call::AnonymousCall(args) => {
                    let args = self.fn_args(args);
                    if args.is_empty() {
                        format!("({})", base)
                    } else {
                        format!("({} {})", base, args.join(" "))
                    }
                }
                Call::MethodCall(m) => {
                    let method = tok_str(m.name());
                    let args = self.fn_args(m.args());
                    if args.is_empty() {
                        format!("(: {} :{})", base, method)
                    } else {
                        format!("(: {} :{} {})", base, method, args.join(" "))
                    }
                }
                _ => base,
            },
            _ => base,
        }
    }

    fn fn_args(&self, args: &FunctionArgs) -> Vec<String> {
        match args {
            FunctionArgs::Parentheses { arguments, .. } => {
                arguments.iter().map(|e| self.expr(e)).collect()
            }
            FunctionArgs::String(s) => vec![str_tok(s)],
            FunctionArgs::TableConstructor(t) => vec![self.table_ctor(t)],
            _ => vec![],
        }
    }

    fn table_ctor(&self, t: &ast::TableConstructor) -> String {
        let fields: Vec<(bool, String)> = t.fields().iter().map(|f| self.field(f)).collect();
        if fields.is_empty() {
            return "{}".to_string();
        }
        if fields.iter().all(|(keyed, _)| !keyed) {
            format!("[{}]", fields.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join(" "))
        } else {
            format!("{{{}}}", fields.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join(" "))
        }
    }

    fn field(&self, f: &ast::Field) -> (bool, String) {
        match f {
            ast::Field::NameKey { key, value, .. } => {
                (true, format!(":{} {}", tok_str(key), self.expr(value)))
            }
            ast::Field::ExpressionKey { key, value, .. } => {
                (true, format!("{} {}", self.expr(key), self.expr(value)))
            }
            ast::Field::NoKey(e) => (false, self.expr(e)),
            _ => (false, String::new()),
        }
    }
}

fn node_line<N: Node>(node: &N) -> u32 {
    node.start_position().map(|p| p.line() as u32).unwrap_or(0)
}

fn tok_str(t: &TokenReference) -> String {
    match t.token().token_type() {
        TokenType::Identifier { identifier } => identifier.to_string(),
        TokenType::Number { text } => text.to_string(),
        TokenType::Symbol { symbol } => symbol.to_string(),
        TokenType::StringLiteral { literal, .. } => format!("\"{}\"", literal),
        _ => String::new(),
    }
}

fn str_tok(t: &TokenReference) -> String {
    match t.token().token_type() {
        TokenType::StringLiteral { literal, .. } => format!("\"{}\"", literal),
        _ => tok_str(t),
    }
}

fn fname_str(name: &FunctionName) -> String {
    let base: Vec<String> = name.names().iter().map(tok_str).collect();
    let s = base.join(".");
    if let Some(m) = name.method_name() {
        format!("{}:{}", s, tok_str(m))
    } else {
        s
    }
}

fn multi_val(exprs: Vec<String>) -> String {
    match exprs.len() {
        0 => "nil".to_string(),
        1 => exprs.into_iter().next().unwrap(),
        _ => format!("(values {})", exprs.join(" ")),
    }
}

fn binop_str(op: &ast::BinOp) -> &'static str {
    use ast::BinOp;
    match op {
        BinOp::And(_) => "and",
        BinOp::Or(_) => "or",
        BinOp::Plus(_) => "+",
        BinOp::Minus(_) => "-",
        BinOp::Star(_) => "*",
        BinOp::Slash(_) => "/",
        BinOp::Percent(_) => "%",
        BinOp::Caret(_) => "^",
        BinOp::TwoDots(_) => "..",
        BinOp::TwoEqual(_) => "=",
        BinOp::TildeEqual(_) => "not=",
        BinOp::LessThan(_) => "<",
        BinOp::LessThanEqual(_) => "<=",
        BinOp::GreaterThan(_) => ">",
        BinOp::GreaterThanEqual(_) => ">=",
        _ => "+",
    }
}

fn unop_str(op: &ast::UnOp) -> &'static str {
    use ast::UnOp;
    match op {
        UnOp::Minus(_) => "-",
        UnOp::Not(_) => "not",
        UnOp::Hash(_) => "#",
        _ => "-",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fnl(lua: &str) -> String {
        transpile(lua).expect("transpile failed").fennel.trim().to_string()
    }

    #[test]
    fn test_local() {
        assert_eq!(fnl("local x = 1"), "(local x 1)");
    }

    #[test]
    fn test_multi_local() {
        assert_eq!(fnl("local a, b = 1, 2"), "(local [a b] (values 1 2))");
    }

    #[test]
    fn test_function() {
        assert_eq!(fnl("local function f(x) return x end"), "(fn f [x] x)");
    }

    #[test]
    fn test_method_call() {
        assert_eq!(fnl("obj:method(1, 2)"), "(: obj :method 1 2)");
    }

    #[test]
    fn test_field_call() {
        assert_eq!(fnl("tbl.fn(x)"), "(tbl.fn x)");
    }

    #[test]
    fn test_if_else() {
        assert_eq!(fnl("if x then y() else z() end"), "(if x (y) (z))");
    }

    #[test]
    fn test_when() {
        assert_eq!(fnl("if x then y() end"), "(when x (y))");
    }

    #[test]
    fn test_for() {
        assert_eq!(fnl("for i = 1, 10 do end"), "(for [i 1 10] )");
    }

    #[test]
    fn test_generic_for() {
        assert_eq!(fnl("for k, v in pairs(t) do end"), "(each [k v (pairs t)] )");
    }

    #[test]
    fn test_table_seq() {
        assert_eq!(fnl("local t = {1, 2, 3}"), "(local t [1 2 3])");
    }

    #[test]
    fn test_table_kv() {
        assert_eq!(fnl("local t = {a = 1}"), "(local t {:a 1})");
    }

    #[test]
    fn test_binop() {
        assert_eq!(fnl("local x = a + b"), "(local x (+ a b))");
    }

    #[test]
    fn test_eq() {
        assert_eq!(fnl("local x = a == b"), "(local x (= a b))");
    }

    #[test]
    fn test_source_map() {
        let out = transpile("local x = 1\nlocal y = 2").unwrap();
        assert_eq!(out.source_map[0], 1);
        assert_eq!(out.source_map[1], 2);
    }

    // ── operators ─────────────────────────────────────────────────────────────

    #[test]
    fn test_not_equal() {
        assert_eq!(fnl("local x = a ~= b"), "(local x (not= a b))");
    }

    #[test]
    fn test_concat() {
        // `..` is right-associative in Lua: a..b..c = a..(b..c)
        assert_eq!(fnl(r#"local s = "hello" .. " " .. "world""#),
                   r#"(local s (.. "hello" (.. " " "world")))"#);
    }

    #[test]
    fn test_unary_not() {
        assert_eq!(fnl("local x = not a"), "(local x (not a))");
    }

    #[test]
    fn test_unary_length() {
        assert_eq!(fnl("local n = #t"), "(local n (# t))");
    }

    #[test]
    fn test_unary_negate() {
        assert_eq!(fnl("local x = -a"), "(local x (- a))");
    }

    #[test]
    fn test_and_or() {
        assert_eq!(fnl("local x = a and b or c"), "(local x (or (and a b) c))");
    }

    #[test]
    fn test_comparison_chain() {
        assert_eq!(fnl("local ok = x >= 0"), "(local ok (>= x 0))");
    }

    // ── control flow ──────────────────────────────────────────────────────────

    #[test]
    fn test_while() {
        assert_eq!(fnl("while x > 0 do x = x - 1 end"),
                   "(while (> x 0) (set x (- x 1)))");
    }

    #[test]
    fn test_repeat_until() {
        assert_eq!(fnl("repeat x = x + 1 until x >= 10"),
                   "(while true (set x (+ x 1)) (when (>= x 10) (lua \"break\")))");
    }

    #[test]
    fn test_numeric_for_with_step() {
        assert_eq!(fnl("for i = 0, 100, 10 do end"), "(for [i 0 100 10] )");
    }

    #[test]
    fn test_if_elseif_else() {
        assert_eq!(
            fnl("if a then x() elseif b then y() else z() end"),
            "(if a (x) b (y) (z))"
        );
    }

    #[test]
    fn test_if_elseif_no_else() {
        assert_eq!(
            fnl("if a then x() elseif b then y() end"),
            "(if a (x) b (y))"
        );
    }

    #[test]
    fn test_do_block() {
        assert_eq!(fnl("do local x = 1 end"), "(do (local x 1))");
    }

    #[test]
    fn test_break() {
        assert_eq!(fnl("while true do break end"), r#"(while true (lua "break"))"#);
    }

    // ── calls ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_nested_call() {
        assert_eq!(fnl("foo(bar(x))"), "(foo (bar x))");
    }

    #[test]
    fn test_chained_method_calls() {
        assert_eq!(fnl("obj:a():b(1)"), "(: (: obj :a) :b 1)");
    }

    #[test]
    fn test_field_index_call() {
        assert_eq!(fnl("tbl.sub.fn(x)"), "(tbl.sub.fn x)");
    }

    #[test]
    fn test_bracket_index() {
        assert_eq!(fnl("local v = t[k]"), "(local v (. t k))");
    }

    #[test]
    fn test_method_no_args() {
        assert_eq!(fnl("obj:reset()"), "(: obj :reset)");
    }

    // ── functions ─────────────────────────────────────────────────────────────

    #[test]
    fn test_global_function() {
        assert_eq!(fnl("function greet(name) return name end"), "(fn greet [name] name)");
    }

    #[test]
    fn test_vararg_function() {
        assert_eq!(fnl("function f(...) return ... end"), "(fn f [...] ...)");
    }

    #[test]
    fn test_anonymous_function_assigned() {
        assert_eq!(fnl("local f = function(x) return x end"), "(local f (fn [x] x))");
    }

    #[test]
    fn test_empty_function() {
        assert_eq!(fnl("local function noop() end"), "(fn noop [] )");
    }

    #[test]
    fn test_multi_return() {
        assert_eq!(fnl("local function f() return 1, 2 end"), "(fn f [] (values 1 2))");
    }

    #[test]
    fn test_table_method() {
        assert_eq!(fnl("function M.foo(x) return x end"), "(fn M.foo [x] x)");
    }

    // ── tables ────────────────────────────────────────────────────────────────

    #[test]
    fn test_empty_table() {
        assert_eq!(fnl("local t = {}"), "(local t {})");
    }

    #[test]
    fn test_table_mixed() {
        // Mixed table: has both keyed and unkeyed fields → emitted as {:k v} form
        // (we group all fields together since Fennel doesn't allow mixed inline)
        let out = fnl("local t = {a = 1, 2}");
        assert!(out.starts_with("(local t {"), "got: {out}");
    }

    #[test]
    fn test_nested_table() {
        assert_eq!(fnl("local t = {inner = {1, 2}}"), "(local t {:inner [1 2]})");
    }

    // ── set / assignment ──────────────────────────────────────────────────────

    #[test]
    fn test_global_assignment() {
        assert_eq!(fnl("x = 42"), "(set x 42)");
    }

    #[test]
    fn test_field_assignment() {
        assert_eq!(fnl("tbl.x = 1"), "(set tbl.x 1)");
    }

    #[test]
    fn test_multi_assign() {
        assert_eq!(fnl("a, b = 1, 2"), "(set [a b] (values 1 2))");
    }

    // ── source map ────────────────────────────────────────────────────────────

    #[test]
    fn test_source_map_skips_blank_lines() {
        // Blank lines don't produce Fennel output, so the second function maps
        // to its actual Lua line (5), not the line after the first function (2).
        let lua = "local function f()\nend\n\n\nlocal function g()\nend";
        let out = transpile(lua).unwrap();
        assert_eq!(out.source_map[0], 1, "f is on Lua line 1");
        assert_eq!(out.source_map[1], 5, "g is on Lua line 5");
    }

    #[test]
    fn test_source_map_three_statements() {
        let lua = "local a = 1\nlocal b = 2\nlocal c = 3";
        let out = transpile(lua).unwrap();
        assert_eq!(out.source_map.len(), 3);
        assert_eq!(out.source_map[0], 1);
        assert_eq!(out.source_map[1], 2);
        assert_eq!(out.source_map[2], 3);
    }

    #[test]
    fn test_source_map_fn_body_not_emitted_at_top_level() {
        // Statements inside a function body are inlined, not top-level forms.
        // The top-level source map should only have one entry (the function itself).
        let lua = "local function f()\n  local x = 1\n  return x\nend";
        let out = transpile(lua).unwrap();
        assert_eq!(out.source_map.len(), 1, "only the function declaration is top-level");
        assert_eq!(out.source_map[0], 1);
    }

    // ── error handling ────────────────────────────────────────────────────────

    #[test]
    fn test_invalid_lua_returns_error() {
        let result = transpile("local x = @@invalid@@");
        assert!(result.is_err(), "invalid Lua should return Err");
    }

    #[test]
    fn test_empty_input() {
        let out = transpile("").unwrap();
        assert_eq!(out.fennel.trim(), "");
        assert!(out.source_map.is_empty());
    }

    // ── integration: real-world-ish patterns ──────────────────────────────────

    #[test]
    fn test_module_pattern() {
        let lua = "\
local function add(a, b)
  return a + b
end
local function greet(name)
  return \"Hello, \" .. name
end";
        let out = transpile(lua).unwrap();
        assert!(out.fennel.contains("(fn add [a b]"), "add function present");
        assert!(out.fennel.contains("(fn greet [name]"), "greet function present");
        // Source map: add on line 1, greet on line 4
        assert_eq!(out.source_map[0], 1);
        assert_eq!(out.source_map[1], 4);
    }

    #[test]
    fn test_oop_method_definitions() {
        let lua = "function Obj:new(x)\n  return x\nend";
        let out = transpile(lua).unwrap();
        assert!(out.fennel.contains("(fn Obj:new [x]"), "method definition form: {}", out.fennel);
    }
}
