//! Three-way merge of one script's text by line.
//!
//! The merge is anchored on lines that cannot be mistaken for another: a
//! line whose text stands exactly once in the last-sync text and exactly once
//! on each side, in the same order on all three. Lines that repeat (blank
//! lines, `end`) are never anchors: a comparison could take one of them for
//! another, and a block both sides hold would then be written twice.
//!
//! The lines between two anchors form one region. A region comes from the
//! side that changed it when the other left it alone, and from either when
//! both made it the same. Where both changed it differently: lines replaced
//! one for one on both sides are settled line by line, each from the side
//! that changed it or from the preferred side; any other region comes whole
//! from the preferred side. Lines one side added are never combined with
//! lines the other added.

use std::collections::HashMap;

use super::ConflictPreference;

/// The merged text, or `None` when both sides changed a region differently
/// and no side is preferred.
pub(super) fn merge_script_lines(
    base: &str,
    editor: &str,
    studio: &str,
    preference: ConflictPreference,
) -> Option<String> {
    let (base, editor, studio) = (lines(base), lines(editor), lines(studio));
    let mut merged = String::new();
    let (mut at_base, mut at_editor, mut at_studio) = (0, 0, 0);
    // The end of the three texts closes the last region.
    let mut stops = anchors(&base, &editor, &studio);
    stops.push((base.len(), editor.len(), studio.len()));
    for (to_base, to_editor, to_studio) in stops {
        let region = region(
            &base[at_base..to_base],
            &editor[at_editor..to_editor],
            &studio[at_studio..to_studio],
            preference,
        )?;
        region.into_iter().for_each(|line| merged.push_str(line));
        if let Some(anchor) = base.get(to_base) {
            merged.push_str(anchor);
        }
        (at_base, at_editor, at_studio) = (to_base + 1, to_editor + 1, to_studio + 1);
    }
    Some(merged)
}

/// Where each anchor line stands in the three texts, in order.
fn anchors(base: &[&str], editor: &[&str], studio: &[&str]) -> Vec<(usize, usize, usize)> {
    let (in_editor, in_studio) = (single_lines(editor), single_lines(studio));
    let in_base = single_lines(base);
    let candidates = base
        .iter()
        .enumerate()
        .filter(|(place, line)| in_base.get(*line) == Some(&Some(*place)))
        .filter_map(|(place, line)| {
            Some((place, (*in_editor.get(line)?)?, (*in_studio.get(line)?)?))
        })
        .collect::<Vec<_>>();
    // A line a side moved is out of order there and anchors nothing.
    let ordered_in_editor =
        longest_rising(&candidates.iter().map(|anchor| anchor.1).collect::<Vec<_>>());
    let ordered_in_studio =
        longest_rising(&candidates.iter().map(|anchor| anchor.2).collect::<Vec<_>>());
    candidates
        .into_iter()
        .enumerate()
        .filter(|(index, _)| ordered_in_editor[*index] && ordered_in_studio[*index])
        .map(|(_, anchor)| anchor)
        .collect()
}

/// Where each text that is a line of `lines` stands, or `None` for a text
/// that stands there more than once.
fn single_lines<'a>(lines: &[&'a str]) -> HashMap<&'a str, Option<usize>> {
    let mut places = HashMap::with_capacity(lines.len());
    for (place, line) in lines.iter().enumerate() {
        places
            .entry(*line)
            .and_modify(|seen| *seen = None)
            .or_insert(Some(place));
    }
    places
}

/// Which of `values` belong to one longest run of rising values.
fn longest_rising(values: &[usize]) -> Vec<bool> {
    // `ends[length]` is the index of the smallest value a rising run of
    // `length + 1` values can end with.
    let mut ends = Vec::<usize>::new();
    let mut before = vec![None; values.len()];
    for (index, value) in values.iter().enumerate() {
        let length = ends.partition_point(|&end| values[end] < *value);
        before[index] = length.checked_sub(1).map(|shorter| ends[shorter]);
        if length == ends.len() {
            ends.push(index);
        } else {
            ends[length] = index;
        }
    }
    let mut chosen = vec![false; values.len()];
    let mut next = ends.last().copied();
    while let Some(index) = next {
        chosen[index] = true;
        next = before[index];
    }
    chosen
}

/// The lines a region ends with, or `None` for a conflict nobody settles.
fn region<'a>(
    base: &[&'a str],
    editor: &[&'a str],
    studio: &[&'a str],
    preference: ConflictPreference,
) -> Option<Vec<&'a str>> {
    if editor == studio || studio == base {
        return Some(editor.to_vec());
    }
    if editor == base {
        return Some(studio.to_vec());
    }
    // Both sides added lines where there were none, and one of them added the
    // other's lines and more: a sync that stopped halfway leaves this.
    if base.is_empty() {
        if studio.starts_with(editor) || studio.ends_with(editor) {
            return Some(studio.to_vec());
        }
        if editor.starts_with(studio) || editor.ends_with(studio) {
            return Some(editor.to_vec());
        }
    }
    let preferred = |editor: &'a str, studio: &'a str| match preference {
        ConflictPreference::Editor => Some(editor),
        ConflictPreference::Studio => Some(studio),
        ConflictPreference::None => None,
    };
    // Each side has as many lines here as there were, and no text of the
    // region stands at one place on one side and at another on another: no
    // line moved, so each line of the region is one line on every side. A
    // line only one side changed is then that side's, also next to a line the
    // two disagree about.
    if base.len() == editor.len()
        && base.len() == studio.len()
        && !stands_elsewhere(editor, base)
        && !stands_elsewhere(studio, base)
        && !stands_elsewhere(editor, studio)
    {
        return base
            .iter()
            .zip(editor)
            .zip(studio)
            .map(|((base, editor), studio)| {
                if editor == studio || studio == base {
                    Some(*editor)
                } else if editor == base {
                    Some(*studio)
                } else {
                    preferred(editor, studio)
                }
            })
            .collect();
    }
    match preference {
        ConflictPreference::Editor => Some(editor.to_vec()),
        ConflictPreference::Studio => Some(studio.to_vec()),
        ConflictPreference::None => None,
    }
}

/// Whether a line of `lines` is also a line of `other` at another place.
fn stands_elsewhere(lines: &[&str], other: &[&str]) -> bool {
    let mut places = std::collections::HashMap::<&str, (usize, usize)>::new();
    for (place, line) in other.iter().enumerate() {
        places.entry(line).or_insert((place, 0)).1 += 1;
    }
    lines.iter().enumerate().any(|(place, line)| {
        places
            .get(line)
            .is_some_and(|(first, count)| *count > 1 || *first != place)
    })
}

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANY: [ConflictPreference; 3] = [
        ConflictPreference::None,
        ConflictPreference::Editor,
        ConflictPreference::Studio,
    ];

    #[test]
    fn a_line_added_next_to_a_line_the_other_side_changed_is_written_once() {
        for (base, editor, studio) in [
            ("a\nb\n", "a\nX\nb\n", "a\nX\nB\n"),
            ("a\nb\n", "a\nX\nb\n", "A\nX\nb\n"),
            ("a\n", "X\na\n", "X\nA\n"),
            ("a\nb\n", "a\nb\nX\n", "a\nB\nX\n"),
        ] {
            for preference in ANY {
                let merged = merge_script_lines(base, editor, studio, preference);
                assert!(
                    merged
                        .as_deref()
                        .is_none_or(|text| text == editor || text == studio),
                    "{preference:?} {base:?}: {merged:?}"
                );
            }
        }
    }

    /// In a block where both sides only replaced lines, a line one side
    /// changed is kept next to a line the two disagree about.
    #[test]
    fn replaced_lines_are_settled_one_by_one() {
        let base = "a\nb\nc\nd\n";
        let editor = "a\nb files\nc files\nd\n";
        let studio = "a\nb studio\nc\nd studio\n";
        assert!(merge_script_lines(base, editor, studio, ConflictPreference::None).is_none());
        assert_eq!(
            merge_script_lines(base, editor, studio, ConflictPreference::Editor).unwrap(),
            "a\nb files\nc files\nd studio\n"
        );
        assert_eq!(
            merge_script_lines(base, editor, studio, ConflictPreference::Studio).unwrap(),
            "a\nb studio\nc files\nd studio\n"
        );
        // A side that dropped a line and added another has moved lines, and
        // the block is one choice again.
        assert_eq!(
            merge_script_lines(
                "p
q
",
                "q
N
",
                "p
Q
",
                ConflictPreference::Studio
            )
            .unwrap(),
            "p
Q
"
        );
        // Neighbouring lines changed by one side each are no conflict.
        assert_eq!(
            merge_script_lines(
                "a\nb\nc\n",
                "a\nB\nc\n",
                "a\nb\nC\n",
                ConflictPreference::None
            )
            .unwrap(),
            "a\nB\nC\n"
        );
    }

    /// A block both sides hold is one block, also where its lines repeat
    /// lines around it.
    #[test]
    fn a_block_both_sides_hold_is_written_once_where_blank_lines_and_ends_repeat() {
        for (base, editor, studio) in [
            (
                "local a = 1

return a
",
                "local a = 1

local b = 2

return a
",
                "local a = 10

local b = 2

return a
",
            ),
            (
                "local function f()
end

return f
",
                "local function f()
end

local function g()
end

return f
",
                "local function f()
	print(1)
end

local function g()
end

return f
",
            ),
            (
                "if a then
end
return a
",
                "if a then
end
if b then
end
return a
",
                "if a then
	x()
end
if b then
end
return a
",
            ),
            (
                "x
y
x
y
z
",
                "x
x
y
x
y
z
",
                "x
y
x
y
x
y
z
",
            ),
        ] {
            for preference in ANY {
                let merged = merge_script_lines(base, editor, studio, preference);
                assert!(
                    merged
                        .as_deref()
                        .is_none_or(|text| text == editor || text == studio),
                    "{preference:?}: {merged:?}"
                );
            }
        }
    }

    /// A text made of a few repeating lines, edited at random.
    fn repeating(rng: &mut Rng, from: &[String]) -> Vec<String> {
        let line = |rng: &mut Rng| match rng.below(5) {
            0 => "
"
            .to_string(),
            1 => "end
"
            .to_string(),
            2 => "	x()
"
            .to_string(),
            _ => format!(
                "local v = {}
",
                rng.below(3)
            ),
        };
        let mut text = Vec::new();
        for kept in from {
            match rng.below(8) {
                0 => {}
                1 => text.push(line(rng)),
                2 => text.extend([line(rng), kept.clone()]),
                _ => text.push(kept.clone()),
            }
        }
        if rng.below(4) == 0 {
            text.push(line(rng));
        }
        text
    }

    /// `from` with lines no text holds yet added to it and put in place of
    /// some of its lines; nothing of it is removed without a new line in its
    /// place.
    fn with_new_lines(rng: &mut Rng, from: &[String], round: usize) -> Vec<String> {
        let mut text = Vec::new();
        let mut new = 0;
        let mut fresh = |text: &mut Vec<String>| {
            new += 1;
            text.push(format!(
                "new line {round} {new}
"
            ));
        };
        for kept in from {
            match rng.below(9) {
                0 => fresh(&mut text),
                1 => {
                    fresh(&mut text);
                    text.push(kept.clone());
                }
                _ => text.push(kept.clone()),
            }
        }
        if rng.below(4) == 0 {
            fresh(&mut text);
        }
        text
    }

    /// Among lines that repeat, a side that holds the other's edits and added
    /// only lines of its own is the merge, or the two conflict where it wrote
    /// into what the other had edited. A block of the other side is never
    /// written a second time, whichever side is called which.
    #[test]
    fn a_side_that_holds_the_others_edits_and_more_is_the_merge_among_repeating_lines() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let (mut clean, mut conflicts) = (0, 0);
        for round in 0..6_000 {
            let seed = (0..rng.below(10))
                .map(|_| String::new())
                .collect::<Vec<_>>();
            let base = repeating(&mut rng, &seed)
                .into_iter()
                .chain((0..rng.below(8)).map(|line| {
                    format!(
                        "local v = {}
",
                        line % 3
                    )
                }))
                .collect::<Vec<_>>();
            let files = repeating(&mut rng, &base);
            let studio = with_new_lines(&mut rng, &files, round);
            let (base, files, studio) = (base.concat(), files.concat(), studio.concat());
            let explain = || format!("round {round}: {base:?} {files:?} {studio:?}");
            match merge_script_lines(&base, &files, &studio, ConflictPreference::None) {
                Some(merged) => {
                    clean += 1;
                    assert!(merged == studio, "{} gave {merged:?}", explain());
                    for preference in ANY {
                        let preferred = merge_script_lines(&base, &files, &studio, preference);
                        assert!(preferred.as_deref() == Some(&studio), "{}", explain());
                    }
                }
                None => conflicts += 1,
            }
            let swapped = merge_script_lines(&base, &studio, &files, ConflictPreference::None);
            assert!(
                swapped.as_deref().is_none_or(|merged| merged == studio),
                "{} swapped gave {swapped:?}",
                explain()
            );
        }
        assert!(
            clean > 1_000 && conflicts > 200,
            "{clean} clean, {conflicts} conflicts"
        );
    }

    /// An anchor is one line: its text stands once in each of the three
    /// texts, at the places the anchor names, and the anchors keep one order.
    #[test]
    fn an_anchor_is_a_line_that_stands_once_on_every_side_in_one_order() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let mut anchored = 0;
        for _ in 0..3_000 {
            let seed = (0..rng.below(12))
                .map(|_| String::new())
                .collect::<Vec<_>>();
            let base = repeating(&mut rng, &seed)
                .into_iter()
                .enumerate()
                .map(|(place, line)| {
                    if place % 3 == 0 {
                        format!(
                            "only {place}
"
                        )
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>();
            let mut editor = repeating(&mut rng, &base);
            let studio = repeating(&mut rng, &base);
            if editor.len() > 2 && rng.below(3) == 0 {
                // A moved line.
                let line = editor.remove(0);
                editor.push(line);
            }
            let texts = [&base, &editor, &studio]
                .map(|text| text.iter().map(String::as_str).collect::<Vec<_>>());
            let found = anchors(&texts[0], &texts[1], &texts[2]);
            anchored += found.len();
            for pair in found.windows(2) {
                assert!(pair[0].0 < pair[1].0 && pair[0].1 < pair[1].1 && pair[0].2 < pair[1].2);
            }
            for (in_base, in_editor, in_studio) in found {
                let line = texts[0][in_base];
                assert!(texts[1][in_editor] == line && texts[2][in_studio] == line);
                for text in &texts {
                    assert_eq!(text.iter().filter(|other| **other == line).count(), 1);
                }
            }
        }
        assert!(anchored > 2_000, "{anchored} anchors");
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 as usize % bound
        }
    }

    /// One side's text: each line of the base kept, dropped or replaced, with
    /// lines added between. A text another side may also write at the same
    /// place carries the place in it, so one side never holds a line twice.
    fn edited(rng: &mut Rng, lines: usize, side: &str) -> String {
        let mut text = String::new();
        for line in 0..lines {
            match rng.below(12) {
                0 => text.push_str(&format!("added by {side} before {line}\n")),
                1 => text.push_str(&format!("added by both before {line}\n")),
                _ => {}
            }
            match rng.below(10) {
                0 => {}
                1 => text.push_str(&format!("line {line} as {side} has it\n")),
                2 => text.push_str(&format!("line {line} as both have it\n")),
                _ => text.push_str(&format!("line {line}\n")),
            }
        }
        if rng.below(8) == 0 {
            text.pop();
        }
        text
    }

    /// The laws of the merge on seeded edits of both sides, with every kind
    /// of edit at every distance from the other side's.
    #[test]
    fn seeded_merges_keep_the_laws_of_a_three_way_merge() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let (mut clean, mut conflicts) = (0, 0);
        for round in 0..4_000 {
            let count = 1 + rng.below(14);
            let base = (0..count)
                .map(|line| format!("line {line}\n"))
                .collect::<String>();
            let editor = edited(&mut rng, count, "the files");
            let studio = edited(&mut rng, count, "Studio");
            let merge = |preference| merge_script_lines(&base, &editor, &studio, preference);
            let none = merge(ConflictPreference::None);

            // A side that changed nothing takes the other's text, and two
            // equal sides are their own merge.
            for preference in ANY {
                assert!(
                    merge_script_lines(&base, &editor, &base, preference).as_deref()
                        == Some(&editor)
                );
                assert!(
                    merge_script_lines(&base, &base, &studio, preference).as_deref()
                        == Some(&studio)
                );
                assert!(
                    merge_script_lines(&base, &editor, &editor, preference).as_deref()
                        == Some(&editor)
                );
            }
            // The answer does not depend on which side is called which.
            assert!(
                none == merge_script_lines(&base, &studio, &editor, ConflictPreference::None),
                "round {round}"
            );
            // A preference only matters where there is a conflict, and with
            // one there is always a text.
            let (by_editor, by_studio) = (
                merge(ConflictPreference::Editor).unwrap(),
                merge(ConflictPreference::Studio).unwrap(),
            );
            match &none {
                Some(text) => {
                    clean += 1;
                    assert!(*text == by_editor && *text == by_studio, "round {round}");
                }
                None => conflicts += 1,
            }
            for merged in [&by_editor, &by_studio] {
                let mut seen = std::collections::HashSet::new();
                for line in lines(merged) {
                    // No line is written twice: no side holds one twice.
                    assert!(
                        seen.insert(line.trim_end_matches('\n')),
                        "round {round}: {line:?} twice in {merged:?}"
                    );
                    // Every line is one a side wrote or left.
                    assert!(
                        lines(&editor).contains(&line) || lines(&studio).contains(&line),
                        "round {round}: {line:?} is from neither side"
                    );
                }
            }
            // A line both sides dropped or replaced does not come back, and
            // a line a side added that the other did not contest is there.
            for line in lines(&base) {
                let kept = lines(&editor).contains(&line) || lines(&studio).contains(&line);
                assert!(kept || !lines(&by_editor).contains(&line), "round {round}");
            }
            if let Some(text) = &none {
                for line in lines(&editor).into_iter().chain(lines(&studio)) {
                    let added = !lines(&base).contains(&line);
                    assert!(
                        !added || lines(text).contains(&line),
                        "round {round}: {line:?} lost"
                    );
                }
            }
        }
        assert!(
            clean > 500 && conflicts > 500,
            "{clean} clean, {conflicts} conflicts"
        );
    }
}
