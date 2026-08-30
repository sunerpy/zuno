//! Normalized comparison of two outputs, with masking made visible.
//!
//! [`diff_normalized`] is the harness's verdict function. Its contract is
//! narrower than it looks: it reports *every* difference that survives the rules
//! it was given, and it prints which rules fired so a reader can see what was
//! masked. It never decides that a difference is unimportant.

use std::collections::BTreeMap;

use crate::normalize::Normalizer;

/// One surviving difference between two normalized outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// 1-based line number in the left (oracle) output, or the insertion point.
    pub left_line: usize,
    /// 1-based line number in the right (subject) output, or the insertion point.
    pub right_line: usize,
    /// The left line, absent when the right side has an extra line.
    pub left: Option<String>,
    /// The right line, absent when the right side is missing a line.
    pub right: Option<String>,
}

/// The result of comparing two outputs under a [`Normalizer`].
#[derive(Debug, Clone)]
pub struct DiffReport {
    /// What the left side is, e.g. `oracle(installed-binary, reports 1.18.12)`.
    pub left_label: String,
    /// What the right side is, e.g. `subject(zuno 0.1.0)`.
    pub right_label: String,
    /// Every difference that survived normalization.
    pub divergences: Vec<Divergence>,
    /// Which rules fired, and how many spans each one masked, summed over both
    /// sides. An empty map on a passing diff means the two sides agreed byte for
    /// byte with nothing hidden.
    pub rules_fired: BTreeMap<String, usize>,
    /// Every rule that was available, whether or not it fired.
    pub rules_available: Vec<String>,
    /// The normalized left text, retained so a failure can be read in full.
    pub left_normalized: String,
    /// The normalized right text.
    pub right_normalized: String,
    /// True when the inputs were too large for the exact line alignment and a
    /// positional comparison was used instead. Positional comparison never hides
    /// a difference; it only reports shifted lines more noisily.
    pub alignment_degraded: bool,
}

/// Inputs above this many lines fall back to positional comparison, because the
/// exact alignment is quadratic in the line count.
const ALIGNMENT_LINE_BUDGET: usize = 2_000;

impl DiffReport {
    /// True when nothing survived normalization.
    #[must_use]
    pub fn is_identical(&self) -> bool {
        self.divergences.is_empty()
    }

    /// The number of surviving differences.
    #[must_use]
    pub fn divergence_count(&self) -> usize {
        self.divergences.len()
    }

    /// Panic with the full rendered report unless the two sides agree.
    ///
    /// # Panics
    ///
    /// If any difference survived normalization.
    pub fn assert_identical(&self) {
        assert!(self.is_identical(), "{}", self.render());
    }

    /// A human-readable report: what was compared, what was masked, what differs.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "differential: {} vs {}",
            self.left_label, self.right_label
        );
        let _ = writeln!(
            out,
            "  normalization rules available: {}",
            if self.rules_available.is_empty() {
                "(none — byte-exact comparison)".to_owned()
            } else {
                self.rules_available.join(", ")
            }
        );
        if self.rules_fired.is_empty() {
            let _ = writeln!(out, "  normalization masked: nothing");
        } else {
            let masked = self
                .rules_fired
                .iter()
                .map(|(name, count)| format!("{name}x{count}"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "  normalization masked: {masked}");
        }
        if self.alignment_degraded {
            let _ = writeln!(
                out,
                "  note: output exceeded {ALIGNMENT_LINE_BUDGET} lines, compared positionally"
            );
        }
        if self.divergences.is_empty() {
            let _ = writeln!(out, "  result: identical");
            return out;
        }
        let _ = writeln!(out, "  result: {} divergence(s)", self.divergences.len());
        for d in &self.divergences {
            match (&d.left, &d.right) {
                (Some(l), Some(r)) => {
                    let _ = writeln!(out, "  @ L{}/R{}", d.left_line, d.right_line);
                    let _ = writeln!(out, "    - {l}");
                    let _ = writeln!(out, "    + {r}");
                }
                (Some(l), None) => {
                    let _ = writeln!(out, "  @ L{} (missing on the right)", d.left_line);
                    let _ = writeln!(out, "    - {l}");
                }
                (None, Some(r)) => {
                    let _ = writeln!(out, "  @ R{} (extra on the right)", d.right_line);
                    let _ = writeln!(out, "    + {r}");
                }
                (None, None) => {}
            }
        }
        out
    }
}

/// Compare two outputs after applying `normalizer`, reporting every difference
/// that survives.
///
/// `left` is conventionally the oracle and `right` the subject; the labels are
/// carried into the report so a failure can never be misattributed to the wrong
/// side or the wrong oracle version.
#[must_use]
pub fn diff_normalized(
    left_label: impl Into<String>,
    left: &str,
    right_label: impl Into<String>,
    right: &str,
    normalizer: &Normalizer,
) -> DiffReport {
    let (left_normalized, left_fired) = normalizer.apply(left);
    let (right_normalized, right_fired) = normalizer.apply(right);

    let mut rules_fired = left_fired;
    for (name, count) in right_fired {
        *rules_fired.entry(name).or_default() += count;
    }

    let left_lines: Vec<&str> = left_normalized.lines().collect();
    let right_lines: Vec<&str> = right_normalized.lines().collect();
    let degraded =
        left_lines.len() > ALIGNMENT_LINE_BUDGET || right_lines.len() > ALIGNMENT_LINE_BUDGET;
    let divergences = if degraded {
        positional(&left_lines, &right_lines)
    } else {
        aligned(&left_lines, &right_lines)
    };

    DiffReport {
        left_label: left_label.into(),
        right_label: right_label.into(),
        divergences,
        rules_fired,
        rules_available: normalizer
            .rule_names()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        left_normalized,
        right_normalized,
        alignment_degraded: degraded,
    }
}

fn positional(left: &[&str], right: &[&str]) -> Vec<Divergence> {
    let mut out = Vec::new();
    for i in 0..left.len().max(right.len()) {
        let l = left.get(i);
        let r = right.get(i);
        if l == r {
            continue;
        }
        out.push(Divergence {
            left_line: i + 1,
            right_line: i + 1,
            left: l.map(|s| (*s).to_owned()),
            right: r.map(|s| (*s).to_owned()),
        });
    }
    out
}

/// Longest-common-subsequence alignment, so an inserted line is reported once
/// instead of cascading through every following line.
fn aligned(left: &[&str], right: &[&str]) -> Vec<Divergence> {
    let (n, m) = (left.len(), right.len());
    let mut lcs = vec![0usize; (n + 1) * (m + 1)];
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[idx(i, j)] = if left[i] == right[j] {
                lcs[idx(i + 1, j + 1)] + 1
            } else {
                lcs[idx(i + 1, j)].max(lcs[idx(i, j + 1)])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if left[i] == right[j] {
            i += 1;
            j += 1;
        } else if lcs[idx(i + 1, j)] >= lcs[idx(i, j + 1)] {
            out.push(Divergence {
                left_line: i + 1,
                right_line: j + 1,
                left: Some(left[i].to_owned()),
                right: None,
            });
            i += 1;
        } else {
            out.push(Divergence {
                left_line: i + 1,
                right_line: j + 1,
                left: None,
                right: Some(right[j].to_owned()),
            });
            j += 1;
        }
    }
    while i < n {
        out.push(Divergence {
            left_line: i + 1,
            right_line: m + 1,
            left: Some(left[i].to_owned()),
            right: None,
        });
        i += 1;
    }
    while j < m {
        out.push(Divergence {
            left_line: n + 1,
            right_line: j + 1,
            left: None,
            right: Some(right[j].to_owned()),
        });
        j += 1;
    }
    coalesce(out)
}

/// Pair a delete immediately followed by an insert at the same position into a
/// single change, which is how a one-line edit reads to a human.
fn coalesce(input: Vec<Divergence>) -> Vec<Divergence> {
    let mut out: Vec<Divergence> = Vec::with_capacity(input.len());
    for d in input {
        match out.last_mut() {
            Some(prev)
                if prev.right.is_none()
                    && d.left.is_none()
                    && prev.left_line + 1 == d.left_line
                    && prev.right_line == d.right_line =>
            {
                prev.right = d.right;
            }
            _ => out.push(d),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(left: &str, right: &str, n: &Normalizer) -> DiffReport {
        diff_normalized("oracle", left, "subject", right, n)
    }

    #[test]
    fn identical_output_is_identical() {
        let r = diff("a\nb\nc\n", "a\nb\nc\n", &Normalizer::none());
        assert!(r.is_identical(), "{}", r.render());
        assert!(r.rules_fired.is_empty());
        assert!(r.render().contains("byte-exact"));
    }

    /// The load-bearing test. A value difference is a real semantic difference and
    /// must survive every normalization rule, or this crate is decoration.
    #[test]
    fn a_real_value_difference_survives_normalization() {
        let left = r#"{"model":"claude-opus-4-7","createdAt":"2026-04-28T21:18:45.535Z"}"#;
        let right = r#"{"model":"claude-haiku-4-5","createdAt":"2026-08-05T09:00:00.000Z"}"#;
        let r = diff(left, right, &Normalizer::default());
        assert!(!r.is_identical(), "a differing model must be reported");
        assert_eq!(r.rules_fired.get("iso8601-timestamp"), Some(&2));
        let rendered = r.render();
        assert!(rendered.contains("claude-opus-4-7"), "{rendered}");
        assert!(rendered.contains("claude-haiku-4-5"), "{rendered}");
    }

    #[test]
    fn only_the_volatile_span_is_masked() {
        let left = "started 2026-04-28T21:18:45.535Z on 127.0.0.1:54321";
        let right = "started 2026-08-05T09:00:00.000Z on 127.0.0.1:41999";
        let r = diff(left, right, &Normalizer::default());
        assert!(r.is_identical(), "{}", r.render());
        assert_eq!(r.rules_fired.get("iso8601-timestamp"), Some(&2));
        assert_eq!(r.rules_fired.get("loopback-port"), Some(&2));
        assert!(r.render().contains("iso8601-timestampx2"));
    }

    #[test]
    fn the_report_names_what_it_masked_even_when_it_passes() {
        let r = diff(
            "at 2026-04-28T21:18:45Z",
            "at 2026-08-05T09:00:00Z",
            &Normalizer::default(),
        );
        assert!(r.is_identical());
        let rendered = r.render();
        assert!(
            rendered.contains("normalization masked: iso8601-timestampx2"),
            "{rendered}"
        );
        assert!(!rendered.contains("masked: nothing"), "{rendered}");
    }

    #[test]
    fn an_inserted_line_is_reported_once_not_as_a_cascade() {
        let r = diff("a\nb\nc\nd\n", "a\nb\nEXTRA\nc\nd\n", &Normalizer::none());
        assert_eq!(r.divergence_count(), 1, "{}", r.render());
        assert_eq!(r.divergences[0].left, None);
        assert_eq!(r.divergences[0].right.as_deref(), Some("EXTRA"));
    }

    #[test]
    fn a_changed_line_is_reported_as_one_change() {
        let r = diff("a\nb\nc\n", "a\nB\nc\n", &Normalizer::none());
        assert_eq!(r.divergence_count(), 1, "{}", r.render());
        assert_eq!(r.divergences[0].left.as_deref(), Some("b"));
        assert_eq!(r.divergences[0].right.as_deref(), Some("B"));
    }

    #[test]
    fn a_missing_line_is_reported() {
        let r = diff("a\nb\nc\n", "a\nc\n", &Normalizer::none());
        assert_eq!(r.divergence_count(), 1, "{}", r.render());
        assert_eq!(r.divergences[0].left.as_deref(), Some("b"));
        assert_eq!(r.divergences[0].right, None);
    }

    #[test]
    fn truncated_output_is_reported_not_forgiven() {
        let r = diff("a\nb\nc\n", "a\n", &Normalizer::none());
        assert_eq!(r.divergence_count(), 2, "{}", r.render());
    }

    #[test]
    fn labels_reach_the_report_so_a_failure_names_its_oracle() {
        let r = diff_normalized(
            "oracle(installed-binary, reports 1.18.12)",
            "x",
            "subject(zuno 0.1.0)",
            "y",
            &Normalizer::none(),
        );
        let rendered = r.render();
        assert!(rendered.contains("reports 1.18.12"), "{rendered}");
        assert!(rendered.contains("zuno 0.1.0"), "{rendered}");
    }

    #[test]
    fn large_inputs_degrade_to_positional_without_hiding_anything() {
        let big = (0..ALIGNMENT_LINE_BUDGET + 10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let perturbed = big.replace("line 5\n", "line 5 CHANGED\n");
        let r = diff(&big, &perturbed, &Normalizer::none());
        assert!(r.alignment_degraded);
        assert!(!r.is_identical());
        assert!(r.render().contains("compared positionally"));
    }
}
