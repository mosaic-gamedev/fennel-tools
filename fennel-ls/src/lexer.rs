/// Fennel lexer — produces a flat list of SpannedTokens from source text.

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    /// Byte offset of the first byte of this token.
    pub start: u32,
    /// Byte offset past the last byte of this token.
    pub end: u32,
    /// 0-based line of `start`.
    pub line: u32,
    /// 0-based byte column of `start`.
    pub col: u32,
    /// 0-based line of `end`.
    pub end_line: u32,
    /// 0-based byte column of `end`.
    pub end_col: u32,
}

impl Span {
    pub fn merge(a: &Span, b: &Span) -> Span {
        Span {
            start: a.start.min(b.start),
            end: a.end.max(b.end),
            line: if a.start <= b.start { a.line } else { b.line },
            col: if a.start <= b.start { a.col } else { b.col },
            end_line: if a.end >= b.end { a.end_line } else { b.end_line },
            end_col: if a.end >= b.end { a.end_col } else { b.end_col },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Delimiters
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    // Reader macros
    Quote,
    Quasiquote,
    Unquote,
    UnquoteSplice,
    HashFn,
    // Literals
    Str(String),
    Keyword(String),
    Number(f64),
    Bool(bool),
    Nil,
    Varargs,
    // Symbols / identifiers (may be multisym like a.b.c or a:method)
    Symbol(String),
    // Comments (only emitted by tokenize_with_comments; text includes the leading `;`)
    Comment(String),
}

#[derive(Debug, Clone)]
pub struct SpannedToken {
    pub token: Token,
    pub span: Span,
}

pub struct Lexer<'src> {
    src: &'src [u8],
    pos: usize,
    line: u32,
    col: u32,
    with_comments: bool,
}

impl<'src> Lexer<'src> {
    pub fn new(src: &'src str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            line: 0,
            col: 0,
            with_comments: false,
        }
    }

    pub fn tokenize(src: &'src str) -> Vec<SpannedToken> {
        let mut lex = Self::new(src);
        let mut out = Vec::new();
        while let Some(tok) = lex.next_token() {
            out.push(tok);
        }
        out
    }

    /// Like `tokenize` but also emits `Token::Comment` for line comments.
    pub fn tokenize_with_comments(src: &'src str) -> Vec<SpannedToken> {
        let mut lex = Self::new(src);
        lex.with_comments = true;
        let mut out = Vec::new();
        while let Some(tok) = lex.next_token() {
            out.push(tok);
        }
        out
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.src.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let c = *self.src.get(self.pos)?;
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
            self.col = 0;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n')) {
            self.advance();
        }
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b';') => {
                    while self.peek().map_or(false, |c| c != b'\n') {
                        self.advance();
                    }
                }
                Some(b'#') if self.pos == 0 && self.peek2() == Some(b'!') => {
                    while self.peek().map_or(false, |c| c != b'\n') {
                        self.advance();
                    }
                }
                _ => break,
            }
        }
    }

    fn span_from(&self, start: usize, start_line: u32, start_col: u32) -> Span {
        Span {
            start: start as u32,
            end: self.pos as u32,
            line: start_line,
            col: start_col,
            end_line: self.line,
            end_col: self.col,
        }
    }

    fn next_token(&mut self) -> Option<SpannedToken> {
        loop {
            if self.with_comments {
                self.skip_whitespace();
                let is_comment = self.peek() == Some(b';');
                let is_shebang = self.pos == 0
                    && self.peek() == Some(b'#')
                    && self.peek2() == Some(b'!');
                if is_comment || is_shebang {
                    let start = self.pos;
                    let sl = self.line;
                    let sc = self.col;
                    while self.peek().map_or(false, |c| c != b'\n') {
                        self.advance();
                    }
                    let text = String::from_utf8_lossy(&self.src[start..self.pos])
                        .into_owned();
                    return Some(SpannedToken {
                        token: Token::Comment(text),
                        span: self.span_from(start, sl, sc),
                    });
                }
            } else {
                self.skip_whitespace_and_comments();
            }

            let start = self.pos;
            let sl = self.line;
            let sc = self.col;

            let ch = self.advance()?;

            let tok = match ch {
                b'(' => Token::LParen,
                b')' => Token::RParen,
                b'{' => Token::LBrace,
                b'}' => Token::RBrace,
                b']' => Token::RBracket,
                b'\'' => Token::Quote,
                b'`' => Token::Quasiquote,
                b',' => {
                    // Comma followed by '@' is always unquote-splice.
                    if self.peek() == Some(b'@') {
                        self.advance();
                        Token::UnquoteSplice
                    } else if self.peek().map_or(true, |c| is_ws(c) || is_delim(c) || c == b',') {
                        // Comma followed by whitespace/delimiter = list separator, skip it.
                        continue;
                    } else {
                        Token::Unquote
                    }
                }
                b'[' => Token::LBracket,
                b'#' => Token::HashFn,
                b'"' => self.scan_string(),
                b':' => {
                    match self.peek() {
                        Some(c) if !is_ws(c) && !is_delim(c) => self.scan_keyword(),
                        _ => Token::Symbol(":".into()),
                    }
                }
                _ => {
                    // Generic atom — read until whitespace or delimiter
                    let mut bytes = vec![ch];
                    while let Some(c) = self.peek() {
                        if is_ws(c) || is_delim(c) {
                            break;
                        }
                        bytes.push(c);
                        self.advance();
                    }
                    let s = String::from_utf8_lossy(&bytes).into_owned();
                    classify_atom(s)
                }
            };

            return Some(SpannedToken {
                token: tok,
                span: self.span_from(start, sl, sc),
            });
        }
    }

    fn scan_string(&mut self) -> Token {
        let mut s = String::new();
        loop {
            match self.advance() {
                None | Some(b'"') => break,
                Some(b'\\') => {
                    match self.peek() {
                        None => break,
                        Some(b'a') => { self.advance(); s.push('\x07'); }
                        Some(b'b') => { self.advance(); s.push('\x08'); }
                        Some(b'f') => { self.advance(); s.push('\x0C'); }
                        Some(b'n') => { self.advance(); s.push('\n'); }
                        Some(b'r') => { self.advance(); s.push('\r'); }
                        Some(b't') => { self.advance(); s.push('\t'); }
                        Some(b'v') => { self.advance(); s.push('\x0B'); }
                        Some(b'\\') => { self.advance(); s.push('\\'); }
                        Some(b'"') => { self.advance(); s.push('"'); }
                        Some(b'\'') => { self.advance(); s.push('\''); }
                        Some(b'\n') | Some(b'\r') => { self.advance(); } // line continuation
                        Some(b'z') => {
                            // \z skips following whitespace (Lua 5.2+)
                            self.advance();
                            while self.peek().map_or(false, |c| matches!(c, b' ' | b'\t' | b'\r' | b'\n')) {
                                self.advance();
                            }
                        }
                        Some(b'x') => {
                            // \xHH — two hex digits
                            self.advance();
                            let hi = self.advance().unwrap_or(b'0');
                            let lo = self.advance().unwrap_or(b'0');
                            let n = hex_digit(hi) * 16 + hex_digit(lo);
                            push_byte(&mut s, n);
                        }
                        Some(b'u') => {
                            // \u{HHHHHH} — Unicode code point
                            self.advance();
                            if self.peek() == Some(b'{') {
                                self.advance();
                                let mut code: u32 = 0;
                                while let Some(c) = self.peek() {
                                    if c == b'}' { self.advance(); break; }
                                    code = code * 16 + hex_digit(c) as u32;
                                    self.advance();
                                }
                                if let Some(ch) = char::from_u32(code) {
                                    s.push(ch);
                                }
                            }
                        }
                        Some(d) if d.is_ascii_digit() => {
                            // \NNN — up to 3 decimal digits (0–255)
                            let d1 = self.advance().unwrap();
                            let mut n = (d1 - b'0') as u32;
                            if self.peek().map_or(false, |c| c.is_ascii_digit()) {
                                let d2 = self.advance().unwrap();
                                n = n * 10 + (d2 - b'0') as u32;
                                if self.peek().map_or(false, |c| c.is_ascii_digit()) {
                                    let d3 = self.advance().unwrap();
                                    n = n * 10 + (d3 - b'0') as u32;
                                }
                            }
                            push_byte(&mut s, n.min(255) as u8);
                        }
                        Some(c) => {
                            self.advance();
                            s.push('\\');
                            s.push(c as char);
                        }
                    }
                }
                Some(c) => {
                    if c < 0x80 {
                        s.push(c as char);
                    } else {
                        // Multi-byte UTF-8: read the required continuation bytes
                        let count = if c >= 0xF0 { 3 } else if c >= 0xE0 { 2 } else { 1 };
                        let mut bytes = vec![c];
                        for _ in 0..count {
                            match self.advance() {
                                Some(b) => bytes.push(b),
                                None => break,
                            }
                        }
                        if let Ok(st) = std::str::from_utf8(&bytes) {
                            s.push_str(st);
                        }
                    }
                }
            }
        }
        Token::Str(s)
    }

    fn scan_keyword(&mut self) -> Token {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if is_ws(c) || is_delim(c) {
                break;
            }
            s.push(c as char);
            self.advance();
        }
        Token::Keyword(s)
    }
}

fn classify_atom(s: String) -> Token {
    match s.as_str() {
        "nil" => Token::Nil,
        "true" => Token::Bool(true),
        "false" => Token::Bool(false),
        "..." => Token::Varargs,
        _ => {
            // Hex number: integer or float, with optional leading sign (-0xff, -0x1.8p+1)
            let (sign, unsigned) = if s.starts_with('-') {
                (-1.0f64, &s[1..])
            } else if s.starts_with('+') {
                (1.0f64, &s[1..])
            } else {
                (1.0f64, s.as_str())
            };
            if let Some(hex) = unsigned.strip_prefix("0x").or_else(|| unsigned.strip_prefix("0X")) {
                let hex_clean: String = hex.chars().filter(|&c| c != '_').collect();
                // Try integer first
                if let Ok(n) = u64::from_str_radix(&hex_clean, 16) {
                    return Token::Number(sign * n as f64);
                }
                // Try hex float (e.g. 1.8p+1)
                if let Some(n) = parse_hex_float(&hex_clean) {
                    return Token::Number(sign * n);
                }
            }
            // Decimal / float — strip numeric underscores (e.g. 150_000)
            let clean: String = s.chars().filter(|&c| c != '_').collect();
            // .nan / .inf special literals (f64::from_str doesn't accept these)
            match clean.as_str() {
                ".nan" | "+.nan" | "-.nan" => return Token::Number(f64::NAN),
                ".inf" | "+.inf"           => return Token::Number(f64::INFINITY),
                "-.inf"                    => return Token::Number(f64::NEG_INFINITY),
                // Fennel (via Lua tonumber) rejects bare nan/inf variants as numbers;
                // Rust's f64::from_str accepts all of them, so we must exclude them.
                // Fennel's infinity literal is `.inf` (with the leading dot), not bare `inf`.
                "nan"  | "-nan"  | "+nan"
                | "inf" | "-inf" | "+inf"
                | "infinity" | "-infinity" | "+infinity"
                => return Token::Symbol(s),
                _ => {}
            }
            if let Ok(n) = clean.parse::<f64>() {
                return Token::Number(n);
            }
            Token::Symbol(s)
        }
    }
}

/// Parse a Lua-style hex float string (prefix `0x`/`0X` already stripped).
/// Handles `1.8p+1`, `1p0`, `.fp-2`, etc.
fn parse_hex_float(s: &str) -> Option<f64> {
    let (mantissa, exp_str) = match s.find(['p', 'P']) {
        Some(p) => (&s[..p], &s[p + 1..]),
        None    => (s, "0"),
    };
    let (int_s, frac_s) = match mantissa.find('.') {
        Some(d) => (&mantissa[..d], Some(&mantissa[d + 1..])),
        None    => (mantissa, None),
    };
    // Require at least one hex digit in the mantissa
    if int_s.is_empty() && frac_s.map_or(true, |f| f.is_empty()) {
        return None;
    }
    let int_part = if int_s.is_empty() {
        0u64
    } else {
        u64::from_str_radix(int_s, 16).ok()?
    };
    let frac_part: f64 = match frac_s {
        Some(f) if !f.is_empty() => {
            u64::from_str_radix(f, 16).ok()? as f64 / 16f64.powi(f.len() as i32)
        }
        _ => 0.0,
    };
    let exp: i32 = exp_str.parse().ok()?;
    Some((int_part as f64 + frac_part) * 2f64.powi(exp))
}

fn hex_digit(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

/// Push a byte value into a String. For bytes 0–127 this is ASCII; for 128–255
/// we store the Latin-1 code point as its Unicode equivalent (U+0080–U+00FF).
fn push_byte(s: &mut String, byte: u8) {
    s.push(char::from_u32(byte as u32).unwrap_or('\u{FFFD}'));
}

pub fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n' | b',')
}

pub fn is_delim(c: u8) -> bool {
    matches!(c, b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'"' | b';')
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Token> {
        Lexer::tokenize(src).into_iter().map(|t| t.token).collect()
    }

    fn spans(src: &str) -> Vec<Span> {
        Lexer::tokenize(src).into_iter().map(|t| t.span).collect()
    }

    // ── Delimiters ───────────────────────────────────────────────────────────

    #[test]
    fn delimiters() {
        assert_eq!(toks("()"), vec![Token::LParen, Token::RParen]);
        assert_eq!(toks("[]"), vec![Token::LBracket, Token::RBracket]);
        assert_eq!(toks("{}"), vec![Token::LBrace, Token::RBrace]);
    }

    #[test]
    fn nested_delimiters() {
        assert_eq!(
            toks("([{}])"),
            vec![
                Token::LParen,
                Token::LBracket,
                Token::LBrace,
                Token::RBrace,
                Token::RBracket,
                Token::RParen,
            ]
        );
    }

    // ── Atoms ────────────────────────────────────────────────────────────────

    #[test]
    fn nil_true_false() {
        assert_eq!(toks("nil"), vec![Token::Nil]);
        assert_eq!(toks("true"), vec![Token::Bool(true)]);
        assert_eq!(toks("false"), vec![Token::Bool(false)]);
    }

    #[test]
    fn varargs() {
        assert_eq!(toks("..."), vec![Token::Varargs]);
    }

    // ── Numbers ──────────────────────────────────────────────────────────────

    #[test]
    fn integer() {
        assert_eq!(toks("42"), vec![Token::Number(42.0)]);
        assert_eq!(toks("0"), vec![Token::Number(0.0)]);
    }

    #[test]
    fn float() {
        assert_eq!(toks("3.14"), vec![Token::Number(3.14)]);
        assert_eq!(toks("1.5e-3"), vec![Token::Number(1.5e-3)]);
        assert_eq!(toks("1e10"), vec![Token::Number(1e10)]);
    }

    #[test]
    fn hex_number() {
        assert_eq!(toks("0xff"), vec![Token::Number(255.0)]);
        assert_eq!(toks("0xFF"), vec![Token::Number(255.0)]);
        assert_eq!(toks("0x10"), vec![Token::Number(16.0)]);
        assert_eq!(toks("0xDEAD"), vec![Token::Number(0xDEAD as f64)]);
    }

    #[test]
    fn scientific_notation() {
        // From Fennel's test-spicy-numbers
        assert_eq!(toks("1.41791343654238e+14"), vec![Token::Number(1.41791343654238e14)]);
        assert_eq!(toks("1.23456789e-13"), vec![Token::Number(1.23456789e-13)]);
    }

    #[test]
    fn number_with_underscores() {
        // Fennel allows _ as a numeric separator
        assert_eq!(toks("150_000"), vec![Token::Number(150_000.0)]);
        assert_eq!(toks("1_000_000"), vec![Token::Number(1_000_000.0)]);
    }

    #[test]
    fn negative_number() {
        // Negation is a function call in Fennel; `-1` is the number
        assert_eq!(toks("-1"), vec![Token::Number(-1.0)]);
        assert_eq!(toks("-3.14"), vec![Token::Number(-3.14)]);
    }

    // ── Symbols ──────────────────────────────────────────────────────────────

    #[test]
    fn basic_symbol() {
        assert_eq!(toks("hello"), vec![Token::Symbol("hello".into())]);
        assert_eq!(toks("my-var"), vec![Token::Symbol("my-var".into())]);
        assert_eq!(toks("_ignored"), vec![Token::Symbol("_ignored".into())]);
    }

    #[test]
    fn operator_symbols() {
        assert_eq!(toks("+"), vec![Token::Symbol("+".into())]);
        assert_eq!(toks("-"), vec![Token::Symbol("-".into())]);
        assert_eq!(toks("->"), vec![Token::Symbol("->".into())]);
        assert_eq!(toks("->>"), vec![Token::Symbol("->>".into())]);
        assert_eq!(toks("not="), vec![Token::Symbol("not=".into())]);
        assert_eq!(toks(".."), vec![Token::Symbol("..".into())]);
    }

    #[test]
    fn multisym_dot() {
        assert_eq!(toks("a.b"), vec![Token::Symbol("a.b".into())]);
        assert_eq!(toks("a.b.c"), vec![Token::Symbol("a.b.c".into())]);
        assert_eq!(toks("string.format"), vec![Token::Symbol("string.format".into())]);
        assert_eq!(toks("math.pi"), vec![Token::Symbol("math.pi".into())]);
    }

    #[test]
    fn multisym_colon() {
        assert_eq!(toks("obj:method"), vec![Token::Symbol("obj:method".into())]);
        assert_eq!(toks("io:read"), vec![Token::Symbol("io:read".into())]);
    }

    #[test]
    fn special_prefix_symbols() {
        // ? prefix for optional params
        assert_eq!(toks("?x"), vec![Token::Symbol("?x".into())]);
        // & modifiers
        assert_eq!(toks("&until"), vec![Token::Symbol("&until".into())]);
        assert_eq!(toks("&into"), vec![Token::Symbol("&into".into())]);
        assert_eq!(toks("&as"), vec![Token::Symbol("&as".into())]);
    }

    // ── Keywords ─────────────────────────────────────────────────────────────

    #[test]
    fn keywords() {
        assert_eq!(toks(":hello"), vec![Token::Keyword("hello".into())]);
        assert_eq!(toks(":uri"), vec![Token::Keyword("uri".into())]);
        assert_eq!(toks(":textDocument"), vec![Token::Keyword("textDocument".into())]);
    }

    #[test]
    fn standalone_colon_is_symbol() {
        // Bare `:` (followed by whitespace/delimiter) is a symbol
        assert_eq!(toks(":"), vec![Token::Symbol(":".into())]);
        assert_eq!(toks(": "), vec![Token::Symbol(":".into())]);
    }

    // ── Reader macros ────────────────────────────────────────────────────────

    #[test]
    fn quote() {
        assert_eq!(toks("'x"), vec![Token::Quote, Token::Symbol("x".into())]);
        assert_eq!(toks("'(1 2)"), vec![Token::Quote, Token::LParen, Token::Number(1.0), Token::Number(2.0), Token::RParen]);
    }

    #[test]
    fn quasiquote() {
        assert_eq!(toks("`x"), vec![Token::Quasiquote, Token::Symbol("x".into())]);
    }

    #[test]
    fn unquote_splice() {
        assert_eq!(toks(",@xs"), vec![Token::UnquoteSplice, Token::Symbol("xs".into())]);
    }

    #[test]
    fn unquote() {
        // `,x` (no space after comma) → Unquote
        assert_eq!(toks(",x"), vec![Token::Unquote, Token::Symbol("x".into())]);
    }

    #[test]
    fn comma_as_separator() {
        // Comma followed by whitespace or delimiter is a list separator (invisible)
        assert_eq!(toks("1, 2"), vec![Token::Number(1.0), Token::Number(2.0)]);
        assert_eq!(toks("[1, 2, 3]"), vec![
            Token::LBracket,
            Token::Number(1.0), Token::Number(2.0), Token::Number(3.0),
            Token::RBracket,
        ]);
    }

    #[test]
    fn hash_fn() {
        assert_eq!(toks("#"), vec![Token::HashFn]);
        assert_eq!(toks("#(+ % 1)"), vec![
            Token::HashFn, Token::LParen,
            Token::Symbol("+".into()), Token::Symbol("%".into()), Token::Number(1.0),
            Token::RParen,
        ]);
    }

    // ── Strings ──────────────────────────────────────────────────────────────

    #[test]
    fn string_basic() {
        assert_eq!(toks("\"hello\""), vec![Token::Str("hello".into())]);
        assert_eq!(toks("\"\""), vec![Token::Str("".into())]);
    }

    #[test]
    fn string_standard_escapes() {
        assert_eq!(toks(r#""\n""#), vec![Token::Str("\n".into())]);
        assert_eq!(toks(r#""\t""#), vec![Token::Str("\t".into())]);
        assert_eq!(toks(r#""\r""#), vec![Token::Str("\r".into())]);
        assert_eq!(toks(r#""\\""#), vec![Token::Str("\\".into())]);
        assert_eq!(toks(r#""\"""#), vec![Token::Str("\"".into())]);
    }

    #[test]
    fn string_lua_control_escapes() {
        // \a = bell (0x07), \b = backspace (0x08), \f = form feed (0x0C), \v = vertical tab (0x0B)
        assert_eq!(toks(r#""\a""#), vec![Token::Str("\x07".into())]);
        assert_eq!(toks(r#""\b""#), vec![Token::Str("\x08".into())]);
        assert_eq!(toks(r#""\f""#), vec![Token::Str("\x0C".into())]);
        assert_eq!(toks(r#""\v""#), vec![Token::Str("\x0B".into())]);
    }

    #[test]
    fn string_decimal_escape() {
        // \NNN is a decimal byte value (Lua style, NOT octal)
        assert_eq!(toks(r#""\032""#), vec![Token::Str(" ".into())]);  // 32 = space
        assert_eq!(toks(r#""\65""#),  vec![Token::Str("A".into())]);  // 65 = 'A'
        assert_eq!(toks(r#""\0""#),   vec![Token::Str("\0".into())]);
        assert_eq!(toks(r#""\9""#),   vec![Token::Str("\x09".into())]); // tab
    }

    #[test]
    fn string_hex_escape() {
        // \xHH — two hex digits
        assert_eq!(toks(r#""\x20""#), vec![Token::Str(" ".into())]);
        assert_eq!(toks(r#""\x41""#), vec![Token::Str("A".into())]);
        assert_eq!(toks(r#""\x0a""#), vec![Token::Str("\n".into())]);
    }

    #[test]
    fn string_unicode_escape() {
        // \u{HHHH} — Unicode code point (Lua 5.3+)
        assert_eq!(toks(r#""\u{20}""#),    vec![Token::Str(" ".into())]);
        assert_eq!(toks(r#""\u{41}""#),    vec![Token::Str("A".into())]);
        assert_eq!(toks(r#""\u{a2}""#),    vec![Token::Str("\u{00A2}".into())]); // ¢
        assert_eq!(toks(r#""\u{20ac}""#),  vec![Token::Str("\u{20AC}".into())]); // €
        assert_eq!(toks(r#""\u{24b62}""#), vec![Token::Str("\u{24B62}".into())]); // 𤭢
    }

    #[test]
    fn string_z_escape_skips_whitespace() {
        // \z eats following whitespace (Lua 5.2+)
        assert_eq!(toks("\"foo\\z   bar\""), vec![Token::Str("foobar".into())]);
        assert_eq!(toks("\"foo\\z\n   bar\""), vec![Token::Str("foobar".into())]);
    }

    #[test]
    fn string_line_continuation() {
        // \<newline> joins lines
        assert_eq!(toks("\"foo\\\nbar\""), vec![Token::Str("foobar".into())]);
    }

    #[test]
    fn string_with_embedded_newline() {
        assert_eq!(toks("\"line1\nline2\""), vec![Token::Str("line1\nline2".into())]);
    }

    // ── Whitespace / comments ────────────────────────────────────────────────

    #[test]
    fn whitespace_ignored() {
        assert_eq!(toks("  (  )  "), vec![Token::LParen, Token::RParen]);
        assert_eq!(toks("\t(\n)\r"), vec![Token::LParen, Token::RParen]);
    }

    #[test]
    fn comments_ignored() {
        assert_eq!(toks("; this is a comment\n42"), vec![Token::Number(42.0)]);
        assert_eq!(toks("(+ ; add\n 1 2)"), vec![
            Token::LParen, Token::Symbol("+".into()),
            Token::Number(1.0), Token::Number(2.0),
            Token::RParen,
        ]);
    }

    #[test]
    fn shebang_ignored() {
        assert_eq!(toks("#!/usr/bin/env fennel\n42"), vec![Token::Number(42.0)]);
    }

    // ── Span tracking ────────────────────────────────────────────────────────

    #[test]
    fn span_line_col() {
        let ts = spans("(+ 1\n   2)");
        // '(' at line 0, col 0
        assert_eq!((ts[0].line, ts[0].col), (0, 0));
        // '+' at line 0, col 1
        assert_eq!((ts[1].line, ts[1].col), (0, 1));
        // '1' at line 0, col 3
        assert_eq!((ts[2].line, ts[2].col), (0, 3));
        // '2' at line 1, col 3
        assert_eq!((ts[3].line, ts[3].col), (1, 3));
    }

    #[test]
    fn span_byte_offsets() {
        let ts = spans("(+ 1 2)");
        assert_eq!((ts[0].start, ts[0].end), (0, 1));  // (
        assert_eq!((ts[1].start, ts[1].end), (1, 2));  // +
        assert_eq!((ts[2].start, ts[2].end), (3, 4));  // 1
        assert_eq!((ts[3].start, ts[3].end), (5, 6));  // 2
        assert_eq!((ts[4].start, ts[4].end), (6, 7));  // )
    }

    #[test]
    fn span_multichar_token() {
        let ts = spans("hello");
        assert_eq!((ts[0].start, ts[0].end), (0, 5));
    }

    // ── Multiple tokens in sequence ──────────────────────────────────────────

    #[test]
    fn full_expression() {
        assert_eq!(
            toks("(fn [x y] (+ x y))"),
            vec![
                Token::LParen,
                Token::Symbol("fn".into()),
                Token::LBracket,
                Token::Symbol("x".into()),
                Token::Symbol("y".into()),
                Token::RBracket,
                Token::LParen,
                Token::Symbol("+".into()),
                Token::Symbol("x".into()),
                Token::Symbol("y".into()),
                Token::RParen,
                Token::RParen,
            ]
        );
    }

    #[test]
    fn table_literal() {
        assert_eq!(
            toks("{:a 1 :b 2}"),
            vec![
                Token::LBrace,
                Token::Keyword("a".into()), Token::Number(1.0),
                Token::Keyword("b".into()), Token::Number(2.0),
                Token::RBrace,
            ]
        );
    }

    #[test]
    fn table_with_comma_separators() {
        // Commas between k-v pairs are separators, not tokens
        assert_eq!(
            toks("{:a 1, :b 2}"),
            vec![
                Token::LBrace,
                Token::Keyword("a".into()), Token::Number(1.0),
                Token::Keyword("b".into()), Token::Number(2.0),
                Token::RBrace,
            ]
        );
    }

    #[test]
    fn quasiquote_with_unquote() {
        // `(+ ,x ,y) — inside quasiquote, commas before symbols produce Unquote
        assert_eq!(
            toks("`(+ ,x ,y)"),
            vec![
                Token::Quasiquote,
                Token::LParen,
                Token::Symbol("+".into()),
                Token::Unquote, Token::Symbol("x".into()),
                Token::Unquote, Token::Symbol("y".into()),
                Token::RParen,
            ]
        );
    }

    #[test]
    fn quasiquote_with_unquote_splice() {
        assert_eq!(
            toks("`(concat ,@xs)"),
            vec![
                Token::Quasiquote,
                Token::LParen,
                Token::Symbol("concat".into()),
                Token::UnquoteSplice, Token::Symbol("xs".into()),
                Token::RParen,
            ]
        );
    }

    // ── Corpus-driven edge cases (from tree-sitter-fennel test suite) ─────────

    // symbols: hash allowed in non-first position
    #[test]
    fn symbol_hash_in_body() {
        assert_eq!(toks("test#test#test"), vec![Token::Symbol("test#test#test".into())]);
    }

    // symbols: dollar-sign prefixed (hashfn args $, $1, $2, $...)
    #[test]
    fn dollar_symbols() {
        assert_eq!(toks("$"),   vec![Token::Symbol("$".into())]);
        assert_eq!(toks("$1"),  vec![Token::Symbol("$1".into())]);
        assert_eq!(toks("$2"),  vec![Token::Symbol("$2".into())]);
        assert_eq!(toks("$..."), vec![Token::Symbol("$...".into())]);
    }

    // symbols: arbitrary operator-like names
    #[test]
    fn symbol_operators() {
        assert_eq!(toks("<>"),   vec![Token::Symbol("<>".into())]);
        assert_eq!(toks("!lol"), vec![Token::Symbol("!lol".into())]);
        assert_eq!(toks("a?"),   vec![Token::Symbol("a?".into())]);
    }

    // symbols: & alone and & with suffix (symbol_option in tree-sitter)
    #[test]
    fn symbol_option_forms() {
        assert_eq!(toks("&"),      vec![Token::Symbol("&".into())]);
        assert_eq!(toks("&#hoi"),  vec![Token::Symbol("&#hoi".into())]);
    }

    // symbols: dot and double-dot as standalone
    #[test]
    fn dot_symbols() {
        assert_eq!(toks("."),  vec![Token::Symbol(".".into())]);
        // ".." is already tested via operator_symbols, but confirm here too
        assert_eq!(toks(".."), vec![Token::Symbol("..".into())]);
    }

    // numbers: Rust f64 accepts a leading '+', so +1 parses as a number
    #[test]
    fn positive_sign_number() {
        assert_eq!(toks("+1"),   vec![Token::Number(1.0)]);
        assert_eq!(toks("+3.14"), vec![Token::Number(3.14)]);
    }

    #[test]
    fn special_float_literals() {
        // .nan / .inf are now parsed as Number tokens (matched before f64::parse)
        assert!(matches!(toks(".nan")[0], Token::Number(n) if n.is_nan()));
        assert!(matches!(toks("+.nan")[0], Token::Number(n) if n.is_nan()));
        assert!(matches!(toks("-.nan")[0], Token::Number(n) if n.is_nan()));
        assert_eq!(toks(".inf"),  vec![Token::Number(f64::INFINITY)]);
        assert_eq!(toks("+.inf"), vec![Token::Number(f64::INFINITY)]);
        assert_eq!(toks("-.inf"), vec![Token::Number(f64::NEG_INFINITY)]);
    }

    #[test]
    fn hex_float_literals() {
        // Lua 5.3 hex floats are now parsed correctly
        // 0x1.1 = 1 + 1/16 = 1.0625
        assert_eq!(toks("0x1.1"), vec![Token::Number(1.0625)]);
        // 0x1p1 = 1 * 2^1 = 2.0
        assert_eq!(toks("0x1p1"), vec![Token::Number(2.0)]);
        // 0x1.8p+1 = (1 + 8/16) * 2^1 = 3.0
        assert_eq!(toks("0x1.8p+1"), vec![Token::Number(3.0)]);
        // 0x.fp-2 = (15/16) * 2^-2 = 0.234375
        assert_eq!(toks("0x.fp-2"), vec![Token::Number(0.234375)]);
        // Integer hex still works
        assert_eq!(toks("0x10"), vec![Token::Number(16.0)]);
    }

    // keywords: special characters after colon
    #[test]
    fn keyword_special_chars() {
        assert_eq!(toks(":#"),      vec![Token::Keyword("#".into())]);
        assert_eq!(toks(":*"),      vec![Token::Keyword("*".into())]);
        assert_eq!(toks(":-"),      vec![Token::Keyword("-".into())]);
        assert_eq!(toks(":+"),      vec![Token::Keyword("+".into())]);
        assert_eq!(toks(":9"),      vec![Token::Keyword("9".into())]);
        assert_eq!(toks(":_"),      vec![Token::Keyword("_".into())]);
        assert_eq!(toks(":/"),      vec![Token::Keyword("/".into())]);
        assert_eq!(toks(":<"),      vec![Token::Keyword("<".into())]);
        assert_eq!(toks(":>"),      vec![Token::Keyword(">".into())]);
        assert_eq!(toks(":="),      vec![Token::Keyword("=".into())]);
        assert_eq!(toks(":^"),      vec![Token::Keyword("^".into())]);
    }

    // keywords: normally-reserved words are valid keywords when colon-prefixed
    #[test]
    fn keyword_reserved_words() {
        assert_eq!(toks(":true"),  vec![Token::Keyword("true".into())]);
        assert_eq!(toks(":false"), vec![Token::Keyword("false".into())]);
        assert_eq!(toks(":nil"),   vec![Token::Keyword("nil".into())]);
    }

    // keywords: double-colon (:: — colon as keyword content)
    #[test]
    fn keyword_double_colon() {
        assert_eq!(toks("::"), vec![Token::Keyword(":".into())]);
    }

    // reader macro: hashfn applied to non-list forms
    #[test]
    fn hashfn_on_atoms() {
        assert_eq!(toks("#nil"),   vec![Token::HashFn, Token::Nil]);
        assert_eq!(toks("#true"),  vec![Token::HashFn, Token::Bool(true)]);
        assert_eq!(toks("#$"),     vec![Token::HashFn, Token::Symbol("$".into())]);
        assert_eq!(toks("#$3"),    vec![Token::HashFn, Token::Symbol("$3".into())]);
    }

    // colon-strings inside lists: (:lol) and (:: ) are valid
    #[test]
    fn colon_string_in_list() {
        assert_eq!(toks("(:lol)"), vec![
            Token::LParen, Token::Keyword("lol".into()), Token::RParen,
        ]);
        // (:: ) — colon-as-keyword inside parens
        assert_eq!(toks("(:: )"), vec![
            Token::LParen, Token::Keyword(":".into()), Token::RParen,
        ]);
    }

    // sequence with rest binding syntax
    #[test]
    fn sequence_with_rest() {
        assert_eq!(toks("[a b & cs]"), vec![
            Token::LBracket,
            Token::Symbol("a".into()), Token::Symbol("b".into()),
            Token::Symbol("&".into()), Token::Symbol("cs".into()),
            Token::RBracket,
        ]);
    }

    // ── Number edge cases ────────────────────────────────────────────────────

    #[test]
    fn negative_hex() {
        assert_eq!(toks("-0xff"), vec![Token::Number(-255.0)]);
        assert_eq!(toks("-0x10"), vec![Token::Number(-16.0)]);
    }

    #[test]
    fn hex_with_underscores() {
        // 0xff_00 = 65280
        assert_eq!(toks("0xff_00"), vec![Token::Number(65280.0)]);
    }

    #[test]
    fn float_with_underscores() {
        assert_eq!(toks("1_000.5"), vec![Token::Number(1000.5)]);
        assert_eq!(toks("3_141.592"), vec![Token::Number(3141.592)]);
    }

    #[test]
    fn positive_exponent() {
        assert_eq!(toks("1e+10"), vec![Token::Number(1e10)]);
        assert_eq!(toks("2.5e+3"), vec![Token::Number(2500.0)]);
    }

    // ── Comma edge cases ─────────────────────────────────────────────────────

    #[test]
    fn comma_before_close_bracket() {
        // Trailing comma: treated as separator, ignored
        assert_eq!(toks("[1,]"), vec![Token::LBracket, Token::Number(1.0), Token::RBracket]);
        assert_eq!(toks("(f a,)"), vec![
            Token::LParen, Token::Symbol("f".into()), Token::Symbol("a".into()), Token::RParen,
        ]);
    }

    #[test]
    fn comma_at_eof_no_crash() {
        // Bare comma at EOF: acts as separator, produces no token
        assert_eq!(toks(","), vec![]);
    }

    // ── Symbol / operator disambiguation ─────────────────────────────────────

    #[test]
    fn plus_adjacent_to_digit_is_number() {
        assert_eq!(toks("+1"), vec![Token::Number(1.0)]);
        assert_eq!(toks("+3.14"), vec![Token::Number(3.14)]);
    }

    #[test]
    fn plus_with_space_before_digit_is_symbol_then_number() {
        assert_eq!(toks("+ 1"), vec![Token::Symbol("+".into()), Token::Number(1.0)]);
        assert_eq!(toks("+ 3.14"), vec![Token::Symbol("+".into()), Token::Number(3.14)]);
    }

    // ── Unicode in strings ───────────────────────────────────────────────────

    #[test]
    fn unicode_in_string_roundtrips() {
        // é (U+00E9) is 2 bytes in UTF-8
        assert_eq!(toks("\"héllo\""), vec![Token::Str("héllo".into())]);
        // € (U+20AC) is 3 bytes in UTF-8
        assert_eq!(toks("\"€\""), vec![Token::Str("€".into())]);
        // 𤭢 (U+24B62) is 4 bytes in UTF-8
        assert_eq!(toks("\"𤭢\""), vec![Token::Str("𤭢".into())]);
    }

    #[test]
    fn unicode_escape_and_raw_unicode_in_same_string() {
        // Raw multi-byte UTF-8 and escape sequence producing the same codepoint
        // "café and h\u{e9}llo" — 'é' appears once raw, once escaped
        assert_eq!(
            toks("\"café and h\\u{e9}llo\""),
            vec![Token::Str("café and héllo".into())],
        );
    }

    #[test]
    fn negative_hex_float() {
        // -0x1.8p+1 = -(1 + 8/16) * 2^1 = -3.0
        assert_eq!(toks("-0x1.8p+1"), vec![Token::Number(-3.0)]);
        // -0x10p0 = -16
        assert_eq!(toks("-0x10p0"), vec![Token::Number(-16.0)]);
    }

    #[test]
    fn bare_hex_prefix_is_symbol() {
        // "0x" with no digits following is not a valid number
        assert_eq!(toks("0x"), vec![Token::Symbol("0x".into())]);
        assert_eq!(toks("0X"), vec![Token::Symbol("0X".into())]);
    }

    #[test]
    fn nan_bare_is_symbol_matching_fennel() {
        // Fennel (via Lua tonumber) rejects bare nan/-nan/+nan as numbers and
        // treats them as symbols. We match that behavior explicitly.
        assert_eq!(toks("nan"),  vec![Token::Symbol("nan".into())]);
        assert_eq!(toks("-nan"), vec![Token::Symbol("-nan".into())]);
        assert_eq!(toks("+nan"), vec![Token::Symbol("+nan".into())]);
    }

    #[test]
    fn inf_bare_is_symbol_matching_fennel() {
        // Fennel's infinity literal is `.inf` (with a leading dot).
        // Bare `inf`, `+inf`, `-inf`, `infinity` etc. are valid identifiers,
        // not number literals. Rust's f64::from_str accepts them so they must
        // be excluded explicitly — same rationale as the nan exclusions above.
        assert_eq!(toks("inf"),       vec![Token::Symbol("inf".into())]);
        assert_eq!(toks("+inf"),      vec![Token::Symbol("+inf".into())]);
        assert_eq!(toks("-inf"),      vec![Token::Symbol("-inf".into())]);
        assert_eq!(toks("infinity"),  vec![Token::Symbol("infinity".into())]);
        assert_eq!(toks("+infinity"), vec![Token::Symbol("+infinity".into())]);
        assert_eq!(toks("-infinity"), vec![Token::Symbol("-infinity".into())]);
        // .inf (with dot) is still a valid number literal
        assert_eq!(toks(".inf"),  vec![Token::Number(f64::INFINITY)]);
        assert_eq!(toks("+.inf"), vec![Token::Number(f64::INFINITY)]);
        assert_eq!(toks("-.inf"), vec![Token::Number(f64::NEG_INFINITY)]);
    }

    #[test]
    fn inf_as_local_name_resolves_in_method_call() {
        // Regression: `inf` was tokenised as Token::Number(INFINITY) by Rust's
        // f64::from_str, so `(local inf {})` never created a binding and
        // `inf.x` / `inf:method` produced a false "unknown identifier" warning.
        use crate::analyzer::{analyze, SymbolEntry};
        use crate::parser::Parser;
        let src = "(local inf {}) inf.x";
        let (ast, _) = Parser::parse(src);
        let result = analyze(&ast);
        let inf_ref: Vec<&SymbolEntry> = result.syms.iter()
            .filter(|s| s.name == "inf.x" && !s.is_def)
            .collect();
        assert!(!inf_ref.is_empty(), "inf.x must appear as a symbol reference");
        assert!(
            inf_ref.iter().all(|s| s.def_byte.is_some()),
            "inf.x must resolve to the `inf` binding, not be unknown"
        );
    }

    #[test]
    fn long_strings_not_supported_tokenize_as_brackets() {
        // Fennel inherits Lua long strings [[ ... ]] but we have no scanner for
        // them. They currently tokenize as two nested bracket tokens, not a Str.
        // This test pins that behavior so it's visible if long-string support
        // is ever added.
        assert_eq!(
            toks("[[hello]]"),
            vec![
                Token::LBracket,
                Token::LBracket,
                Token::Symbol("hello".into()),
                Token::RBracket,
                Token::RBracket,
            ],
        );
    }
}
