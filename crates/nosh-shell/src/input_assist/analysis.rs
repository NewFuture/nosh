use std::ops::Range;

use brush_parser::{ParseError, SourceSpan, Token, TokenizerError, ast, word};

use super::*;
use crate::command_context::{self, Scope};
use crate::trigger;

const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "select", "in", "do", "done", "while", "until",
    "case", "esac", "function", "time", "coproc", "[[", "]]", "{", "}", "!",
];

struct Coordinates {
    bytes: Vec<usize>,
    offset: usize,
}

impl Coordinates {
    fn new(text: &str, offset: usize) -> Self {
        Self {
            bytes: text
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(text.len()))
                .collect(),
            offset,
        }
    }

    fn range(&self, loc: &SourceSpan) -> Option<Range<usize>> {
        Some(
            self.offset + self.bytes.get(loc.start.index)?
                ..self.offset + self.bytes.get(loc.end.index)?,
        )
    }
}

struct Analyzer<'a> {
    input: &'a Input,
    options: brush_parser::ParserOptions,
    result: Analysis,
    roles: Vec<Role>,
    nodes: usize,
    limited: bool,
}

pub(super) fn analyze(input: &Input) -> Analysis {
    let mut a = Analyzer {
        input,
        options: input.context.options(),
        result: Analysis::new(input.version),
        roles: vec![Role::Text; input.text.len().min(MAX_INPUT)],
        nodes: 0,
        limited: false,
    };
    if input.text.len() > MAX_INPUT {
        a.limit();
        return a.result;
    }
    let trimmed = input.text.trim_start();
    let ctx = &input.context;
    if ctx.ai_enabled
        && ((!ctx.ai_prefix.is_empty() && trimmed.starts_with(&ctx.ai_prefix))
            || trigger::apostrophe_prose(input.text.trim()))
    {
        a.paint(0..input.text.len(), Role::Ai);
        a.find(0..input.text.len(), State::Known, Reason::Ai);
    } else {
        let ai_head = ctx.ai_enabled
            && !input.command_input
            && !ctx.ai_builtin.is_empty()
            && !ctx.ai_builtin_shadowed
            && !ctx.abbreviations.applicable.contains(&ctx.ai_builtin)
            && trimmed
                .strip_prefix(&ctx.ai_builtin)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
        if ai_head {
            // Resolve the host command before interpreting its possibly non-shell
            // prose. A real executable with this name still shadows the AI entry.
            let start = input.text.len() - trimmed.len();
            let range = start..start + ctx.ai_builtin.len();
            a.result.ai_candidate = true;
            a.paint(range.clone(), Role::Command);
            a.query(Query {
                range,
                word: ctx.ai_builtin.clone(),
                kind: QueryKind::Command {
                    path: ctx.path.clone(),
                    ai_on_missing: true,
                },
                definite: true,
            });
        } else {
            let mut scope = Scope {
                dynamic: ctx.command_traps,
                files_changed: ctx.command_traps,
                ..Scope::default()
            };
            a.region(&input.text, 0, &mut scope, 0, true);
        }
    }
    a.result.spans = super::spans(&input.text, &a.roles);
    if a.result.spans.len() > MAX_SPANS {
        a.limit();
        a.result.spans = vec![Span {
            range: 0..input.text.len(),
            role: Role::Text,
        }];
    }
    a.result
}

impl Analyzer<'_> {
    fn limit(&mut self) {
        if !self.limited {
            self.limited = true;
            self.result.findings.push(Finding {
                range: 0..0,
                state: State::Unavailable,
                reason: Reason::Limit,
            });
        }
    }

    fn step(&mut self, depth: usize) -> bool {
        self.nodes += 1;
        if self.nodes > MAX_NODES || depth > MAX_DEPTH {
            self.limit();
        }
        !self.limited
    }

    fn paint(&mut self, range: Range<usize>, role: Role) {
        if range.start <= range.end
            && self.input.text.is_char_boundary(range.start)
            && self.input.text.is_char_boundary(range.end)
            && let Some(slots) = self.roles.get_mut(range)
        {
            slots.fill(role);
        } else {
            self.limit();
        }
    }

    fn command_role(&mut self, range: Range<usize>, role: Role) {
        if let Some(slots) = self.roles.get_mut(range) {
            for slot in slots {
                if matches!(*slot, Role::Text | Role::Keyword | Role::Command) {
                    *slot = role;
                }
            }
        } else {
            self.limit();
        }
    }

    fn find(&mut self, range: Range<usize>, state: State, reason: Reason) {
        if self.result.findings.len() < MAX_QUERIES {
            self.result.findings.push(Finding {
                range,
                state,
                reason,
            });
        } else {
            self.limit();
        }
    }

    fn query(&mut self, query: Query) {
        if query.word.contains('\0') {
            self.find(
                query.range,
                State::Error,
                Reason::Syntax("NUL is not valid in a command or path".into()),
            );
            return;
        }
        if query.word.is_empty() && matches!(query.kind, QueryKind::Path(_)) {
            self.find(
                query.range,
                State::Error,
                Reason::Syntax("empty path".into()),
            );
            return;
        }
        if self.result.queries.len() == MAX_QUERIES {
            self.limit();
        } else if !query.range.is_empty() {
            self.result.queries.push(query);
        }
    }

    fn region(
        &mut self,
        text: &str,
        offset: usize,
        scope: &mut Scope,
        depth: usize,
        recover: bool,
    ) {
        if !self.step(depth) {
            return;
        }
        let coords = Coordinates::new(text, offset);
        let tokens =
            match brush_parser::uncached_tokenize_str(text, &self.options.tokenizer_options()) {
                Ok(tokens) => tokens,
                Err(error) => {
                    let state = if error.is_incomplete() {
                        State::Incomplete
                    } else {
                        State::Error
                    };
                    let anchor = incomplete_anchor(&error)
                        .and_then(|i| coords.bytes.get(i))
                        .map(|i| offset + i);
                    let range = anchor.map_or(offset..offset, |i| i..offset + text.len());
                    if anchor.is_some() {
                        self.paint(range.clone(), Role::Incomplete);
                    }
                    self.find(
                        range,
                        state,
                        if state == State::Incomplete {
                            Reason::Incomplete(short_error(&error))
                        } else {
                            Reason::Syntax(short_error(&error))
                        },
                    );
                    if recover {
                        // One genuine prefix, never appended delimiters or a fabricated full AST.
                        // Without an opening position, only try the first whitespace boundary.
                        let end = anchor
                            .map(|i| i - offset)
                            .or_else(|| text.find(char::is_whitespace));
                        if let Some(end) = end
                            && end > 0
                        {
                            let prefix = &text[..end];
                            if let Ok(mut tokens) = brush_parser::uncached_tokenize_str(
                                prefix,
                                &self.options.tokenizer_options(),
                            ) {
                                // A quote can be inside a word: foo"bar is not the argument foo.
                                if anchor.is_some()
                                    && tokens.last().is_some_and(|token| {
                                        coords
                                            .range(token.location())
                                            .is_some_and(|r| r.end == offset + end)
                                    })
                                {
                                    tokens.pop();
                                }
                                let retained = tokens
                                    .last()
                                    .and_then(|token| coords.range(token.location()))
                                    .map_or(0, |range| range.end - offset);
                                self.tokens(&tokens, &text[..retained], &coords, scope, depth);
                                if let Ok(program) =
                                    brush_parser::parse_tokens(&tokens, &self.options)
                                {
                                    let mut partial = scope.clone();
                                    partial.files_changed = true;
                                    self.program(&program, &coords, &mut partial, depth);
                                }
                            }
                        }
                    }
                    return;
                }
            };
        if tokens.len() > MAX_NODES {
            self.limit();
            return;
        }
        self.tokens(&tokens, text, &coords, scope, depth);
        if self.limited {
            return;
        }
        match brush_parser::parse_tokens(&tokens, &self.options) {
            Ok(program) => self.program(&program, &coords, scope, depth),
            Err(ParseError::ParsingAtEndOfInput) => {
                self.find(
                    offset + text.len()..offset + text.len(),
                    State::Incomplete,
                    Reason::Incomplete("expected more input".into()),
                );
                // A trailing pipeline has a real, complete left-hand program.
                if tokens.last().is_some_and(|t| {
                    matches!(t, Token::Operator(op, _) if matches!(op.as_str(), "|" | "||" | "&&" | "|&"))
                }) && let Ok(program) =
                    brush_parser::parse_tokens(&tokens[..tokens.len() - 1], &self.options)
                {
                    self.program(&program, &coords, scope, depth);
                }
            }
            Err(error) => {
                let range = match &error {
                    ParseError::ParsingNear(pos) => tokens
                        .iter()
                        .find(|t| t.location().start.index == pos.index)
                        .and_then(|t| coords.range(t.location())),
                    _ => None,
                }
                .unwrap_or(offset..offset);
                self.paint(range.clone(), Role::Error);
                let detail = self
                    .input
                    .text
                    .get(range.clone())
                    .filter(|s| !s.is_empty())
                    .map_or_else(
                        || short_error(&error),
                        |token| short_error(format!("unexpected token {token:?}: {error}")),
                    );
                self.find(range, State::Error, Reason::Syntax(detail));
            }
        }
    }

    fn tokens(
        &mut self,
        tokens: &[Token],
        text: &str,
        coords: &Coordinates,
        scope: &mut Scope,
        depth: usize,
    ) {
        let mut previous = 0;
        for token in tokens {
            if !self.step(depth) {
                return;
            }
            let Some(range) = coords.range(token.location()) else {
                self.limit();
                return;
            };
            let start = range.start - coords.offset;
            if let Some(gap) = text.get(previous..start)
                && let Some(comment) = gap.find('#')
            {
                self.paint(
                    coords.offset + previous + comment..range.start,
                    Role::Comment,
                );
            }
            previous = range.end - coords.offset;
            match token {
                Token::Operator(..) => self.paint(range, Role::Operator),
                Token::Word(..) => {
                    let Some(raw) = self.input.text.get(range.clone()) else {
                        self.limit();
                        return;
                    };
                    if KEYWORDS.contains(&raw) {
                        self.paint(range.clone(), Role::Keyword);
                    }
                    if raw.len() > MAX_WORD {
                        self.limit();
                        continue;
                    }
                    if let Ok(pieces) = word::parse(raw, &self.options) {
                        self.pieces(&pieces, raw, range.start, scope, depth + 1);
                    }
                }
            }
        }
        if let Some(gap) = text.get(previous..)
            && let Some(comment) = gap.find('#')
        {
            self.paint(
                coords.offset + previous + comment..coords.offset + text.len(),
                Role::Comment,
            );
        }
    }

    fn pieces(
        &mut self,
        pieces: &[word::WordPieceWithSource],
        raw: &str,
        offset: usize,
        scope: &mut Scope,
        depth: usize,
    ) {
        use word::WordPiece as W;
        for piece in pieces {
            if !self.step(depth) {
                return;
            }
            let range = offset + piece.start_index..offset + piece.end_index;
            match &piece.piece {
                W::SingleQuotedText(_) | W::AnsiCQuotedText(_) | W::EscapeSequence(_) => {
                    self.paint(range, Role::String);
                }
                W::DoubleQuotedSequence(inner) | W::GettextDoubleQuotedSequence(inner) => {
                    self.paint(range, Role::String);
                    self.pieces(inner, raw, offset, scope, depth + 1);
                }
                W::ParameterExpansion(_) | W::TildeExpansion(_) | W::ArithmeticExpression(_) => {
                    self.paint(range, Role::Variable);
                }
                W::CommandSubstitution(_) | W::BackquotedCommandSubstitution(_) => {
                    self.paint(range.clone(), Role::Operator);
                    let opening = if matches!(piece.piece, W::CommandSubstitution(_)) {
                        2
                    } else {
                        1
                    };
                    if let Some(body) =
                        raw.get(piece.start_index + opening..piece.end_index.saturating_sub(1))
                    {
                        self.region(
                            body,
                            range.start + opening,
                            &mut scope.clone(),
                            depth + 1,
                            false,
                        );
                    }
                }
                W::Text(_) => {}
            }
        }
    }

    fn literal(&self, word: &ast::Word, scope: &Scope) -> Option<String> {
        if word.value.len() > MAX_WORD {
            return None;
        }
        command_context::literal(word, &self.options, &|expr| {
            if scope.dynamic {
                return None;
            }
            match expr {
                word::TildeExpr::Home => self.input.context.home.clone(),
                word::TildeExpr::WorkingDir => self.input.context.cwd.to_str().map(str::to_owned),
                word::TildeExpr::OldWorkingDir => self.input.context.oldpwd.clone(),
                _ => None,
            }
        })
    }

    fn command_kind(&self, name: &str, scope: &Scope) -> Option<Role> {
        let context = &self.input.context;
        if scope.dynamic {
            None
        } else if context.aliases.contains(name) {
            Some(Role::Alias)
        } else if scope.functions.contains(name) || context.functions.contains(name) {
            Some(Role::Function)
        } else if context.builtins.contains(name) {
            Some(Role::Builtin)
        } else if context.abbreviations.applicable.contains(name) {
            Some(Role::Abbreviation)
        } else {
            None
        }
    }

    fn word_range(&self, word: &ast::Word, coords: &Coordinates) -> Range<usize> {
        word.loc
            .as_ref()
            .and_then(|loc| coords.range(loc))
            .unwrap_or(coords.offset..coords.offset)
    }

    fn program(
        &mut self,
        program: &ast::Program,
        coords: &Coordinates,
        scope: &mut Scope,
        depth: usize,
    ) {
        for list in &program.complete_commands {
            self.list(list, coords, scope, depth + 1);
        }
    }

    fn list(
        &mut self,
        list: &ast::CompoundList,
        coords: &Coordinates,
        scope: &mut Scope,
        depth: usize,
    ) {
        if !self.step(depth) {
            return;
        }
        for ast::CompoundListItem(and_or, separator) in &list.0 {
            let mut child = scope.clone();
            self.pipeline(&and_or.first, coords, &mut child, depth + 1);
            for next in &and_or.additional {
                let (ast::AndOr::And(p) | ast::AndOr::Or(p)) = next;
                let mut optional = child.clone();
                self.pipeline(p, coords, &mut optional, depth + 1);
                child.merge_optional(&optional);
            }
            if matches!(separator, ast::SeparatorOperator::Async) {
                scope.files_changed = true;
            } else {
                *scope = child;
            }
        }
    }

    fn pipeline(
        &mut self,
        pipeline: &ast::Pipeline,
        coords: &Coordinates,
        scope: &mut Scope,
        depth: usize,
    ) {
        if pipeline.seq.len() == 1 {
            self.command(&pipeline.seq[0], coords, scope, depth + 1);
        } else {
            let inherited = scope.clone();
            for (index, command) in pipeline.seq.iter().enumerate() {
                let mut child = inherited.clone();
                child.files_changed = true;
                self.command(command, coords, &mut child, depth + 1);
                if index + 1 == pipeline.seq.len() {
                    scope.merge_optional(&child);
                }
            }
            scope.files_changed = true;
        }
    }

    fn command(
        &mut self,
        command: &ast::Command,
        coords: &Coordinates,
        scope: &mut Scope,
        depth: usize,
    ) {
        if !self.step(depth) {
            return;
        }
        match command {
            ast::Command::Simple(simple) => self.simple(simple, coords, scope, depth),
            ast::Command::Function(function) => {
                let range = self.word_range(&function.fname, coords);
                self.command_role(range, Role::Function);
                if let Some(name) = self.literal(&function.fname, scope) {
                    scope.functions.insert(name);
                }
                let mut body = scope.clone();
                body.dynamic = true;
                body.files_changed = true;
                self.redirects(&function.body.1, coords, &mut body);
                self.compound(&function.body.0, coords, &mut body, depth + 1);
            }
            ast::Command::Compound(command, redirects) => {
                self.redirects(redirects, coords, scope);
                self.compound(command, coords, scope, depth + 1);
            }
            ast::Command::ExtendedTest(test, redirects) => {
                scope.files_changed |= command_context::extended_test_may_write(&test.expr);
                self.redirects(redirects, coords, scope);
            }
        }
    }

    fn simple(
        &mut self,
        simple: &ast::SimpleCommand,
        coords: &Coordinates,
        scope: &mut Scope,
        depth: usize,
    ) {
        let mut local = scope.clone();
        let mut parent_dynamic = false;
        let items = command_context::items(simple);
        for item in items.clone() {
            match item {
                ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, _) => {
                    let value = match &assignment.value {
                        ast::AssignmentValue::Scalar(word) => self.literal(word, &local),
                        _ => None,
                    };
                    parent_dynamic |=
                        command_context::assignment_value_changes_resolution(&assignment.value);
                    local.assignment(assignment, value);
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, child) => {
                    let mut nested = local.clone();
                    nested.files_changed = true;
                    nested.correction_blocked = true;
                    self.list(&child.list, coords, &mut nested, depth + 1);
                    local.files_changed = true;
                    local.correction_blocked = true;
                }
                _ => {}
            }
        }
        let name = simple
            .word_or_name
            .as_ref()
            .and_then(|w| self.literal(w, &local));
        let context = &self.input.context;
        let role = name
            .as_deref()
            .and_then(|name| self.command_kind(name, scope));
        let check_prose =
            role.is_none() && !local.dynamic && context.ai_enabled && context.trigger_on_error;
        let mut argv: Vec<String> = name.iter().filter(|_| check_prose).cloned().collect();
        let mut printf_sets_variable = false;
        for item in items {
            match item {
                ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    parent_dynamic |= command_context::redirect_changes_resolution(redirect);
                    self.redirect(redirect, coords, &mut local);
                }
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    let range = self.word_range(word, coords);
                    self.command_role(range.clone(), Role::Text);
                    let value = self.literal(word, &local);
                    local.files_changed |= value.is_none() && command_context::word_may_write(word);
                    parent_dynamic |= command_context::word_changes_resolution(word);
                    printf_sets_variable |= value.as_deref() == Some("-v");
                    if check_prose && argv.len() < 32 {
                        argv.push(value.clone().unwrap_or_else(|| word.value.clone()));
                    }
                    if name.as_deref() == Some("cd") && role == Some(Role::Builtin) {
                        if !word.value.starts_with('-') {
                            self.path_query(range, value, &local, PathUse::Directory);
                        }
                    } else if let Some(value) = value
                        && !value.starts_with('-')
                        && !value.is_empty()
                        && !value.contains("://")
                    {
                        let use_ = if value.contains('/') || matches!(value.as_str(), "." | "..") {
                            PathUse::Explicit
                        } else {
                            PathUse::Argument
                        };
                        self.path_query(range, Some(value), &local, use_);
                    }
                }
                _ => {}
            }
        }
        let Some(word) = &simple.word_or_name else {
            *scope = local;
            return;
        };
        let range = self.word_range(word, coords);
        self.command_role(range.clone(), Role::Command);
        let Some(name) = name else {
            self.find(range, State::Unknown, Reason::Dynamic);
            scope.dynamic = true;
            scope.files_changed = true;
            return;
        };
        if let Some(role) = role {
            self.command_role(range.clone(), role);
            if role == Role::Abbreviation {
                self.find(range, State::Known, Reason::Abbreviation);
            }
        } else if local.dynamic {
            self.find(range, State::Unknown, Reason::Dynamic);
        } else {
            self.query(Query {
                range,
                word: name.clone(),
                kind: QueryKind::Command {
                    path: local.path.clone().or_else(|| context.path.clone()),
                    ai_on_missing: check_prose
                        && (trigger::looks_like_question(&argv) || trigger::contains_cjk(&name)),
                },
                definite: !local.files_changed,
            });
        }
        scope.files_changed |= local.files_changed;
        scope.dynamic |= parent_dynamic;
        scope.after_command(
            &name,
            role == Some(Role::Builtin),
            matches!(
                role,
                Some(Role::Alias | Role::Function | Role::Abbreviation)
            ),
            name == "printf" && printf_sets_variable,
        );
    }

    fn path(&mut self, word: &ast::Word, coords: &Coordinates, scope: &Scope, use_: PathUse) {
        let range = self.word_range(word, coords);
        let value = if scope.dynamic {
            None
        } else {
            self.literal(word, scope)
        };
        self.path_query(range, value, scope, use_);
    }

    fn path_query(
        &mut self,
        range: Range<usize>,
        value: Option<String>,
        scope: &Scope,
        use_: PathUse,
    ) {
        if scope.dynamic {
            if use_ != PathUse::Argument {
                self.find(range, State::Unknown, Reason::Dynamic);
            }
        } else if let Some(value) = value {
            self.query(Query {
                range,
                word: value,
                kind: QueryKind::Path(use_),
                definite: !scope.files_changed,
            });
        } else {
            self.find(range, State::Unknown, Reason::Dynamic);
        }
    }

    fn redirect(&mut self, redirect: &ast::IoRedirect, coords: &Coordinates, scope: &mut Scope) {
        use ast::{IoFileRedirectKind as K, IoFileRedirectTarget as T, IoRedirect as R};
        scope.files_changed |= match redirect {
            R::File(_, _, T::Filename(word) | T::Duplicate(word))
            | R::HereString(_, word)
            | R::OutputAndError(word, _) => self.literal(word, scope).is_none(),
            R::HereDocument(_, doc) => doc.requires_expansion,
            R::File(_, _, T::ProcessSubstitution(..)) => true,
            _ => false,
        };
        match redirect {
            R::File(_, kind, T::Filename(word)) => {
                let use_ = if matches!(kind, K::Read) {
                    PathUse::Read
                } else {
                    PathUse::Write
                };
                self.path(word, coords, scope, use_);
            }
            R::OutputAndError(word, _) => self.path(word, coords, scope, PathUse::Write),
            R::File(_, _, T::Duplicate(word))
                if self
                    .literal(word, scope)
                    .is_none_or(|s| s != "-" && s.parse::<u32>().is_err()) =>
            {
                self.find(
                    self.word_range(word, coords),
                    State::Unknown,
                    Reason::Dynamic,
                );
            }
            _ => {}
        }
        scope.files_changed |= command_context::writes_files(redirect);
    }

    fn redirects(
        &mut self,
        redirects: &Option<ast::RedirectList>,
        coords: &Coordinates,
        scope: &mut Scope,
    ) {
        for redirect in redirects.iter().flat_map(|r| &r.0) {
            self.redirect(redirect, coords, scope);
        }
    }

    fn compound(
        &mut self,
        command: &ast::CompoundCommand,
        coords: &Coordinates,
        scope: &mut Scope,
        depth: usize,
    ) {
        use ast::CompoundCommand as C;
        if !self.step(depth) {
            return;
        }
        match command {
            C::BraceGroup(group) => self.list(&group.list, coords, scope, depth),
            C::Subshell(child) => {
                let mut child_scope = scope.clone();
                self.list(&child.list, coords, &mut child_scope, depth);
                scope.files_changed |= child_scope.files_changed;
                scope.correction_blocked |= child_scope.correction_blocked;
            }
            C::Coprocess(child) => {
                self.command(&child.body, coords, &mut scope.clone(), depth);
                scope.files_changed = true;
                scope.correction_blocked = true;
            }
            C::ForClause(loop_) => {
                scope.for_loop(loop_);
                self.optional(&loop_.body.list, coords, scope, depth);
            }
            C::ArithmeticForClause(loop_) => {
                scope.dynamic = true;
                self.optional(&loop_.body.list, coords, scope, depth);
            }
            C::WhileClause(loop_) | C::UntilClause(loop_) => {
                self.list(&loop_.0, coords, scope, depth);
                self.optional(&loop_.1.list, coords, scope, depth);
            }
            C::IfClause(branch) => {
                self.list(&branch.condition, coords, scope, depth);
                self.optional(&branch.then, coords, scope, depth);
                for branch in branch.elses.iter().flatten() {
                    let mut child = scope.clone();
                    if let Some(condition) = &branch.condition {
                        self.list(condition, coords, &mut child, depth);
                    }
                    self.optional(&branch.body, coords, &mut child, depth);
                    scope.merge_optional(&child);
                }
            }
            C::CaseClause(case) => {
                for branch in &case.cases {
                    if let Some(list) = &branch.cmd {
                        self.optional(list, coords, scope, depth);
                    }
                }
            }
            C::Arithmetic(_) => scope.dynamic = true,
        }
    }

    fn optional(
        &mut self,
        list: &ast::CompoundList,
        coords: &Coordinates,
        scope: &mut Scope,
        depth: usize,
    ) {
        let mut child = scope.clone();
        self.list(list, coords, &mut child, depth);
        scope.merge_optional(&child);
    }
}

fn incomplete_anchor(error: &TokenizerError) -> Option<usize> {
    match error {
        TokenizerError::UnterminatedSingleQuote(pos)
        | TokenizerError::UnterminatedDoubleQuote(pos)
        | TokenizerError::UnterminatedAnsiCQuote(pos)
        | TokenizerError::UnterminatedBackquote(pos)
        | TokenizerError::UnterminatedExtendedGlob(pos) => Some(pos.index),
        _ => None,
    }
}
