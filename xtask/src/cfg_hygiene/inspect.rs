//! Construct boundary skipping and mutable-logic detection for `check-cfg-hygiene`.

use super::find_matching_delim;
use super::lexer::{Tok, TokKind};

/// Advances past the item, block, statement, match arm, or struct field starting at `start`.
pub(super) fn skip_attached_construct(tokens: &[Tok], start: usize) -> usize {
    if start >= tokens.len() {
        return start;
    }
    let mut i = start;

    if tokens[i].is_punct("{") {
        return find_matching_delim(tokens, i, "{", "}").map_or(tokens.len(), |c| c + 1);
    }
    if tokens[i].is_ident("if") {
        return skip_if_chain(tokens, i);
    }
    if tokens[i].is_ident("match")
        || tokens[i].is_ident("while")
        || tokens[i].is_ident("for")
        || tokens[i].is_ident("loop")
    {
        return skip_until_block_end(tokens, i);
    }

    let kw_idx = skip_qualifiers(tokens, i);
    if let Some(tok) = tokens.get(kw_idx) {
        if tok.is_ident("macro_rules") && tokens.get(kw_idx + 1).is_some_and(|t| t.is_punct("!")) {
            return skip_macro_rules(tokens, kw_idx);
        }
        if tok.is_ident("fn")
            || tok.is_ident("impl")
            || tok.is_ident("trait")
            || tok.is_ident("mod")
            || tok.is_ident("struct")
            || tok.is_ident("enum")
            || tok.is_ident("union")
            || tok.is_ident("extern")
        {
            return skip_item_decl_or_body(tokens, kw_idx);
        }
        if tok.is_ident("use") || tok.is_ident("type") || tok.is_ident("const") || tok.is_ident("static") {
            return skip_until_semicolon(tokens, kw_idx);
        }
    }

    if tokens[i].is_ident("let") {
        return skip_until_semicolon(tokens, i);
    }

    let mut depth_paren = 0usize;
    let mut depth_bracket = 0usize;
    let mut depth_brace = 0usize;
    let mut saw_fat_arrow = false;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.is_punct("(") {
            depth_paren += 1;
        } else if t.is_punct(")") {
            if depth_paren == 0 {
                return i;
            }
            depth_paren -= 1;
        } else if t.is_punct("[") {
            depth_bracket += 1;
        } else if t.is_punct("]") {
            if depth_bracket == 0 {
                return i;
            }
            depth_bracket -= 1;
        } else if t.is_punct("{") {
            depth_brace += 1;
        } else if t.is_punct("}") {
            if depth_brace == 0 {
                return i;
            }
            depth_brace -= 1;
            if depth_brace == 0 && depth_paren == 0 && depth_bracket == 0 && saw_fat_arrow {
                let next = i + 1;
                return if tokens.get(next).is_some_and(|n| n.is_punct(",")) { next + 1 } else { next };
            }
        } else if depth_paren == 0 && depth_bracket == 0 && depth_brace == 0 {
            if t.is_punct("=>") {
                saw_fat_arrow = true;
            } else if t.is_punct(";") || t.is_punct(",") {
                return i + 1;
            }
        }
        i += 1;
    }
    tokens.len()
}

fn skip_macro_rules(tokens: &[Tok], kw_idx: usize) -> usize {
    let mut i = kw_idx + 2; // past `macro_rules` `!`
    if matches!(tokens.get(i).map(|t| &t.kind), Some(TokKind::Ident(_))) {
        i += 1;
    }
    let Some(delim) = tokens.get(i) else {
        return tokens.len();
    };
    if delim.is_punct("{") {
        let end = find_matching_delim(tokens, i, "{", "}").map_or(tokens.len(), |c| c + 1);
        return if tokens.get(end).is_some_and(|t| t.is_punct(";")) { end + 1 } else { end };
    }
    if delim.is_punct("(") {
        let end = find_matching_delim(tokens, i, "(", ")").map_or(tokens.len(), |c| c + 1);
        return if tokens.get(end).is_some_and(|t| t.is_punct(";")) { end + 1 } else { end };
    }
    if delim.is_punct("[") {
        let end = find_matching_delim(tokens, i, "[", "]").map_or(tokens.len(), |c| c + 1);
        return if tokens.get(end).is_some_and(|t| t.is_punct(";")) { end + 1 } else { end };
    }
    skip_until_semicolon(tokens, i)
}

fn skip_if_chain(tokens: &[Tok], mut i: usize) -> usize {
    loop {
        i = skip_until_block_end(tokens, i);
        if tokens.get(i).is_some_and(|t| t.is_ident("else")) {
            i += 1;
            if tokens.get(i).is_some_and(|t| t.is_ident("if")) {
                continue;
            }
            return skip_until_block_end(tokens, i);
        }
        return i;
    }
}

fn skip_until_block_end(tokens: &[Tok], mut i: usize) -> usize {
    let mut depth_paren = 0usize;
    let mut depth_bracket = 0usize;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.is_punct("(") {
            depth_paren += 1;
        } else if t.is_punct(")") {
            depth_paren = depth_paren.saturating_sub(1);
        } else if t.is_punct("[") {
            depth_bracket += 1;
        } else if t.is_punct("]") {
            depth_bracket = depth_bracket.saturating_sub(1);
        } else if depth_paren == 0 && depth_bracket == 0 && t.is_punct("{") {
            return find_matching_delim(tokens, i, "{", "}").map_or(tokens.len(), |c| c + 1);
        }
        i += 1;
    }
    tokens.len()
}

fn skip_item_decl_or_body(tokens: &[Tok], mut i: usize) -> usize {
    let mut depth_paren = 0usize;
    let mut depth_bracket = 0usize;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.is_punct("(") {
            depth_paren += 1;
        } else if t.is_punct(")") {
            depth_paren = depth_paren.saturating_sub(1);
        } else if t.is_punct("[") {
            depth_bracket += 1;
        } else if t.is_punct("]") {
            depth_bracket = depth_bracket.saturating_sub(1);
        } else if depth_paren == 0 && depth_bracket == 0 {
            if t.is_punct(";") {
                return i + 1;
            }
            if t.is_punct("{") {
                return find_matching_delim(tokens, i, "{", "}").map_or(tokens.len(), |c| c + 1);
            }
        }
        i += 1;
    }
    tokens.len()
}

fn skip_until_semicolon(tokens: &[Tok], mut i: usize) -> usize {
    let mut depth = 0usize;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.is_punct("(") || t.is_punct("[") || t.is_punct("{") {
            depth += 1;
        } else if t.is_punct(")") || t.is_punct("]") || t.is_punct("}") {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && t.is_punct(";") {
            return i + 1;
        }
        i += 1;
    }
    tokens.len()
}

/// Skips visibility and item qualifiers without swallowing `const NAME: T = ...;` or `extern "C" { ... }` / `extern crate`.
fn skip_qualifiers(tokens: &[Tok], mut i: usize) -> usize {
    loop {
        let Some(tok) = tokens.get(i) else {
            return i;
        };
        if tok.is_ident("pub") {
            i += 1;
            if tokens.get(i).is_some_and(|t| t.is_punct("("))
                && let Some(close) = find_matching_delim(tokens, i, "(", ")")
            {
                i = close + 1;
            }
            continue;
        }
        if tok.is_ident("default") || tok.is_ident("unsafe") || tok.is_ident("async") {
            i += 1;
            continue;
        }
        if tok.is_ident("const")
            && tokens
                .get(i + 1)
                .is_some_and(|next| next.is_ident("fn") || next.is_ident("unsafe") || next.is_ident("extern"))
        {
            i += 1;
            continue;
        }
        if tok.is_ident("extern") {
            let after_abi =
                if matches!(tokens.get(i + 1).map(|t| &t.kind), Some(TokKind::StringLit(_))) { i + 2 } else { i + 1 };
            if tokens.get(after_abi).is_some_and(|t| t.is_ident("fn")) {
                i = after_abi;
                continue;
            }
        }
        return i;
    }
}

pub(super) fn inspect_windows_construct(slice: &[Tok]) -> Option<String> {
    if slice.is_empty() {
        return None;
    }
    let kw_idx = skip_qualifiers(slice, 0);
    if let Some(tok) = slice.get(kw_idx) {
        if tok.is_ident("fn") {
            let name = match slice.get(kw_idx + 1).map(|t| &t.kind) {
                Some(TokKind::Ident(n)) => n.as_str(),
                _ => "<anonymous>",
            };
            return Some(format!("Windows-only function `{name}` outside .cargo/windows-only"));
        }
        if tok.is_ident("impl") {
            return Some("Windows-only `impl` block outside .cargo/windows-only".to_owned());
        }
        if tok.is_ident("trait") {
            if trait_has_default_method_body(&slice[kw_idx..]) {
                return Some("Windows-only `trait` with default method body outside .cargo/windows-only".to_owned());
            }
            return None;
        }
        if tok.is_ident("mod") {
            let name = match slice.get(kw_idx + 1).map(|t| &t.kind) {
                Some(TokKind::Ident(n)) => n.as_str(),
                _ => "<anonymous>",
            };
            if slice.get(kw_idx + 2).is_some_and(|t| t.is_punct("{")) {
                return Some(format!("inline Windows-only module `mod {name} {{ ... }}` outside .cargo/windows-only"));
            }
            return None;
        }
        if tok.is_ident("use")
            || tok.is_ident("type")
            || tok.is_ident("struct")
            || tok.is_ident("enum")
            || tok.is_ident("union")
            || tok.is_ident("const")
            || tok.is_ident("static")
            || tok.is_ident("extern")
            || tok.is_ident("macro_rules")
        {
            return None;
        }
    }

    // Struct field: `[pub[(...)]] ident : Type [,|}]`
    if let Some(TokKind::Ident(_)) = slice.get(kw_idx).map(|t| &t.kind)
        && slice.get(kw_idx + 1).is_some_and(|t| t.is_punct(":"))
    {
        return None;
    }

    // Enum variant without `=>` (e.g. `Windows { bridge: ConPtyBridge, eof: bool },`)
    let fat_arrow_pos = find_top_level_punct(slice, "=>");
    if fat_arrow_pos.is_none()
        && !slice[0].is_ident("let")
        && !slice[0].is_ident("if")
        && !slice[0].is_ident("match")
        && !slice[0].is_ident("for")
        && !slice[0].is_ident("while")
        && !slice[0].is_ident("loop")
        && !slice[0].is_punct("{")
        && !slice.iter().any(|t| t.is_punct(";"))
        && matches!(&slice[0].kind, TokKind::Ident(name) if name.chars().next().is_some_and(char::is_uppercase))
        && slice.get(1).is_some_and(|t| t.is_punct("{") || t.is_punct("(") || t.is_punct(","))
    {
        return None;
    }

    // Match arm: check both any `if` match guard before `=>` and the arm body after `=>`.
    if let Some(arrow_pos) = fat_arrow_pos {
        if let Some(guard_if) = find_top_level_ident(&slice[..arrow_pos], "if")
            && let Some(reason) = find_mutable_logic_reason(&slice[guard_if..arrow_pos])
        {
            return Some(format!("mutable Windows-only logic ({reason}) outside .cargo/windows-only"));
        }
        return find_mutable_logic_reason(&slice[arrow_pos + 1..])
            .map(|reason| format!("mutable Windows-only logic ({reason}) outside .cargo/windows-only"));
    }

    let expr = if slice[0].is_ident("let") {
        let eq_pos = find_top_level_punct(slice, "=")?;
        &slice[eq_pos + 1..]
    } else {
        slice
    };

    find_mutable_logic_reason(expr)
        .map(|reason| format!("mutable Windows-only logic ({reason}) outside .cargo/windows-only"))
}

/// Returns true if a `trait ... { ... }` slice contains any `fn` with a `{ ... }` default body.
fn trait_has_default_method_body(slice: &[Tok]) -> bool {
    let Some(open) = slice.iter().position(|t| t.is_punct("{")) else {
        return false;
    };
    let Some(close) = find_matching_delim(slice, open, "{", "}") else {
        return false;
    };
    let body = &slice[open + 1..close];
    let mut i = 0usize;
    while i < body.len() {
        if body[i].is_ident("fn") {
            let mut depth = 0usize;
            i += 1;
            while i < body.len() {
                if body[i].is_punct("(") || body[i].is_punct("[") {
                    depth += 1;
                } else if body[i].is_punct(")") || body[i].is_punct("]") {
                    depth = depth.saturating_sub(1);
                } else if depth == 0 {
                    if body[i].is_punct("{") {
                        return true;
                    }
                    if body[i].is_punct(";") {
                        i += 1;
                        break;
                    }
                }
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    false
}

fn find_top_level_punct(slice: &[Tok], punct: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, tok) in slice.iter().enumerate() {
        if tok.is_punct("(") || tok.is_punct("[") || tok.is_punct("{") {
            depth += 1;
        } else if tok.is_punct(")") || tok.is_punct("]") || tok.is_punct("}") {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && tok.is_punct(punct) {
            return Some(i);
        }
    }
    None
}

fn find_top_level_ident(slice: &[Tok], ident: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, tok) in slice.iter().enumerate() {
        if tok.is_punct("(") || tok.is_punct("[") || tok.is_punct("{") {
            depth += 1;
        } else if tok.is_punct(")") || tok.is_punct("]") || tok.is_punct("}") {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && tok.is_ident(ident) {
            return Some(i);
        }
    }
    None
}

/// Scans an expression/block token slice for constructs `cargo-mutants` mutates on Windows.
fn find_mutable_logic_reason(tokens: &[Tok]) -> Option<&'static str> {
    let mut i = 0usize;
    while i < tokens.len() {
        let tok = &tokens[i];

        // Skip turbofish `::<Type>` and generic type arguments `UpperIdent<Type>`.
        if tok.is_punct("::") && tokens.get(i + 1).is_some_and(|t| t.is_punct("<")) {
            i = skip_angle_group(tokens, i + 1);
            continue;
        }
        if matches!(&tok.kind, TokKind::Ident(n) if n.chars().next().is_some_and(char::is_uppercase))
            && tokens.get(i + 1).is_some_and(|t| t.is_punct("<"))
        {
            i = skip_angle_group(tokens, i + 1);
            continue;
        }

        // Skip type expressions after `as` or closure return `-> Type {`.
        if tok.is_ident("as") {
            i = skip_type_expr(tokens, i + 1);
            continue;
        }
        if tok.is_punct("->") {
            i = skip_type_expr(tokens, i + 1);
            continue;
        }

        // Skip `let pat [: Type] =` header (scanning only the initializer RHS).
        if tok.is_ident("let") {
            i += 1;
            let mut depth = 0usize;
            while i < tokens.len() {
                if tokens[i].is_punct("(") || tokens[i].is_punct("[") || tokens[i].is_punct("{") {
                    depth += 1;
                } else if tokens[i].is_punct(")") || tokens[i].is_punct("]") || tokens[i].is_punct("}") {
                    depth = depth.saturating_sub(1);
                } else if depth == 0 && (tokens[i].is_punct("=") || tokens[i].is_punct(";")) {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if tok.is_ident("const") || tok.is_ident("use") || tok.is_ident("type") {
            i = skip_until_semicolon(tokens, i);
            continue;
        }

        if tok.is_ident("fn") {
            return Some("nested `fn` definition");
        }
        if tok.is_ident("impl") {
            return Some("nested `impl` block");
        }
        if tok.is_ident("if") && !tokens.get(i + 1).is_some_and(|t| t.is_ident("let")) {
            return Some("conditional `if` branch");
        }
        if tok.is_ident("while") {
            return Some("`while` loop");
        }
        if tok.is_ident("loop") {
            return Some("`loop`");
        }
        if tok.is_ident("match") {
            return Some("`match` dispatch");
        }

        if let TokKind::Punct(p) = &tok.kind {
            match p.as_str() {
                "==" | "!=" | "<=" | ">=" => return Some("comparison operator"),
                "&&" => {
                    if !tokens.get(i + 1).is_some_and(|t| t.is_ident("let")) {
                        return Some("boolean logic operator");
                    }
                }
                "||" => {
                    if i > 0 && tokens[i - 1].is_value_end() {
                        return Some("boolean logic operator");
                    }
                }
                "|" => {
                    if i > 0 && tokens[i - 1].is_value_end() {
                        return Some("bitwise OR operator `|`");
                    }
                    // Opening `|` of a closure parameter list `|a: Vec<u8>, b|`: skip to closing `|`.
                    if let Some(end) = skip_closure_params(tokens, i) {
                        i = end;
                        continue;
                    }
                }
                "&" => {
                    if i > 0 && tokens[i - 1].is_value_end() {
                        return Some("bitwise AND operator `&`");
                    }
                }
                "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|=" | "^=" => return Some("compound assignment operator"),
                "<<" | ">>" | "+" | "/" | "%" | "^" => return Some("arithmetic/bitwise operator"),
                "-" if i > 0 && tokens[i - 1].is_value_end() => return Some("subtraction operator"),
                "*" if i > 0 && tokens[i - 1].is_value_end() => return Some("multiplication operator"),
                "!" => {
                    let is_macro = i > 0 && matches!(tokens[i - 1].kind, TokKind::Ident(_));
                    if !is_macro {
                        return Some("boolean negation `!`");
                    }
                }
                "<" | ">" => {
                    let lhs_is_value = i > 0
                        && match &tokens[i - 1].kind {
                            TokKind::Ident(name) => name.chars().next().is_some_and(|c| c.is_lowercase() || c == '_'),
                            TokKind::NumberLit => true,
                            TokKind::Punct(lp) => matches!(lp.as_str(), ")" | "]"),
                            _ => false,
                        };
                    if lhs_is_value {
                        return Some(if p == "<" { "comparison operator `<`" } else { "comparison operator `>`" });
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Skips from an opening closure `|` across its parameter list (including typed generics) past the closing `|`.
fn skip_closure_params(tokens: &[Tok], open_bar: usize) -> Option<usize> {
    let mut i = open_bar + 1;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut angle = 0usize;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.is_punct("(") {
            paren += 1;
        } else if t.is_punct(")") {
            if paren == 0 {
                return None;
            }
            paren -= 1;
        } else if t.is_punct("[") {
            bracket += 1;
        } else if t.is_punct("]") {
            if bracket == 0 {
                return None;
            }
            bracket -= 1;
        } else if t.is_punct("<") {
            angle += 1;
        } else if t.is_punct(">") {
            angle = angle.saturating_sub(1);
        } else if t.is_punct(">>") {
            angle = angle.saturating_sub(2);
        } else if paren == 0 && bracket == 0 && angle == 0 {
            if t.is_punct("|") {
                return Some(i + 1);
            }
            if t.is_punct(";") || t.is_punct("{") {
                return None;
            }
        }
        i += 1;
    }
    None
}

/// Skips a type expression after `as` or `->` so generic `<...>` / `+` bounds are not misread as operators.
fn skip_type_expr(tokens: &[Tok], mut i: usize) -> usize {
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut angle = 0usize;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.is_punct("(") {
            paren += 1;
        } else if t.is_punct(")") {
            if paren == 0 {
                return i;
            }
            paren -= 1;
        } else if t.is_punct("[") {
            bracket += 1;
        } else if t.is_punct("]") {
            if bracket == 0 {
                return i;
            }
            bracket -= 1;
        } else if t.is_punct("<") {
            angle += 1;
        } else if t.is_punct(">") {
            if angle == 0 {
                return i;
            }
            angle -= 1;
        } else if t.is_punct(">>") {
            if angle == 0 {
                return i;
            }
            angle = angle.saturating_sub(2);
        } else if paren == 0 && bracket == 0 && angle == 0 {
            let still_type = matches!(t.kind, TokKind::Ident(_))
                || matches!(
                    &t.kind,
                    TokKind::Punct(p) if matches!(p.as_str(), "::" | "&" | "*" | "+" | "!" | "_")
                );
            if !still_type {
                return i;
            }
        }
        i += 1;
    }
    tokens.len()
}

fn skip_angle_group(tokens: &[Tok], open_idx: usize) -> usize {
    let mut depth = 0usize;
    for (idx, tok) in tokens.iter().enumerate().skip(open_idx) {
        if tok.is_punct("<") {
            depth += 1;
        } else if tok.is_punct(">") {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return idx + 1;
            }
        } else if tok.is_punct(">>") {
            depth = depth.saturating_sub(2);
            if depth == 0 {
                return idx + 1;
            }
        } else if tok.is_punct(";") || tok.is_punct("{") {
            return idx;
        }
    }
    tokens.len()
}
