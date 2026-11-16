//! Which part of a wait's condition kept it from holding.
use full_moon::ast::{BinOp, Expression, LastStmt};
use serde_json::Value;

/// A condition taken apart for one look after its wait ran out: the
/// statements ahead of the returned expression, and that expression's `and`
/// operands in order, each with the left side of its comparison when it is
/// one.
pub(super) struct ConditionParts {
    prefix: String,
    parts: Vec<(String, Option<String>)>,
}

fn unwrapped(mut expression: &Expression) -> &Expression {
    while let Expression::Parentheses {
        expression: inner, ..
    } = expression
    {
        expression = inner;
    }
    expression
}

fn operands<'a>(expression: &'a Expression, found: &mut Vec<&'a Expression>) {
    match unwrapped(expression) {
        Expression::BinaryOperator {
            lhs,
            binop: BinOp::And(_),
            rhs,
        } => {
            operands(lhs, found);
            operands(rhs, found);
        }
        operand => found.push(operand),
    }
}

fn compared_side(operand: &Expression) -> Option<String> {
    let Expression::BinaryOperator { lhs, binop, .. } = operand else {
        return None;
    };
    matches!(
        binop,
        BinOp::TwoEqual(_)
            | BinOp::TildeEqual(_)
            | BinOp::LessThan(_)
            | BinOp::LessThanEqual(_)
            | BinOp::GreaterThan(_)
            | BinOp::GreaterThanEqual(_)
    )
    .then(|| lhs.to_string().trim().to_string())
}

impl ConditionParts {
    /// The parts of a wait's probe body, `return (EXPRESSION)` or a chunk that
    /// returns one. A condition that is one plain value has nothing to take
    /// apart.
    pub(super) fn of(body: &str) -> Option<Self> {
        let parsed = full_moon::parse_fallible(body, full_moon::LuaVersion::luau());
        if !parsed.errors().is_empty() {
            return None;
        }
        let ast = parsed.into_ast();
        let block = ast.nodes();
        let Some(LastStmt::Return(returned)) = block.last_stmt() else {
            return None;
        };
        let mut found = Vec::new();
        operands(returned.returns().iter().next()?, &mut found);
        let parts = found
            .into_iter()
            .map(|operand| {
                (
                    operand.to_string().trim().to_string(),
                    compared_side(operand),
                )
            })
            .collect::<Vec<_>>();
        if parts.len() < 2 && parts.iter().all(|(_, side)| side.is_none()) {
            return None;
        }
        let prefix = block
            .stmts()
            .map(|statement| statement.to_string().trim().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        Some(Self { prefix, parts })
    }

    /// Luau that evaluates the operands in order and returns `true`, the
    /// number of the first that does not hold (0 when all do), its value and
    /// the value of its compared side.
    pub(super) fn probe(&self) -> String {
        let mut code = format!(
            "{}\nlocal function __read(read)\n\tlocal ok, value = pcall(read)\n\tif not ok then return false, 'error: ' .. tostring(value) end\n\treturn value, tostring(value)\nend\n",
            self.prefix
        );
        for (index, (part, side)) in self.parts.iter().enumerate() {
            let side = match side {
                Some(side) => format!("select(2, __read(function() return ({side}\n) end))"),
                None => "nil".to_string(),
            };
            code.push_str(&format!(
                "do\n\tlocal holds, text = __read(function() return ({part}\n) end)\n\tif not holds then return true, {}, text, {side} end\nend\n",
                index + 1
            ));
        }
        code.push_str("return true, 0");
        code
    }

    /// What the probe's results say in words: the operand that does not hold
    /// and the value that makes it so.
    pub(super) fn stopped_at(&self, results: &[Value]) -> Option<String> {
        const SHOWN: usize = 120;
        let shown = |value: Option<&Value>| {
            let text = value.and_then(Value::as_str)?;
            Some(match text.char_indices().nth(SHOWN) {
                Some((end, _)) => format!("{}...", &text[..end]),
                None => text.to_string(),
            })
        };
        let number = results.get(1).and_then(Value::as_u64)? as usize;
        let (part, side) = self.parts.get(number.checked_sub(1)?)?;
        let value = shown(results.get(2))?;
        Some(match (side, shown(results.get(3))) {
            (Some(side), Some(side_value)) if !value.starts_with("error: ") => {
                format!("`{part}` does not hold: `{side}` is {side_value}")
            }
            _ => format!("`{part}` is {value}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_condition_is_taken_apart_at_its_ands_and_comparisons() {
        let parts = ConditionParts::of(
            "return (shared.Round ~= nil and shared.Round.Map and shared.Round.Map.Name == 'Skid Pad'\n)",
        )
        .unwrap();
        assert_eq!(
            parts.parts,
            [
                (
                    "shared.Round ~= nil".to_string(),
                    Some("shared.Round".to_string())
                ),
                ("shared.Round.Map".to_string(), None),
                (
                    "shared.Round.Map.Name == 'Skid Pad'".to_string(),
                    Some("shared.Round.Map.Name".to_string())
                ),
            ]
        );
        assert_eq!(parts.prefix, "");
        assert_eq!(
            parts
                .stopped_at(&[json!(true), json!(3), json!("false"), json!("Baseplate")])
                .unwrap(),
            "`shared.Round.Map.Name == 'Skid Pad'` does not hold: `shared.Round.Map.Name` is Baseplate"
        );
        assert_eq!(
            parts
                .stopped_at(&[json!(true), json!(2), json!("nil"), Value::Null])
                .unwrap(),
            "`shared.Round.Map` is nil"
        );
        assert_eq!(
            parts
                .stopped_at(&[
                    json!(true),
                    json!(1),
                    json!("error: attempt to index nil"),
                    json!("error: x")
                ])
                .unwrap(),
            "`shared.Round ~= nil` is error: attempt to index nil"
        );
        assert_eq!(parts.stopped_at(&[json!(true), json!(0)]), None);
        assert!(parts.probe().ends_with("return true, 0"));
    }

    #[test]
    fn a_chunk_keeps_its_statements_and_one_plain_value_has_no_parts() {
        let chunk = ConditionParts::of(
            "local h = shared.RoundHandler\nreturn h.GameState == 'InRound' and (h.Map ~= nil)",
        )
        .unwrap();
        assert_eq!(chunk.prefix, "local h = shared.RoundHandler");
        assert_eq!(chunk.parts.len(), 2);
        assert_eq!(
            chunk.parts[1],
            ("h.Map ~= nil".to_string(), Some("h.Map".to_string()))
        );
        assert!(
            chunk
                .probe()
                .starts_with("local h = shared.RoundHandler\nlocal function __read")
        );
        assert!(ConditionParts::of("return (workspace.Ready\n)").is_none());
        assert!(ConditionParts::of("return (panel.Visible or other.Visible\n)").is_none());
        assert!(ConditionParts::of("return (workspace.Count > 3\n)").is_some());
        assert!(ConditionParts::of("return (").is_none());
    }
}
