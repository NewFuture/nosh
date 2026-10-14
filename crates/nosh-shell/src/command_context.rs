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
        let writes = command
            .values
            .iter()
            .flatten()
            .any(|word| word_effects(word).may_write);
        self.files_changed |= writes;
        self.correction_blocked |= writes;
    }

    pub fn merge_optional(&mut self, branch: &Self) {
        self.dynamic |=
            branch.dynamic || branch.functions != self.functions || branch.path != self.path;
        self.correction_blocked |= branch.correction_blocked;
        self.files_changed |= branch.files_changed;
    }

    pub fn assignment(
        &mut self,
        assignment: &ast::Assignment,
        value: Option<String>,
    ) -> WordEffects {
        let effects = match &assignment.value {
            ast::AssignmentValue::Scalar(word) => word_effects(word),
            _ => WordEffects::UNKNOWN,
        };
        let dynamic_side_effect = value.is_none() && effects.may_write;
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
        } else {
            self.dynamic |= effects.changes_resolution;
        }
        effects
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

/// Expansion effects only; opening a redirection target is tracked separately.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WordEffects {
    pub may_write: bool,
    pub changes_resolution: bool,
}

impl WordEffects {
    const UNKNOWN: Self = Self {
        may_write: true,
        changes_resolution: true,
    };

    fn merge(&mut self, other: Self) {
        self.may_write |= other.may_write;
        self.changes_resolution |= other.changes_resolution;
    }
}

pub(crate) fn word_effects(word: &ast::Word) -> WordEffects {
    text_effects(Some(&word.value))
}

fn pieces_effects(pieces: &[word::WordPieceWithSource]) -> WordEffects {
    use word::WordPiece as W;
    let mut effects = WordEffects::default();
    for piece in pieces {
        effects.merge(match &piece.piece {
            W::CommandSubstitution(_) | W::BackquotedCommandSubstitution(_) => WordEffects {
                may_write: true,
                ..WordEffects::default()
            },
            W::ArithmeticExpression(_) => WordEffects::UNKNOWN,
            W::ParameterExpansion(expr) => parameter_effects(expr),
            W::DoubleQuotedSequence(inner) | W::GettextDoubleQuotedSequence(inner) => {
                pieces_effects(inner)
            }
            W::Text(_)
            | W::SingleQuotedText(_)
            | W::AnsiCQuotedText(_)
            | W::TildeExpansion(_)
            | W::EscapeSequence(_) => WordEffects::default(),
        });
        if effects == WordEffects::UNKNOWN {
            break;
        }
    }
    effects
}

fn parameter_effects(expr: &word::ParameterExpr) -> WordEffects {
    use word::{Parameter as N, ParameterExpr as P};
    match expr {
        P::AssignDefaultValues {
            parameter,
            indirect,
            default_value,
            ..
        } => WordEffects {
            may_write: true,
            changes_resolution: *indirect
                || matches!(parameter, N::Named(name) if matches!(name.as_str(), "PATH" | "HOME" | "OLDPWD"))
                || text_effects(default_value.as_deref()).changes_resolution,
        },
        P::UseDefaultValues { default_value, .. }
        | P::IndicateErrorIfNullOrUnset {
            error_message: default_value,
            ..
        }
        | P::UseAlternativeValue {
            alternative_value: default_value,
            ..
        } => text_effects(default_value.as_deref()),
        P::RemoveSmallestSuffixPattern { pattern, .. }
        | P::RemoveLargestSuffixPattern { pattern, .. }
        | P::RemoveSmallestPrefixPattern { pattern, .. }
        | P::RemoveLargestPrefixPattern { pattern, .. } => text_effects(pattern.as_deref()),
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
        } => {
            let mut effects = text_effects(pattern.as_deref());
            effects.changes_resolution |= *indirect;
            effects
        }
        P::ReplaceSubstring {
            indirect,
            pattern,
            replacement,
            ..
        } => {
            let mut effects = text_effects(Some(pattern));
            effects.changes_resolution |= *indirect;
            if effects != WordEffects::UNKNOWN {
                effects.merge(text_effects(replacement.as_deref()));
            }
            effects
        }
        P::Substring { .. } => WordEffects {
            changes_resolution: true,
            ..WordEffects::default()
        },
        P::Parameter { indirect, .. }
        | P::ParameterLength { indirect, .. }
        | P::Transform { indirect, .. } => WordEffects {
            changes_resolution: *indirect,
            ..WordEffects::default()
        },
        P::VariableNames { .. } | P::MemberKeys { .. } => WordEffects::default(),
    }
}

fn text_effects(text: Option<&str>) -> WordEffects {
    let Some(value) = text else {
        return WordEffects::default();
    };
    word::parse(value, &brush_parser::ParserOptions::default())
        .map_or(WordEffects::UNKNOWN, |pieces| pieces_effects(&pieces))
}

pub(crate) fn extended_test_may_write(expr: &ast::ExtendedTestExpr) -> bool {
    use ast::ExtendedTestExpr as E;
    match expr {
        E::And(left, right) | E::Or(left, right) => {
            extended_test_may_write(left) || extended_test_may_write(right)
        }
        E::Not(inner) | E::Parenthesized(inner) => extended_test_may_write(inner),
        E::UnaryTest(_, word) => word_effects(word).may_write,
        E::BinaryTest(_, left, right) => {
            word_effects(left).may_write || word_effects(right).may_write
        }
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

pub(crate) fn redirect_effects(redirect: &ast::IoRedirect) -> WordEffects {
    use ast::{IoFileRedirectTarget as T, IoRedirect as R};
    match redirect {
        R::File(_, _, T::ProcessSubstitution(..)) => WordEffects {
            may_write: true,
            ..WordEffects::default()
        },
        R::File(_, _, T::Filename(word) | T::Duplicate(word))
        | R::HereString(_, word)
        | R::OutputAndError(word, _) => word_effects(word),
        R::HereDocument(_, doc) => WordEffects {
            may_write: doc.requires_expansion,
            changes_resolution: doc.requires_expansion,
        },
        R::File(_, _, T::Fd(_)) => WordEffects::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn effects(text: &str) -> (bool, bool) {
        let word = ast::Word {
            value: text.into(),
            loc: None,
        };
        let effects = word_effects(&word);
        (effects.may_write, effects.changes_resolution)
    }

    #[test]
    fn words_distinguish_file_effects_from_resolution_changes() {
        for (text, writes, changes) in [
            ("literal", false, false),
            ("'${PATH:=/runtime}'", false, false),
            ("$'$(touch marker)'", false, false),
            (r"\$PATH", false, false),
            ("~", false, false),
            ("$value", false, false),
            ("${#value}", false, false),
            ("${value@Q}", false, false),
            ("${!prefix*}", false, false),
            ("${!items[@]}", false, false),
            ("${value:-plain}", false, false),
            ("${value:?message}", false, false),
            ("${value:+plain}", false, false),
            ("${value#pattern}", false, false),
            ("${value##pattern}", false, false),
            ("${value%pattern}", false, false),
            ("${value%%pattern}", false, false),
            ("${value^p}", false, false),
            ("${value^^p}", false, false),
            ("${value,p}", false, false),
            ("${value,,p}", false, false),
            ("${value/pattern/replacement}", false, false),
            ("$(PATH=/runtime)", true, false),
            ("`touch marker`", true, false),
            ("${value:=plain}", true, false),
            ("${value:-$(touch marker)}", true, false),
            ("${value:?$(touch marker)}", true, false),
            ("${value:+$(touch marker)}", true, false),
            ("${value#$(touch marker)}", true, false),
            ("${value##$(touch marker)}", true, false),
            ("${value%$(touch marker)}", true, false),
            ("${value%%$(touch marker)}", true, false),
            ("${value^$(touch marker)}", true, false),
            ("${value^^$(touch marker)}", true, false),
            ("${value,$(touch marker)}", true, false),
            ("${value,,$(touch marker)}", true, false),
            ("${value/$(touch marker)/replacement}", true, false),
            ("${value/pattern/$(touch marker)}", true, false),
            ("${!name}", false, true),
            ("${!name@Q}", false, true),
            ("${!name^p}", false, true),
            ("${!name^^p}", false, true),
            ("${!name,p}", false, true),
            ("${!name,,p}", false, true),
            ("${!name/pattern/replacement}", false, true),
            ("${value:1}", false, true),
            ("$((PATH=1))", true, true),
            ("${PATH:=/runtime}", true, true),
            ("${HOME:=/runtime}", true, true),
            ("${OLDPWD:=/runtime}", true, true),
            ("${!name:=plain}", true, true),
            ("${value:=${PATH:=/runtime}}", true, true),
            ("${value:-${PATH:=/runtime}}", true, true),
            ("${value/${!name}/$(touch marker)}", true, true),
        ] {
            assert!(
                word::parse(text, &brush_parser::ParserOptions::default()).is_ok(),
                "{text}"
            );
            assert_eq!(effects(text), (writes, changes), "{text}");
        }
    }

    #[test]
    fn concatenated_and_quoted_words_combine_both_effects() {
        let parts = [
            ("literal", false, false),
            ("${value}", false, false),
            ("$(touch marker)", true, false),
            ("`touch marker`", true, false),
            ("${value:=plain}", true, false),
            ("${!name}", false, true),
            ("${value:1}", false, true),
            ("${PATH:=/runtime}", true, true),
            ("$((i+=1))", true, true),
        ];
        for (left, left_writes, left_changes) in parts {
            for (right, right_writes, right_changes) in parts {
                for text in [
                    format!("{left}{right}"),
                    format!("\"{left}{right}\""),
                    format!("$\"{left}{right}\""),
                ] {
                    assert_eq!(
                        effects(&text),
                        (left_writes || right_writes, left_changes || right_changes),
                        "{text}"
                    );
                }
            }
        }
    }

    #[test]
    fn malformed_words_keep_both_effects_unknown() {
        for text in ["'", "\"", "`", "word'"] {
            assert!(
                word::parse(text, &brush_parser::ParserOptions::default()).is_err(),
                "{text}"
            );
            assert_eq!(effects(text), (true, true), "{text}");
        }
    }

    #[test]
    fn redirects_distinguish_opening_files_from_expansion_effects() {
        for (text, writes, blocks, changes) in [
            (": > out", true, false, false),
            (": < input", false, false, false),
            (": <> input", true, false, false),
            (": &> out", true, false, false),
            (": 2>&1", false, false, false),
            (": > \"$(touch out)\"", true, true, false),
            (": <<< \"${PATH:=/runtime}\"", false, true, true),
            (": <<< \"${!name}\"", false, false, true),
            (": < <(touch out)", true, true, false),
            (": <<EOF\n${PATH:=/runtime}\nEOF", false, true, true),
            (": <<'EOF'\n${PATH:=/runtime}\nEOF", false, false, false),
        ] {
            let options = brush_parser::ParserOptions::default();
            let program = brush_parser::Parser::new(std::io::Cursor::new(text), &options)
                .parse_program()
                .unwrap();
            let ast::Command::Simple(simple) = &program.complete_commands[0].0[0].0.first.seq[0]
            else {
                panic!("expected a simple command: {text}");
            };
            let redirect = items(simple)
                .find_map(|item| match item {
                    ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => Some(redirect),
                    _ => None,
                })
                .unwrap();
            let effects = redirect_effects(redirect);
            assert_eq!(
                (
                    writes_files(redirect),
                    effects.may_write,
                    effects.changes_resolution,
                ),
                (writes, blocks, changes),
                "{text}"
            );
        }
    }
}
