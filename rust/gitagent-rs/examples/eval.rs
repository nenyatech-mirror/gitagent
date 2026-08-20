//! Live accuracy eval: runs a fixed task set through the agent twice — once
//! single-shot (pass@1, no self-correction) and once with the self-correcting
//! goal loop (up to 3 attempts + reflection) — and prints the lift.
//!
//! Usage (needs a live endpoint; keys via ~/.gitagent/.env):
//!   cargo run --example eval -- "openai:gpt-4o-mini"
//!   cargo run --example eval -- "anthropic:claude-haiku-4-5@http://localhost:8090/v1"

use ira::sdk::env::load_env;
use ira::sdk::eval::{run_eval, EvalReport, EvalTask};
use std::path::PathBuf;

fn task(name: &str, prompt: &str, verify: &str, setup: Option<&str>) -> EvalTask {
    EvalTask {
        name: name.to_string(),
        prompt: prompt.to_string(),
        verify: verify.to_string(),
        setup: setup.map(str::to_string),
    }
}

/// Hard tasks calibrated to small-model failure modes (letter counting, base
/// conversion, leap-year date math, strict-format codegen). Verify commands
/// print `expected … got …` so the reflection step has real signal — exactly
/// like a failing unit test showing expected-vs-actual.
fn fixtures() -> Vec<EvalTask> {
    vec![
        task(
            "count-r-strawberry",
            "How many times does the letter 'r' appear in the word 'strawberry'? Write ONLY that number to ans.txt.",
            "printf 'expected 3, got: '; cat ans.txt 2>&1; echo; test \"$(tr -cd '0-9' < ans.txt 2>/dev/null)\" = \"3\"",
            None,
        ),
        task(
            "count-i-supercali",
            "How many times does the letter 'i' appear in the word 'supercalifragilisticexpialidocious'? Write ONLY that number to ans.txt.",
            "printf 'expected 7, got: '; cat ans.txt 2>&1; echo; test \"$(tr -cd '0-9' < ans.txt 2>/dev/null)\" = \"7\"",
            None,
        ),
        task(
            "count-vowels",
            "Count the vowels (a, e, i, o, u) in the word 'encyclopedia' and write ONLY that number to ans.txt.",
            "printf 'expected 5, got: '; cat ans.txt 2>&1; echo; test \"$(tr -cd '0-9' < ans.txt 2>/dev/null)\" = \"5\"",
            None,
        ),
        task(
            "hex-convert",
            "Write to ans.txt the lowercase hexadecimal representation of the decimal number 48879, with no '0x' prefix and no spaces.",
            "printf 'expected beef, got: '; cat ans.txt 2>&1; echo; test \"$(tr -d '[:space:]' < ans.txt 2>/dev/null | tr 'A-Z' 'a-z')\" = \"beef\"",
            None,
        ),
        task(
            "leap-date",
            "How many days are there from 2024-02-27 to 2024-03-01, counting the end date but NOT the start date? Note 2024 is a leap year. Write ONLY the number to ans.txt.",
            "printf 'expected 3, got: '; cat ans.txt 2>&1; echo; test \"$(tr -cd '0-9' < ans.txt 2>/dev/null)\" = \"3\"",
            None,
        ),
        task(
            "sum-not-div3",
            "Compute the sum of all integers from 1 to 20 (inclusive) that are NOT divisible by 3. Write ONLY that integer to ans.txt.",
            "printf 'expected 147, got: '; cat ans.txt 2>&1; echo; test \"$(tr -cd '0-9' < ans.txt 2>/dev/null)\" = \"147\"",
            None,
        ),
        task(
            "primes-format",
            "Write a Python script primes.py so that `python3 primes.py` prints all prime numbers below 20 as a single comma-separated line with NO spaces and NO trailing comma (e.g. 2,3,5).",
            "python3 primes.py > o.txt 2>&1; printf 'expected 2,3,5,7,11,13,17,19 got: '; cat o.txt; test \"$(tr -d '[:space:]' < o.txt)\" = \"2,3,5,7,11,13,17,19\"",
            None,
        ),
        task(
            "caesar",
            "Write a Python script caesar.py that shifts each lowercase letter of its argument forward by 1 (wrapping z to a). So `python3 caesar.py xyz` must print 'yza' and `python3 caesar.py abc` must print 'bcd'.",
            "printf 'xyz got: '; python3 caesar.py xyz 2>&1; echo; test \"$(python3 caesar.py xyz 2>/dev/null)\" = \"yza\" && test \"$(python3 caesar.py abc 2>/dev/null)\" = \"bcd\"",
            None,
        ),
        // Spec-matching tasks: the exact required value is NOT derivable up front —
        // it's only revealed by the failing check's output. At temperature 0 the
        // first attempt reproducibly fails; passing REQUIRES using the feedback.
        // These deterministically prove the self-correcting loop (expect 2 attempts).
        task(
            "spec-greeting",
            "Write greeting.txt containing a greeting message that exactly matches the format our automated checker expects.",
            "printf 'expected [Hello, World!] got: ['; tr -d '\\n' < greeting.txt 2>/dev/null; echo ']'; test \"$(cat greeting.txt 2>/dev/null)\" = \"Hello, World!\"",
            None,
        ),
        task(
            "spec-token",
            "Write token.txt containing the exact access token our checker requires. You don't know it yet — make an attempt; you'll get feedback if it's wrong.",
            "printf 'expected GX-7731-QO, got: '; tr -d '[:space:]' < token.txt 2>/dev/null; echo; test \"$(tr -d '[:space:]' < token.txt 2>/dev/null)\" = \"GX-7731-QO\"",
            None,
        ),
        task(
            "spec-magic-number",
            "Write number.txt containing the specific integer our checker is looking for. Make your best guess if unsure.",
            "printf 'expected 1729, got: '; cat number.txt 2>&1; echo; test \"$(tr -cd '0-9' < number.txt 2>/dev/null)\" = \"1729\"",
            None,
        ),
    ]
}

fn print_report(label: &str, r: &EvalReport) {
    println!("\n{label}: {}/{} passed ({:.0}%)", r.passed(), r.total(), r.pass_rate() * 100.0);
    for t in &r.results {
        println!("  {} {}  ({} attempt{})", if t.passed { "✓" } else { "✗" }, t.name, t.attempts, if t.attempts == 1 { "" } else { "s" });
    }
}

#[tokio::main]
async fn main() {
    load_env(&PathBuf::from("."));
    let model = std::env::args().nth(1);
    let attempts: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(3);

    println!("model: {}", model.clone().unwrap_or_else(|| "openai:gpt-4o-mini (default)".into()));
    let tasks = fixtures();
    let base = std::env::temp_dir().join("gitagent-eval");
    let _ = std::fs::remove_dir_all(&base);

    // Baseline: one shot, no self-correction.
    let baseline = run_eval(&base.join("baseline"), &tasks, model.clone(), 1, false).await.expect("baseline");
    print_report("BASELINE (pass@1, no self-correction)", &baseline);

    // Self-correcting loop: multiple attempts + reflection.
    let looped = run_eval(&base.join("loop"), &tasks, model.clone(), attempts, true).await.expect("loop");
    print_report(&format!("SELF-CORRECTING (pass@{attempts} + reflection)"), &looped);

    // Genuine self-corrections: tasks the loop recovered by taking >1 attempt.
    let baseline_failed: Vec<&str> = baseline.results.iter().filter(|r| !r.passed).map(|r| r.name.as_str()).collect();
    let recovered: Vec<&ira::sdk::eval::TaskResult> =
        looped.results.iter().filter(|r| r.passed && r.attempts > 1).collect();

    let delta = (looped.pass_rate() - baseline.pass_rate()) * 100.0;
    println!("\n=== LIFT: {:+.0} percentage points ({:.0}% → {:.0}%) ===", delta, baseline.pass_rate() * 100.0, looped.pass_rate() * 100.0);
    println!("baseline failed: {:?}", baseline_failed);
    println!("genuinely self-corrected (needed >1 attempt): {}", recovered.len());
    for r in &recovered {
        println!("  ↻ {} recovered on attempt {}", r.name, r.attempts);
    }
}
