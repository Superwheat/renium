use anyhow::{Context, Result, bail};
use serde_json::Value;

#[derive(Clone, Copy, PartialEq)]
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

pub(super) fn has_jsonc_comments(text: &str) -> Result<bool> {
    Ok(tokenize(text)?
        .into_iter()
        .any(|token| matches!(token.kind, TokenKind::LineComment | TokenKind::BlockComment)))
}

pub(crate) fn parse_jsonc_value(text: &str) -> Result<Value> {
    let mut json = String::with_capacity(text.len());
    let mut tokens = tokenize(text)?
        .into_iter()
        .filter(|token| !matches!(token.kind, TokenKind::LineComment | TokenKind::BlockComment))
        .peekable();
    while let Some(token) = tokens.next() {
        if token.kind == TokenKind::Comma
            && tokens
                .peek()
                .is_some_and(|next| next.kind == TokenKind::Close)
        {
            continue;
        }
        json.push_str(&token.text);
    }
    serde_json::from_str(&json).context("Invalid JSON")
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
