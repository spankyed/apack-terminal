//! Minimal Rust tokenizer for `check-cfg-hygiene` (strips comments and literals with 1-based lines).

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TokKind {
    Ident(String),
    StringLit(String),
    CharLit,
    NumberLit,
    Punct(String),
}

#[derive(Debug, Clone)]
pub(super) struct Tok {
    pub(super) line: usize,
    pub(super) kind: TokKind,
}

impl Tok {
    pub(super) fn is_ident(&self, expected: &str) -> bool {
        matches!(&self.kind, TokKind::Ident(s) if s == expected)
    }

    pub(super) fn is_punct(&self, expected: &str) -> bool {
        matches!(&self.kind, TokKind::Punct(s) if s == expected)
    }

    pub(super) fn is_value_end(&self) -> bool {
        match &self.kind {
            TokKind::Ident(s) => !matches!(
                s.as_str(),
                "let"
                    | "mut"
                    | "ref"
                    | "return"
                    | "break"
                    | "in"
                    | "if"
                    | "else"
                    | "while"
                    | "for"
                    | "match"
                    | "as"
                    | "where"
                    | "move"
                    | "pub"
            ),
            TokKind::NumberLit | TokKind::StringLit(_) | TokKind::CharLit => true,
            TokKind::Punct(p) => matches!(p.as_str(), ")" | "]"),
        }
    }
}

pub(super) fn tokenize(source: &str) -> Vec<Tok> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1usize;

    while i < bytes.len() {
        let b = bytes[i];

        if b == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Line comment
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Nested block comment
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'\n' {
                    line += 1;
                    i += 1;
                } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }

        // Raw string literal: r"...", r#"..."#, br#"..."#, cr#"..."#
        if let Some((next_i, lines_added, val)) = lex_raw_string(source, bytes, i) {
            out.push(Tok { line, kind: TokKind::StringLit(val) });
            line += lines_added;
            i = next_i;
            continue;
        }

        // Byte/C string literal: b"...", c"..."
        if (b == b'b' || b == b'c') && bytes.get(i + 1) == Some(&b'"') {
            let (next_i, lines_added, val) = lex_normal_string(source, bytes, i + 1);
            out.push(Tok { line, kind: TokKind::StringLit(val) });
            line += lines_added;
            i = next_i;
            continue;
        }

        // Normal string literal
        if b == b'"' {
            let (next_i, lines_added, val) = lex_normal_string(source, bytes, i);
            out.push(Tok { line, kind: TokKind::StringLit(val) });
            line += lines_added;
            i = next_i;
            continue;
        }

        // Byte char literal: b'x'
        if b == b'b'
            && bytes.get(i + 1) == Some(&b'\'')
            && let Some(end) = lex_char_literal(bytes, i + 1)
        {
            out.push(Tok { line, kind: TokKind::CharLit });
            i = end;
            continue;
        }

        // Char literal vs lifetime ('a / 'static)
        if b == b'\'' {
            if let Some(end) = lex_char_literal(bytes, i) {
                out.push(Tok { line, kind: TokKind::CharLit });
                i = end;
                continue;
            }
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            continue;
        }

        // Raw identifier: r#ident
        if b == b'r'
            && bytes.get(i + 1) == Some(&b'#')
            && bytes.get(i + 2).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
        {
            let start = i + 2;
            i = start;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            out.push(Tok { line, kind: TokKind::Ident(format!("r#{}", &source[start..i])) });
            continue;
        }

        // Identifier
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            out.push(Tok { line, kind: TokKind::Ident(source[start..i].to_owned()) });
            continue;
        }

        // Number literal
        if b.is_ascii_digit() {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            out.push(Tok { line, kind: TokKind::NumberLit });
            continue;
        }

        // Multi-char punctuation (longest match first)
        let rem = &source[i..];
        let matched = [
            "..=", "<<=", ">>=", "..", "::", "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "+=", "-=", "*=", "/=",
            "%=", "&=", "|=", "^=", "<<", ">>",
        ]
        .into_iter()
        .find(|p| rem.starts_with(p));
        if let Some(p) = matched {
            out.push(Tok { line, kind: TokKind::Punct(p.to_owned()) });
            i += p.len();
            continue;
        }

        // Single-char punctuation (or non-ASCII character)
        let ch = rem.chars().next().unwrap();
        out.push(Tok { line, kind: TokKind::Punct(ch.to_string()) });
        i += ch.len_utf8();
    }

    out
}

fn lex_raw_string(source: &str, bytes: &[u8], start: usize) -> Option<(usize, usize, String)> {
    let r_pos = match bytes[start] {
        b'r' => start,
        b'b' | b'c' if bytes.get(start + 1) == Some(&b'r') => start + 1,
        _ => return None,
    };
    let mut hashes = 0usize;
    let mut j = r_pos + 1;
    while j < bytes.len() && bytes[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    if bytes.get(j) != Some(&b'"') {
        return None;
    }
    let content_start = j + 1;
    let mut k = content_start;
    let mut lines_added = 0usize;
    while k < bytes.len() {
        if bytes[k] == b'\n' {
            lines_added += 1;
            k += 1;
            continue;
        }
        if bytes[k] == b'"' && k + 1 + hashes <= bytes.len() && bytes[k + 1..k + 1 + hashes].iter().all(|&c| c == b'#')
        {
            let val = source[content_start..k].to_owned();
            return Some((k + 1 + hashes, lines_added, val));
        }
        k += 1;
    }
    Some((bytes.len(), lines_added, source[content_start..].to_owned()))
}

fn lex_normal_string(source: &str, bytes: &[u8], quote_pos: usize) -> (usize, usize, String) {
    let mut k = quote_pos + 1;
    let mut lines_added = 0usize;
    let mut val = String::new();
    while k < bytes.len() {
        if bytes[k] == b'\\' {
            if let Some(&esc) = bytes.get(k + 1) {
                if esc == b'\n' {
                    lines_added += 1;
                } else {
                    val.push(esc as char);
                }
                k += 2;
            } else {
                k += 1;
            }
            continue;
        }
        if bytes[k] == b'"' {
            return (k + 1, lines_added, val);
        }
        if bytes[k] == b'\n' {
            lines_added += 1;
        }
        let ch = source[k..].chars().next().unwrap();
        val.push(ch);
        k += ch.len_utf8();
    }
    (bytes.len(), lines_added, val)
}

fn lex_char_literal(bytes: &[u8], quote_pos: usize) -> Option<usize> {
    let mut i = quote_pos + 1;
    if i >= bytes.len() {
        return None;
    }
    if bytes[i] == b'\\' {
        i += 2;
        while i < bytes.len() && bytes[i] != b'\'' {
            if bytes[i] == b'\n' || bytes[i] == b' ' || bytes[i] == b';' {
                return None;
            }
            i += 1;
        }
        (bytes.get(i) == Some(&b'\'')).then_some(i + 1)
    } else {
        let s = std::str::from_utf8(&bytes[i..]).ok()?;
        let ch = s.chars().next()?;
        let next = i + ch.len_utf8();
        (bytes.get(next) == Some(&b'\'')).then_some(next + 1)
    }
}
