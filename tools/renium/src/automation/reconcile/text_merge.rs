//! Three-way merge of one script's text by line.
//!
//! A line that stands unchanged in the last-sync text and on both sides is
//! kept. The lines between two such lines form one region; a region comes
//! whole from one side, never mixed: from the side that changed it when the
//! other left it alone, from either when both made it the same, and from the
//! preferred side when both changed it differently. Every merged line is
//! therefore a line of one side at its place there, and none is written
//! twice because both sides hold it.

use similar::{Algorithm, DiffOp, capture_diff_slices};

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
    let in_editor = unchanged_lines(&base, &editor);
    let in_studio = unchanged_lines(&base, &studio);
    let mut merged = String::new();
    let (mut at_base, mut at_editor, mut at_studio) = (0, 0, 0);
    loop {
        let kept =
            (at_base..base.len()).find_map(|line| Some((line, in_editor[line]?, in_studio[line]?)));
        let (to_base, to_editor, to_studio) =
            kept.unwrap_or((base.len(), editor.len(), studio.len()));
        let region = region(
            &base[at_base..to_base],
            &editor[at_editor..to_editor],
            &studio[at_studio..to_studio],
            preference,
        )?;
        region.iter().for_each(|line| merged.push_str(line));
        if kept.is_none() {
            return Some(merged);
        }
        merged.push_str(base[to_base]);
        (at_base, at_editor, at_studio) = (to_base + 1, to_editor + 1, to_studio + 1);
    }
}

/// The lines a region ends with, or `None` for a conflict nobody settles.
fn region<'a>(
    base: &[&'a str],
    editor: &'a [&'a str],
    studio: &'a [&'a str],
    preference: ConflictPreference,
) -> Option<&'a [&'a str]> {
    if editor == studio || studio == base {
        return Some(editor);
    }
    if editor == base {
        return Some(studio);
    }
    // Both sides added lines where there were none, and one of them added the
    // other's lines and more: a sync that stopped halfway leaves this.
    if base.is_empty() {
        if studio.starts_with(editor) || studio.ends_with(editor) {
            return Some(studio);
        }
        if editor.starts_with(studio) || editor.ends_with(studio) {
            return Some(editor);
        }
    }
    match preference {
        ConflictPreference::Editor => Some(editor),
        ConflictPreference::Studio => Some(studio),
        ConflictPreference::None => None,
    }
}

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// For each line of `base`, where it stands unchanged in `side`.
fn unchanged_lines(base: &[&str], side: &[&str]) -> Vec<Option<usize>> {
    let mut places = vec![None; base.len()];
    for op in capture_diff_slices(Algorithm::Myers, base, side) {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            for offset in 0..len {
                places[old_index + offset] = Some(new_index + offset);
            }
        }
    }
    places
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
