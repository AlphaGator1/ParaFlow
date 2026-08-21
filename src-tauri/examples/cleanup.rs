//! Quality harness for the dictation cleanup pass.
//!
//! Runs representative transcripts through the real `Polish` prompt and prints
//! before/after pairs with timings. Several cases are adversarial: an
//! instruction and a question that the model must *write down* rather than act
//! on, and a dictionary term that must not be inserted where it was not said.
//! If those start failing, the cleanup pass has become actively dangerous and
//! this is where it shows up.
//!
//!   cargo run --example cleanup
//!   cargo run --example cleanup -- Qwen3-1.7B-Q4_K_M.gguf

use paraflow_lib::dictionary::Dictionary;
use paraflow_lib::polish::{Polish, PolishConfig, Style};
use std::path::PathBuf;
use std::time::Instant;

/// (label, style, raw transcript, what we are checking)
///
/// Written the way whisper and Parakeet actually emit text - punctuated and
/// capitalised - because that is what the cleanup pass really receives. An
/// earlier version fed lowercase unpunctuated strings, which made the task look
/// harder than it is and produced misleading results.
const CASES: &[(&str, Style, &str, &str)] = &[
    (
        "fillers",
        Style::Prose,
        "Um, so I was thinking we could, uh, maybe push the release to next week \
         because, like, the tests are still kind of flaky, you know?",
        "fillers removed, meaning intact",
    ),
    (
        "self-correction",
        Style::Prose,
        "Let's meet on Tuesday, no, Wednesday at three in the afternoon.",
        "only Wednesday survives",
    ),
    (
        "instruction (adversarial)",
        Style::Prose,
        "Write a Python function that reverses a string.",
        "MUST be written down, not obeyed",
    ),
    (
        "question (adversarial)",
        Style::Prose,
        "Hey, what time does the standup start tomorrow morning?",
        "MUST stay a question, not be answered",
    ),
    (
        "dictated list",
        Style::Prose,
        "We need three things. New line. Bullet point. Better logging. \
         Bullet point. Faster startup. Bullet point. A proper test suite.",
        "formatted as a list",
    ),
    (
        "false start",
        Style::Prose,
        "Can you send the, uh, the report, the quarterly report to Marcus before Friday?",
        "stutter collapsed",
    ),
    // --- per-app tone -----------------------------------------------------
    (
        "code style",
        Style::Code,
        "get checkout dash b feature slash retry handling",
        "terse, no trailing full stop; ideally a usable command",
    ),
    (
        "code style, prose sentence",
        Style::Code,
        "Um, so this function, it needs a null check before the dereference.",
        "fillers gone, but still readable prose - not mangled into code",
    ),
    (
        "mail style",
        Style::Mail,
        "thanks for the quick turnaround on that, ill review it this afternoon",
        "complete sentences, full punctuation",
    ),
    (
        "chat style",
        Style::Chat,
        "yeah that works for me, ill be there in about ten",
        "conversational, not formalised into an essay",
    ),
    // --- dictionary -------------------------------------------------------
    (
        "dictionary correction",
        Style::Prose,
        "Can you ask Prea to redeploy the kuber netes cluster?",
        "Priya and Kubernetes corrected to dictionary spelling",
    ),
    (
        "dictionary restraint (adversarial)",
        Style::Prose,
        "The weather is lovely today and the coffee is good.",
        "MUST NOT sprinkle in dictionary terms that were never said",
    ),
    (
        "dictionary, real mishearing",
        Style::Prose,
        "I was using clawed code to write the installer this morning.",
        "\"clawed code\" -> \"Claude Code\"; observed in use, not invented",
    ),
    (
        "dictionary near-miss (adversarial)",
        Style::Prose,
        "The cat clawed the sofa and then sat on the keyboard.",
        "MUST stay \"clawed\" - a dictionary term must not eat an ordinary word",
    ),
    // --- hands-free -------------------------------------------------------
    (
        "two-minute hands-free recording",
        Style::Prose,
        LONG_DICTATION,
        "the WHOLE thing comes back, punctuated, inside the request budget",
    ),
];

/// A full hands-free recording: two minutes of fairly quick speech, which is
/// what the latch hands to cleanup when it runs all the way to its ceiling.
///
/// This is the shape the old budgets were not sized for. Measured on the
/// development machine: 2382 characters in, 2351 out, 1.75 s - which fits the
/// old 512-token ceiling with almost nothing spare and the old flat 2.5 s
/// timeout with about 30% spare. Neither had room for someone talking faster.
///
/// Check two things when this runs: that the last sentence about the rollback
/// story is present, and that the timing sits inside `polish::request_timeout`.
const LONG_DICTATION: &str = "\
Okay, so I wanted to walk through where we are with the migration before the \
review on Thursday, because there are a few things that I don't think are \
written down anywhere yet. Um, the first thing is that the data import is \
basically done. We finished the last batch on Monday, and the row counts \
match on both sides, which is, you know, the main thing we were worried \
about. There are about four hundred records that failed validation, but \
they're all from the twenty nineteen archive and, honestly, I think we should \
just leave those out rather than hold up the whole thing. The second thing is \
performance. The new queries are faster in almost every case, but there's one \
report, the quarterly rollup, that got noticeably slower, and I haven't worked \
out why yet. My guess is it's an index we didn't recreate when we rebuilt the \
table, but I want to actually measure it before I go changing things, because \
last time we guessed at something like this we spent two days on the wrong \
problem. Third, and this is the one I really want to talk about on Thursday, \
we still don't have a rollback plan that anyone has tested. We have a plan on \
paper, and it looks reasonable, but nobody has actually run it end to end, and \
I really don't want the first time we try it to be at two in the morning in \
the middle of an incident with everyone watching. So what I'd like to propose \
is that we spend, uh, maybe a day next week doing a full dry run on the \
staging environment, restore included, and write down how long each step \
actually takes rather than how long we think it takes. Fourth thing, and I \
keep forgetting to mention this, we're going to need someone from the finance \
team available on the day of the cutover, because the reconciliation reports \
run overnight and if those come out wrong we need somebody who can tell us \
whether it's a real problem or just a timing artefact. I've asked Priya but I \
haven't heard back yet. Last thing, and this is minor, the documentation is \
out of date in a few places, mostly the bits about the old import script that \
doesn't exist any more. I'll fix that myself, it's not a blocker, I just don't \
want anyone reading it and getting confused about which system is live. So \
yeah, that's roughly where we are. I think we're in decent shape overall, and \
the timeline still looks fine, the only thing that genuinely worries me is the \
rollback story.";

/// Terms fed to the model for this run, mirroring a user's dictionary.txt.
const TEST_TERMS: &str = "Priya\nKubernetes\nParaFlow\nAnthropic\nClaude\nClaude Code\n";

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("warn").init();

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    // Which acceleration backend to exercise. Vulkan is ~7x smaller on disk
    // than CUDA and runs on AMD and Intel too, so it matters for packaging.
    let accel = std::env::var("PARAFLOW_ACCEL").unwrap_or_else(|_| "cuda".into());
    let server_bin = root.join(format!("vendor/llama-{accel}/llama-server.exe"));
    let model = root.join("models").join(
        std::env::args().nth(1).unwrap_or_else(|| "Qwen3-4B-Instruct-2507-Q4_K_M.gguf".into()),
    );

    // Use a fixed dictionary rather than the user's real one, so the harness
    // gives the same answer on any machine.
    let dictionary = Dictionary::from_text(TEST_TERMS);

    println!("accel : {accel}");
    println!("model : {}", model.display());
    println!("dict  : {} terms", dictionary.len());
    print!("loading... ");
    let began = Instant::now();
    let polish = Polish::start(PolishConfig {
        server_bin,
        model,
        gpu_layers: 99,
        threads: 8,
        dictionary,
    })?;
    println!("ready in {} ms\n", began.elapsed().as_millis());

    let mut total = 0u128;
    for (label, style, raw, expectation) in CASES {
        let began = Instant::now();
        let cleaned = polish.apply(raw, *style);
        let ms = began.elapsed().as_millis();
        total += ms;

        println!("\x1b[1m{label}\x1b[0m  [{style:?}]  ({expectation})");
        println!("  \x1b[2min \x1b[0m {raw}");
        println!("  \x1b[36mout\x1b[0m {}", cleaned.replace('\n', "\n      "));
        // Cleanup fails open, so a rejected result comes back as the input
        // rather than as an error, and nothing here can tell the two apart -
        // several of the adversarial cases are *supposed* to come back
        // untouched. What the counts can catch is the failure that actually
        // threatens a long dictation: output far shorter than the input means
        // the tail went missing, to a token ceiling or to the model deciding
        // to summarise.
        let lost_the_tail = cleaned.len() * 10 < raw.len() * 6;
        println!(
            "  \x1b[2m{ms} ms, {} in -> {} out chars{}\x1b[0m\n",
            raw.len(),
            cleaned.len(),
            if lost_the_tail { "  \x1b[33m<- SHORT: check the tail survived\x1b[0m" } else { "" }
        );
    }

    println!("mean {} ms over {} cases", total / CASES.len() as u128, CASES.len());
    Ok(())
}
