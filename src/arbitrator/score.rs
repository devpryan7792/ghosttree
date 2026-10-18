//! Multi-criteria candidate scoring (SPEC §7).
//!
//! ```text
//! Final Score = w1*S_tests + w2*S_cov - w3*S_churn - w4*S_time
//!             = 0.50*S_tests + 0.20*S_cov - 0.15*S_churn - 0.15*S_time
//! ```
//!
//! Interpretation notes for the under-specified terms:
//! * `S_cov` (delta test coverage) requires a coverage tool the task does not
//!   configure; it is an input that defaults to 0 so the term is inert.
//! * `S_churn`'s "Baseline Files Changed" denominator is the number of files
//!   changed in the candidate diff, i.e. churn = lines rewritten per changed
//!   file. Rewriting whole unrelated files drives the numerator up sharply,
//!   which is exactly the hallucination signature §7 says to penalize.
//! * If any test failed (or the run failed outright), the final score is
//!   capped at 40 so a lucky low-churn run can never outrank a green one.

use crate::db::{DiffStat, TestResults};

/// Score ceiling applied when any test (or the run itself) failed (SPEC §7).
pub const FAILING_CAP: f64 = 40.0;
/// Denominator of `S_time`: 600 s of execution saturates the penalty (SPEC §7).
pub const TIME_NORMALIZER_SECS: f64 = 600.0;

/// Criteria weights `w1..w4` (SPEC §7 defaults).
#[derive(Debug, Clone, PartialEq)]
pub struct Weights {
    pub tests: f64,
    pub cov: f64,
    pub churn: f64,
    pub time: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            tests: 0.50,
            cov: 0.20,
            churn: 0.15,
            time: 0.15,
        }
    }
}

/// Everything the scorer knows about one candidate.
#[derive(Debug, Clone, Default)]
pub struct ScoreInputs {
    /// Parsed test counters, when a test run produced parseable output.
    pub test_results: Option<TestResults>,
    /// True when the candidate's grading run (agent + harness) succeeded;
    /// drives `S_tests` when counters are missing.
    pub succeeded: bool,
    /// Delta coverage percent (0 when coverage tooling is not configured).
    pub coverage_delta: Option<f64>,
    /// Diff against the task base commit (`None` = no committed changes).
    pub diff_stat: Option<DiffStat>,
    /// Total candidate execution time (agent + harness).
    pub duration_secs: f64,
}

/// Transparent breakdown of one computed score.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreBreakdown {
    pub s_tests: f64,
    pub s_cov: f64,
    pub s_churn: f64,
    pub s_time: f64,
    pub raw: f64,
    pub final_score: f64,
    /// True when the §7 failing cap determined `final_score`.
    pub capped: bool,
}

impl ScoreBreakdown {
    /// Score a candidate (SPEC §7). Deterministic and side-effect free.
    pub fn compute(inputs: &ScoreInputs, weights: &Weights) -> Self {
        let s_tests = s_tests(inputs);
        let s_cov = inputs
            .coverage_delta
            .map(|cov| cov.clamp(0.0, 100.0))
            .unwrap_or(0.0);
        let s_churn = s_churn(inputs.diff_stat.as_ref());
        let s_time = (inputs.duration_secs.max(0.0) / TIME_NORMALIZER_SECS).min(1_000.0);

        let raw = weights.tests * s_tests + weights.cov * s_cov
            - weights.churn * s_churn
            - weights.time * s_time;
        let failed_run = run_failed(inputs);
        let final_score = if failed_run {
            raw.min(FAILING_CAP)
        } else {
            raw
        };
        Self {
            s_tests,
            s_cov,
            s_churn,
            s_time,
            raw,
            final_score,
            capped: failed_run && raw > FAILING_CAP,
        }
    }
}

/// `S_tests = passed / total * 100`; a run with no parseable counters falls
/// back to the harness verdict (success = fully green probe, failure = 0).
fn s_tests(inputs: &ScoreInputs) -> f64 {
    match &inputs.test_results {
        Some(results) if results.passed + results.failed > 0 => {
            100.0 * f64::from(results.passed) / f64::from(results.passed + results.failed)
        }
        _ => {
            if inputs.succeeded {
                100.0
            } else {
                0.0
            }
        }
    }
}

/// Lines churned per changed file (SPEC §7 `S_churn`); no diff means no churn.
fn s_churn(diff: Option<&DiffStat>) -> f64 {
    match diff {
        Some(stat) => {
            let lines = stat.insertions + stat.deletions;
            if lines == 0 {
                0.0
            } else {
                lines as f64 / stat.files_changed.max(1) as f64
            }
        }
        None => 0.0,
    }
}

/// The §7 cap applies when any parsed test failed or the run failed outright.
fn run_failed(inputs: &ScoreInputs) -> bool {
    match &inputs.test_results {
        Some(results) => results.failed > 0 || !inputs.succeeded,
        None => !inputs.succeeded,
    }
}

// ---------------------------------------------------------------------------
// Test output parsing (`test_results_json` source data).
// ---------------------------------------------------------------------------

/// Counters recovered from free-form test-runner output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedCounts {
    pub passed: u32,
    pub failed: u32,
}

/// Parse pass/fail counters out of a test log (best effort, formats in
/// priority order: cargo, pytest, jest, generic `<n> passed|failed|failing`).
///
/// Returns `None` when no counter pair could be recovered; callers then fall
/// back to the harness exit verdict (one implicit smoke test).
pub fn parse_test_output(output: &str) -> Option<ParsedCounts> {
    cargo_counts(output)
        .or_else(|| pytest_counts(output))
        .or_else(|| jest_counts(output))
        .or_else(|| generic_counts(output))
}

/// `test result: ok. 10 passed; 2 failed; 1 ignored; ...`
fn cargo_counts(output: &str) -> Option<ParsedCounts> {
    let line = output.lines().find(|l| l.contains("test result:"))?;
    let passed = extract_label_number(line, "passed")?;
    let failed = extract_label_number(line, "failed")?;
    Some(PassedCountsWith(passed, failed).into())
}

/// `====== 3 passed, 2 failed, 1 error in 0.42s ======`
fn pytest_counts(output: &str) -> Option<ParsedCounts> {
    let line = output
        .lines()
        .find(|l| l.contains(" in ") && l.contains("passed"))?;
    let passed = extract_label_number(line, "passed")?;
    // A pytest summary with zero failures simply omits the "failed" term.
    let failed = extract_label_number(line, "failed").unwrap_or(0);
    let errors = extract_label_number(line, "error").unwrap_or(0);
    Some(PassedCountsWith(passed, failed + errors).into())
}

/// `Tests: 4 passed, 1 failed, 5 total`
fn jest_counts(output: &str) -> Option<ParsedCounts> {
    let line = output
        .lines()
        .find(|l| l.trim_start().starts_with("Tests:"))?;
    let passed = extract_label_number(line, "passed")?;
    let failed = extract_label_number(line, "failed")
        .unwrap_or_else(|| extract_label_number(line, "failed to run").unwrap_or(0));
    Some(PassedCountsWith(passed, failed).into())
}

/// Generic runner vocabulary: `12 passing (2s)`, `2 failing`, `7 passed`,
/// `N passed, M failed`. Scans word-number pairs anywhere in the log.
fn generic_counts(output: &str) -> Option<ParsedCounts> {
    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut matched = false;
    for (number, label) in number_label_pairs(output) {
        let n = match number {
            Some(n) => n,
            None => continue,
        };
        let label = label.trim_matches([';', ',', ':']);
        match label {
            "passed" | "passing" | "ok" => {
                passed = passed.saturating_add(n);
                matched = true;
            }
            "failed" | "failing" => {
                failed = failed.saturating_add(n);
                matched = true;
            }
            _ => {}
        }
    }
    matched.then_some(PassedCountsWith(passed, failed).into())
}

/// Pair every plain integer in `text` with the word that follows it.
fn number_label_pairs(text: &str) -> Vec<(Option<u32>, &str)> {
    let mut pairs = Vec::new();
    let mut tokens = text.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        let number = token.trim_end_matches([':', ',']).parse::<u32>().ok();
        let label = tokens.peek().copied().unwrap_or("");
        pairs.push((number, label));
    }
    pairs
}

/// `<number> <label>` where label may carry trailing punctuation; the first
/// occurrence wins so summary lines are not double-counted.
fn extract_label_number(text: &str, label: &str) -> Option<u32> {
    let mut tokens = text.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        let next = tokens.peek().copied().unwrap_or("");
        let next_clean = next.trim_matches([';', ',', ':']);
        if next_clean == label {
            let cleaned = token.trim_matches([';', ',', ':']);
            if let Ok(n) = cleaned.parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

/// Tuple newtype so the small constructors above read uniformly.
struct PassedCountsWith(u32, u32);

impl From<PassedCountsWith> for ParsedCounts {
    fn from(PassedCountsWith(passed, failed): PassedCountsWith) -> Self {
        Self { passed, failed }
    }
}

/// Convert parsed counters into the persisted `test_results_json` shape. A
/// run with no parseable counters counts as a single implicit smoke test
/// gated by the harness verdict (1/0 green, 0/1 red).
pub fn results_from_output(
    parsed: Option<ParsedCounts>,
    succeeded: bool,
    duration_ms: u64,
) -> TestResults {
    match parsed {
        Some(counts) => TestResults {
            passed: counts.passed,
            failed: counts.failed,
            duration_ms,
        },
        None => TestResults {
            passed: u32::from(succeeded),
            failed: u32::from(!succeeded),
            duration_ms,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tests(passed: u32, failed: u32) -> TestResults {
        TestResults {
            passed,
            failed,
            duration_ms: 0,
        }
    }

    #[test]
    fn perfect_small_diff_scores_high() {
        let inputs = ScoreInputs {
            test_results: Some(tests(8, 0)),
            succeeded: true,
            coverage_delta: None,
            diff_stat: Some(DiffStat {
                files_changed: 2,
                insertions: 20,
                deletions: 4,
            }),
            duration_secs: 30.0,
        };
        let s = ScoreBreakdown::compute(&inputs, &Weights::default());
        assert!((s.s_tests - 100.0).abs() < 1e-9);
        assert!((s.s_churn - 12.0).abs() < 1e-9);
        // 0.5*100 - 0.15*12 - 0.15*(30/600) = 50 - 1.8 - 0.0075
        assert!(
            (s.final_score - (50.0 - 1.8 - 0.0075)).abs() < 1e-9,
            "{}",
            s.final_score
        );
        assert!(!s.capped);
    }

    #[test]
    fn failing_tests_are_capped_at_40() {
        // S_tests = 90 -> raw = 45 - small penalties; the cap must hold it at 40.
        let inputs = ScoreInputs {
            test_results: Some(tests(9, 1)),
            succeeded: false,
            coverage_delta: None,
            diff_stat: Some(DiffStat {
                files_changed: 10,
                insertions: 10,
                deletions: 10,
            }),
            duration_secs: 0.0,
        };
        let s = ScoreBreakdown::compute(&inputs, &Weights::default());
        // raw = 0.5*90 - 0.15*churn(2.0) = 44.7 — above the cap either way.
        assert!(s.raw > FAILING_CAP, "{}", s.raw);
        assert!((s.final_score - FAILING_CAP).abs() < 1e-9);
        assert!(s.capped);
    }

    #[test]
    fn failed_run_without_parseable_tests_is_capped_and_zeroed() {
        let inputs = ScoreInputs {
            test_results: None,
            succeeded: false,
            coverage_delta: None,
            diff_stat: None,
            duration_secs: 12.0,
        };
        let s = ScoreBreakdown::compute(&inputs, &Weights::default());
        assert!((s.s_tests - 0.0).abs() < 1e-9);
        assert!((s.final_score - (-0.15 * 12.0 / 600.0)).abs() < 1e-9);
        assert!(s.final_score < FAILING_CAP);
    }

    #[test]
    fn hallucinating_high_churn_candidate_loses() {
        let base = ScoreInputs {
            test_results: Some(tests(8, 0)),
            succeeded: true,
            coverage_delta: None,
            diff_stat: Some(DiffStat {
                files_changed: 2,
                insertions: 20,
                deletions: 4,
            }),
            duration_secs: 30.0,
        };
        let mut wild = base.clone();
        wild.diff_stat = Some(DiffStat {
            files_changed: 40,
            insertions: 1800,
            deletions: 900,
        });
        let clean = ScoreBreakdown::compute(&base, &Weights::default());
        let churned = ScoreBreakdown::compute(&wild, &Weights::default());
        assert!(churned.final_score < clean.final_score);
        // churn = 2700 lines / 40 files = 67.5 vs 24/2 = 12; the delta is the
        // §7 weight applied to that difference.
        assert!((churned.s_churn - 67.5).abs() < 1e-9);
        assert!((clean.final_score - churned.final_score - 0.15 * (67.5 - 12.0)).abs() < 1e-9);
    }

    #[test]
    fn slower_candidate_is_penalized() {
        let fast = ScoreInputs {
            test_results: Some(tests(4, 0)),
            succeeded: true,
            duration_secs: 10.0,
            ..Default::default()
        };
        let mut slow = fast.clone();
        slow.duration_secs = 300.0;
        let a = ScoreBreakdown::compute(&fast, &Weights::default());
        let b = ScoreBreakdown::compute(&slow, &Weights::default());
        assert!(b.final_score < a.final_score);
        assert!((a.final_score - b.final_score - 0.15 * (290.0 / 600.0)).abs() < 1e-9);
    }

    #[test]
    fn coverage_delta_adds_up_to_twenty_percent() {
        let plain = ScoreInputs {
            test_results: Some(tests(4, 0)),
            succeeded: true,
            ..Default::default()
        };
        let mut covered = plain.clone();
        covered.coverage_delta = Some(25.0);
        let plain_score = ScoreBreakdown::compute(&plain, &Weights::default());
        let covered_score = ScoreBreakdown::compute(&covered, &Weights::default());
        assert!((covered_score.final_score - plain_score.final_score - 0.20 * 25.0).abs() < 1e-9);
    }

    #[test]
    fn no_diff_means_no_churn_penalty() {
        let inputs = ScoreInputs {
            test_results: Some(tests(4, 0)),
            succeeded: true,
            ..Default::default()
        };
        let s = ScoreBreakdown::compute(&inputs, &Weights::default());
        assert!((s.s_churn - 0.0).abs() < 1e-9);
    }

    #[test]
    fn parses_cargo_success_and_failure_summaries() {
        let ok = "running 12 tests\n\
                  test pool::stresses ... ok\n\
                  test result: ok. 10 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out\n";
        assert_eq!(
            parse_test_output(ok),
            Some(ParsedCounts {
                passed: 10,
                failed: 0
            })
        );
        let bad =
            "test result: FAILED. 5 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out\n";
        assert_eq!(
            parse_test_output(bad),
            Some(ParsedCounts {
                passed: 5,
                failed: 3
            })
        );
    }

    #[test]
    fn parses_pytest_summary() {
        let out = "================= 3 passed, 2 failed, 1 error in 0.42s =================";
        assert_eq!(
            parse_test_output(out),
            Some(ParsedCounts {
                passed: 3,
                failed: 3
            })
        );
        let clean = "============================= 4 passed in 1.20s =============================";
        assert_eq!(
            parse_test_output(clean),
            Some(ParsedCounts {
                passed: 4,
                failed: 0
            })
        );
    }

    #[test]
    fn parses_jest_summary() {
        let out = "Tests:       4 passed, 1 failed, 5 total\n\
                   Snapshots:   0 total\n\
                   Time:        2.114 s";
        assert_eq!(
            parse_test_output(out),
            Some(ParsedCounts {
                passed: 4,
                failed: 1
            })
        );
    }

    #[test]
    fn parses_generic_runner_vocabulary() {
        assert_eq!(
            parse_test_output("12 passing (2s)\n2 failing\n1 pending\n"),
            Some(ParsedCounts {
                passed: 12,
                failed: 2
            })
        );
        assert_eq!(
            parse_test_output("7 passed; 0 failed\n"),
            Some(ParsedCounts {
                passed: 7,
                failed: 0
            })
        );
    }

    #[test]
    fn unparseable_output_yields_none() {
        assert_eq!(parse_test_output("ALL SYSTEMS NOMINAL\n"), None);
        assert_eq!(parse_test_output(""), None);
        // A bare exit verdict is not a counter pair.
        assert_eq!(parse_test_output("segmentation fault\n"), None);
    }

    #[test]
    fn smoke_fallback_counts_one_implicit_test() {
        assert_eq!(
            results_from_output(None, true, 15),
            tests(1, 0).with_duration(15)
        );
        assert_eq!(
            results_from_output(None, false, 15),
            tests(0, 1).with_duration(15)
        );
        let parsed = Some(ParsedCounts {
            passed: 2,
            failed: 1,
        });
        assert_eq!(results_from_output(parsed, true, 15), {
            let mut t = tests(2, 1);
            t.duration_ms = 15;
            t
        });
    }

    impl TestResults {
        fn with_duration(mut self, ms: u64) -> Self {
            self.duration_ms = ms;
            self
        }
    }
}
