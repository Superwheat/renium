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
//! both made it the same. A region both changed, each keeping its number of
//! lines and moving none of them, is settled by rows: the rows
//! no side changed part it into runs, and a run only one side changed is
//! that side's. Everything else both sides changed is a conflict, and a
//! conflict leaves no merged text at all: the caller takes one side's whole
//! script, so no text is ever put together from two answers to one conflict.

use std::collections::HashMap;

/// The merged text, or `None` when the two sides conflict.
pub(super) fn merge_script_lines(base: &str, editor: &str, studio: &str) -> Option<String> {
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

/// The lines a region ends with, or `None` for a conflict.
fn region<'a>(base: &[&'a str], editor: &[&'a str], studio: &[&'a str]) -> Option<Vec<&'a str>> {
    if let Some(side) = changed_side(base, editor, studio) {
        return Some(side.to_vec());
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
    if !rows_are_the_same_lines(base, editor, studio) {
        return None;
    }
    let quiet = |row: usize| base[row] == editor[row] && base[row] == studio[row];
    let mut merged = Vec::with_capacity(base.len());
    let mut row = 0;
    while row < base.len() {
        // A run of rows a side changed ends at the next row all three hold
        // alike; a row no side changed is a run of its own.
        let end = (row + 1..base.len())
            .find(|next| quiet(row) || quiet(*next))
            .unwrap_or(base.len());
        merged.extend(changed_side(
            &base[row..end],
            &editor[row..end],
            &studio[row..end],
        )?);
        row = end;
    }
    Some(merged)
}

/// The side whose lines stand where at most one side changed them, or both
/// changed them alike.
fn changed_side<'s, 'a>(
    base: &[&'a str],
    editor: &'s [&'a str],
    studio: &'s [&'a str],
) -> Option<&'s [&'a str]> {
    if editor == studio || studio == base {
        Some(editor)
    } else if editor == base {
        Some(studio)
    } else {
        None
    }
}

/// Whether every row of the region is one line on all three sides: each side
/// has as many lines as there were, and a text a side wrote into a row
/// stands nowhere on the other side but in that row. A side that moved a
/// line, or took one out and put another in elsewhere, wrote a text the
/// other side still holds in the row it was in, and fails this.
fn rows_are_the_same_lines(base: &[&str], editor: &[&str], studio: &[&str]) -> bool {
    if base.len() != editor.len() || base.len() != studio.len() {
        return false;
    }
    let (in_editor, in_studio) = (places(editor), places(studio));
    let wrote_in_place = |side: &[&str], other: &HashMap<&str, (usize, usize)>| {
        side.iter().enumerate().all(|(row, line)| {
            *line == base[row]
                || other
                    .get(line)
                    .is_none_or(|(first, count)| *count == 1 && *first == row)
        })
    };
    wrote_in_place(editor, &in_studio) && wrote_in_place(studio, &in_editor)
}

/// The first row each text of `lines` stands in, and in how many rows.
fn places<'a>(lines: &[&'a str]) -> HashMap<&'a str, (usize, usize)> {
    let mut places = HashMap::with_capacity(lines.len());
    for (row, line) in lines.iter().enumerate() {
        places.entry(*line).or_insert((row, 0)).1 += 1;
    }
    places
}

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_added_next_to_a_line_the_other_side_changed_is_written_once() {
        for (base, editor, studio) in [
            ("a\nb\n", "a\nX\nb\n", "a\nX\nB\n"),
            ("a\nb\n", "a\nX\nb\n", "A\nX\nb\n"),
            ("a\n", "X\na\n", "X\nA\n"),
            ("a\nb\n", "a\nb\nX\n", "a\nB\nX\n"),
        ] {
            let merged = merge_script_lines(base, editor, studio);
            assert!(
                merged
                    .as_deref()
                    .is_none_or(|text| text == editor || text == studio),
                "{base:?}: {merged:?}"
            );
        }
    }

    /// Lines that repeat between two edits part them like any line both
    /// sides left alone.
    #[test]
    fn edits_parted_by_lines_that_repeat_are_both_kept() {
        for (base, editor, studio, both) in [
            (
                "a = 1\ntask.wait(1)\nb = 1\ntask.wait(1)\nc = 1\n",
                "a = 2\ntask.wait(1)\nb = 1\ntask.wait(1)\nc = 1\n",
                "a = 1\ntask.wait(1)\nb = 2\ntask.wait(1)\nc = 1\n",
                "a = 2\ntask.wait(1)\nb = 2\ntask.wait(1)\nc = 1\n",
            ),
            (
                "local t = {\n\t0,\n\t0,\n\t1,\n\t0,\n}\n",
                "local t = {\n\t5,\n\t0,\n\t1,\n\t0,\n}\n",
                "local t = {\n\t0,\n\t0,\n\t1,\n\t7,\n}\n",
                "local t = {\n\t5,\n\t0,\n\t1,\n\t7,\n}\n",
            ),
            (
                "a()\n\n\nb()\n\n\nc()\n\n\n",
                "A()\n\n\nb()\n\n\nc()\n\n\n",
                "a()\n\n\nB()\n\n\nc()\n\n\n",
                "A()\n\n\nB()\n\n\nc()\n\n\n",
            ),
        ] {
            assert_eq!(
                merge_script_lines(base, editor, studio).as_deref(),
                Some(both)
            );
            assert_eq!(
                merge_script_lines(base, studio, editor).as_deref(),
                Some(both)
            );
        }
    }

    /// Rows next to each other changed by one side each, a rename one side
    /// made across a row the other edited, and a moved line beside an edit
    /// are conflicts: no text is put together from parts of both answers.
    #[test]
    fn edits_that_touch_or_cross_a_moved_line_are_a_conflict() {
        for (base, editor, studio) in [
            ("a\nb\nc\n", "a\nB\nc\n", "a\nb\nC\n"),
            (
                "a\nb\nc\nd\n",
                "a\nb files\nc files\nd\n",
                "a\nb studio\nc\nd studio\n",
            ),
            (
                "local x = 1\nprint(x)\nreturn 0\n",
                "local y = 1\nprint(y)\nreturn 0\n",
                "local x = 1\nprint(x + 1)\nreturn 0\n",
            ),
            ("a\nb\nc\nd\n", "a\nc\nb\nd\n", "a\nb\nc\nD\n"),
            ("a\nb\nc\n", "b\na\nc\n", "a\nB\nc\n"),
            ("p\nq\n", "q\nN\n", "p\nQ\n"),
            // The same new text written into different rows.
            ("\n\n\n\n", "N\n\n\n\n", "\n\n\nN\n"),
            // A line taken out and another put in further down.
            ("x\ny\ny\ny\nw\n", "y\ny\ny\nw\nz\n", "x\ny\nY\ny\nw\n"),
        ] {
            assert_eq!(merge_script_lines(base, editor, studio), None, "{base:?}");
            assert_eq!(merge_script_lines(base, studio, editor), None, "{base:?}");
        }
    }

    /// A block both sides hold is one block, also where its lines repeat
    /// lines around it.
    #[test]
    fn a_block_both_sides_hold_is_written_once_where_blank_lines_and_ends_repeat() {
        for (base, editor, studio) in [
            (
                "local a = 1\n\nreturn a\n",
                "local a = 1\n\nlocal b = 2\n\nreturn a\n",
                "local a = 10\n\nlocal b = 2\n\nreturn a\n",
            ),
            (
                "local function f()\nend\n\nreturn f\n",
                "local function f()\nend\n\nlocal function g()\nend\n\nreturn f\n",
                "local function f()\n\tprint(1)\nend\n\nlocal function g()\nend\n\nreturn f\n",
            ),
            (
                "if a then\nend\nreturn a\n",
                "if a then\nend\nif b then\nend\nreturn a\n",
                "if a then\n\tx()\nend\nif b then\nend\nreturn a\n",
            ),
            (
                "x\ny\nx\ny\nz\n",
                "x\nx\ny\nx\ny\nz\n",
                "x\ny\nx\ny\nx\ny\nz\n",
            ),
        ] {
            let merged = merge_script_lines(base, editor, studio);
            assert!(
                merged
                    .as_deref()
                    .is_none_or(|text| text == editor || text == studio),
                "{merged:?}"
            );
        }
    }

    /// A text made of a few repeating lines, edited at random.
    fn repeating(rng: &mut Rng, from: &[String]) -> Vec<String> {
        let line = |rng: &mut Rng| match rng.below(5) {
            0 => "\n".to_string(),
            1 => "end\n".to_string(),
            2 => "\tx()\n".to_string(),
            _ => format!("local v = {}\n", rng.below(3)),
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
            text.push(format!("new line {round} {new}\n"));
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
                .chain((0..rng.below(8)).map(|line| format!("local v = {}\n", line % 3)))
                .collect::<Vec<_>>();
            let files = repeating(&mut rng, &base);
            let studio = with_new_lines(&mut rng, &files, round);
            let (base, files, studio) = (base.concat(), files.concat(), studio.concat());
            let explain = || format!("round {round}: {base:?} {files:?} {studio:?}");
            let merged = merge_script_lines(&base, &files, &studio);
            match &merged {
                Some(merged) => {
                    clean += 1;
                    assert!(*merged == studio, "{} gave {merged:?}", explain());
                }
                None => conflicts += 1,
            }
            // The answer does not depend on which side is called which.
            assert!(
                merged == merge_script_lines(&base, &studio, &files),
                "{} differs when swapped",
                explain()
            );
        }
        assert!(
            clean > 1_000 && conflicts > 200,
            "{clean} clean, {conflicts} conflicts"
        );
    }

    /// Among lines that repeat, a line each side put a new one in place of
    /// is replaced by both when a line stands between the two, and the two
    /// conflict when they are the same line or neighbours.
    #[test]
    fn replaced_rows_of_a_repeating_text_merge_unless_they_touch() {
        let mut rng = Rng(0x0dd0_5eed_1234_abcd);
        let (mut clean, mut conflicts) = (0, 0);
        for round in 0..3_000 {
            let count = 2 + rng.below(12);
            let base = (0..count)
                .map(|_| ["\n", "end\n", "\tx()\n"][rng.below(3)].to_string())
                .collect::<Vec<_>>();
            let (at_files, at_studio) = (rng.below(count), rng.below(count));
            let (mut files, mut studio) = (base.clone(), base.clone());
            files[at_files] = format!("files {round}\n");
            studio[at_studio] = format!("studio {round}\n");
            let mut both = files.clone();
            both[at_studio] = studio[at_studio].clone();
            let merged = merge_script_lines(&base.concat(), &files.concat(), &studio.concat());
            if at_files.abs_diff(at_studio) > 1 {
                clean += 1;
                assert_eq!(merged, Some(both.concat()), "round {round}: {base:?}");
            } else {
                conflicts += 1;
                assert_eq!(merged, None, "round {round}: {base:?}");
            }
        }
        assert!(
            clean > 1_000 && conflicts > 300,
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
                        format!("only {place}\n")
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

            // A side that changed nothing takes the other's text, and two
            // equal sides are their own merge.
            assert!(merge_script_lines(&base, &editor, &base).as_deref() == Some(&editor));
            assert!(merge_script_lines(&base, &base, &studio).as_deref() == Some(&studio));
            assert!(merge_script_lines(&base, &editor, &editor).as_deref() == Some(&editor));

            let merged = merge_script_lines(&base, &editor, &studio);
            // The answer does not depend on which side is called which.
            assert!(
                merged == merge_script_lines(&base, &studio, &editor),
                "round {round}"
            );
            let Some(merged) = merged else {
                conflicts += 1;
                continue;
            };
            clean += 1;
            let mut seen = std::collections::HashSet::new();
            for line in lines(&merged) {
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
            // A line a side dropped or replaced does not come back, and a
            // line a side added is there.
            for line in lines(&base) {
                let kept = lines(&editor).contains(&line) && lines(&studio).contains(&line);
                assert!(
                    kept == lines(&merged).contains(&line),
                    "round {round}: {line:?} in {merged:?}"
                );
            }
            for line in lines(&editor).into_iter().chain(lines(&studio)) {
                let added = !lines(&base).contains(&line);
                assert!(
                    !added || lines(&merged).contains(&line),
                    "round {round}: {line:?} lost"
                );
            }
        }
        assert!(
            clean > 500 && conflicts > 500,
            "{clean} clean, {conflicts} conflicts"
        );
    }
}
