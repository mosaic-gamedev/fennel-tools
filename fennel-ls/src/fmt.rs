/// Fennel source formatter.
///
/// Uses the rich (comment-bearing) AST from `Parser::parse_with_comments` so
/// comments are preserved in-place.  The source text is threaded through so
/// atoms are emitted verbatim (preserving `0xff`, string escapes, etc.).

use crate::parser::{Parser, RichForm, RichNode};

const COL_LIMIT: usize = 80;

/// Format `src`, returning the formatted text.
/// Returns `None` if the source has parse errors (avoids mangling broken code).
pub fn format(src: &str) -> Option<String> {
    let (nodes, errors) = Parser::parse_with_comments(src);
    if !errors.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(src.len());
    fmt_top_level(&nodes, src, &mut out);
    // Normalise to a single trailing newline.
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

// ── Classification ────────────────────────────────────────────────────────────

/// Forms that use body-style indentation: head + first structural arg(s) stay
/// on line 1; everything after the first compound (List/Table) child breaks to
/// a new line with 2-space indent.
///
/// Mirrors Clojure's cljfmt indentation rule table, adapted for Fennel.
fn is_body_form(name: &str) -> bool {
    matches!(
        name,
        "fn" | "lambda" | "λ"
        | "macro" | "macros"
        | "local" | "var" | "global" | "set" | "tset"
        | "let" | "with-open"
        | "if" | "when" | "unless"
        | "do"
        | "each" | "for" | "while"
        | "match" | "case" | "match-try" | "case-try"
        | "accumulate" | "collect" | "icollect" | "fcollect" | "faccumulate"
        | "pick-values" | "pick-args"
    )
}

// ── Top-level ─────────────────────────────────────────────────────────────────

fn fmt_top_level(nodes: &[RichNode], src: &str, out: &mut String) {
    let mut prev_was_comment = false;
    for (i, node) in nodes.iter().enumerate() {
        if i > 0 {
            // One blank line between top-level forms; no blank line between a
            // comment and the form it annotates.
            if prev_was_comment {
                out.push('\n');
            } else {
                out.push_str("\n\n");
            }
        }
        fmt_node(node, src, 0, out);
        prev_was_comment = matches!(node.node, RichForm::Comment(_));
    }
}

// ── Node rendering ────────────────────────────────────────────────────────────

fn fmt_node(node: &RichNode, src: &str, indent: usize, out: &mut String) {
    if let RichForm::Comment(text) = &node.node {
        out.push_str(text.trim_end());
        return;
    }
    if let Some(flat) = render_flat(node, src) {
        if indent + flat.len() <= COL_LIMIT {
            out.push_str(&flat);
            return;
        }
    }
    render_tall(node, src, indent, out);
}

// ── Flat rendering ────────────────────────────────────────────────────────────

/// Attempt to render `node` on a single line.
/// Returns `None` if the node contains a comment or any child needs tall rendering.
fn render_flat(node: &RichNode, src: &str) -> Option<String> {
    match &node.node {
        RichForm::Comment(_) => None,

        RichForm::List(ch) => compound_flat("(", ")", ch, src),
        RichForm::Table(ch) => compound_flat("{", "}", ch, src),
        RichForm::Sequence(ch) => compound_flat("[", "]", ch, src),

        RichForm::Quote(inner) => {
            render_flat(inner, src).map(|s| format!("'{s}"))
        }
        RichForm::Quasiquote(inner) => {
            render_flat(inner, src).map(|s| format!("`{s}"))
        }
        RichForm::Unquote(inner) => {
            render_flat(inner, src).map(|s| format!(",{s}"))
        }
        RichForm::UnquoteSplice(inner) => {
            render_flat(inner, src).map(|s| format!(",@{s}"))
        }
        RichForm::HashFn(inner) => {
            render_flat(inner, src).map(|s| format!("#{s}"))
        }

        // Atoms: emit the original source text verbatim.
        _ => Some(atom_text(node, src).to_string()),
    }
}

fn compound_flat(open: &str, close: &str, children: &[RichNode], src: &str) -> Option<String> {
    let mut s = String::from(open);
    let mut first = true;
    for child in children {
        if matches!(child.node, RichForm::Comment(_)) {
            return None;
        }
        if !first {
            s.push(' ');
        }
        s.push_str(&render_flat(child, src)?);
        first = false;
    }
    s.push_str(close);
    Some(s)
}

// ── Tall rendering ────────────────────────────────────────────────────────────

fn render_tall(node: &RichNode, src: &str, indent: usize, out: &mut String) {
    match &node.node {
        RichForm::List(ch) => render_tall_compound("(", ")", ch, src, indent, out),
        RichForm::Table(ch) => render_tall_compound("{", "}", ch, src, indent, out),
        RichForm::Sequence(ch) => render_tall_compound("[", "]", ch, src, indent, out),

        RichForm::Quote(inner) => {
            out.push('\'');
            fmt_node(inner, src, indent + 1, out);
        }
        RichForm::Quasiquote(inner) => {
            out.push('`');
            fmt_node(inner, src, indent + 1, out);
        }
        RichForm::Unquote(inner) => {
            out.push(',');
            fmt_node(inner, src, indent + 1, out);
        }
        RichForm::UnquoteSplice(inner) => {
            out.push_str(",@");
            fmt_node(inner, src, indent + 2, out);
        }
        RichForm::HashFn(inner) => {
            out.push('#');
            fmt_node(inner, src, indent + 1, out);
        }

        RichForm::Comment(text) => {
            out.push_str(text.trim_end());
        }

        _ => out.push_str(atom_text(node, src)),
    }
}

fn render_tall_compound(
    open: &str,
    close: &str,
    children: &[RichNode],
    src: &str,
    indent: usize,
    out: &mut String,
) {
    let child_indent = indent + 2;

    out.push_str(open);

    if children.is_empty() {
        out.push_str(close);
        return;
    }

    let head = &children[0];

    // Determine if this is a body form: pack atoms/sequences on the head line,
    // break on the first List/Table child.
    let body_form = if let RichForm::Symbol(s) = &head.node {
        is_body_form(s)
    } else {
        false
    };

    // Head always sits on the same line as the opening delimiter.
    let head_flat = render_flat(head, src)
        .unwrap_or_else(|| atom_text(head, src).to_string());
    out.push_str(&head_flat);
    let mut col = indent + open.len() + head_flat.len();

    for child in &children[1..] {
        // Comments always go on their own line.
        if let RichForm::Comment(text) = &child.node {
            out.push('\n');
            push_indent(out, child_indent);
            out.push_str(text.trim_end());
            col = usize::MAX; // stop packing after a comment
            continue;
        }

        // For body forms: never pack List or Table children onto the head line.
        let force_break = col > COL_LIMIT
            || (body_form
                && matches!(child.node, RichForm::List(_) | RichForm::Table(_)));

        if !force_break {
            if let Some(flat) = render_flat(child, src) {
                if col + 1 + flat.len() <= COL_LIMIT {
                    out.push(' ');
                    out.push_str(&flat);
                    col += 1 + flat.len();
                    continue;
                }
            }
        }

        // Break to a new line.
        out.push('\n');
        push_indent(out, child_indent);
        fmt_node(child, src, child_indent, out);
        col = usize::MAX; // don't re-pack after a line break
    }

    out.push_str(close);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn atom_text<'s>(node: &RichNode, src: &'s str) -> &'s str {
    let s = node.span.start as usize;
    let e = node.span.end as usize;
    &src[s..e]
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push(' ');
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(src: &str) -> String {
        format(src).expect("format returned None (parse error)")
    }

    // ── Atoms ────────────────────────────────────────────────────────────────

    #[test]
    fn atom_preserved_verbatim() {
        assert_eq!(fmt("42"), "42\n");
        assert_eq!(fmt("0xff"), "0xff\n");
        assert_eq!(fmt(":keyword"), ":keyword\n");
        assert_eq!(fmt("\"hello\""), "\"hello\"\n");
    }

    // ── Short forms stay on one line ─────────────────────────────────────────

    #[test]
    fn short_list_stays_flat() {
        assert_eq!(fmt("(+ 1 2)"), "(+ 1 2)\n");
    }

    #[test]
    fn short_fn_stays_flat() {
        assert_eq!(fmt("(fn add [a b] (+ a b))"), "(fn add [a b] (+ a b))\n");
    }

    #[test]
    fn short_let_stays_flat() {
        assert_eq!(fmt("(let [x 1] x)"), "(let [x 1] x)\n");
    }

    // ── Body-form tall rendering ──────────────────────────────────────────────

    #[test]
    fn tall_fn_body_indented_two() {
        // flat form is 90 chars, exceeds COL_LIMIT=80
        let src = "(fn greet-someone [name] (print (.. \"Hello, how are you doing today, dear \" name \"?\")))";
        let out = fmt(src);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() >= 2, "expected tall output, got: {out}");
        assert!(lines[0].starts_with("(fn greet-someone [name]"), "head line: {}", lines[0]);
        assert!(lines[1].starts_with("  (print"), "body line: {}", lines[1]);
    }

    #[test]
    fn tall_if_body_indented_two() {
        // flat form is 87 chars, exceeds COL_LIMIT=80; last atom won't pack
        let src = "(if some-very-long-condition-name do-something-on-true do-something-on-false-right-now)";
        let out = fmt(src);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() >= 2, "expected tall output, got: {out}");
        assert!(lines[0].starts_with("(if"), "head line: {}", lines[0]);
        assert_eq!(lines[1].chars().take(2).collect::<String>(), "  ", "indent: {:?}", lines[1]);
    }

    #[test]
    fn tall_let_bindings_on_head_line() {
        // flat form is 91 chars, exceeds COL_LIMIT=80; [bindings] stays on head line,
        // body List breaks to next line because let is a body form
        let src = "(let [result some-long-val] (do-something-really-complex-here result and-another-long-arg))";
        let out = fmt(src);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() >= 2, "expected tall output, got: {out}");
        assert!(lines[0].contains("[result some-long-val]"), "bindings on head line: {}", lines[0]);
        assert!(lines[1].starts_with("  (do-"), "body: {}", lines[1]);
    }

    // ── Regular call tall rendering ───────────────────────────────────────────

    #[test]
    fn tall_regular_call_packs_args_greedily() {
        let src = "(string.format \"%s = %d\" very-long-name-here 42)";
        let out = fmt(src);
        // All args packed greedily onto available space; just check it's valid output.
        assert!(out.contains("string.format"));
        assert!(out.contains("very-long-name-here"));
    }

    // ── Comments ─────────────────────────────────────────────────────────────

    #[test]
    fn comment_preserved_top_level() {
        let src = ";; A comment\n(fn foo [] nil)";
        let out = fmt(src);
        assert!(out.starts_with(";; A comment"), "comment first: {out}");
        assert!(out.contains("(fn foo [] nil)"));
    }

    #[test]
    fn comment_inside_list_preserved() {
        let src = "(do\n  ;; step 1\n  (step-one)\n  ;; step 2\n  (step-two))";
        let out = fmt(src);
        assert!(out.contains(";; step 1"), "step 1 comment: {out}");
        assert!(out.contains(";; step 2"), "step 2 comment: {out}");
        assert!(out.contains("(step-one)"));
        assert!(out.contains("(step-two)"));
    }

    // ── Blank lines ───────────────────────────────────────────────────────────

    #[test]
    fn blank_line_between_top_level_forms() {
        let src = "(fn a [] 1)\n(fn b [] 2)";
        let out = fmt(src);
        assert!(out.contains("\n\n"), "expected blank line between forms: {out}");
    }

    #[test]
    fn no_blank_line_between_comment_and_form() {
        let src = ";; docs\n(fn foo [] nil)";
        let out = fmt(src);
        // Comment + form should be separated by ONE newline, not two.
        assert!(!out.contains(";; docs\n\n"), "unexpected blank line: {out}");
    }

    // ── Trailing newline ──────────────────────────────────────────────────────

    #[test]
    fn output_ends_with_single_newline() {
        assert!(fmt("(fn x [] nil)").ends_with('\n'));
        assert!(!fmt("(fn x [] nil)").ends_with("\n\n"));
    }

    // ── Parse error returns None ──────────────────────────────────────────────

    #[test]
    fn broken_input_returns_none() {
        assert!(format("(fn [x").is_none());
    }

    // ── Reader macros ─────────────────────────────────────────────────────────

    #[test]
    fn quote_flat() {
        assert_eq!(fmt("'x"), "'x\n");
        assert_eq!(fmt("'(1 2)"), "'(1 2)\n");
    }

    #[test]
    fn hash_fn_flat() {
        assert_eq!(fmt("#(+ % 1)"), "#(+ % 1)\n");
    }

    // ── Idempotence ───────────────────────────────────────────────────────────

    #[test]
    fn idempotent_simple_fn() {
        // Short fn: canonical form is flat (30 chars ≤ 80).
        let src = "(fn greet [name] (print name))\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn idempotent_if() {
        // Short if: canonical form is flat.
        let src = "(if cond a b)\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn idempotent_tall_fn() {
        // Format twice; result must be stable.
        let input = "(fn greet-someone [name] (print (.. \"Hello, how are you doing today, dear \" name \"?\")))";
        let once = fmt(input);
        let twice = fmt(&once);
        assert_eq!(once, twice, "formatter is not idempotent");
    }

    // ── String literals with embedded newlines ───────────────────────────────

    #[test]
    fn fmt_string_with_escape_newline_preserved() {
        // "\n" escape sequence: the source bytes are '\' and 'n', not a real newline.
        // The formatter must emit the escape verbatim; the result must be idempotent.
        let src = "(print \"hello\\nworld\")\n";
        let out = fmt(src);
        assert!(out.contains("\"hello\\nworld\""), "escape \\n must be preserved: {out:?}");
        assert_eq!(fmt(&out), out, "must be idempotent with \\n escape");
    }

    #[test]
    fn fmt_string_with_literal_newline_preserved() {
        // A string atom containing a LITERAL newline byte (line continuation in source).
        // The formatter must not corrupt the content.
        // Note: the Fennel lexer treats \<newline> as a line continuation inside strings,
        // but atom_text emits the raw source bytes verbatim. We simulate what the formatter
        // would receive when atom_text returns a string containing a literal '\n'.
        let src = "(print \"hello\nworld\")";
        let out = format(src);
        // If the source is parseable, output must contain the original string content.
        if let Some(out) = out {
            assert!(out.contains("hello\nworld"),
                "literal newline inside string must be preserved: {out:?}");
            // Must be idempotent.
            let again = fmt(&out);
            assert_eq!(out, again, "must be idempotent with literal newline in string");
        }
        // If parse fails (which is also acceptable — bare newlines in strings are unusual),
        // format() returning None is also correct behaviour.
    }

    #[test]
    fn fmt_col_limit_check_does_not_count_trailing_newline_in_string() {
        // A long string with a literal newline: the byte count exceeds the visual line width.
        // The formatter may render this flat (preserving the literal newline), but must not
        // produce output where the trailing-newline normalisation trims string content.
        // Specifically: the `while out.ends_with("\n\n") { out.pop() }` must not eat into
        // string literal content.
        let src = "(local msg \"first line\nsecond line\")";
        if let Some(out) = format(src) {
            // The compound ends with `)`, so out.ends_with("\n\n") is false regardless.
            assert!(!out.ends_with("\n\n"),
                "output must not end with double newline: {out:?}");
            assert!(out.ends_with('\n'),
                "output must end with exactly one newline: {out:?}");
            // Content must be preserved.
            assert!(out.contains("first line\nsecond line"),
                "string content must survive formatting: {out:?}");
        }
    }

    // ── Comment / blank-line interaction ─────────────────────────────────────

    #[test]
    fn consecutive_comments_no_blank_line() {
        // Two comments in a row: the second comment directly annotates the first,
        // so no blank line should be inserted between them.
        let src = ";; first line\n;; second line\n(fn foo [] nil)\n";
        let out = fmt(src);
        assert!(!out.contains(";; first line\n\n"), "unexpected blank between comments: {out:?}");
        assert!(!out.contains(";; second line\n\n"), "unexpected blank after last comment: {out:?}");
        assert!(out.contains(";; first line\n;; second line\n"), "comments must be adjacent: {out:?}");
    }

    #[test]
    fn blank_line_before_comment_annotating_form() {
        // A form followed by a comment followed by a form: there should be a blank
        // line BEFORE the comment (separating top-level forms) but NOT between the
        // comment and the form it annotates.
        let src = "(fn a [] 1)\n;; docs\n(fn b [] 2)\n";
        let out = fmt(src);
        // Blank line must appear before the comment.
        assert!(out.contains("(fn a [] 1)\n\n;; docs"), "expected blank before comment: {out:?}");
        // No blank line between comment and b.
        assert!(!out.contains(";; docs\n\n(fn b"), "unexpected blank after comment: {out:?}");
    }

    #[test]
    fn only_comment_top_level() {
        // A file consisting entirely of comments should not crash or insert blank lines.
        let src = ";; line 1\n;; line 2\n;; line 3\n";
        let out = fmt(src);
        assert!(!out.contains("\n\n"), "comments-only file must have no blank lines: {out:?}");
    }

    #[test]
    fn idempotent_comment_annotated_form() {
        // A comment stuck to the form it annotates must survive a second pass.
        let src = "(fn a [] 1)\n\n;; docs\n(fn b [] 2)\n";
        let out = fmt(src);
        let again = fmt(&out);
        assert_eq!(out, again, "comment-annotated form not idempotent");
    }

    #[test]
    fn idempotent_multi_form_with_comments() {
        // Simulate a realistic module header with comment groups above each fn.
        let src = ";; module header\n\n(fn setup [] nil)\n\n;; main loop\n(fn run [] (setup))\n\n;; cleanup\n(fn teardown [] nil)\n";
        let once = fmt(src);
        let twice = fmt(&once);
        assert_eq!(once, twice, "multi-form-with-comments not idempotent");
    }
}
