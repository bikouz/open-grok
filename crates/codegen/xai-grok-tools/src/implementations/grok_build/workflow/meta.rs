//! Extraction and validation of the `export const meta = {...}` header that
//! every workflow script must start with.
//!
//! The meta object is required to be a *pure literal* (objects, arrays,
//! strings, numbers, booleans, null — no identifiers, calls, spreads, or
//! template interpolation) so it can be read without executing the script.
//! Extraction is a small string/comment-aware brace matcher; parsing is a
//! recursive-descent JS-literal reader that produces `serde_json::Value`.

use serde_json::{Map as JsonMap, Value as JsonValue};

/// Parsed and validated workflow meta header.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowMeta {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub phases: Vec<WorkflowMetaPhase>,
    /// The full meta object as JSON, re-exposed to the script as `meta`.
    pub value: JsonValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowMetaPhase {
    pub title: String,
    pub detail: Option<String>,
}

/// Script split into its validated meta header and the remaining body.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitScript {
    pub meta: WorkflowMeta,
    /// Script source with the entire `export const meta = {...};` declaration
    /// removed. Leading comments/whitespace before the declaration are kept.
    pub body: String,
}

const MAX_META_NAME_LEN: usize = 64;

/// Split a workflow script into meta + body, validating the meta contract.
pub fn split_script(source: &str) -> Result<SplitScript, String> {
    let decl_start = find_meta_declaration(source)?;
    let after_keyword = &source[decl_start.equals_end..];
    let brace_offset = after_keyword
        .find(|c: char| !c.is_whitespace())
        .ok_or_else(|| meta_contract_error("`export const meta =` is not followed by an object"))?;
    if !after_keyword[brace_offset..].starts_with('{') {
        return Err(meta_contract_error(
            "`meta` must be an object literal starting with `{`",
        ));
    }
    let literal_start = decl_start.equals_end + brace_offset;
    let literal_end = match_braces(source, literal_start)?;
    let literal = &source[literal_start..literal_end];

    let value = parse_pure_literal(literal)?;
    let meta = validate_meta(value)?;

    // Consume an optional trailing `;` after the object literal.
    let mut body_start = literal_end;
    let rest = &source[literal_end..];
    let non_ws = rest
        .find(|c: char| !c.is_whitespace())
        .unwrap_or(rest.len());
    if rest[non_ws..].starts_with(';') {
        body_start = literal_end + non_ws + 1;
    }

    let mut body = String::with_capacity(source.len());
    body.push_str(&source[..decl_start.export_start]);
    body.push_str(&source[body_start..]);

    Ok(SplitScript { meta, body })
}

struct MetaDeclaration {
    /// Byte offset where `export` begins.
    export_start: usize,
    /// Byte offset just past the `=` sign.
    equals_end: usize,
}

/// Locate `export const meta =` at the top of the script, allowing leading
/// whitespace and comments only.
fn find_meta_declaration(source: &str) -> Result<MetaDeclaration, String> {
    let mut idx = 0usize;
    let bytes = source.as_bytes();
    loop {
        let rest = &source[idx..];
        let trimmed = rest.trim_start();
        idx += rest.len() - trimmed.len();
        if trimmed.starts_with("//") {
            match source[idx..].find('\n') {
                Some(nl) => idx += nl + 1,
                None => idx = source.len(),
            }
            continue;
        }
        if trimmed.starts_with("/*") {
            match source[idx + 2..].find("*/") {
                Some(end) => idx += 2 + end + 2,
                None => return Err(meta_contract_error("unterminated block comment")),
            }
            continue;
        }
        break;
    }
    let export_start = idx;
    let mut cursor = idx;
    for keyword in ["export", "const", "meta"] {
        let after = &source[cursor..];
        let trimmed = after.trim_start();
        let ws = after.len() - trimmed.len();
        if cursor != idx && ws == 0 {
            return Err(meta_contract_error(
                "workflow scripts must begin with `export const meta = {...}`",
            ));
        }
        if !trimmed.starts_with(keyword) {
            return Err(meta_contract_error(
                "workflow scripts must begin with `export const meta = {...}`",
            ));
        }
        cursor += ws + keyword.len();
        // The keyword must end at a word boundary.
        if let Some(&next) = bytes.get(cursor)
            && (next == b'_' || next == b'$' || next.is_ascii_alphanumeric())
        {
            return Err(meta_contract_error(
                "workflow scripts must begin with `export const meta = {...}`",
            ));
        }
    }
    let after = &source[cursor..];
    let trimmed = after.trim_start();
    if !trimmed.starts_with('=') {
        return Err(meta_contract_error(
            "workflow scripts must begin with `export const meta = {...}`",
        ));
    }
    let equals_end = cursor + (after.len() - trimmed.len()) + 1;
    Ok(MetaDeclaration {
        export_start,
        equals_end,
    })
}

/// Given `source[start] == '{'`, return the byte offset just past the matching
/// closing brace, skipping braces inside strings and comments.
fn match_braces(source: &str, start: usize) -> Result<usize, String> {
    debug_assert!(source[start..].starts_with('{'));
    let mut depth = 0usize;
    let mut chars = source[start..].char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(start + offset + ch.len_utf8());
                }
            }
            '\'' | '"' | '`' => {
                if ch == '`'
                    && let Some(interp) = scan_template_for_interpolation(source, start + offset)
                {
                    return Err(meta_contract_error(&format!(
                        "template interpolation at byte {interp} — meta must be a pure literal"
                    )));
                }
                let quote = ch;
                let mut terminated = false;
                while let Some((_, inner)) = chars.next() {
                    match inner {
                        '\\' => {
                            let _ = chars.next();
                        }
                        c if c == quote => {
                            terminated = true;
                            break;
                        }
                        _ => {}
                    }
                }
                if !terminated {
                    return Err(meta_contract_error("unterminated string in meta"));
                }
            }
            '/' => match chars.peek() {
                Some((_, '/')) => {
                    for (_, inner) in chars.by_ref() {
                        if inner == '\n' {
                            break;
                        }
                    }
                }
                Some((_, '*')) => {
                    let _ = chars.next();
                    let mut prev = '\0';
                    let mut terminated = false;
                    for (_, inner) in chars.by_ref() {
                        if prev == '*' && inner == '/' {
                            terminated = true;
                            break;
                        }
                        prev = inner;
                    }
                    if !terminated {
                        return Err(meta_contract_error("unterminated comment in meta"));
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    Err(meta_contract_error("unbalanced braces in meta object"))
}

/// Detect `${` inside a template literal starting at `open` (a backtick).
fn scan_template_for_interpolation(source: &str, open: usize) -> Option<usize> {
    let mut chars = source[open + 1..].char_indices();
    let mut prev = '\0';
    while let Some((offset, ch)) = chars.next() {
        match ch {
            '\\' => {
                let _ = chars.next();
                prev = '\0';
                continue;
            }
            '`' => return None,
            '{' if prev == '$' => return Some(open + 1 + offset),
            _ => {}
        }
        prev = ch;
    }
    None
}

fn meta_contract_error(detail: &str) -> String {
    format!(
        "invalid workflow meta: {detail}. Every workflow script must begin with \
         `export const meta = {{ name, description, phases? }}` where the object is a pure \
         literal (no variables, function calls, spreads, or template interpolation)."
    )
}

// ───────────────────────────────────────────────────────────────────────────
// Pure JS literal parser
// ───────────────────────────────────────────────────────────────────────────

struct LiteralParser {
    chars: Vec<char>,
    pos: usize,
}

/// Parse a pure JS literal (object/array/string/number/boolean/null) into JSON.
pub fn parse_pure_literal(source: &str) -> Result<JsonValue, String> {
    let mut parser = LiteralParser {
        chars: source.chars().collect(),
        pos: 0,
    };
    parser.skip_trivia()?;
    let value = parser.parse_value()?;
    parser.skip_trivia()?;
    if parser.pos != parser.chars.len() {
        return Err(meta_contract_error(&format!(
            "unexpected trailing content after literal: `{}`",
            parser.remaining_preview()
        )));
    }
    Ok(value)
}

impl LiteralParser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek();
        if ch.is_some() {
            self.pos += 1;
        }
        ch
    }

    fn remaining_preview(&self) -> String {
        self.chars[self.pos..]
            .iter()
            .take(24)
            .collect::<String>()
            .trim()
            .to_string()
    }

    fn skip_trivia(&mut self) -> Result<(), String> {
        loop {
            while matches!(self.peek(), Some(c) if c.is_whitespace()) {
                self.pos += 1;
            }
            if self.peek() == Some('/') {
                match self.chars.get(self.pos + 1) {
                    Some('/') => {
                        while let Some(c) = self.peek() {
                            self.pos += 1;
                            if c == '\n' {
                                break;
                            }
                        }
                        continue;
                    }
                    Some('*') => {
                        self.pos += 2;
                        let mut prev = '\0';
                        let mut terminated = false;
                        while let Some(c) = self.bump() {
                            if prev == '*' && c == '/' {
                                terminated = true;
                                break;
                            }
                            prev = c;
                        }
                        if !terminated {
                            return Err(meta_contract_error("unterminated comment in meta"));
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            return Ok(());
        }
    }

    fn parse_value(&mut self) -> Result<JsonValue, String> {
        match self.peek() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('\'') | Some('"') | Some('`') => self.parse_string().map(JsonValue::String),
            Some(c) if c == '-' || c == '+' || c.is_ascii_digit() || c == '.' => {
                self.parse_number()
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
                let word = self.parse_identifier();
                match word.as_str() {
                    "true" => Ok(JsonValue::Bool(true)),
                    "false" => Ok(JsonValue::Bool(false)),
                    "null" => Ok(JsonValue::Null),
                    other => Err(meta_contract_error(&format!(
                        "identifier `{other}` is not allowed — meta must be a pure literal"
                    ))),
                }
            }
            Some('.') => self.parse_number(),
            Some(other) => Err(meta_contract_error(&format!(
                "unexpected character `{other}` in literal"
            ))),
            None => Err(meta_contract_error("unexpected end of literal")),
        }
    }

    fn parse_identifier(&mut self) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                out.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        out
    }

    fn parse_object(&mut self) -> Result<JsonValue, String> {
        debug_assert_eq!(self.peek(), Some('{'));
        self.pos += 1;
        let mut map = JsonMap::new();
        loop {
            self.skip_trivia()?;
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    return Ok(JsonValue::Object(map));
                }
                Some('.') => {
                    // `...spread` at key position — reject with a clearer message.
                    return Err(meta_contract_error(
                        "spread syntax is not allowed — meta must be a pure literal",
                    ));
                }
                None => return Err(meta_contract_error("unterminated object literal")),
                _ => {}
            }
            let key = match self.peek() {
                Some('\'') | Some('"') | Some('`') => self.parse_string()?,
                Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
                    self.parse_identifier()
                }
                Some(other) => {
                    return Err(meta_contract_error(&format!(
                        "unexpected character `{other}` where an object key was expected"
                    )));
                }
                None => return Err(meta_contract_error("unterminated object literal")),
            };
            if key.is_empty() {
                return Err(meta_contract_error("empty object key"));
            }
            self.skip_trivia()?;
            if self.bump() != Some(':') {
                return Err(meta_contract_error(&format!(
                    "object key `{key}` must be followed by `:` (shorthand and methods are not \
                     literals)"
                )));
            }
            self.skip_trivia()?;
            let value = self.parse_value()?;
            map.insert(key, value);
            self.skip_trivia()?;
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some('}') => {}
                _ => {
                    return Err(meta_contract_error(&format!(
                        "expected `,` or `}}` in object, found `{}`",
                        self.remaining_preview()
                    )));
                }
            }
        }
    }

    fn parse_array(&mut self) -> Result<JsonValue, String> {
        debug_assert_eq!(self.peek(), Some('['));
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_trivia()?;
            match self.peek() {
                Some(']') => {
                    self.pos += 1;
                    return Ok(JsonValue::Array(items));
                }
                None => return Err(meta_contract_error("unterminated array literal")),
                _ => {}
            }
            items.push(self.parse_value()?);
            self.skip_trivia()?;
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some(']') => {}
                _ => {
                    return Err(meta_contract_error(&format!(
                        "expected `,` or `]` in array, found `{}`",
                        self.remaining_preview()
                    )));
                }
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        let quote = self.bump().expect("caller checked quote");
        let mut out = String::new();
        while let Some(c) = self.bump() {
            match c {
                '\\' => {
                    let Some(escaped) = self.bump() else {
                        return Err(meta_contract_error("unterminated string escape"));
                    };
                    match escaped {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'v' => out.push('\u{b}'),
                        '0' => out.push('\0'),
                        'u' => {
                            let mut code = String::new();
                            if self.peek() == Some('{') {
                                self.pos += 1;
                                while let Some(h) = self.peek() {
                                    if h == '}' {
                                        break;
                                    }
                                    code.push(h);
                                    self.pos += 1;
                                }
                                if self.bump() != Some('}') {
                                    return Err(meta_contract_error("bad \\u{...} escape"));
                                }
                            } else {
                                for _ in 0..4 {
                                    let Some(h) = self.bump() else {
                                        return Err(meta_contract_error("bad \\u escape"));
                                    };
                                    code.push(h);
                                }
                            }
                            let Some(ch) =
                                u32::from_str_radix(&code, 16).ok().and_then(char::from_u32)
                            else {
                                return Err(meta_contract_error("bad unicode escape"));
                            };
                            out.push(ch);
                        }
                        'x' => {
                            let mut code = String::new();
                            for _ in 0..2 {
                                let Some(h) = self.bump() else {
                                    return Err(meta_contract_error("bad \\x escape"));
                                };
                                code.push(h);
                            }
                            let Some(ch) =
                                u32::from_str_radix(&code, 16).ok().and_then(char::from_u32)
                            else {
                                return Err(meta_contract_error("bad hex escape"));
                            };
                            out.push(ch);
                        }
                        other => out.push(other),
                    }
                }
                c if c == quote => return Ok(out),
                '$' if quote == '`' && self.peek() == Some('{') => {
                    return Err(meta_contract_error(
                        "template interpolation is not allowed — meta must be a pure literal",
                    ));
                }
                other => out.push(other),
            }
        }
        Err(meta_contract_error("unterminated string in meta"))
    }

    fn parse_number(&mut self) -> Result<JsonValue, String> {
        let mut text = String::new();
        if matches!(self.peek(), Some('-') | Some('+')) {
            if self.peek() == Some('-') {
                text.push('-');
            }
            self.pos += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '+' || c == '-' {
                // Allow exponent signs only right after e/E.
                if (c == '+' || c == '-') && !matches!(text.chars().last(), Some('e') | Some('E')) {
                    break;
                }
                if c != '_' {
                    text.push(c);
                }
                self.pos += 1;
            } else {
                break;
            }
        }
        if text.is_empty() || text == "-" {
            return Err(meta_contract_error("malformed number in literal"));
        }
        serde_json::from_str::<JsonValue>(&text)
            .ok()
            .filter(JsonValue::is_number)
            .ok_or_else(|| meta_contract_error(&format!("malformed number `{text}` in literal")))
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Meta validation
// ───────────────────────────────────────────────────────────────────────────

fn validate_meta(value: JsonValue) -> Result<WorkflowMeta, String> {
    let JsonValue::Object(ref map) = value else {
        return Err(meta_contract_error("`meta` must be an object"));
    };
    let name = map
        .get("name")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| meta_contract_error("`meta.name` must be a non-empty string"))?;
    if name.len() > MAX_META_NAME_LEN {
        return Err(meta_contract_error(&format!(
            "`meta.name` must be at most {MAX_META_NAME_LEN} characters"
        )));
    }
    let description = map
        .get("description")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| meta_contract_error("`meta.description` must be a non-empty string"))?;
    let when_to_use = match map.get("whenToUse") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err(meta_contract_error("`meta.whenToUse` must be a string"));
        }
    };
    let phases = match map.get("phases") {
        None | Some(JsonValue::Null) => Vec::new(),
        Some(JsonValue::Array(items)) => {
            let mut phases = Vec::with_capacity(items.len());
            for item in items {
                let JsonValue::Object(phase) = item else {
                    return Err(meta_contract_error(
                        "`meta.phases` entries must be objects with a `title`",
                    ));
                };
                let title = phase
                    .get("title")
                    .and_then(JsonValue::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        meta_contract_error("`meta.phases[].title` must be a non-empty string")
                    })?;
                let detail = phase
                    .get("detail")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string);
                phases.push(WorkflowMetaPhase {
                    title: title.to_string(),
                    detail,
                });
            }
            phases
        }
        Some(_) => {
            return Err(meta_contract_error("`meta.phases` must be an array"));
        }
    };
    Ok(WorkflowMeta {
        name: name.to_string(),
        description: description.to_string(),
        when_to_use,
        phases,
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BASIC: &str = r#"export const meta = {
  name: 'find-bugs',
  description: "Find bugs and verify them",
  phases: [
    { title: 'Find', detail: 'parallel finders' },
    { title: 'Verify' },
  ],
}
const found = await agent("look");
return found;
"#;

    #[test]
    fn splits_basic_script() {
        let split = split_script(BASIC).expect("valid script");
        assert_eq!(split.meta.name, "find-bugs");
        assert_eq!(split.meta.description, "Find bugs and verify them");
        assert_eq!(split.meta.phases.len(), 2);
        assert_eq!(split.meta.phases[0].title, "Find");
        assert_eq!(
            split.meta.phases[0].detail.as_deref(),
            Some("parallel finders")
        );
        assert_eq!(split.meta.phases[1].detail, None);
        assert!(!split.body.contains("export const meta"));
        assert!(split.body.contains("await agent(\"look\")"));
    }

    #[test]
    fn allows_leading_comments_and_semicolon() {
        let src = "// header\n/* block */\nexport const meta = { name: 'x', description: 'y' };\nlog('hi');";
        let split = split_script(src).expect("valid");
        assert_eq!(split.meta.name, "x");
        assert!(split.body.contains("// header"));
        assert!(split.body.contains("log('hi');"));
        assert!(!split.body.contains("export"));
    }

    #[test]
    fn keeps_braces_inside_strings() {
        let src = "export const meta = { name: 'a}b', description: '{{nested}} \\' quote' }\nrest";
        let split = split_script(src).expect("valid");
        assert_eq!(split.meta.name, "a}b");
        assert_eq!(split.meta.description, "{{nested}} ' quote");
        assert_eq!(split.body.trim(), "rest");
    }

    #[test]
    fn rejects_missing_meta() {
        let err = split_script("const x = 1;").unwrap_err();
        assert!(err.contains("must begin with"));
    }

    #[test]
    fn rejects_computed_meta() {
        let err = split_script("export const meta = { name: NAME, description: 'd' }").unwrap_err();
        assert!(err.contains("pure literal"));
    }

    #[test]
    fn rejects_function_call_in_meta() {
        let err =
            split_script("export const meta = { name: makeName(), description: 'd' }").unwrap_err();
        assert!(err.contains("pure literal") || err.contains("expected"));
    }

    #[test]
    fn rejects_template_interpolation() {
        let err =
            split_script("export const meta = { name: `a${1}`, description: 'd' }").unwrap_err();
        assert!(err.contains("pure literal"));
    }

    #[test]
    fn rejects_spread() {
        let err = split_script("export const meta = { ...base, name: 'x', description: 'd' }")
            .unwrap_err();
        assert!(err.contains("meta"));
    }

    #[test]
    fn parses_numbers_booleans_null_and_nesting() {
        let value = parse_pure_literal(
            "{ a: -1.5e3, b: [true, false, null, 'x'], c: { d: 0 }, 'e-f': 2, }",
        )
        .expect("valid literal");
        assert_eq!(
            value,
            json!({"a": -1500.0, "b": [true, false, null, "x"], "c": {"d": 0}, "e-f": 2})
        );
    }

    #[test]
    fn requires_name_and_description() {
        let err = split_script("export const meta = { name: 'x' }\n").unwrap_err();
        assert!(err.contains("description"));
        let err = split_script("export const meta = { description: 'x' }\n").unwrap_err();
        assert!(err.contains("name"));
    }

    #[test]
    fn rejects_bad_phase_entries() {
        let err =
            split_script("export const meta = { name: 'x', description: 'd', phases: ['Scan'] }\n")
                .unwrap_err();
        assert!(err.contains("phases"));
    }

    #[test]
    fn meta_comments_are_allowed() {
        let src = "export const meta = {\n  // the name\n  name: 'x', /* desc */ description: 'd',\n}\nbody";
        let split = split_script(src).expect("valid");
        assert_eq!(split.meta.name, "x");
        assert_eq!(split.body.trim(), "body");
    }
}
