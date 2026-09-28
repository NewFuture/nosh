//! Static facts shared by input diagnostics and submission routing. No I/O or expansion.

use std::collections::BTreeSet;

use brush_parser::{ast, word};

#[derive(Clone, Default)]
pub(crate) struct Scope {
    pub functions: BTreeSet<String>,
    pub dynamic: bool,
    pub correction_blocked: bool,
    pub files_changed: bool,
    pub path: Option<String>,
}

impl Scope {
    pub fn for_loop(&mut self, command: &ast::ForClauseCommand) {
        self.dynamic |= matches!(command.variable_name.as_str(), "PATH" | "HOME" | "OLDPWD");
        let writes = command.values.iter().flatten().any(word_may_write);
        self.files_changed |= writes;
        self.correction_blocked |= writes;
    }

    pub fn merge_optional(&mut self, branch: &Self) {
        self.dynamic |=
            branch.dynamic || branch.functions != self.functions || branch.path != self.path;
        self.correction_blocked |= branch.correction_blocked;
        self.files_changed |= branch.files_changed;
    }

    pub fn assignment(&mut self, assignment: &ast::Assignment, value: Option<String>) {
        let dynamic_side_effect = value.is_none() && assignment_value_may_write(&assignment.value);
        self.files_changed |= dynamic_side_effect;
        self.correction_blocked |= dynamic_side_effect;
        if let ast::AssignmentName::VariableName(name) = &assignment.name
            && matches!(name.as_str(), "PATH" | "HOME" | "OLDPWD")
        {
            if name == "PATH" && !assignment.append {
                self.path = value;
                self.dynamic |= self.path.is_none();
            } else {
                self.dynamic = true;
            }
        } else if assignment_value_changes_resolution(&assignment.value) {
            self.dynamic = true;
        }
    }

    pub fn after_command(&mut self, name: &str, builtin: bool, opaque: bool, sets_variable: bool) {
        let pure_builtin = builtin
            && (matches!(name, ":" | "true" | "false" | "echo")
                || (name == "printf" && !sets_variable));
        self.files_changed |= !pure_builtin;
        let can_change_resolution = opaque
            || sets_variable
            || (!builtin && name.contains('/'))
            || matches!(
                name,
                "." | "source"
                    | "eval"
                    | "unset"
                    | "alias"
                    | "unalias"
                    | "enable"
                    | "hash"
                    | "set"
                    | "shopt"
                    | "export"
                    | "declare"
                    | "typeset"
                    | "read"
                    | "cd"
                    | "pushd"
                    | "popd"
                    | "command"
                    | "builtin"
            );
        self.correction_blocked |= can_change_resolution;
        self.dynamic |= can_change_resolution;
    }
}

pub(crate) fn word_may_write(word: &ast::Word) -> bool {
    parsed_word_pieces(word).is_none_or(|pieces| pieces_may_write(&pieces))
}

pub(crate) fn word_changes_resolution(word: &ast::Word) -> bool {
    parsed_word_pieces(word).is_none_or(|pieces| pieces_change_resolution(&pieces))
}

fn assignment_value_may_write(value: &ast::AssignmentValue) -> bool {
    match value {
        ast::AssignmentValue::Scalar(word) => word_may_write(word),
        _ => true,
    }
}

pub(crate) fn assignment_value_changes_resolution(value: &ast::AssignmentValue) -> bool {
    match value {
        ast::AssignmentValue::Scalar(word) => word_changes_resolution(word),
        _ => true,
    }
}

fn parsed_word_pieces(word: &ast::Word) -> Option<Vec<word::WordPieceWithSource>> {
    word::parse(&word.value, &brush_parser::ParserOptions::default()).ok()
}

fn pieces_may_write(pieces: &[word::WordPieceWithSource]) -> bool {
    use word::WordPiece as W;
    pieces.iter().any(|piece| match &piece.piece {
        W::CommandSubstitution(_) | W::BackquotedCommandSubstitution(_) => true,
        W::ArithmeticExpression(_) => true,
        W::ParameterExpansion(expr) => parameter_expr_may_write(expr),
        W::DoubleQuotedSequence(inner) | W::GettextDoubleQuotedSequence(inner) => {
            pieces_may_write(inner)
        }
        W::Text(_)
        | W::SingleQuotedText(_)
        | W::AnsiCQuotedText(_)
        | W::TildeExpansion(_)
        | W::EscapeSequence(_) => false,
    })
}

fn pieces_change_resolution(pieces: &[word::WordPieceWithSource]) -> bool {
    use word::WordPiece as W;
    pieces.iter().any(|piece| match &piece.piece {
        W::ArithmeticExpression(_) => true,
        W::ParameterExpansion(expr) => parameter_expr_changes_resolution(expr),
        W::DoubleQuotedSequence(inner) | W::GettextDoubleQuotedSequence(inner) => {
            pieces_change_resolution(inner)
        }
        W::Text(_)
        | W::SingleQuotedText(_)
        | W::AnsiCQuotedText(_)
        | W::TildeExpansion(_)
        | W::CommandSubstitution(_)
        | W::BackquotedCommandSubstitution(_)
        | W::EscapeSequence(_) => false,
    })
}

fn parameter_expr_may_write(expr: &word::ParameterExpr) -> bool {
    use word::ParameterExpr as P;
    match expr {
        P::AssignDefaultValues { .. } => true,
        P::UseDefaultValues { default_value, .. }
        | P::IndicateErrorIfNullOrUnset {
            error_message: default_value,
            ..
        }
        | P::UseAlternativeValue {
            alternative_value: default_value,
            ..
        } => text_may_write(default_value.as_deref()),
        P::RemoveSmallestSuffixPattern { pattern, .. }
        | P::RemoveLargestSuffixPattern { pattern, .. }
        | P::RemoveSmallestPrefixPattern { pattern, .. }
        | P::RemoveLargestPrefixPattern { pattern, .. }
        | P::UppercaseFirstChar { pattern, .. }
        | P::UppercasePattern { pattern, .. }
        | P::LowercaseFirstChar { pattern, .. }
        | P::LowercasePattern { pattern, .. } => text_may_write(pattern.as_deref()),
        P::ReplaceSubstring {
            pattern,
            replacement,
            ..
        } => text_may_write(Some(pattern)) || text_may_write(replacement.as_deref()),
        P::Parameter { .. }
        | P::ParameterLength { .. }
        | P::Substring { .. }
        | P::Transform { .. }
        | P::VariableNames { .. }
        | P::MemberKeys { .. } => false,
    }
}

fn parameter_expr_changes_resolution(expr: &word::ParameterExpr) -> bool {
    use word::{Parameter as N, ParameterExpr as P};
    let changes_parameter = |parameter: &N, indirect: bool| {
        indirect
            || matches!(parameter, N::Named(name) if matches!(name.as_str(), "PATH" | "HOME" | "OLDPWD"))
    };
    match expr {
        P::AssignDefaultValues {
            parameter,
            indirect,
            default_value,
            ..
        } => {
            changes_parameter(parameter, *indirect)
                || text_changes_resolution(default_value.as_deref())
        }
        P::UseDefaultValues { default_value, .. }
        | P::IndicateErrorIfNullOrUnset {
            error_message: default_value,
            ..
        }
        | P::UseAlternativeValue {
            alternative_value: default_value,
            ..
        } => text_changes_resolution(default_value.as_deref()),
        P::RemoveSmallestSuffixPattern { pattern, .. }
        | P::RemoveLargestSuffixPattern { pattern, .. }
        | P::RemoveSmallestPrefixPattern { pattern, .. }
        | P::RemoveLargestPrefixPattern { pattern, .. } => {
            text_changes_resolution(pattern.as_deref())
        }
        P::UppercaseFirstChar {
            indirect, pattern, ..
        }
        | P::UppercasePattern {
            indirect, pattern, ..
        }
        | P::LowercaseFirstChar {
            indirect, pattern, ..
        }
        | P::LowercasePattern {
            indirect, pattern, ..
        } => *indirect || text_changes_resolution(pattern.as_deref()),
        P::ReplaceSubstring {
            indirect,
            pattern,
            replacement,
            ..
        } => {
            *indirect
                || text_changes_resolution(Some(pattern))
                || text_changes_resolution(replacement.as_deref())
        }
        P::Substring { .. } => true,
        P::Parameter { indirect, .. }
        | P::ParameterLength { indirect, .. }
        | P::Transform { indirect, .. } => *indirect,
        P::VariableNames { .. } | P::MemberKeys { .. } => false,
    }
}

fn text_may_write(text: Option<&str>) -> bool {
    text_pieces_any(text, pieces_may_write)
}

fn text_changes_resolution(text: Option<&str>) -> bool {
    text_pieces_any(text, pieces_change_resolution)
}

fn text_pieces_any(
    text: Option<&str>,
    check: impl FnOnce(&[word::WordPieceWithSource]) -> bool,
) -> bool {
    let Some(value) = text else {
        return false;
    };
    word::parse(value, &brush_parser::ParserOptions::default())
        .ok()
        .map(|pieces| check(&pieces))
        .unwrap_or(true)
}

pub(crate) fn extended_test_may_write(expr: &ast::ExtendedTestExpr) -> bool {
    use ast::ExtendedTestExpr as E;
    match expr {
        E::And(left, right) | E::Or(left, right) => {
            extended_test_may_write(left) || extended_test_may_write(right)
        }
        E::Not(inner) | E::Parenthesized(inner) => extended_test_may_write(inner),
        E::UnaryTest(_, word) => word_may_write(word),
        E::BinaryTest(_, left, right) => word_may_write(left) || word_may_write(right),
    }
}

pub(crate) fn literal(
    value: &ast::Word,
    options: &brush_parser::ParserOptions,
    tilde: &impl Fn(&word::TildeExpr) -> Option<String>,
) -> Option<String> {
    let pieces = word::parse(&value.value, options).ok()?;
    if !matches!(value.value.as_str(), "[" | "]")
        && pieces.iter().any(|piece| {
            matches!(&piece.piece, word::WordPiece::Text(text)
                if text.contains(['*', '?', '[', '{', '}']))
        })
    {
        return None;
    }
    crate::trigger::static_word_pieces(&pieces, tilde)
}

pub(crate) fn writes_files(redirect: &ast::IoRedirect) -> bool {
    use ast::{IoFileRedirectKind as K, IoFileRedirectTarget as T, IoRedirect as R};
    matches!(
        redirect,
        R::OutputAndError(..)
            | R::File(_, K::Write | K::Append | K::Clobber | K::ReadAndWrite, _)
            | R::File(_, _, T::ProcessSubstitution(..))
    )
}

pub(crate) fn redirect_blocks_correction(redirect: &ast::IoRedirect) -> bool {
    use ast::{IoFileRedirectTarget as T, IoRedirect as R};
    match redirect {
        R::File(_, _, T::ProcessSubstitution(..)) => true,
        R::File(_, _, T::Filename(word) | T::Duplicate(word))
        | R::HereString(_, word)
        | R::OutputAndError(word, _) => word_may_write(word),
        R::HereDocument(_, doc) => doc.requires_expansion,
        R::File(_, _, T::Fd(_)) => false,
    }
}

pub(crate) fn redirect_changes_resolution(redirect: &ast::IoRedirect) -> bool {
    use ast::{IoFileRedirectTarget as T, IoRedirect as R};
    match redirect {
        R::File(_, _, T::Filename(word) | T::Duplicate(word))
        | R::HereString(_, word)
        | R::OutputAndError(word, _) => word_changes_resolution(word),
        R::HereDocument(_, doc) => doc.requires_expansion,
        R::File(_, _, T::ProcessSubstitution(..) | T::Fd(_)) => false,
    }
}

pub(crate) fn items(
    simple: &ast::SimpleCommand,
) -> impl Iterator<Item = &ast::CommandPrefixOrSuffixItem> + Clone {
    simple
        .prefix
        .iter()
        .flat_map(|p| &p.0)
        .chain(simple.suffix.iter().flat_map(|s| &s.0))
}
