/// Fennel parser — converts a token stream into an AST.

use crate::lexer::{Span, SpannedToken, Token};

/// A node in the Fennel AST, paired with its source span.
#[derive(Debug, Clone)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

/// Fennel AST node types.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Form {
    // Atoms
    Symbol(String),
    Keyword(String),
    Str(String),
    Number(f64),
    Bool(bool),
    Nil,
    Varargs,
    // Compound
    List(Vec<Spanned<Form>>),
    Table(Vec<Spanned<Form>>),
    Sequence(Vec<Spanned<Form>>),
    // Reader macros
    Quote(Box<Spanned<Form>>),
    Quasiquote(Box<Spanned<Form>>),
    Unquote(Box<Spanned<Form>>),
    UnquoteSplice(Box<Spanned<Form>>),
    HashFn(Box<Spanned<Form>>),
}

pub type AstNode = Spanned<Form>;

/// Parse errors that don't halt parsing (recovery is best-effort).
#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

pub struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
    pub errors: Vec<ParseError>,
}

impl Parser {
    pub fn new(tokens: Vec<SpannedToken>) -> Self {
        Self {
            tokens,
            pos: 0,
            errors: Vec::new(),
        }
    }

    pub fn parse(src: &str) -> (Vec<AstNode>, Vec<ParseError>) {
        let tokens = crate::lexer::Lexer::tokenize(src);
        let mut parser = Self::new(tokens);
        let forms = parser.parse_all();
        (forms, parser.errors)
    }

    fn peek(&self) -> Option<&SpannedToken> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<&SpannedToken> {
        let t = self.tokens.get(self.pos)?;
        self.pos += 1;
        Some(t)
    }

    fn parse_all(&mut self) -> Vec<AstNode> {
        let mut forms = Vec::new();
        while self.peek().is_some() {
            match self.parse_form() {
                Some(f) => forms.push(f),
                None => {
                    // Skip unmatched closing delimiter
                    let span = self.peek().map(|t| t.span.clone());
                    self.advance();
                    if let Some(s) = span {
                        self.errors.push(ParseError {
                            message: "unexpected closing delimiter".into(),
                            span: s,
                        });
                    }
                }
            }
        }
        forms
    }

    fn parse_form(&mut self) -> Option<AstNode> {
        let tok = self.advance()?;
        let span = tok.span.clone();

        let form = match &tok.token {
            Token::LParen => {
                let (forms, end_span) = self.parse_until(Token::RParen, &span)?;
                let merged = Span::merge(&span, &end_span);
                return Some(Spanned { node: Form::List(forms), span: merged });
            }
            Token::LBrace => {
                let (forms, end_span) = self.parse_until(Token::RBrace, &span)?;
                let merged = Span::merge(&span, &end_span);
                return Some(Spanned { node: Form::Table(forms), span: merged });
            }
            Token::LBracket => {
                let (forms, end_span) = self.parse_until(Token::RBracket, &span)?;
                let merged = Span::merge(&span, &end_span);
                return Some(Spanned { node: Form::Sequence(forms), span: merged });
            }
            Token::Quote => {
                let inner = self.parse_form()?;
                let merged = Span::merge(&span, &inner.span);
                return Some(Spanned {
                    node: Form::Quote(Box::new(inner)),
                    span: merged,
                });
            }
            Token::Quasiquote => {
                let inner = self.parse_form()?;
                let merged = Span::merge(&span, &inner.span);
                return Some(Spanned {
                    node: Form::Quasiquote(Box::new(inner)),
                    span: merged,
                });
            }
            Token::Unquote => {
                let inner = self.parse_form()?;
                let merged = Span::merge(&span, &inner.span);
                return Some(Spanned {
                    node: Form::Unquote(Box::new(inner)),
                    span: merged,
                });
            }
            Token::UnquoteSplice => {
                let inner = self.parse_form()?;
                let merged = Span::merge(&span, &inner.span);
                return Some(Spanned {
                    node: Form::UnquoteSplice(Box::new(inner)),
                    span: merged,
                });
            }
            Token::HashFn => {
                let inner = self.parse_form()?;
                let merged = Span::merge(&span, &inner.span);
                return Some(Spanned {
                    node: Form::HashFn(Box::new(inner)),
                    span: merged,
                });
            }
            // Atoms
            Token::Symbol(s) => Form::Symbol(s.clone()),
            Token::Keyword(s) => Form::Keyword(s.clone()),
            Token::Str(s) => Form::Str(s.clone()),
            Token::Number(n) => Form::Number(*n),
            Token::Bool(b) => Form::Bool(*b),
            Token::Nil => Form::Nil,
            Token::Varargs => Form::Varargs,
            // Unmatched closing delimiters — error recovery
            Token::RParen | Token::RBrace | Token::RBracket => {
                self.errors.push(ParseError {
                    message: "unexpected closing delimiter".into(),
                    span: span.clone(),
                });
                return None;
            }
        };

        Some(Spanned { node: form, span })
    }

    /// Parse forms until the matching closing delimiter.
    /// Returns (forms, closing-span) or emits an error and returns None.
    fn parse_until(&mut self, close: Token, open_span: &Span) -> Option<(Vec<AstNode>, Span)> {
        let mut forms = Vec::new();
        loop {
            match self.peek() {
                None => {
                    self.errors.push(ParseError {
                        message: "unclosed delimiter".into(),
                        span: open_span.clone(),
                    });
                    // Return what we have so far rather than failing entirely
                    let fake_end = self
                        .tokens
                        .last()
                        .map(|t| t.span.clone())
                        .unwrap_or_else(|| open_span.clone());
                    return Some((forms, fake_end));
                }
                Some(t) if t.token == close => {
                    let end_span = t.span.clone();
                    self.advance();
                    return Some((forms, end_span));
                }
                _ => {
                    if let Some(f) = self.parse_form() {
                        forms.push(f);
                    }
                }
            }
        }
    }
}

/// Walk the AST and call `f` for each node (depth-first, pre-order).
#[allow(dead_code)]
pub fn walk<F: FnMut(&AstNode)>(nodes: &[AstNode], f: &mut F) {
    for node in nodes {
        walk_node(node, f);
    }
}

fn walk_node<F: FnMut(&AstNode)>(node: &AstNode, f: &mut F) {
    f(node);
    match &node.node {
        Form::List(children)
        | Form::Table(children)
        | Form::Sequence(children) => {
            for child in children {
                walk_node(child, f);
            }
        }
        Form::Quote(inner)
        | Form::Quasiquote(inner)
        | Form::Unquote(inner)
        | Form::UnquoteSplice(inner)
        | Form::HashFn(inner) => walk_node(inner, f),
        _ => {}
    }
}

/// Helper: get the head symbol of a list (first element if it's a Symbol).
pub fn head_sym(forms: &[AstNode]) -> Option<&str> {
    forms.first().and_then(|f| {
        if let Form::Symbol(s) = &f.node {
            Some(s.as_str())
        } else {
            None
        }
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> (Vec<AstNode>, Vec<ParseError>) {
        Parser::parse(src)
    }

    fn parse_ok(src: &str) -> Vec<AstNode> {
        let (forms, errors) = parse(src);
        assert!(errors.is_empty(), "unexpected parse errors for {:?}: {:?}", src, errors);
        forms
    }

    fn form(src: &str) -> Form {
        let forms = parse_ok(src);
        assert_eq!(forms.len(), 1, "expected exactly one form, got {}", forms.len());
        forms.into_iter().next().unwrap().node
    }

    fn sym(s: &str) -> Form { Form::Symbol(s.into()) }
    fn kw(s: &str)  -> Form { Form::Keyword(s.into()) }
    fn num(n: f64)  -> Form { Form::Number(n) }
    fn str_(s: &str) -> Form { Form::Str(s.into()) }

    fn list_f(items: Vec<Form>) -> Form {
        Form::List(items.into_iter().map(|n| Spanned { node: n, span: dummy_span() }).collect())
    }
    fn seq_f(items: Vec<Form>) -> Form {
        Form::Sequence(items.into_iter().map(|n| Spanned { node: n, span: dummy_span() }).collect())
    }
    fn tbl_f(items: Vec<Form>) -> Form {
        Form::Table(items.into_iter().map(|n| Spanned { node: n, span: dummy_span() }).collect())
    }
    fn quote_f(inner: Form) -> Form {
        Form::Quote(Box::new(Spanned { node: inner, span: dummy_span() }))
    }
    fn quasi_f(inner: Form) -> Form {
        Form::Quasiquote(Box::new(Spanned { node: inner, span: dummy_span() }))
    }
    fn unquote_f(inner: Form) -> Form {
        Form::Unquote(Box::new(Spanned { node: inner, span: dummy_span() }))
    }
    fn splice_f(inner: Form) -> Form {
        Form::UnquoteSplice(Box::new(Spanned { node: inner, span: dummy_span() }))
    }
    fn hash_fn_f(inner: Form) -> Form {
        Form::HashFn(Box::new(Spanned { node: inner, span: dummy_span() }))
    }

    fn dummy_span() -> crate::lexer::Span {
        crate::lexer::Span { start: 0, end: 0, line: 0, col: 0, end_line: 0, end_col: 0 }
    }

    /// Compare two Forms by structure only, ignoring spans.
    fn forms_match(a: &Form, b: &Form) -> bool {
        match (a, b) {
            (Form::Symbol(x), Form::Symbol(y)) => x == y,
            (Form::Keyword(x), Form::Keyword(y)) => x == y,
            (Form::Str(x), Form::Str(y)) => x == y,
            (Form::Number(x), Form::Number(y)) => (x - y).abs() < 1e-10,
            (Form::Bool(x), Form::Bool(y)) => x == y,
            (Form::Nil, Form::Nil) | (Form::Varargs, Form::Varargs) => true,
            (Form::List(a), Form::List(b))
            | (Form::Sequence(a), Form::Sequence(b))
            | (Form::Table(a), Form::Table(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| forms_match(&x.node, &y.node))
            }
            (Form::Quote(a), Form::Quote(b))
            | (Form::Quasiquote(a), Form::Quasiquote(b))
            | (Form::Unquote(a), Form::Unquote(b))
            | (Form::UnquoteSplice(a), Form::UnquoteSplice(b))
            | (Form::HashFn(a), Form::HashFn(b)) => forms_match(&a.node, &b.node),
            _ => false,
        }
    }

    fn assert_form(src: &str, expected: Form) {
        let got = form(src);
        assert!(
            forms_match(&got, &expected),
            "parse({:?})\n  got:      {:?}\n  expected: {:?}",
            src, got, expected
        );
    }

    // ── Empty input ──────────────────────────────────────────────────────────

    #[test]
    fn empty_input() {
        let (forms, errors) = parse("");
        assert!(forms.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn only_comments() {
        let (forms, errors) = parse("; just a comment\n; another");
        assert!(forms.is_empty());
        assert!(errors.is_empty());
    }

    // ── Atoms ────────────────────────────────────────────────────────────────

    #[test]
    fn atom_nil() {
        assert_form("nil", Form::Nil);
    }

    #[test]
    fn atom_bool() {
        assert_form("true", Form::Bool(true));
        assert_form("false", Form::Bool(false));
    }

    #[test]
    fn atom_number() {
        assert_form("42", num(42.0));
        assert_form("3.14", num(3.14));
        assert_form("0xff", num(255.0));
        assert_form("1e10", num(1e10));
        assert_form("150_000", num(150_000.0));
    }

    #[test]
    fn atom_string() {
        assert_form("\"hello\"", str_("hello"));
        assert_form("\"\"", str_(""));
        assert_form(r#""\n""#, str_("\n"));
        assert_form(r#""\t""#, str_("\t"));
        assert_form(r#""\x20""#, str_(" "));
        assert_form(r#""\032""#, str_(" "));
        assert_form(r#""\u{41}""#, str_("A"));
    }

    #[test]
    fn atom_symbol() {
        assert_form("hello", sym("hello"));
        assert_form("->", sym("->"));
        assert_form("a.b.c", sym("a.b.c"));
        assert_form("obj:method", sym("obj:method"));
    }

    #[test]
    fn atom_keyword() {
        assert_form(":hello", kw("hello"));
        assert_form(":uri", kw("uri"));
    }

    #[test]
    fn atom_varargs() {
        assert_form("...", Form::Varargs);
    }

    // ── Lists ────────────────────────────────────────────────────────────────

    #[test]
    fn empty_list() {
        assert_form("()", list_f(vec![]));
    }

    #[test]
    fn simple_call() {
        assert_form("(+ 1 2)", list_f(vec![sym("+"), num(1.0), num(2.0)]));
    }

    #[test]
    fn nested_list() {
        assert_form(
            "(+ 1 (* 2 3))",
            list_f(vec![
                sym("+"),
                num(1.0),
                list_f(vec![sym("*"), num(2.0), num(3.0)]),
            ]),
        );
    }

    #[test]
    fn list_with_string() {
        assert_form("(print \"hi\")", list_f(vec![sym("print"), str_("hi")]));
    }

    // ── Sequences (square brackets) ──────────────────────────────────────────

    #[test]
    fn empty_sequence() {
        assert_form("[]", seq_f(vec![]));
    }

    #[test]
    fn sequence_of_numbers() {
        assert_form("[1 2 3]", seq_f(vec![num(1.0), num(2.0), num(3.0)]));
    }

    #[test]
    fn sequence_params() {
        assert_form("[x y z]", seq_f(vec![sym("x"), sym("y"), sym("z")]));
    }

    #[test]
    fn sequence_with_commas() {
        // Commas are whitespace
        assert_form("[1, 2, 3]", seq_f(vec![num(1.0), num(2.0), num(3.0)]));
    }

    #[test]
    fn nested_sequence() {
        assert_form(
            "[[a b] c]",
            seq_f(vec![seq_f(vec![sym("a"), sym("b")]), sym("c")]),
        );
    }

    // ── Tables (curly braces) ────────────────────────────────────────────────

    #[test]
    fn empty_table() {
        assert_form("{}", tbl_f(vec![]));
    }

    #[test]
    fn table_keyword_values() {
        assert_form("{:a 1 :b 2}", tbl_f(vec![kw("a"), num(1.0), kw("b"), num(2.0)]));
    }

    #[test]
    fn table_with_commas() {
        assert_form("{:a 1, :b 2}", tbl_f(vec![kw("a"), num(1.0), kw("b"), num(2.0)]));
    }

    #[test]
    fn table_shorthand_binding() {
        // {: name} — shorthand for {:name name}
        assert_form("{: x}", tbl_f(vec![sym(":"), sym("x")]));
    }

    // ── Reader macros ────────────────────────────────────────────────────────

    #[test]
    fn quoted_symbol() {
        assert_form("'x", quote_f(sym("x")));
    }

    #[test]
    fn quoted_list() {
        assert_form("'(1 2 3)", quote_f(list_f(vec![num(1.0), num(2.0), num(3.0)])));
    }

    #[test]
    fn quasiquoted_form() {
        assert_form("`(+ x y)", quasi_f(list_f(vec![sym("+"), sym("x"), sym("y")])));
    }

    #[test]
    fn quasiquote_with_unquote() {
        assert_form(
            "`(+ ,x ,y)",
            quasi_f(list_f(vec![
                sym("+"),
                unquote_f(sym("x")),
                unquote_f(sym("y")),
            ])),
        );
    }

    #[test]
    fn quasiquote_with_splice() {
        assert_form(
            "`(list ,@xs)",
            quasi_f(list_f(vec![sym("list"), splice_f(sym("xs"))])),
        );
    }

    #[test]
    fn hash_fn() {
        assert_form("#(+ % 1)", hash_fn_f(list_f(vec![sym("+"), sym("%"), num(1.0)])));
    }

    // ── Real Fennel patterns ─────────────────────────────────────────────────

    #[test]
    fn fn_definition() {
        assert_form(
            "(fn add [a b] (+ a b))",
            list_f(vec![
                sym("fn"), sym("add"),
                seq_f(vec![sym("a"), sym("b")]),
                list_f(vec![sym("+"), sym("a"), sym("b")]),
            ]),
        );
    }

    #[test]
    fn let_binding() {
        assert_form(
            "(let [x 1 y 2] (+ x y))",
            list_f(vec![
                sym("let"),
                seq_f(vec![sym("x"), num(1.0), sym("y"), num(2.0)]),
                list_f(vec![sym("+"), sym("x"), sym("y")]),
            ]),
        );
    }

    #[test]
    fn local_binding() {
        assert_form(
            "(local name value)",
            list_f(vec![sym("local"), sym("name"), sym("value")]),
        );
    }

    #[test]
    fn destructuring_sequence() {
        assert_form(
            "(let [[a b] (values 1 2)] a)",
            list_f(vec![
                sym("let"),
                seq_f(vec![
                    seq_f(vec![sym("a"), sym("b")]),
                    list_f(vec![sym("values"), num(1.0), num(2.0)]),
                ]),
                sym("a"),
            ]),
        );
    }

    #[test]
    fn icollect_with_until() {
        // Pattern from fennel-ls: (icollect [ok ast parser &until (not ok)] ast)
        assert_form(
            "(icollect [ok ast parser &until (not ok)] ast)",
            list_f(vec![
                sym("icollect"),
                seq_f(vec![
                    sym("ok"), sym("ast"), sym("parser"),
                    sym("&until"),
                    list_f(vec![sym("not"), sym("ok")]),
                ]),
                sym("ast"),
            ]),
        );
    }

    #[test]
    fn match_pattern() {
        assert_form(
            "(match x :a 1 :b 2)",
            list_f(vec![sym("match"), sym("x"), kw("a"), num(1.0), kw("b"), num(2.0)]),
        );
    }

    #[test]
    fn each_loop() {
        assert_form(
            "(each [k v (pairs t)] body)",
            list_f(vec![
                sym("each"),
                seq_f(vec![sym("k"), sym("v"), list_f(vec![sym("pairs"), sym("t")])]),
                sym("body"),
            ]),
        );
    }

    #[test]
    fn for_loop() {
        assert_form(
            "(for [i 1 10] body)",
            list_f(vec![
                sym("for"),
                seq_f(vec![sym("i"), num(1.0), num(10.0)]),
                sym("body"),
            ]),
        );
    }

    // ── Multiple top-level forms ─────────────────────────────────────────────

    #[test]
    fn multiple_forms() {
        let (forms, errors) = parse("(+ 1 2) (+ 3 4)");
        assert!(errors.is_empty());
        assert_eq!(forms.len(), 2);
        assert!(forms_match(&forms[0].node, &list_f(vec![sym("+"), num(1.0), num(2.0)])));
        assert!(forms_match(&forms[1].node, &list_f(vec![sym("+"), num(3.0), num(4.0)])));
    }

    // ── Span tracking ────────────────────────────────────────────────────────

    #[test]
    fn list_span_covers_parens() {
        let (forms, _) = parse("(+ 1 2)");
        let span = &forms[0].span;
        assert_eq!(span.start, 0);
        assert_eq!(span.end, 7);
    }

    #[test]
    fn symbol_span() {
        let (forms, _) = parse("hello");
        let span = &forms[0].span;
        assert_eq!((span.start, span.end), (0, 5));
        assert_eq!((span.line, span.col), (0, 0));
    }

    #[test]
    fn inner_symbol_span() {
        let (forms, _) = parse("(fn hello [])");
        if let Form::List(items) = &forms[0].node {
            let name_span = &items[1].span;
            assert_eq!(name_span.start, 4);
            assert_eq!(name_span.end, 9);
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn multiline_span() {
        let (forms, _) = parse("(+\n  1\n  2)");
        let span = &forms[0].span;
        assert_eq!(span.line, 0);
        assert_eq!(span.end_line, 2);
    }

    // ── Error recovery ───────────────────────────────────────────────────────

    #[test]
    fn unclosed_list_error() {
        let (forms, errors) = parse("(+ 1 2");
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("unclosed"));
        // Still returns the partial form
        assert_eq!(forms.len(), 1);
    }

    #[test]
    fn unexpected_closing_delimiter() {
        let (forms, errors) = parse("(+ 1 2))");
        assert!(!errors.is_empty());
        assert_eq!(forms.len(), 1); // The valid (+ 1 2) form is still returned
    }

    #[test]
    fn nested_unclosed() {
        let (_, errors) = parse("(let [x 1");
        assert!(!errors.is_empty());
    }

    #[test]
    fn recovers_after_error() {
        // Even after an unclosed form, following forms are parsed
        let (forms, _) = parse("(+ 1 2) (+ 3 4)");
        assert_eq!(forms.len(), 2);
    }

    // ── Corpus-driven tests (from tree-sitter-fennel test suite) ─────────────

    // hashfn applied to non-list forms
    #[test]
    fn hashfn_on_atoms() {
        assert_form("#nil",  hash_fn_f(Form::Nil));
        assert_form("#true", hash_fn_f(Form::Bool(true)));
        assert_form("#$",    hash_fn_f(sym("$")));
        assert_form("#$3",   hash_fn_f(sym("$3")));
    }

    #[test]
    fn hashfn_on_sequence() {
        assert_form(
            "#[$1 $2 $3]",
            hash_fn_f(seq_f(vec![sym("$1"), sym("$2"), sym("$3")])),
        );
    }

    // method call: multi_symbol_method in call position (forms.txt / statements.txt)
    #[test]
    fn method_call_in_head() {
        // (foo.bar.baz:method :arg) — head is a multisym with colon suffix
        assert_form(
            "(foo.bar.baz:method :arg)",
            list_f(vec![sym("foo.bar.baz:method"), kw("arg")]),
        );
    }

    // (foo.bar.baz :method :arg) — two-arg form, head is a plain multisym
    #[test]
    fn multisym_in_head() {
        assert_form(
            "(foo.bar.baz :method :arg)",
            list_f(vec![sym("foo.bar.baz"), kw("method"), kw("arg")]),
        );
    }

    // quote / unquote as named forms (not reader macros)
    #[test]
    fn quote_form() {
        assert_form("(quote expr)", list_f(vec![sym("quote"), sym("expr")]));
    }

    #[test]
    fn unquote_form() {
        assert_form("(unquote expr)", list_f(vec![sym("unquote"), sym("expr")]));
    }

    // fn with varargs in params
    #[test]
    fn fn_with_varargs() {
        assert_form(
            "(fn f [a b ...] b)",
            list_f(vec![
                sym("fn"), sym("f"),
                seq_f(vec![sym("a"), sym("b"), Form::Varargs]),
                sym("b"),
            ]),
        );
    }

    // lambda with rest binding [a & rest]
    #[test]
    fn lambda_with_rest_binding() {
        assert_form(
            "(lambda f [a & rest] rest)",
            list_f(vec![
                sym("lambda"), sym("f"),
                seq_f(vec![sym("a"), sym("&"), sym("rest")]),
                sym("rest"),
            ]),
        );
    }

    // each with &until option
    #[test]
    fn each_with_until() {
        assert_form(
            "(each [key value (pairs t) &until (= key :stop)] body)",
            list_f(vec![
                sym("each"),
                seq_f(vec![
                    sym("key"), sym("value"),
                    list_f(vec![sym("pairs"), sym("t")]),
                    sym("&until"),
                    list_f(vec![sym("="), sym("key"), kw("stop")]),
                ]),
                sym("body"),
            ]),
        );
    }

    // accumulate form
    #[test]
    fn accumulate_form() {
        assert_form(
            "(accumulate [sum 0 k v (pairs t)] (+ sum v))",
            list_f(vec![
                sym("accumulate"),
                seq_f(vec![
                    sym("sum"), num(0.0),
                    sym("k"), sym("v"),
                    list_f(vec![sym("pairs"), sym("t")]),
                ]),
                list_f(vec![sym("+"), sym("sum"), sym("v")]),
            ]),
        );
    }

    // icollect with &into
    #[test]
    fn icollect_with_into() {
        assert_form(
            "(icollect [v (ipairs t) &into out] v)",
            list_f(vec![
                sym("icollect"),
                seq_f(vec![
                    sym("v"),
                    list_f(vec![sym("ipairs"), sym("t")]),
                    sym("&into"), sym("out"),
                ]),
                sym("v"),
            ]),
        );
    }

    // case with where guard
    #[test]
    fn case_with_where_guard() {
        assert_form(
            "(case x (where a (= a 1)) :yes _ :no)",
            list_f(vec![
                sym("case"), sym("x"),
                list_f(vec![sym("where"), sym("a"), list_f(vec![sym("="), sym("a"), num(1.0)])]),
                kw("yes"),
                sym("_"), kw("no"),
            ]),
        );
    }

    // case with (where (or pat1 pat2)) — or-pattern
    #[test]
    fn case_where_or_pattern() {
        assert_form(
            "(case x (where (or :a :b)) :yes)",
            list_f(vec![
                sym("case"), sym("x"),
                list_f(vec![
                    sym("where"),
                    list_f(vec![sym("or"), kw("a"), kw("b")]),
                ]),
                kw("yes"),
            ]),
        );
    }

    // import-macros form
    #[test]
    fn import_macros_form() {
        assert_form(
            "(import-macros {: some} :mylib)",
            list_f(vec![
                sym("import-macros"),
                tbl_f(vec![sym(":"), sym("some")]),
                kw("mylib"),
            ]),
        );
    }

    // complex table literal with various key types (from edge-cases corpus)
    #[test]
    fn table_complex_keys() {
        // {: value (+ 1 2) #nil "key" value}
        assert_form(
            r#"{"key" value : name}"#,
            tbl_f(vec![
                str_("key"), sym("value"),
                sym(":"), sym("name"),
            ]),
        );
    }

    // var binding with sequence destructuring
    #[test]
    fn var_with_sequence_binding() {
        assert_form(
            "(var (a b c) 42)",
            list_f(vec![
                sym("var"),
                list_f(vec![sym("a"), sym("b"), sym("c")]),
                num(42.0),
            ]),
        );
    }

    // global binding with rest
    #[test]
    fn global_with_rest_binding() {
        assert_form(
            "(global [a b & cs] 42)",
            list_f(vec![
                sym("global"),
                seq_f(vec![sym("a"), sym("b"), sym("&"), sym("cs")]),
                num(42.0),
            ]),
        );
    }

    // local with table destructuring including &as
    #[test]
    fn local_table_binding_with_as() {
        assert_form(
            "(local {:A a &as cs} 42)",
            list_f(vec![
                sym("local"),
                tbl_f(vec![kw("A"), sym("a"), sym("&as"), sym("cs")]),
                num(42.0),
            ]),
        );
    }

    // comments inside a list are ignored (parser sees through them)
    #[test]
    fn comments_inside_list() {
        assert_form(
            "(print ; a comment\n foo)",
            list_f(vec![sym("print"), sym("foo")]),
        );
    }

    // shebang line at top of program
    #[test]
    fn shebang_in_program() {
        let (forms, errors) = parse("#!/usr/bin/env fennel\n(print \"hi\")");
        assert!(errors.is_empty());
        assert_eq!(forms.len(), 1);
        assert!(forms_match(&forms[0].node, &list_f(vec![sym("print"), str_("hi")])));
    }

    // ── Missing form coverage ────────────────────────────────────────────────

    #[test]
    fn do_block() {
        assert_form(
            "(do (local x 1) x)",
            list_f(vec![
                sym("do"),
                list_f(vec![sym("local"), sym("x"), num(1.0)]),
                sym("x"),
            ]),
        );
    }

    #[test]
    fn when_form() {
        assert_form(
            "(when cond body)",
            list_f(vec![sym("when"), sym("cond"), sym("body")]),
        );
    }

    #[test]
    fn unless_form() {
        assert_form(
            "(unless cond body)",
            list_f(vec![sym("unless"), sym("cond"), sym("body")]),
        );
    }

    #[test]
    fn while_loop() {
        assert_form(
            "(while running (step))",
            list_f(vec![sym("while"), sym("running"), list_f(vec![sym("step")])]),
        );
    }

    #[test]
    fn set_form() {
        assert_form(
            "(set x 42)",
            list_f(vec![sym("set"), sym("x"), num(42.0)]),
        );
    }

    #[test]
    fn with_open_form() {
        assert_form(
            "(with-open [f (io.open :file)] f)",
            list_f(vec![
                sym("with-open"),
                seq_f(vec![sym("f"), list_f(vec![sym("io.open"), kw("file")])]),
                sym("f"),
            ]),
        );
    }

    #[test]
    fn collect_form() {
        assert_form(
            "(collect [k v (pairs t)] k v)",
            list_f(vec![
                sym("collect"),
                seq_f(vec![sym("k"), sym("v"), list_f(vec![sym("pairs"), sym("t")])]),
                sym("k"),
                sym("v"),
            ]),
        );
    }

    #[test]
    fn fcollect_form() {
        assert_form(
            "(fcollect [i 1 10] i)",
            list_f(vec![
                sym("fcollect"),
                seq_f(vec![sym("i"), num(1.0), num(10.0)]),
                sym("i"),
            ]),
        );
    }

    #[test]
    fn faccumulate_form() {
        assert_form(
            "(faccumulate [acc 0 i 1 10] (+ acc i))",
            list_f(vec![
                sym("faccumulate"),
                seq_f(vec![sym("acc"), num(0.0), sym("i"), num(1.0), num(10.0)]),
                list_f(vec![sym("+"), sym("acc"), sym("i")]),
            ]),
        );
    }

    #[test]
    fn case_try_form() {
        assert_form(
            "(case-try (may-fail) x (+ x 1) catch e (tostring e))",
            list_f(vec![
                sym("case-try"),
                list_f(vec![sym("may-fail")]),
                sym("x"),
                list_f(vec![sym("+"), sym("x"), num(1.0)]),
                sym("catch"),
                sym("e"),
                list_f(vec![sym("tostring"), sym("e")]),
            ]),
        );
    }

    #[test]
    fn double_quote() {
        assert_form("''x", quote_f(quote_f(sym("x"))));
    }

    #[test]
    fn anonymous_fn() {
        // (fn [] nil) — no name, empty params
        assert_form(
            "(fn [] nil)",
            list_f(vec![sym("fn"), seq_f(vec![]), Form::Nil]),
        );
    }

    #[test]
    fn multivalue_destructuring() {
        // (local (a b) (values 1 2)) — list pattern on LHS
        assert_form(
            "(local (a b) (values 1 2))",
            list_f(vec![
                sym("local"),
                list_f(vec![sym("a"), sym("b")]),
                list_f(vec![sym("values"), num(1.0), num(2.0)]),
            ]),
        );
    }

    #[test]
    fn hashfn_applied_to_quote() {
        // #'x — HashFn wrapping a quoted symbol; parse_form is recursive so this
        // chains correctly without special-casing
        assert_form("#'x", hash_fn_f(quote_f(sym("x"))));
    }

    #[test]
    fn malformed_fn_no_params_no_crash() {
        // (fn) is syntactically valid (the parser doesn't enforce fn arity) —
        // it produces a list with a single `fn` symbol and no parse errors.
        // The important thing is it doesn't panic.
        let src = "(fn)";
        let (roots, errors) = Parser::parse(src);
        assert!(errors.is_empty(), "no parse errors for bare (fn)");
        assert_eq!(roots.len(), 1);
        assert!(matches!(&roots[0].node, Form::List(_)));
    }
}
