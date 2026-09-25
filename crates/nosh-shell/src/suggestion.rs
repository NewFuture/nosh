//! Non-executing suggestion checks. Unknown dynamic behavior is not a rejection.

use std::{collections::BTreeMap, rc::Rc};

use brush_parser::{ast, word};

use crate::{
    EmbeddedShell, Resolution,
    trigger::{static_word, static_word_with_tilde},
};

#[derive(Clone, Default)]
struct Scope {
    // None means a definition depends on runtime control flow.
    functions: BTreeMap<String, Option<Rc<ast::FunctionBody>>>,
    dynamic: bool,
}

impl Scope {
    fn merge_optional(&mut self, branch: Self) {
        for (name, body) in &mut self.functions {
            if !branch.functions.contains_key(name) {
                *body = None;
            }
        }
        for (name, body) in branch.functions {
            if !matches!((self.functions.get(&name), &body),
                (Some(Some(a)), Some(b)) if Rc::ptr_eq(a, b))
            {
                self.functions.insert(name, None);
            }
        }
        self.dynamic |= branch.dynamic;
    }
}

pub(crate) fn validate(text: &str, shell: &EmbeddedShell) -> bool {
    let Ok(program) = shell.parse(text) else {
        return false;
    };
    if program
        .complete_commands
        .iter()
        .map(|c| c.0.len())
        .sum::<usize>()
        != 1
    {
        return false;
    }
    Check {
        shell,
        calls_left: 128,
    }
    .program(&program, &mut Scope::default())
}

struct Check<'a> {
    shell: &'a EmbeddedShell,
    calls_left: usize,
}

impl Check<'_> {
    fn program(&mut self, program: &ast::Program, scope: &mut Scope) -> bool {
        let inherited = scope.clone();
        program
            .complete_commands
            .iter()
            .all(|c| self.list(c, scope))
            && self.declarations(&inherited, scope)
    }

    fn function(&mut self, body: &ast::FunctionBody, scope: &mut Scope) -> bool {
        if self.calls_left == 0 {
            scope.dynamic = true;
            return true;
        }
        self.calls_left -= 1;
        self.redirects(&body.1, scope) && self.compound(&body.0, scope)
    }

    fn list(&mut self, list: &ast::CompoundList, scope: &mut Scope) -> bool {
        list.0
            .iter()
            .all(|ast::CompoundListItem(and_or, separator)| {
                if matches!(separator, ast::SeparatorOperator::Async) {
                    self.isolated(scope, |check, child| check.and_or(and_or, child))
                } else {
                    self.and_or(and_or, scope)
                }
            })
    }

    fn isolated(
        &mut self,
        scope: &Scope,
        visit: impl FnOnce(&mut Self, &mut Scope) -> bool,
    ) -> bool {
        let mut child = scope.clone();
        visit(self, &mut child) && self.declarations(scope, &child)
    }

    fn declarations(&mut self, inherited: &Scope, scope: &Scope) -> bool {
        // Delay uncalled bodies until the enclosing shell scope is complete.
        // Actual invocations use their execution-order scope instead.
        scope
            .functions
            .iter()
            .filter_map(|(name, body)| {
                let body = body.as_ref()?;
                if matches!(inherited.functions.get(name), Some(Some(old)) if Rc::ptr_eq(old, body))
                {
                    None
                } else {
                    Some(body.clone())
                }
            })
            .collect::<Vec<_>>()
            .iter()
            .all(|body| self.isolated(scope, |check, child| check.function(body, child)))
    }

    fn and_or(&mut self, list: &ast::AndOrList, scope: &mut Scope) -> bool {
        if !self.pipeline(&list.first, scope) {
            return false;
        }
        for next in &list.additional {
            let (ast::AndOr::And(p) | ast::AndOr::Or(p)) = next;
            let mut branch = scope.clone();
            if !self.pipeline(p, &mut branch) {
                return false;
            }
            scope.merge_optional(branch);
        }
        true
    }

    fn pipeline(&mut self, pipeline: &ast::Pipeline, scope: &mut Scope) -> bool {
        if pipeline.seq.len() == 1 {
            return self.command(&pipeline.seq[0], scope);
        }
        for (index, command) in pipeline.seq.iter().enumerate() {
            let mut child = scope.clone();
            if !self.command(command, &mut child) || !self.declarations(scope, &child) {
                return false;
            }
            // Bash's lastpipe option can retain the final command's definitions.
            if index + 1 == pipeline.seq.len() {
                scope.merge_optional(child);
            }
        }
        true
    }

    fn command(&mut self, command: &ast::Command, scope: &mut Scope) -> bool {
        match command {
            ast::Command::Simple(simple) => {
                if !simple
                    .prefix
                    .iter()
                    .flat_map(|p| &p.0)
                    .chain(simple.suffix.iter().flat_map(|s| &s.0))
                    .all(|item| self.item(item, scope))
                {
                    return false;
                }
                let Some(word) = &simple.word_or_name else {
                    return true;
                };
                if !self.word(word, scope) {
                    return false;
                }
                let Some(name) = static_word_with_tilde(word, &|expr| {
                    if scope.dynamic {
                        return None;
                    }
                    match expr {
                        word::TildeExpr::Home => self.shell.var("HOME"),
                        word::TildeExpr::WorkingDir => self.shell.cwd().to_str().map(str::to_owned),
                        word::TildeExpr::OldWorkingDir => self.shell.var("OLDPWD"),
                        _ => None,
                    }
                }) else {
                    scope.dynamic = true;
                    scope.functions.clear();
                    return true;
                };
                if let Some(body) = scope.functions.get(&name).cloned() {
                    if let Some(body) = body {
                        return self.function(&body, scope);
                    }
                    scope.dynamic = true;
                    scope.functions.clear();
                    return true;
                }
                let resolution = self.shell.resolve(&name);
                if !scope.dynamic && resolution == Resolution::NotFound {
                    return false;
                }
                if matches!(
                    name.as_str(),
                    "." | "source"
                        | "eval"
                        | "unset"
                        | "cd"
                        | "pushd"
                        | "popd"
                        | "command"
                        | "builtin"
                ) || matches!(resolution, Resolution::Alias(_) | Resolution::Function)
                {
                    // These can change definitions/resolution beyond static facts.
                    scope.dynamic = true;
                    scope.functions.clear();
                }
                true
            }
            ast::Command::Function(function) => {
                if let Some(name) = static_word(&function.fname) {
                    scope
                        .functions
                        .insert(name, Some(Rc::new(function.body.clone())));
                } else {
                    scope.dynamic = true;
                }
                true
            }
            ast::Command::Compound(compound, redirects) => {
                self.redirects(redirects, scope) && self.compound(compound, scope)
            }
            ast::Command::ExtendedTest(test, redirects) => {
                self.redirects(redirects, scope) && self.test(&test.expr, scope)
            }
        }
    }

    fn item(&mut self, item: &ast::CommandPrefixOrSuffixItem, scope: &mut Scope) -> bool {
        match item {
            ast::CommandPrefixOrSuffixItem::Word(w) => self.word(w, scope),
            ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, _) => {
                if let ast::AssignmentName::ArrayElementName(_, index) = &assignment.name
                    && !self.words(index, scope)
                {
                    return false;
                }
                let valid = match &assignment.value {
                    ast::AssignmentValue::Scalar(w) => self.word(w, scope),
                    ast::AssignmentValue::Array(items) => items.iter().all(|(key, value)| {
                        key.as_ref().is_none_or(|w| self.word(w, scope)) && self.word(value, scope)
                    }),
                };
                // Later lookups must not use stale PATH or tilde expansion state.
                if matches!(&assignment.name, ast::AssignmentName::VariableName(name)
                    if matches!(name.as_str(), "PATH" | "HOME" | "OLDPWD"))
                {
                    scope.dynamic = true;
                }
                valid
            }
            ast::CommandPrefixOrSuffixItem::IoRedirect(r) => self.redirect(r, scope),
            ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, child) => {
                self.isolated(scope, |check, scope| check.list(&child.list, scope))
            }
        }
    }

    fn redirects(&mut self, redirects: &Option<ast::RedirectList>, scope: &mut Scope) -> bool {
        redirects
            .iter()
            .flat_map(|r| &r.0)
            .all(|r| self.redirect(r, scope))
    }

    fn redirect(&mut self, redirect: &ast::IoRedirect, scope: &mut Scope) -> bool {
        use ast::{IoFileRedirectTarget as T, IoRedirect as R};
        match redirect {
            R::File(_, _, T::Filename(w) | T::Duplicate(w))
            | R::HereString(_, w)
            | R::OutputAndError(w, _) => self.word(w, scope),
            R::File(_, _, T::ProcessSubstitution(_, child)) => {
                self.isolated(scope, |check, scope| check.list(&child.list, scope))
            }
            R::File(_, _, T::Fd(_)) => true,
            R::HereDocument(_, doc) => {
                !doc.requires_expansion
                    || word::parse_heredoc(&doc.doc.value, &brush_parser::ParserOptions::default())
                        .is_ok_and(|pieces| self.pieces(&pieces, scope))
            }
        }
    }

    fn compound(&mut self, command: &ast::CompoundCommand, scope: &mut Scope) -> bool {
        use ast::CompoundCommand as C;
        match command {
            C::BraceGroup(group) => self.list(&group.list, scope),
            C::Subshell(child) => {
                self.isolated(scope, |check, scope| check.list(&child.list, scope))
            }
            C::Coprocess(child) => {
                self.isolated(scope, |check, scope| check.command(&child.body, scope))
            }
            C::Arithmetic(expr) => self.words(&expr.expr.value, scope),
            C::ForClause(loop_) => {
                if !loop_.values.iter().flatten().all(|w| self.word(w, scope)) {
                    return false;
                }
                self.optional(&loop_.body.list, scope)
            }
            C::ArithmeticForClause(loop_) => {
                [&loop_.initializer, &loop_.condition, &loop_.updater]
                    .into_iter()
                    .flatten()
                    .all(|e| self.words(&e.value, scope))
                    && self.optional(&loop_.body.list, scope)
            }
            C::WhileClause(loop_) | C::UntilClause(loop_) => {
                self.list(&loop_.0, scope) && self.optional(&loop_.1.list, scope)
            }
            C::IfClause(branch) => {
                if !self.list(&branch.condition, scope) || !self.optional(&branch.then, scope) {
                    return false;
                }
                branch.elses.iter().flatten().all(|branch| {
                    let mut child = scope.clone();
                    let valid = branch
                        .condition
                        .as_ref()
                        .is_none_or(|c| self.list(c, &mut child))
                        && self.list(&branch.body, &mut child);
                    scope.merge_optional(child);
                    valid
                })
            }
            C::CaseClause(case) => {
                self.word(&case.value, scope)
                    && case.cases.iter().all(|branch| {
                        branch.patterns.iter().all(|p| self.word(p, scope))
                            && branch.cmd.as_ref().is_none_or(|c| self.optional(c, scope))
                    })
            }
        }
    }

    fn optional(&mut self, list: &ast::CompoundList, scope: &mut Scope) -> bool {
        let mut child = scope.clone();
        let valid = self.list(list, &mut child);
        scope.merge_optional(child);
        valid
    }

    fn test(&mut self, test: &ast::ExtendedTestExpr, scope: &mut Scope) -> bool {
        use ast::ExtendedTestExpr as T;
        match test {
            T::And(a, b) | T::Or(a, b) => self.test(a, scope) && self.test(b, scope),
            T::Not(a) | T::Parenthesized(a) => self.test(a, scope),
            T::UnaryTest(_, w) => self.word(w, scope),
            T::BinaryTest(_, a, b) => self.word(a, scope) && self.word(b, scope),
        }
    }

    fn word(&mut self, word: &ast::Word, scope: &Scope) -> bool {
        self.words(&word.value, scope)
    }

    fn words(&mut self, text: &str, scope: &Scope) -> bool {
        let Ok(pieces) = word::parse(text, &brush_parser::ParserOptions::default()) else {
            return false;
        };
        self.pieces(&pieces, scope)
    }

    fn pieces(&mut self, pieces: &[word::WordPieceWithSource], scope: &Scope) -> bool {
        use word::WordPiece as W;
        pieces.iter().all(|piece| match &piece.piece {
            W::CommandSubstitution(text) | W::BackquotedCommandSubstitution(text) => self
                .shell
                .parse(text)
                .is_ok_and(|p| self.program(&p, &mut scope.clone())),
            W::DoubleQuotedSequence(p) | W::GettextDoubleQuotedSequence(p) => self.pieces(p, scope),
            W::ArithmeticExpression(e) => self.words(&e.value, scope),
            W::ParameterExpansion(p) => self.parameter(p, scope),
            _ => true,
        })
    }

    fn parameter(&mut self, parameter: &word::ParameterExpr, scope: &Scope) -> bool {
        use word::ParameterExpr as P;
        let indexed = match parameter {
            P::Parameter { parameter, .. }
            | P::UseDefaultValues { parameter, .. }
            | P::AssignDefaultValues { parameter, .. }
            | P::IndicateErrorIfNullOrUnset { parameter, .. }
            | P::UseAlternativeValue { parameter, .. }
            | P::ParameterLength { parameter, .. }
            | P::RemoveSmallestSuffixPattern { parameter, .. }
            | P::RemoveLargestSuffixPattern { parameter, .. }
            | P::RemoveSmallestPrefixPattern { parameter, .. }
            | P::RemoveLargestPrefixPattern { parameter, .. }
            | P::Substring { parameter, .. }
            | P::Transform { parameter, .. }
            | P::UppercaseFirstChar { parameter, .. }
            | P::UppercasePattern { parameter, .. }
            | P::LowercaseFirstChar { parameter, .. }
            | P::LowercasePattern { parameter, .. }
            | P::ReplaceSubstring { parameter, .. } => parameter,
            P::VariableNames { .. } | P::MemberKeys { .. } => return true,
        };
        if let word::Parameter::NamedWithIndex { index, .. } = indexed
            && !self.words(index, scope)
        {
            return false;
        }
        match parameter {
            P::UseDefaultValues {
                default_value: value,
                ..
            }
            | P::AssignDefaultValues {
                default_value: value,
                ..
            }
            | P::IndicateErrorIfNullOrUnset {
                error_message: value,
                ..
            }
            | P::UseAlternativeValue {
                alternative_value: value,
                ..
            }
            | P::RemoveSmallestSuffixPattern { pattern: value, .. }
            | P::RemoveLargestSuffixPattern { pattern: value, .. }
            | P::RemoveSmallestPrefixPattern { pattern: value, .. }
            | P::RemoveLargestPrefixPattern { pattern: value, .. }
            | P::UppercaseFirstChar { pattern: value, .. }
            | P::UppercasePattern { pattern: value, .. }
            | P::LowercaseFirstChar { pattern: value, .. }
            | P::LowercasePattern { pattern: value, .. } => {
                value.as_ref().is_none_or(|v| self.words(v, scope))
            }
            P::Substring { offset, length, .. } => {
                self.words(&offset.value, scope)
                    && length.as_ref().is_none_or(|v| self.words(&v.value, scope))
            }
            P::ReplaceSubstring {
                pattern,
                replacement,
                ..
            } => {
                self.words(pattern, scope)
                    && replacement.as_ref().is_none_or(|v| self.words(v, scope))
            }
            _ => true,
        }
    }
}
