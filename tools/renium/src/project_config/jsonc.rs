use anyhow::{Context, Result, bail};
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Open,
    Close,
    Comma,
    Colon,
    Value,
    LineComment,
    BlockComment,
}

struct Token {
    kind: TokenKind,
    text: String,
}

pub(super) fn format_jsonc(text: &str) -> Result<String> {
    let tokens = tokenize(text)?;
    let mut output = String::new();
    let mut depth = 0usize;
    let mut line_start = true;
    let mut previous = None;
    for (index, token) in tokens.iter().enumerate() {
        let next = tokens.get(index + 1).map(|token| token.kind);
        match token.kind {
            TokenKind::Open => {
                write_indent(&mut output, depth, &mut line_start);
                output.push_str(&token.text);
                depth += 1;
                if next != Some(TokenKind::Close) {
                    output.push('\n');
                    line_start = true;
                }
            }
            TokenKind::Close => {
                depth = depth.saturating_sub(1);
                if !line_start {
                    output.push('\n');
                    line_start = true;
                }
                write_indent(&mut output, depth, &mut line_start);
                output.push_str(&token.text);
            }
            TokenKind::Comma => {
                output.push(',');
                output.push('\n');
                line_start = true;
            }
            TokenKind::Colon => {
                output.push_str(": ");
                line_start = false;
            }
            TokenKind::Value => {
                write_indent(&mut output, depth, &mut line_start);
                if matches!(previous, Some(TokenKind::Value | TokenKind::BlockComment)) {
                    output.push(' ');
                }
                output.push_str(&token.text);
            }
            TokenKind::LineComment => {
                write_indent(&mut output, depth, &mut line_start);
                if !output.ends_with([' ', '\n']) {
                    output.push(' ');
                }
                output.push_str(token.text.trim_end());
                output.push('\n');
                line_start = true;
            }
            TokenKind::BlockComment => {
                write_indent(&mut output, depth, &mut line_start);
                if !output.ends_with([' ', '\n']) {
                    output.push(' ');
                }
                output.push_str(token.text.trim());
                if matches!(
                    next,
                    Some(
                        TokenKind::Value
                            | TokenKind::Open
                            | TokenKind::LineComment
                            | TokenKind::BlockComment
                    )
                ) {
                    output.push('\n');
                    line_start = true;
                }
            }
        }
        previous = Some(token.kind);
    }
    while output.ends_with([' ', '\t', '\r', '\n']) {
        output.pop();
    }
    output.push('\n');
    Ok(output)
}

pub(crate) fn parse_jsonc_value(text: &str) -> Result<Value> {
    let stripped = strip_comments(text)?;
    serde_json::from_str(&strip_trailing_commas(&stripped)).context("Invalid JSON")
}

fn write_indent(output: &mut String, depth: usize, line_start: &mut bool) {
    if *line_start {
        output.push_str(&"  ".repeat(depth));
        *line_start = false;
    }
}

fn tokenize(text: &str) -> Result<Vec<Token>> {
    let chars = text.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index].is_whitespace() {
            index += 1;
            continue;
        }
        let start = index;
        let kind = match chars[index] {
            '{' | '[' => {
                index += 1;
                TokenKind::Open
            }
            '}' | ']' => {
                index += 1;
                TokenKind::Close
            }
            ',' => {
                index += 1;
                TokenKind::Comma
            }
            ':' => {
                index += 1;
                TokenKind::Colon
            }
            '"' => {
                index += 1;
                let mut escaped = false;
                while index < chars.len() {
                    let character = chars[index];
                    index += 1;
                    if escaped {
                        escaped = false;
                    } else if character == '\\' {
                        escaped = true;
                    } else if character == '"' {
                        break;
                    }
                }
                if chars.get(index.saturating_sub(1)) != Some(&'"') {
                    bail!("Unterminated JSON string");
                }
                TokenKind::Value
            }
            '/' if chars.get(index + 1) == Some(&'/') => {
                index += 2;
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
                TokenKind::LineComment
            }
            '/' if chars.get(index + 1) == Some(&'*') => {
                index += 2;
                while index + 1 < chars.len() && !(chars[index] == '*' && chars[index + 1] == '/') {
                    index += 1;
                }
                if index + 1 >= chars.len() {
                    bail!("Unterminated JSON block comment");
                }
                index += 2;
                TokenKind::BlockComment
            }
            _ => {
                index += 1;
                while index < chars.len() {
                    let character = chars[index];
                    if character.is_whitespace()
                        || matches!(character, '{' | '}' | '[' | ']' | ',' | ':')
                        || (character == '/' && matches!(chars.get(index + 1), Some('/' | '*')))
                    {
                        break;
                    }
                    index += 1;
                }
                TokenKind::Value
            }
        };
        tokens.push(Token {
            kind,
            text: chars[start..index].iter().collect(),
        });
    }
    Ok(tokens)
}

fn strip_comments(text: &str) -> Result<String> {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let current = bytes[index];
        if in_string {
            output.push(current);
            if escaped {
                escaped = false;
            } else if current == b'\\' {
                escaped = true;
            } else if current == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if current == b'"' {
            in_string = true;
            output.push(current);
            index += 1;
            continue;
        }
        if current == b'/' && bytes.get(index + 1) == Some(&b'/') {
            output.extend_from_slice(b"  ");
            index += 2;
            while index < bytes.len() && !matches!(bytes[index], b'\r' | b'\n') {
                output.push(b' ');
                index += 1;
            }
            continue;
        }
        if current == b'/' && bytes.get(index + 1) == Some(&b'*') {
            output.extend_from_slice(b"  ");
            index += 2;
            let mut closed = false;
            while index < bytes.len() {
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    output.extend_from_slice(b"  ");
                    index += 2;
                    closed = true;
                    break;
                }
                output.push(if matches!(bytes[index], b'\r' | b'\n') {
                    bytes[index]
                } else {
                    b' '
                });
                index += 1;
            }
            if !closed {
                bail!("Unterminated block comment");
            }
            continue;
        }
        output.push(current);
        index += 1;
    }
    if in_string {
        bail!("Unterminated JSON string");
    }
    String::from_utf8(output).context("JSONC is not UTF-8")
}

fn strip_trailing_commas(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let current = bytes[index];
        if in_string {
            output.push(current);
            if escaped {
                escaped = false;
            } else if current == b'\\' {
                escaped = true;
            } else if current == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if current == b'"' {
            in_string = true;
            output.push(current);
            index += 1;
            continue;
        }
        if current == b',' {
            let mut next = index + 1;
            while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                next += 1;
            }
            if next < bytes.len() && matches!(bytes[next], b'}' | b']') {
                output.push(b' ');
                index += 1;
                continue;
            }
        }
        output.push(current);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_else(|_| text.to_string())
}
