use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Score(pub u8, pub usize, pub usize, pub usize);

pub(crate) fn rank(
    value: &str,
    query: &str,
    fuzzy: bool,
    nocase: bool,
) -> Option<(Score, Vec<usize>)> {
    let insensitive = nocase || !query.chars().any(char::is_uppercase);
    let folded = |character: char| {
        if insensitive {
            character.to_ascii_lowercase()
        } else {
            character
        }
    };
    if !fuzzy && !value.starts_with(query) {
        return None;
    }
    let mut wanted = query.chars();
    let mut next = wanted.next();
    let mut matched = Vec::new();
    let mut gaps = 0;
    let mut previous = None;
    for (ordinal, (offset, character)) in value.char_indices().enumerate() {
        let Some(target) = next else { break };
        if folded(character) == folded(target) {
            if let Some(previous) = previous {
                gaps += ordinal - previous - 1;
            }
            previous = Some(ordinal);
            matched.push(offset);
            next = wanted.next();
        }
    }
    if next.is_some() {
        return None;
    }
    let tier = if value == query {
        0
    } else if insensitive && value.eq_ignore_ascii_case(query) {
        1
    } else if value.starts_with(query) {
        2
    } else if insensitive
        && value
            .get(..query.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(query))
    {
        3
    } else {
        4
    };
    let first = matched.first().copied().unwrap_or(0);
    let boundary = first == 0
        || value[..first]
            .chars()
            .next_back()
            .is_some_and(|character| !character.is_alphanumeric());
    let mut graphemes = value.grapheme_indices(true).enumerate().peekable();
    let mut index = 0;
    for offset in &mut matched {
        while let Some(&(next, (start, _))) = graphemes.peek()
            && start <= *offset
        {
            index = next;
            graphemes.next();
        }
        *offset = index;
    }
    matched.dedup();
    Some((
        Score(tier, usize::from(!boundary), gaps, value.len()),
        matched,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_smart_case_and_unicode_positions() {
        let mut names = ["my-project", "projects", "proj", "project", "Project"];
        names.sort_by_key(|value| (rank(value, "proj", true, false).unwrap().0, *value));
        assert_eq!(
            names,
            ["proj", "project", "projects", "Project", "my-project"]
        );
        assert!(rank("project", "Proj", true, false).is_none());
        assert!(rank("project", "Proj", true, true).is_some());
        assert_eq!(rank("a\u{301}中b", "a中", true, false).unwrap().1, [0, 1]);
        assert_eq!(
            rank("a\u{301}中b", "a\u{301}中", true, false).unwrap().1,
            [0, 1]
        );
        assert_eq!(
            rank("x-👩\u{200d}💻ab", "👩💻b", true, false).unwrap().1,
            [2, 4]
        );
        assert_eq!(
            rank("x-abc", "ac", true, false).unwrap().0,
            Score(4, 0, 1, 5)
        );
        assert_eq!(
            rank("xabc", "ac", true, false).unwrap().0,
            Score(4, 1, 1, 4)
        );
        assert!(rank("make-target", "mt", false, false).is_none());
    }
}
