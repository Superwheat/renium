use std::collections::HashMap;

use full_moon::ast;
use full_moon::ast::punctuated::Punctuated;
use full_moon::node::Node;
use full_moon::tokenizer::TokenReference;
use full_moon::visitors::Visitor;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LuauFunction {
    pub(crate) name: String,
    pub(crate) parameters: Vec<String>,
    pub(crate) first_line: usize,
    pub(crate) last_line: usize,
    pub(crate) depth: usize,
}

impl LuauFunction {
    /// Whether `wanted` is the printed name or its last segment.
    pub(crate) fn is_named(&self, wanted: &str) -> bool {
        self.name == wanted || self.last_segment() == wanted
    }

    /// The printed name after its last `.` or `:`, `b` of `a.b` and of `a:b`.
    pub(crate) fn last_segment(&self) -> &str {
        self.name.rsplit(['.', ':']).next().unwrap_or(&self.name)
    }

    /// Whether the function's lines include `line`.
    pub(crate) fn holds(&self, line: usize) -> bool {
        (self.first_line..=self.last_line).contains(&line)
    }

    /// The number of lines from the function's first through its last.
    pub(crate) fn line_count(&self) -> usize {
        self.last_line - self.first_line + 1
    }
}

struct Outline<'a> {
    source: &'a str,
    assigned: HashMap<usize, (String, usize, usize)>,
    found: Vec<(usize, usize, LuauFunction)>,
}

fn text(token: &TokenReference) -> String {
    token.token().to_string()
}

fn line(token: &TokenReference) -> usize {
    token.token().start_position().line()
}

impl Outline<'_> {
    /// A node as it is written in the source, from its first token through
    /// its last, each run of whitespace as one space.
    fn written(&self, node: &impl Node) -> String {
        let (start, end) = node.tokens().fold((usize::MAX, 0), |(start, end), token| {
            (
                start.min(token.token().start_position().bytes()),
                end.max(token.token().end_position().bytes()),
            )
        });
        self.source
            .get(start..end)
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Lists one function: `first_line` is where its statement starts, and
    /// the bytes from its `function` keyword through its `end` nest it among
    /// the others.
    fn add(
        &mut self,
        name: String,
        first_line: usize,
        function_token: &TokenReference,
        body: &ast::FunctionBody,
    ) {
        let parameters = body
            .parameters()
            .iter()
            .filter_map(|parameter| match parameter {
                ast::Parameter::Name(name) => Some(text(name)),
                ast::Parameter::Ellipsis(_) => Some("...".to_string()),
                _ => None,
            })
            .collect();
        let end = body.end_token().token().end_position();
        self.found.push((
            function_token.token().start_position().bytes(),
            end.bytes(),
            LuauFunction {
                name,
                parameters,
                first_line,
                last_line: end.line(),
                depth: 0,
            },
        ));
    }

    /// Remembers an anonymous function by its `function` keyword, so that the
    /// visit of the function itself lists it under `name`, from `first_line`
    /// or else from the line of that keyword.
    fn name_function(&mut self, value: &ast::Expression, name: String, first_line: Option<usize>) {
        // A function in parentheses or under a type assertion is still the
        // assigned function, and its lines run to the end of that expression.
        let last_line = value
            .tokens()
            .map(|token| token.token().end_position().line())
            .max()
            .unwrap_or(0);
        let mut value = value;
        while let ast::Expression::Parentheses { expression, .. }
        | ast::Expression::TypeAssertion { expression, .. } = value
        {
            value = expression;
        }
        if let ast::Expression::Function(function) = value {
            let start = function.function_token().token().start_position();
            self.assigned.insert(
                start.bytes(),
                (name, first_line.unwrap_or(start.line()), last_line),
            );
        }
    }

    /// Names the anonymous functions among `values` after the target at the
    /// same position of an assignment. The first starts on the statement's
    /// first line, a later one on its own `function` line.
    fn name_values(
        &mut self,
        names: impl Iterator<Item = String>,
        values: &Punctuated<ast::Expression>,
        first_line: usize,
    ) {
        for (index, (name, value)) in names.zip(values.iter()).enumerate() {
            self.name_function(value, name, (index == 0).then_some(first_line));
        }
    }
}

impl Visitor for Outline<'_> {
    fn visit_function_declaration(&mut self, node: &ast::FunctionDeclaration) {
        let mut name = node
            .name()
            .names()
            .iter()
            .map(text)
            .collect::<Vec<_>>()
            .join(".");
        if let Some(method) = node.name().method_name() {
            name.push(':');
            name.push_str(&text(method));
        }
        let first = node
            .attributes()
            .next()
            .map_or(node.function_token(), |attribute| attribute.at_sign());
        self.add(name, line(first), node.function_token(), node.body());
    }

    fn visit_local_function(&mut self, node: &ast::LocalFunction) {
        let first = node
            .attributes()
            .next()
            .map_or(node.local_token(), |attribute| attribute.at_sign());
        self.add(
            text(node.name()),
            line(first),
            node.function_token(),
            node.body(),
        );
    }

    fn visit_const_function(&mut self, node: &ast::luau::ConstFunction) {
        let first = node
            .attributes()
            .next()
            .map_or(node.const_token(), |attribute| attribute.at_sign());
        self.add(
            text(node.name()),
            line(first),
            node.function_token(),
            node.body(),
        );
    }

    fn visit_local_assignment(&mut self, node: &ast::LocalAssignment) {
        self.name_values(
            node.names().iter().map(text),
            node.expressions(),
            line(node.local_token()),
        );
    }

    fn visit_const_assignment(&mut self, node: &ast::luau::ConstAssignment) {
        self.name_values(
            node.names().iter().map(text),
            node.expressions(),
            line(node.const_token()),
        );
    }

    fn visit_assignment(&mut self, node: &ast::Assignment) {
        let Some(start) = node.start_position() else {
            return;
        };
        let names = node
            .variables()
            .iter()
            .map(|variable| self.written(variable))
            .collect::<Vec<_>>();
        self.name_values(names.into_iter(), node.expressions(), start.line());
    }

    fn visit_field(&mut self, node: &ast::Field) {
        match node {
            ast::Field::NameKey { key, value, .. } => {
                self.name_function(value, text(key), Some(line(key)));
            }
            ast::Field::ExpressionKey {
                brackets,
                key,
                value,
                ..
            } => {
                let name = format!("[{}]", self.written(key));
                self.name_function(value, name, Some(line(brackets.tokens().0)));
            }
            _ => {}
        }
    }

    fn visit_anonymous_function(&mut self, node: &ast::AnonymousFunction) {
        let start = node.function_token().token().start_position().bytes();
        if let Some((name, first_line, last_line)) = self.assigned.remove(&start) {
            self.add(name, first_line, node.function_token(), node.body());
            if let Some((_, _, function)) = self.found.last_mut() {
                function.last_line = function.last_line.max(last_line);
            }
        }
    }
}

/// A compile error as `line N: message`; a syntax error's own text starts at
/// `N:`.
fn compiler_error(error: mlua::Error) -> String {
    match error {
        mlua::Error::SyntaxError { message, .. } => format!("line {message}"),
        other => other.to_string(),
    }
}

/// Parses a script and lists its functions in source order, each with the
/// number of listed functions around it as its depth.
fn parse_functions(source: &str) -> Result<Vec<LuauFunction>, String> {
    let parsed = full_moon::parse_fallible(source, full_moon::LuaVersion::luau());
    if let Some(error) = parsed.errors().first() {
        return Err(format!(
            "line {}: {}",
            error.range().0.line(),
            error.error_message()
        ));
    }
    let mut outline = Outline {
        source,
        assigned: HashMap::new(),
        found: Vec::new(),
    };
    outline.visit_ast(parsed.ast());
    outline.found.sort_by_key(|(start, _, _)| *start);
    let mut open: Vec<usize> = Vec::new();
    Ok(outline
        .found
        .into_iter()
        .map(|(start, end, mut function)| {
            while open.last().is_some_and(|outer| *outer < start) {
                open.pop();
            }
            function.depth = open.len();
            open.push(end);
            function
        })
        .collect())
}

/// The named functions of a Luau script in source order: every `function`
/// declaration (`function a.b.c()`, `function a:b()`, `local function f()`)
/// and every anonymous function assigned to a name or a table field
/// (`local f = function()`, `t.f = function()`, `{ f = function() }`), under
/// the name it is written with. Lines are 1-based and `depth` counts the
/// listed functions around one. A script that does not parse gives the first
/// error as `line N: message`. The compiler reads the script first: its
/// recursion limit turns excessive nesting away before the recursive parser,
/// which runs on its own deep stack, meets it.
pub(crate) fn luau_functions(source: &str) -> Result<Vec<LuauFunction>, String> {
    mlua::Compiler::new()
        .compile(source)
        .map_err(compiler_error)?;
    crate::studio::automation::on_luau_parser_stack(|| parse_functions(source))
        .map_err(|error| error.to_string())?
}

#[cfg(test)]
mod tests {
    use super::luau_functions;

    fn outline(source: &str) -> Vec<String> {
        luau_functions(source)
            .unwrap()
            .into_iter()
            .map(|function| {
                format!(
                    "{}{}-{} {}({})",
                    " ".repeat(function.depth),
                    function.first_line,
                    function.last_line,
                    function.name,
                    function.parameters.join(", ")
                )
            })
            .collect()
    }

    #[test]
    fn every_way_to_name_a_function_is_listed_with_its_lines() {
        let source = "\
local Shop = {}
function Shop.Items.buy(player: Player, id: string): boolean
\treturn true
end
function Shop:Refund(player, ...)
end
local function round(value: number)
\treturn math.floor(value + 0.5)
end
local clamp = function(value, low, high)
\treturn math.clamp(value, low, high)
end
Shop.Sell = function(player)
end
Shop.handlers[\"open\"] = function() end
local wrapped = (
\tfunction(value)
\t\treturn value
\tend
)
local asserted = function()
\treturn \"\"
end :: (
\t() -> string
)
return Shop
";
        assert_eq!(
            outline(source),
            [
                "2-4 Shop.Items.buy(player, id)",
                "5-6 Shop:Refund(player, ...)",
                "7-9 round(value)",
                "10-12 clamp(value, low, high)",
                "13-14 Shop.Sell(player)",
                "15-15 Shop.handlers[\"open\"]()",
                "16-20 wrapped(value)",
                "21-25 asserted()",
            ]
        );
    }

    #[test]
    fn nested_and_table_field_functions_are_listed_and_callbacks_are_not() {
        let source = "\
local function outer(player)
\tlocal function inner()
\t\tlocal deepest = function() end
\tend
\ttask.spawn(function()
\t\tlocal function spawned() end
\tend)
end
local Handlers = {
\tOpen = function(player)
\tend,
\t[\"Close\"] = function() end,
\t[Enum.KeyCode.E] = function() end,
}
Players.PlayerAdded:Connect(function(player)
\tprint(player)
end)
";
        assert_eq!(
            outline(source),
            [
                "1-8 outer(player)",
                " 2-4 inner()",
                "  3-3 deepest()",
                " 6-6 spawned()",
                "10-11 Open(player)",
                "12-12 [\"Close\"]()",
                "13-13 [Enum.KeyCode.E]()",
            ]
        );
    }

    #[test]
    fn lines_follow_multi_line_strings_comments_and_several_assignments() {
        let source = "\
--[[ a comment
over two lines ]]
local text = [[
line
]]
local a, b = function() end,
\tfunction()
\tend
";
        assert_eq!(outline(source), ["6-6 a()", "7-8 b()"]);
    }

    #[test]
    fn a_name_matches_whole_or_by_its_last_segment() {
        let functions =
            luau_functions("function A.b:c() end\nlocal function d() end\nfunction c() end")
                .unwrap();
        let named = |wanted: &str| {
            functions
                .iter()
                .filter(|function| function.is_named(wanted))
                .map(|function| function.name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(named("c"), ["A.b:c", "c"]);
        assert_eq!(named("A.b:c"), ["A.b:c"]);
        assert_eq!(named("d"), ["d"]);
        assert!(named("b").is_empty());
        assert!(named("b:c").is_empty());
    }

    #[test]
    fn a_script_that_does_not_parse_gives_the_first_error_and_its_line() {
        let error = luau_functions("local function f()\n\treturn 1\nend\nlocal x = )\nreturn x\n")
            .unwrap_err();
        assert!(error.starts_with("line 4: "), "{error}");
        assert!(!error.contains('\n'), "{error}");
        let nested = format!("{}return 1{}", "do ".repeat(2000), " end".repeat(2000));
        assert!(luau_functions(&nested).is_err());
    }
}
