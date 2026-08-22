//! What a transcript frame costs, measured before any cache is built.
//!
//! `.omo/plans/memory-perf-optimization.md` §11.3 lists R2-R5 — a prepared-frame
//! cache, incremental body reuse, a per-message line cache and a large-buffer shrink.
//! §10.1 forbids adopting the reference implementation's figures, and §1 sets the
//! standard that an optimisation without a measured number does not ship. So this
//! file measures the thing those four items claim to fix, on this project, before
//! any of them is written.
//!
//! # The three questions
//!
//! 1. **Is there an O(n²)?** [`TranscriptView::lines`] walks every message and
//!    re-renders every part on every frame, and assistant prose goes through
//!    `markdown::render`, which re-parses its source. If per-frame cost grows with
//!    the *stable prefix* that cannot have changed, then a streaming turn of F
//!    frames over a transcript of N messages costs O(F·N) for a tail that is O(1).
//! 2. **What does a frame cost?** The redraw scheduler already coalesces engine
//!    events to 60 FPS (`app.rs:41-45`). If a frame is cheap at realistic sizes, a
//!    cache is unjustified complexity.
//! 3. **How large is a prepared frame?** M1 measured a 1,198,872 KiB W-real median
//!    (`docs/perf-methodology.md`). A cache that retains prepared frames only matters
//!    to memory if a prepared frame is a meaningful share of that.
//!
//! # Method
//!
//! Five runs per point, reported as min / median / max with a `max/min` ratio, which
//! is the shape `docs/perf-methodology.md` froze for M1 and D0. A single sample is
//! not a measurement, and a difference inside the spread is a null result.
//!
//! The sweep is gated on `ZUNO_RENDER_COST=1` so the ordinary `cargo test -p
//! zuno-tui` gate does not spend seconds on a clock. Reproduce with:
//!
//! ```sh
//! ZUNO_RENDER_COST=1 cargo test -p zuno-tui --test render_cost -- --nocapture --test-threads=1
//! ```
//!
//! The assertions that run unconditionally carry no clock: they pin the *shape* of
//! the workload (how many messages, how many rows, that the prefix is unchanged),
//! because G1 established that the regression-catching assertion is the structural
//! one and the timing is the reported evidence.

use std::time::{Duration, Instant};
use zuno_tui::views::ViewContext;
use zuno_tui::views::message::{Message, MessagePart, Role, ToolStatus, TranscriptView};

/// Runs per measured point, matching D0's `RUNS`.
const RUNS: usize = 5;

/// The width every measurement lays out at: a conventional terminal.
const WIDTH: u16 = 100;

/// Transcript sizes swept, in messages.
///
/// 931 is the pinned W-real subject's message count (`perf::subject::W_REAL_SUBJECT`),
/// so the largest point is the size the memory gates already measure against.
const SIZES: [usize; 6] = [2, 16, 64, 256, 512, 931];

/// Whether the timed sweep should run.
fn measuring() -> bool {
    std::env::var_os("ZUNO_RENDER_COST").is_some_and(|value| value == "1")
}

/// min / median / max and the `max/min` ratio, D0's reported shape.
fn spread(mut samples: Vec<Duration>) -> (Duration, Duration, Duration, f64) {
    samples.sort_unstable();
    let min = samples[0];
    let median = samples[samples.len() / 2];
    let max = samples[samples.len() - 1];
    let ratio = max.as_secs_f64() / min.as_secs_f64().max(f64::MIN_POSITIVE);
    (min, median, max, ratio)
}

/// One assistant reply: markdown prose, a bullet list, and a fenced Rust block.
///
/// Fenced code is included because it is the expensive arm — `markdown::render`
/// hands it to the bounded tree-sitter highlighter — and a workload of bare prose
/// would understate what a real reply costs. The body is deterministic in `index` so
/// two transcripts of the same size are byte-identical.
fn assistant_reply(index: usize) -> Message {
    let text = format!(
        "## Step {index}\n\n\
         The change lands in `crates/zuno-tui/src/views/message.rs`, where the \
         transcript folds engine events into parts. **Note** that the width is \
         measured in columns.\n\n\
         - reads the file at revision {index}\n\
         - rewrites the guard\n\
         - re-runs the affected tests\n\n\
         ```rust\n\
         fn guard_{index}(width: u16) -> usize {{\n\
         \x20   let columns = usize::from(width);\n\
         \x20   columns.saturating_sub(2)\n\
         }}\n\
         ```\n\n\
         That keeps the row inside the frame at every width.\n"
    );
    Message {
        role: Role::Assistant,
        id: Some(format!("msg-{index}")),
        parts: vec![
            MessagePart::Reasoning {
                text: format!("Checking how step {index} interacts with the guard above."),
                duration_secs: Some(1.5),
                streaming: false,
            },
            MessagePart::Text { text },
            MessagePart::Tool {
                call_id: format!("call-{index}"),
                name: String::from("read"),
                ui_intent: zuno_tool::ToolUiIntent::Generic,
                arguments: format!(
                    "{{\"filePath\":\"crates/zuno-tui/src/views/message.rs\",\"offset\":{index}}}"
                ),
                title: Some(String::from("Read message.rs")),
                status: ToolStatus::Completed,
                output: Some(format!(
                    "line one of {index}\nline two of {index}\nline three of {index}\nline four of {index}"
                )),
                diff: None,
            },
        ],
    }
}

/// One user prompt, taken literally by the renderer.
fn user_prompt(index: usize) -> Message {
    Message::user(format!(
        "Please look at step {index} and tell me whether the guard still holds at \
         narrow widths."
    ))
}

/// A view over `messages` alternating user prompts and assistant replies.
fn view_of(messages: usize) -> TranscriptView {
    let mut view = TranscriptView::new(ViewContext::defaults());
    let transcript = view.transcript_mut();
    for index in 0..messages {
        if index.is_multiple_of(2) {
            transcript.push(user_prompt(index));
        } else {
            transcript.push(assistant_reply(index));
        }
    }
    view
}

/// Time `lines(WIDTH)` `RUNS` times and return the produced row count with the spread.
fn time_frame(view: &TranscriptView) -> (usize, Vec<Duration>) {
    // One untimed call first: the first frame at a new size pays page faults and the
    // highlighter's per-language configuration build, which is a startup cost rather
    // than a per-frame one. G1 discards its first round for the same reason.
    let rows = view.lines(WIDTH).len();
    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = Instant::now();
        let produced = view.lines(WIDTH);
        samples.push(started.elapsed());
        assert_eq!(
            produced.len(),
            rows,
            "the same input produced a different row count"
        );
    }
    (rows, samples)
}

/// R2/R3 — per-frame cost as a function of transcript size.
///
/// This is the measurement that decides whether an O(n²) exists: the tail of a
/// streaming turn is O(1) work, so any growth here is work spent re-deriving rows
/// that could not have changed.
#[test]
fn render_cost_per_frame_by_transcript_size() {
    // Unconditional and clockless: the workload must actually scale, or a flat
    // timing curve would be a property of the fixture rather than of the renderer.
    let small = view_of(2).lines(WIDTH).len();
    let large = view_of(64).lines(WIDTH).len();
    assert!(
        large > small * 8,
        "the sweep's fixture does not grow with its size: 2 messages produced {small} rows \
         and 64 produced {large}"
    );

    if !measuring() {
        return;
    }

    println!("\nR2/R3 — one full frame at {WIDTH} columns, {RUNS} runs per size");
    println!(
        "  {:>6}  {:>7}  {:>11}  {:>11}  {:>11}  {:>8}  {:>10}",
        "msgs", "rows", "min", "median", "max", "max/min", "us/msg"
    );
    let mut baseline: Option<(usize, Duration)> = None;
    for size in SIZES {
        let view = view_of(size);
        let (rows, samples) = time_frame(&view);
        let (min, median, max, ratio) = spread(samples);
        let per_message = median.as_secs_f64() * 1e6 / size as f64;
        println!(
            "  {size:>6}  {rows:>7}  {min:>11.3?}  {median:>11.3?}  {max:>11.3?}  \
             {ratio:>7.4}x  {per_message:>9.3}"
        );
        if size == SIZES[0] {
            baseline = Some((size, median));
        } else if let Some((base_size, base_median)) = baseline {
            let size_factor = size as f64 / base_size as f64;
            let time_factor =
                median.as_secs_f64() / base_median.as_secs_f64().max(f64::MIN_POSITIVE);
            println!("          {size_factor:.1}x the messages cost {time_factor:.2}x the time");
        }
    }
}

/// R3 — what one streaming delta costs when the prefix cannot have changed.
///
/// A streaming turn appends to the last assistant message only. Every frame it
/// produces re-renders the whole transcript, so this reports the frame cost against
/// the marginal cost of the tail that actually changed.
#[test]
fn render_cost_of_a_streaming_delta_against_its_stable_prefix() {
    // Clockless: the prefix rows must be byte-identical across a tail append, which
    // is the property any prefix-reuse cache would depend on. If this ever fails,
    // reuse is unsound and no timing matters.
    let mut view = view_of(64);
    let before = view.lines(WIDTH);
    let prefix_rows = before.len();
    view.transcript_mut()
        .push(Message::user(String::from("one more")));
    let after = view.lines(WIDTH);
    assert!(
        after.len() > prefix_rows,
        "appending a message produced no new rows"
    );
    for (index, (old, new)) in before.iter().zip(after.iter()).enumerate() {
        assert_eq!(
            old.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>(),
            new.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>(),
            "row {index} of the stable prefix changed when a message was appended to the tail"
        );
    }

    if !measuring() {
        return;
    }

    println!("\nR3 — one streaming frame: whole transcript versus the changed tail");
    println!(
        "  {:>6}  {:>11}  {:>11}  {:>11}  {:>8}  {:>10}",
        "msgs", "min", "median", "max", "max/min", "tail %"
    );
    for size in SIZES {
        // The frame a streaming delta forces: the whole transcript, prefix included.
        let mut streaming = view_of(size);
        streaming.transcript_mut().push(assistant_reply(size + 1));
        let (_, whole) = time_frame(&streaming);
        // The tail alone: what a prefix-reusing renderer would have to redo.
        let mut tail = TranscriptView::new(ViewContext::defaults());
        tail.transcript_mut().push(assistant_reply(size + 1));
        let (_, tail_samples) = time_frame(&tail);
        let (min, median, max, ratio) = spread(whole);
        let (_, tail_median, _, _) = spread(tail_samples);
        let share = tail_median.as_secs_f64() * 100.0 / median.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "  {size:>6}  {min:>11.3?}  {median:>11.3?}  {max:>11.3?}  {ratio:>7.4}x  \
             {share:>9.2}"
        );
    }
}

/// R2/R5 — how many bytes a prepared frame holds.
///
/// Reported against M1's 1,198,872 KiB W-real median so the memory case for
/// retaining prepared frames can be judged rather than assumed.
#[test]
fn render_cost_prepared_frame_footprint() {
    // Clockless: a frame must hold at least its own visible text, or the byte count
    // below would be measuring an empty structure.
    let view = view_of(16);
    let lines = view.lines(WIDTH);
    assert!(
        prepared_bytes(&lines) > lines.len(),
        "the prepared frame holds fewer bytes than it has rows, so it is empty"
    );

    if !measuring() {
        return;
    }

    println!("\nR2/R5 — prepared frame footprint at {WIDTH} columns");
    println!(
        "  {:>6}  {:>7}  {:>9}  {:>12}  {:>14}",
        "msgs", "rows", "spans", "bytes", "% of M1 W-real"
    );
    // M1's tuned-jemalloc W-real median, from docs/perf-methodology.md.
    const M1_W_REAL_KIB: f64 = 1_198_872.0;
    for size in SIZES {
        let view = view_of(size);
        let lines = view.lines(WIDTH);
        let spans = lines.iter().map(|line| line.spans.len()).sum::<usize>();
        let bytes = prepared_bytes(&lines);
        let share = (bytes as f64 / 1024.0) * 100.0 / M1_W_REAL_KIB;
        println!(
            "  {size:>6}  {:>7}  {spans:>9}  {bytes:>12}  {share:>13.4}",
            lines.len()
        );
    }
}

/// Where a frame's time actually goes, before deciding what to cache.
///
/// A cache above the transcript would amortise whatever is slow. This attributes the
/// cost first, because the answer decides *which* cache: if the expense is in laying
/// rows out, a per-message row cache is the fix; if it is in one call the renderer
/// makes per code fence, then memoising that call fixes every caller at once,
/// including the ones a transcript cache never covers.
#[test]
fn render_cost_attribution_between_prose_and_a_code_fence() {
    // Clockless: the two bodies must differ only by the fence, or the difference
    // below would be attributing the cost of unrelated text.
    let prose = prose_only();
    let fenced = format!("{prose}\n```rust\nfn a() -> u8 {{ 1 }}\n```\n");
    assert!(
        fenced.starts_with(&prose),
        "the fenced body is not the prose body plus a fence, so the delta is not the fence"
    );

    if !measuring() {
        return;
    }

    let context = ViewContext::defaults();
    let palette = context.palette();
    println!("\nR4 — what one markdown render costs, by body");
    println!(
        "  {:<28}  {:>11}  {:>11}  {:>11}  {:>8}",
        "body", "min", "median", "max", "max/min"
    );
    let bodies = [
        (String::from("prose only"), prose.clone()),
        (String::from("prose + 1 rust fence"), fenced.clone()),
        (
            String::from("prose + 2 rust fences"),
            format!("{fenced}\n```rust\nfn b() -> u8 {{ 2 }}\n```\n"),
        ),
        (
            String::from("prose + 1 json fence"),
            format!("{prose}\n```json\n{{\"a\": 1}}\n```\n"),
        ),
    ];
    let mut prose_median = Duration::ZERO;
    for (label, body) in &bodies {
        let _ = zuno_tui::views::markdown::render(body, WIDTH, &palette);
        let mut samples = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let started = Instant::now();
            let rows = zuno_tui::views::markdown::render(body, WIDTH, &palette);
            samples.push(started.elapsed());
            assert!(!rows.is_empty());
        }
        let (min, median, max, ratio) = spread(samples);
        println!("  {label:<28}  {min:>11.3?}  {median:>11.3?}  {max:>11.3?}  {ratio:>7.4}x");
        if label == "prose only" {
            prose_median = median;
        } else {
            let delta = median.saturating_sub(prose_median);
            println!("      the fence(s) add {delta:.3?} over prose");
        }
    }
}

/// The prose half of [`assistant_reply`]'s body, with no fence.
fn prose_only() -> String {
    String::from(
        "## Step 1\n\n\
         The change lands in `crates/zuno-tui/src/views/message.rs`, where the \
         transcript folds engine events into parts. **Note** that the width is \
         measured in columns.\n\n\
         - reads the file at revision 1\n\
         - rewrites the guard\n\
         - re-runs the affected tests\n",
    )
}

/// What a frame costs through the shipping render path, with the cache in it.
///
/// The three measurements above time [`TranscriptView::lines`], which is deliberately
/// cache-free — it is the specification the cache is checked against. This times what a
/// user actually pays: [`zuno_tui::app::render_offscreen`] drives `Component::render`,
/// which is the only caller of the cached path.
///
/// It reports the two cases that matter and they are different cases. A **keystroke**
/// forces a frame while no message changed, so every message is recalled. A **streaming
/// delta** changes the last message, so one message is re-rendered and the rest recalled.
/// Both were previously the full-transcript cost.
#[test]
fn render_cost_through_the_shipping_render_path() {
    // Clockless: the drawn buffer must be the same whichever way the frame was built, at
    // the height a real viewport has. A timing win on a wrong frame is not a win.
    let mut cached = view_of(16);
    let first = zuno_tui::app::render_offscreen(&mut cached, WIDTH, 40).expect("infallible");
    let second = zuno_tui::app::render_offscreen(&mut cached, WIDTH, 40).expect("infallible");
    assert_eq!(
        first, second,
        "the second frame of an unchanged transcript differs from the first"
    );

    if !measuring() {
        return;
    }

    println!("\nR2/R3/R4 — Component::render, the path a user pays for");
    println!(
        "  {:>6}  {:<18}  {:>11}  {:>11}  {:>11}  {:>8}",
        "msgs", "case", "min", "median", "max", "max/min"
    );
    for size in SIZES {
        let mut view = view_of(size);
        // Warm the cache, then time frames that change nothing: the keystroke case.
        let _ = zuno_tui::app::render_offscreen(&mut view, WIDTH, 40);
        let mut samples = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let started = Instant::now();
            let buffer = zuno_tui::app::render_offscreen(&mut view, WIDTH, 40);
            samples.push(started.elapsed());
            assert!(buffer.is_ok());
        }
        let (min, median, max, ratio) = spread(samples);
        println!(
            "  {size:>6}  {:<18}  {min:>11.3?}  {median:>11.3?}  {max:>11.3?}  {ratio:>7.4}x",
            "unchanged"
        );

        // The streaming case: one delta into the tail before each frame.
        let mut streaming = view_of(size);
        streaming.transcript_mut().push(assistant_reply(size + 1));
        let _ = zuno_tui::app::render_offscreen(&mut streaming, WIDTH, 40);
        let mut samples = Vec::with_capacity(RUNS);
        for round in 0..RUNS {
            append_delta(&mut streaming, round);
            let started = Instant::now();
            let buffer = zuno_tui::app::render_offscreen(&mut streaming, WIDTH, 40);
            samples.push(started.elapsed());
            assert!(buffer.is_ok());
        }
        let (min, median, max, ratio) = spread(samples);
        println!(
            "  {size:>6}  {:<18}  {min:>11.3?}  {median:>11.3?}  {max:>11.3?}  {ratio:>7.4}x",
            "streaming delta"
        );
    }
}

/// Grow the transcript's last text part, the way a provider delta does.
fn append_delta(view: &mut TranscriptView, round: usize) {
    let messages = view.transcript().messages().len();
    let transcript = view.transcript_mut();
    let mut replacement = assistant_reply(messages);
    replacement.parts.push(MessagePart::Text {
        text: format!(" delta {round}"),
    });
    transcript.push(replacement);
}

/// What serving a frame from a cache would cost, before building one.
///
/// [`TranscriptView::lines`] returns owned rows, so any cache above it pays a clone on
/// every hit. If the clone costs what the render costs, the cache buys nothing and the
/// right answer is to close R2/R4 rather than build them. This prices the hit.
#[test]
fn render_cost_of_cloning_a_prepared_frame() {
    // Clockless: a clone must be span-for-span equal to its source, or the price below
    // is for something that could not be served.
    let view = view_of(16);
    let lines = view.lines(WIDTH);
    let copy = lines.clone();
    assert_eq!(lines.len(), copy.len());
    for (index, (original, cloned)) in lines.iter().zip(copy.iter()).enumerate() {
        assert_eq!(
            original
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>(),
            cloned
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>(),
            "row {index} did not survive a clone"
        );
    }

    if !measuring() {
        return;
    }

    println!("\nR2/R4 — the price of a cache hit: cloning an already-prepared frame");
    println!(
        "  {:>6}  {:>7}  {:>11}  {:>11}  {:>11}  {:>8}",
        "msgs", "rows", "min", "median", "max", "max/min"
    );
    for size in SIZES {
        let view = view_of(size);
        let lines = view.lines(WIDTH);
        let mut samples = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let started = Instant::now();
            let cloned = lines.clone();
            samples.push(started.elapsed());
            assert_eq!(cloned.len(), lines.len());
        }
        let (min, median, max, ratio) = spread(samples);
        println!(
            "  {size:>6}  {:>7}  {min:>11.3?}  {median:>11.3?}  {max:>11.3?}  {ratio:>7.4}x",
            lines.len()
        );
    }
}

/// The heap a prepared frame holds: span text plus the per-span and per-row structs.
fn prepared_bytes(lines: &[ratatui::text::Line<'static>]) -> usize {
    lines
        .iter()
        .map(|line| {
            std::mem::size_of::<ratatui::text::Line<'static>>()
                + line
                    .spans
                    .iter()
                    .map(|span| {
                        std::mem::size_of::<ratatui::text::Span<'static>>() + span.content.len()
                    })
                    .sum::<usize>()
        })
        .sum()
}
