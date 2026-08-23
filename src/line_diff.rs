use std::collections::HashMap;

const CONTEXT_LINES: usize = 3;
const MAX_EXACT_CELLS: usize = 1_000_000;
const MAX_RECURSION_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChangeTag {
    Equal,
    Delete,
    Insert,
}

#[derive(Clone, Copy, Debug)]
struct Change<'a> {
    tag: ChangeTag,
    line: &'a str,
}

pub(crate) fn render_diff(old: &str, new: &str) -> (usize, usize, String) {
    if old == new {
        return (0, 0, String::new());
    }

    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let mut changes = Vec::with_capacity(old_lines.len().saturating_add(new_lines.len()));
    diff_region(
        &old_lines,
        &new_lines,
        0,
        old_lines.len(),
        0,
        new_lines.len(),
        0,
        &mut changes,
    );

    let added = changes
        .iter()
        .filter(|change| change.tag == ChangeTag::Insert)
        .count();
    let removed = changes
        .iter()
        .filter(|change| change.tag == ChangeTag::Delete)
        .count();
    let unified = render_unified(&changes);
    (added, removed, unified.trim_end().to_owned())
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split_inclusive('\n').collect()
    }
}

#[allow(clippy::too_many_arguments)]
fn diff_region<'a>(
    old: &[&'a str],
    new: &[&'a str],
    mut old_start: usize,
    mut old_end: usize,
    mut new_start: usize,
    mut new_end: usize,
    depth: usize,
    out: &mut Vec<Change<'a>>,
) {
    while old_start < old_end && new_start < new_end && old[old_start] == new[new_start] {
        out.push(Change {
            tag: ChangeTag::Equal,
            line: old[old_start],
        });
        old_start += 1;
        new_start += 1;
    }

    let mut common_suffix = 0;
    while old_start < old_end && new_start < new_end && old[old_end - 1] == new[new_end - 1] {
        common_suffix += 1;
        old_end -= 1;
        new_end -= 1;
    }

    if old_start == old_end {
        push_range(out, ChangeTag::Insert, &new[new_start..new_end]);
    } else if new_start == new_end {
        push_range(out, ChangeTag::Delete, &old[old_start..old_end]);
    } else if exact_cells(old_end - old_start, new_end - new_start) <= MAX_EXACT_CELLS {
        exact_lcs(&old[old_start..old_end], &new[new_start..new_end], out);
    } else if depth >= MAX_RECURSION_DEPTH {
        push_range(out, ChangeTag::Delete, &old[old_start..old_end]);
        push_range(out, ChangeTag::Insert, &new[new_start..new_end]);
    } else {
        let anchors = patience_anchors(&old[old_start..old_end], &new[new_start..new_end]);
        if anchors.is_empty() {
            push_range(out, ChangeTag::Delete, &old[old_start..old_end]);
            push_range(out, ChangeTag::Insert, &new[new_start..new_end]);
        } else {
            let mut previous_old = old_start;
            let mut previous_new = new_start;
            for (relative_old, relative_new) in anchors {
                let anchor_old = old_start + relative_old;
                let anchor_new = new_start + relative_new;
                diff_region(
                    old,
                    new,
                    previous_old,
                    anchor_old,
                    previous_new,
                    anchor_new,
                    depth + 1,
                    out,
                );
                out.push(Change {
                    tag: ChangeTag::Equal,
                    line: old[anchor_old],
                });
                previous_old = anchor_old + 1;
                previous_new = anchor_new + 1;
            }
            diff_region(
                old,
                new,
                previous_old,
                old_end,
                previous_new,
                new_end,
                depth + 1,
                out,
            );
        }
    }

    let suffix_old_start = old_end;
    for offset in 0..common_suffix {
        out.push(Change {
            tag: ChangeTag::Equal,
            line: old[suffix_old_start + offset],
        });
    }
}

fn exact_cells(old_len: usize, new_len: usize) -> usize {
    old_len
        .checked_add(1)
        .and_then(|old| new_len.checked_add(1).and_then(|new| old.checked_mul(new)))
        .unwrap_or(usize::MAX)
}

fn exact_lcs<'a>(old: &[&'a str], new: &[&'a str], out: &mut Vec<Change<'a>>) {
    let width = new.len() + 1;
    let mut lengths = vec![0_u32; (old.len() + 1) * width];

    for old_index in (0..old.len()).rev() {
        for new_index in (0..new.len()).rev() {
            let index = old_index * width + new_index;
            lengths[index] = if old[old_index] == new[new_index] {
                lengths[(old_index + 1) * width + new_index + 1] + 1
            } else {
                lengths[(old_index + 1) * width + new_index]
                    .max(lengths[old_index * width + new_index + 1])
            };
        }
    }

    let mut old_index = 0;
    let mut new_index = 0;
    while old_index < old.len() && new_index < new.len() {
        if old[old_index] == new[new_index] {
            out.push(Change {
                tag: ChangeTag::Equal,
                line: old[old_index],
            });
            old_index += 1;
            new_index += 1;
        } else if lengths[(old_index + 1) * width + new_index]
            >= lengths[old_index * width + new_index + 1]
        {
            out.push(Change {
                tag: ChangeTag::Delete,
                line: old[old_index],
            });
            old_index += 1;
        } else {
            out.push(Change {
                tag: ChangeTag::Insert,
                line: new[new_index],
            });
            new_index += 1;
        }
    }
    push_range(out, ChangeTag::Delete, &old[old_index..]);
    push_range(out, ChangeTag::Insert, &new[new_index..]);
}

fn patience_anchors(old: &[&str], new: &[&str]) -> Vec<(usize, usize)> {
    let mut old_occurrences: HashMap<&str, (usize, usize)> = HashMap::new();
    for (index, line) in old.iter().copied().enumerate() {
        old_occurrences
            .entry(line)
            .and_modify(|entry| entry.0 += 1)
            .or_insert((1, index));
    }

    let mut new_occurrences: HashMap<&str, (usize, usize)> = HashMap::new();
    for (index, line) in new.iter().copied().enumerate() {
        new_occurrences
            .entry(line)
            .and_modify(|entry| entry.0 += 1)
            .or_insert((1, index));
    }

    let mut candidates = Vec::new();
    for (old_index, line) in old.iter().copied().enumerate() {
        let Some(&(old_count, _)) = old_occurrences.get(line) else {
            continue;
        };
        let Some(&(new_count, new_index)) = new_occurrences.get(line) else {
            continue;
        };
        if old_count == 1 && new_count == 1 {
            candidates.push((old_index, new_index));
        }
    }
    longest_increasing_new_indices(&candidates)
}

fn longest_increasing_new_indices(candidates: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut tails: Vec<usize> = Vec::new();
    let mut previous: Vec<Option<usize>> = vec![None; candidates.len()];

    for candidate_index in 0..candidates.len() {
        let new_index = candidates[candidate_index].1;
        let position = tails.partition_point(|&tail_index| candidates[tail_index].1 < new_index);
        if position > 0 {
            previous[candidate_index] = Some(tails[position - 1]);
        }
        if position == tails.len() {
            tails.push(candidate_index);
        } else {
            tails[position] = candidate_index;
        }
    }

    let mut selected = Vec::with_capacity(tails.len());
    let mut current = tails.last().copied();
    while let Some(index) = current {
        selected.push(candidates[index]);
        current = previous[index];
    }
    selected.reverse();
    selected
}

fn push_range<'a>(out: &mut Vec<Change<'a>>, tag: ChangeTag, lines: &[&'a str]) {
    out.extend(lines.iter().copied().map(|line| Change { tag, line }));
}

fn render_unified(changes: &[Change<'_>]) -> String {
    let changed_indices: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter_map(|(index, change)| (change.tag != ChangeTag::Equal).then_some(index))
        .collect();
    if changed_indices.is_empty() {
        return String::new();
    }

    let mut old_positions = Vec::with_capacity(changes.len() + 1);
    let mut new_positions = Vec::with_capacity(changes.len() + 1);
    let mut old_position = 0;
    let mut new_position = 0;
    for change in changes {
        old_positions.push(old_position);
        new_positions.push(new_position);
        match change.tag {
            ChangeTag::Equal => {
                old_position += 1;
                new_position += 1;
            }
            ChangeTag::Delete => old_position += 1,
            ChangeTag::Insert => new_position += 1,
        }
    }
    old_positions.push(old_position);
    new_positions.push(new_position);

    let mut hunks = Vec::new();
    for changed_index in changed_indices {
        let start = changed_index.saturating_sub(CONTEXT_LINES);
        let end = (changed_index + CONTEXT_LINES + 1).min(changes.len());
        if let Some((_, previous_end)) = hunks.last_mut()
            && start <= *previous_end
        {
            *previous_end = (*previous_end).max(end);
            continue;
        }
        hunks.push((start, end));
    }

    let mut rendered = String::new();
    for (start, end) in hunks {
        let old_count = changes[start..end]
            .iter()
            .filter(|change| change.tag != ChangeTag::Insert)
            .count();
        let new_count = changes[start..end]
            .iter()
            .filter(|change| change.tag != ChangeTag::Delete)
            .count();
        rendered.push_str("@@ -");
        rendered.push_str(&format_range(old_positions[start], old_count));
        rendered.push_str(" +");
        rendered.push_str(&format_range(new_positions[start], new_count));
        rendered.push_str(" @@\n");

        for change in &changes[start..end] {
            let prefix = match change.tag {
                ChangeTag::Equal => ' ',
                ChangeTag::Delete => '-',
                ChangeTag::Insert => '+',
            };
            push_unified_line(&mut rendered, prefix, change.line);
        }
    }
    rendered
}

fn format_range(start: usize, count: usize) -> String {
    match count {
        0 => format!("{start},0"),
        1 => (start + 1).to_string(),
        _ => format!("{},{}", start + 1, count),
    }
}

fn push_unified_line(rendered: &mut String, prefix: char, line: &str) {
    rendered.push(prefix);
    rendered.push_str(line);
    if !line.ends_with('\n') {
        rendered.push('\n');
        rendered.push_str("\\ No newline at end of file\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use similar::{ChangeTag as SimilarTag, TextDiff};

    fn oracle(old: &str, new: &str) -> (usize, usize, String) {
        let diff = TextDiff::from_lines(old, new);
        let mut added = 0;
        let mut removed = 0;
        for change in diff.iter_all_changes() {
            match change.tag() {
                SimilarTag::Insert => added += 1,
                SimilarTag::Delete => removed += 1,
                SimilarTag::Equal => {}
            }
        }
        let rendered = diff
            .unified_diff()
            .context_radius(CONTEXT_LINES)
            .to_string();
        (added, removed, rendered.trim_end().to_owned())
    }

    #[test]
    fn basic_unified_diff_matches_oracle() {
        let cases = [
            ("one\ntwo\n", "one\nchanged\nthree\n"),
            ("", "new\n"),
            ("old\n", ""),
            ("one\ntwo\nthree\n", "one\ntwo\nthree\n"),
            ("one\ntwo", "one\nchanged"),
            ("a\nb\nc\nd\ne\nf\ng\nh\n", "a\nB\nc\nd\ne\nf\nG\nh\n"),
        ];
        for (old, new) in cases {
            assert_eq!(render_diff(old, new), oracle(old, new));
        }
    }

    #[test]
    fn generated_unique_line_edits_match_similar_oracle() -> noprop::TestResult {
        test_support::run(0x4c49_4e45_4449_4646, 4096, |ctx| {
            let old_len = noprop::sample_usize_in(ctx, 0..=24);
            let mut old = String::new();
            let mut new = String::new();
            let mut next_insert = 0usize;

            for index in 0..old_len {
                let line = format!("shared-{index}\n");
                old.push_str(&line);

                let inserts_before = noprop::sample_usize_in(ctx, 0..=2);
                for _ in 0..inserts_before {
                    new.push_str(&format!("insert-{next_insert}\n"));
                    next_insert += 1;
                }

                match noprop::sample_usize_in(ctx, 0..=3) {
                    0 => {}
                    1 => {
                        new.push_str(&format!("replace-{index}\n"));
                    }
                    _ => new.push_str(&line),
                }
            }
            let trailing = noprop::sample_usize_in(ctx, 0..=2);
            for _ in 0..trailing {
                new.push_str(&format!("insert-{next_insert}\n"));
                next_insert += 1;
            }

            assert_eq!(render_diff(&old, &new), oracle(&old, &new));
            Ok(())
        })
    }

    #[test]
    fn generated_small_arbitrary_edits_match_oracle_counts() -> noprop::TestResult {
        test_support::run(0x4c49_4e45_434f_554e, 4096, |ctx| {
            const TOKENS: &[&str] = &["a\n", "b\n", "c\n", "d\n", "x\n", "y\n"];
            let old_len = noprop::sample_usize_in(ctx, 0..=20);
            let new_len = noprop::sample_usize_in(ctx, 0..=20);
            let mut old = String::new();
            let mut new = String::new();
            for _ in 0..old_len {
                old.push_str(TOKENS[noprop::sample_usize_in(ctx, 0..TOKENS.len())]);
            }
            for _ in 0..new_len {
                new.push_str(TOKENS[noprop::sample_usize_in(ctx, 0..TOKENS.len())]);
            }

            let actual = render_diff(&old, &new);
            let expected = oracle(&old, &new);
            assert_eq!((actual.0, actual.1), (expected.0, expected.1));
            assert_eq!(actual.2.is_empty(), old == new);
            Ok(())
        })
    }

    #[test]
    fn large_distinct_inputs_use_bounded_fallback() {
        let old = (0..4_000)
            .map(|index| format!("old-{index}\n"))
            .collect::<String>();
        let new = (0..4_000)
            .map(|index| format!("new-{index}\n"))
            .collect::<String>();
        let (added, removed, unified) = render_diff(&old, &new);
        assert_eq!(added, 4_000);
        assert_eq!(removed, 4_000);
        assert!(unified.starts_with("@@ -1,4000 +1,4000 @@\n"));
    }

    #[test]
    fn patience_anchors_preserve_order() {
        let old = [
            "a\n",
            "left\n",
            "anchor-1\n",
            "middle\n",
            "anchor-2\n",
            "right\n",
        ];
        let new = [
            "a\n",
            "other\n",
            "anchor-1\n",
            "changed\n",
            "anchor-2\n",
            "tail\n",
        ];
        assert_eq!(patience_anchors(&old, &new), vec![(0, 0), (2, 2), (4, 4)]);
    }
}
