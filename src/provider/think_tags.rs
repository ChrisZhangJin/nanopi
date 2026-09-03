//! Splits a LEADING `<think>…</think>` block inlined in OpenAI-compat
//! `delta.content` text into `Text` / `Think` segments, streaming-safe
//! across arbitrary chunk boundaries.
//!
//! `<think>…</think>` is not an OpenAI wire feature — it's a model-level
//! convention (R1 and its distills, QwQ, GLM, etc.) that gets served
//! through any OpenAI-compatible endpoint: ollama, vLLM, DeepSeek direct,
//! gateways. Some endpoints lift it into `reasoning_content`; many pass
//! it straight through in `content`. Left alone, that text prints as
//! ordinary reply text and pollutes `assistant_text`, the session
//! transcript, and `--output json`.
//!
//! # The position rule
//!
//! Only a **leading** `<think>` block counts as reasoning. Leading means:
//! nothing but whitespace has been emitted as ordinary text in this
//! stream before the opener. R1-lineage reasoning is always the first
//! thing in the message — that's the output format. A model *discussing*
//! the tag (explaining prompt formats, writing it inside a code fence)
//! produces it mid-answer, after some other text — so position
//! discriminates the two cases far more accurately than a vendor
//! allowlist, is vendor-independent, and needs no maintenance as vendors
//! come and go.
//!
//! Consequences, deliberate:
//! - A `<think>` opener appearing after any non-whitespace text is
//!   literal text, not a thinking block — including a second block after
//!   a first one closed. Once the leading window is over, it's over for
//!   the rest of the stream.
//! - Leading whitespace before the opener does not defeat the rule
//!   (`"\n\n<think>…"` still counts as leading).
//! - A nested opener while already inside a (leading) block is literal
//!   thinking content; the first `</think>` closes it.
//!
//! This splitter never drops a byte: an unclosed tag or a partial
//! delimiter prefix at end of stream is flushed via
//! [`InlineThinkSplitter::finish`].
//!
//! Residual failure mode: a model whose reasoning block arrives after
//! some other text (rare, but observed on flaky gateways that prepend a
//! stray token) degrades to the old behavior — it renders literally
//! instead of as a thinking block. That's an accepted, narrow cost: a
//! fence-aware / vendor-aware parser would need to guess intent, which
//! is itself a source of silent loss (see the `mere_mention_of_tag_*`→
//! now-literal history in this file's tests).

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// One classified span of streamed text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Think(String),
}

impl Segment {
    /// The inner string, regardless of variant. Handy for building the
    /// no-loss round-trip assertion in tests.
    #[cfg(test)]
    fn as_str(&self) -> &str {
        match self {
            Segment::Text(s) => s,
            Segment::Think(s) => s,
        }
    }
}

/// Streaming state machine that recognizes a LEADING `<think>` /
/// `</think>` pair across arbitrary chunk boundaries.
///
/// Holds a `carry` buffer containing any bytes not yet safely classified
/// (either because they might be the start of a delimiter, or because a
/// full delimiter has not yet arrived). The carry is bounded by
/// `CLOSE.len()` — the longer of the two delimiters — so it never grows
/// with total stream length (see T-L1S-01).
///
/// `leading` tracks whether a block may still start: true until either
/// non-whitespace text is emitted outside a block, or a block has
/// already opened and closed once. See the module docs for the full
/// position rule.
#[derive(Debug)]
pub struct InlineThinkSplitter {
    carry: String,
    inside: bool,
    leading: bool,
}

impl Default for InlineThinkSplitter {
    fn default() -> Self {
        Self::new()
    }
}

impl InlineThinkSplitter {
    pub fn new() -> Self {
        Self {
            carry: String::new(),
            inside: false,
            leading: true,
        }
    }

    /// Feed one SSE content delta; returns zero or more complete segments.
    /// Any trailing partial-tag prefix is retained internally.
    pub fn push(&mut self, chunk: &str) -> Vec<Segment> {
        if chunk.is_empty() {
            return Vec::new();
        }
        self.carry.push_str(chunk);
        self.drain(false)
    }

    /// End of stream: emit everything still buffered. Never drops bytes.
    pub fn finish(&mut self) -> Vec<Segment> {
        self.drain(true)
    }

    /// Core scan loop. When `flush_all` is true (end of stream), any
    /// remaining carry is emitted outright instead of held back as a
    /// possible partial delimiter prefix.
    fn drain(&mut self, flush_all: bool) -> Vec<Segment> {
        let mut out = Vec::new();
        loop {
            if self.inside {
                // Already inside a (leading) block: only CLOSE matters.
                if let Some(idx) = self.carry.find(CLOSE) {
                    let before = self.carry[..idx].to_string();
                    if !before.is_empty() {
                        out.push(Segment::Think(before));
                    }
                    self.carry = self.carry[idx + CLOSE.len()..].to_string();
                    self.inside = false;
                    // The leading window is over: one block has now run
                    // to completion, so no future opener — however much
                    // whitespace precedes it — is ever leading again.
                    self.leading = false;
                    continue;
                }
                if flush_all {
                    if !self.carry.is_empty() {
                        let text = std::mem::take(&mut self.carry);
                        out.push(Segment::Think(text));
                    }
                } else {
                    let keep = longest_prefix_suffix(&self.carry, CLOSE);
                    let split_at = self.carry.len() - keep;
                    if split_at > 0 {
                        let emit = self.carry[..split_at].to_string();
                        if !emit.is_empty() {
                            out.push(Segment::Think(emit));
                        }
                        self.carry = self.carry[split_at..].to_string();
                    }
                }
                break;
            }

            if !self.leading {
                // Past the leading window (either a block already ran,
                // or non-whitespace text already went out): `<think>` is
                // now indistinguishable from any other literal substring.
                // Flush verbatim, no delimiter scanning at all.
                if !self.carry.is_empty() {
                    let text = std::mem::take(&mut self.carry);
                    out.push(Segment::Text(text));
                }
                break;
            }

            // Still eligible for a leading block: look for the opener.
            if let Some(idx) = self.carry.find(OPEN) {
                let before_is_whitespace =
                    self.carry[..idx].chars().all(char::is_whitespace);
                if before_is_whitespace {
                    let before = self.carry[..idx].to_string();
                    if !before.is_empty() {
                        out.push(Segment::Text(before));
                    }
                    self.carry = self.carry[idx + OPEN.len()..].to_string();
                    self.inside = true;
                    continue;
                }
                // Non-whitespace precedes this opener: the stream can
                // never start a leading block. Disarm and reprocess —
                // the `!self.leading` branch above will flush
                // everything, including this `<think>`, as literal text.
                self.leading = false;
                continue;
            }

            // No full opener yet. Emit anything that can't possibly be
            // a delimiter prefix; hold back the rest. If what we emit
            // contains non-whitespace, the leading window is over.
            if flush_all {
                if !self.carry.is_empty() {
                    let text = std::mem::take(&mut self.carry);
                    if !text.chars().all(char::is_whitespace) {
                        self.leading = false;
                    }
                    out.push(Segment::Text(text));
                }
            } else {
                let keep = longest_prefix_suffix(&self.carry, OPEN);
                let split_at = self.carry.len() - keep;
                if split_at > 0 {
                    let emit = self.carry[..split_at].to_string();
                    if !emit.is_empty() {
                        if !emit.chars().all(char::is_whitespace) {
                            self.leading = false;
                        }
                        out.push(Segment::Text(emit));
                    }
                    self.carry = self.carry[split_at..].to_string();
                }
            }
            break;
        }
        out
    }
}

/// Length (in bytes) of the longest suffix of `s` that is also a proper
/// prefix of `delim`. Used to decide how much of the carry buffer might
/// still turn into a delimiter as more chunks arrive, so we hold it back
/// rather than emitting it prematurely.
///
/// Operates on char boundaries: candidate suffix lengths are chosen from
/// `s`'s `char_indices`, so a multi-byte character is never split.
fn longest_prefix_suffix(s: &str, delim: &str) -> usize {
    // Only need to consider suffixes up to delim.len() - 1 bytes long
    // (a full match would have been caught by `find` already).
    let max_len = delim.len().saturating_sub(1);
    // Walk char boundaries from the end, longest candidate first.
    let boundaries: Vec<usize> = s
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(s.len()))
        .collect();
    for &start in boundaries.iter().rev() {
        let suffix = &s[start..];
        if suffix.len() > max_len {
            continue;
        }
        if suffix.is_empty() {
            continue;
        }
        if delim.starts_with(suffix) {
            return suffix.len();
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concat_segments(segs: &[Segment]) -> String {
        segs.iter().map(|s| s.as_str()).collect()
    }

    #[test]
    fn leading_tag_in_one_chunk_splits() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("<think>b</think>c");
        segs.extend(sp.finish());
        assert_eq!(
            segs,
            vec![Segment::Think("b".into()), Segment::Text("c".into())]
        );
    }

    /// Position rule, positive case: leading whitespace before the
    /// opener must not defeat leading-block detection.
    #[test]
    fn leading_whitespace_before_opener_still_counts_as_leading() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("\n\n<think>reasoning</think>answer");
        segs.extend(sp.finish());
        assert_eq!(
            segs,
            vec![
                Segment::Text("\n\n".into()),
                Segment::Think("reasoning".into()),
                Segment::Text("answer".into()),
            ]
        );
    }

    /// Position rule, negative case: any non-whitespace text before the
    /// opener means it's never a thinking block, however far into the
    /// stream. Formerly split (`a<think>b</think>c` → three segments)
    /// under the two-vendor allowlist; now the whole thing is literal.
    #[test]
    fn opener_after_non_whitespace_text_is_literal() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("a<think>b</think>c");
        segs.extend(sp.finish());
        assert_eq!(segs, vec![Segment::Text("a<think>b</think>c".into())]);
    }

    /// The case this whole rule exists to protect: a model discussing
    /// or quoting the tag mid-answer (e.g. inside a code fence
    /// explaining the R1 output format) must not have that span eaten.
    #[test]
    fn code_fence_mid_answer_is_untouched() {
        let mut sp = InlineThinkSplitter::new();
        let input = "Sure — R1 wraps reasoning like:\n```\n<think>...</think>\n```\ndone";
        let mut segs = sp.push(input);
        segs.extend(sp.finish());
        assert_eq!(segs, vec![Segment::Text(input.into())]);
    }

    /// A second `<think>…</think>` after the first one closed is no
    /// longer treated as reasoning — the leading window closed the
    /// moment the first block completed. Formerly this yielded two
    /// `Think` segments and one `Text` segment under the vendor-gated
    /// design; now only the first block is reasoning, and everything
    /// from `x` onward (including the second block's literal tags) is
    /// one Text span.
    #[test]
    fn second_block_after_first_closes_stays_literal() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("<think>a</think>x<think>b</think>");
        segs.extend(sp.finish());
        assert_eq!(
            segs,
            vec![
                Segment::Think("a".into()),
                Segment::Text("x<think>b</think>".into()),
            ]
        );
    }

    /// The test that matters: split at EVERY char-boundary offset in the
    /// fixed input, feed the two halves separately, drain, and assert the
    /// concatenated result is identical across every split point. Uses a
    /// leading fixture so the splitter's OPEN/CLOSE state machine is
    /// actually exercised (not short-circuited by the position rule).
    #[test]
    fn split_at_every_offset_produces_identical_result() {
        let input = "<think>mid</think>post";
        let expected = {
            let mut sp = InlineThinkSplitter::new();
            let mut segs = sp.push(input);
            segs.extend(sp.finish());
            concat_segments(&segs)
        };

        let boundaries: Vec<usize> = input
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(input.len()))
            .collect();

        for &i in &boundaries {
            let mut sp = InlineThinkSplitter::new();
            let mut segs = sp.push(&input[..i]);
            segs.extend(sp.push(&input[i..]));
            segs.extend(sp.finish());
            let got = concat_segments(&segs);
            assert_eq!(got, expected, "mismatch splitting at offset {i}");
            // No segment is ever empty.
            assert!(segs.iter().all(|s| !s.as_str().is_empty()));
        }
    }

    #[test]
    fn byte_at_a_time_matches_whole_chunk() {
        let input = "<think>mid</think>post";
        let expected = {
            let mut sp = InlineThinkSplitter::new();
            let mut segs = sp.push(input);
            segs.extend(sp.finish());
            concat_segments(&segs)
        };

        let mut sp = InlineThinkSplitter::new();
        let mut segs = Vec::new();
        for ch in input.chars() {
            let mut buf = [0u8; 4];
            segs.extend(sp.push(ch.encode_utf8(&mut buf)));
        }
        segs.extend(sp.finish());
        // Feeding one char per `push` call yields more (but never empty,
        // never mis-tagged) segments than a whole-chunk feed — the
        // no-loss invariant is on the concatenated text, not segment
        // count.
        assert_eq!(concat_segments(&segs), expected);
        assert!(segs.iter().all(|s| !s.as_str().is_empty()));
    }

    #[test]
    fn cjk_reasoning_trace_split_at_every_offset() {
        // Multi-byte CJK content inside and outside the tag — the exact
        // case that motivated char-boundary-only slicing. Leading
        // fixture (opens the stream) so it still exercises the state
        // machine under the position rule.
        let input = "<think>推理内容</think>后缀";
        let expected = {
            let mut sp = InlineThinkSplitter::new();
            let mut segs = sp.push(input);
            segs.extend(sp.finish());
            concat_segments(&segs)
        };
        let boundaries: Vec<usize> = input
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(input.len()))
            .collect();
        for &i in &boundaries {
            let mut sp = InlineThinkSplitter::new();
            let mut segs = sp.push(&input[..i]);
            segs.extend(sp.push(&input[i..]));
            segs.extend(sp.finish());
            assert_eq!(concat_segments(&segs), expected, "mismatch at offset {i}");
        }
    }

    /// CJK whitespace-only preamble (full-width space, ideographic
    /// space) before the opener must also count as leading — the
    /// whitespace check is Unicode-aware via `char::is_whitespace`.
    #[test]
    fn cjk_whitespace_preamble_still_counts_as_leading() {
        let mut sp = InlineThinkSplitter::new();
        // U+3000 IDEOGRAPHIC SPACE
        let mut segs = sp.push("\u{3000}<think>推理</think>后缀");
        segs.extend(sp.finish());
        assert_eq!(
            segs,
            vec![
                Segment::Text("\u{3000}".into()),
                Segment::Think("推理".into()),
                Segment::Text("后缀".into()),
            ]
        );
    }

    /// A nested opener while already inside a (leading) block is literal
    /// thinking content; the FIRST `</think>` closes.
    #[test]
    fn nested_opener_is_literal_inside_thinking() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("<think>a<think>b</think>c");
        segs.extend(sp.finish());
        assert_eq!(
            segs,
            vec![
                Segment::Think("a<think>b".into()),
                Segment::Text("c".into()),
            ]
        );
    }

    /// Unclosed LEADING tag flushes as Think at end of stream — the
    /// no-loss invariant, and confirmation that an in-flight leading
    /// block doesn't need a closer to be recognized as reasoning.
    #[test]
    fn unclosed_leading_tag_flushes_as_think_at_finish() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("<think>partial");
        segs.extend(sp.finish());
        assert_eq!(segs, vec![Segment::Think("partial".into())]);
    }

    /// An unclosed opener that ISN'T leading (text precedes it) is just
    /// literal text, same as the closed case.
    #[test]
    fn unclosed_non_leading_tag_is_literal_text() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("x<think>partial");
        segs.extend(sp.finish());
        let total: String = concat_segments(&segs);
        assert_eq!(total, "x<think>partial");
        assert!(segs.iter().all(|s| matches!(s, Segment::Text(_))));
    }

    #[test]
    fn partial_prefix_at_end_of_stream_is_not_lost() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("abc<thi");
        segs.extend(sp.finish());
        let total: String = concat_segments(&segs);
        assert_eq!(total, "abc<thi");
        // Nothing should have been classified as Think — we never saw a
        // full opener.
        assert!(segs.iter().all(|s| matches!(s, Segment::Text(_))));
    }

    #[test]
    fn bare_closer_with_no_opener_is_literal_text() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("no opener </think> here");
        segs.extend(sp.finish());
        let total: String = concat_segments(&segs);
        assert_eq!(total, "no opener </think> here");
        assert!(segs.iter().all(|s| matches!(s, Segment::Text(_))));
    }

    /// Mere mid-text mention of the tag no longer splits under the
    /// position rule — this is the whole point of the change. Formerly
    /// (vendor-gated design) this DID split; now "the " precedes the
    /// opener, so it's never leading, and the entire string is literal.
    #[test]
    fn mere_mid_text_mention_of_tag_no_longer_splits() {
        let mut sp = InlineThinkSplitter::new();
        let input = "the <think> tag is used by mimo";
        let mut segs = sp.push(input);
        segs.extend(sp.finish());
        assert_eq!(segs, vec![Segment::Text(input.into())]);
    }

    #[test]
    fn empty_chunks_and_segments_never_emitted() {
        let mut sp = InlineThinkSplitter::new();
        assert_eq!(sp.push(""), Vec::<Segment>::new());
        let segs = sp.push("<think></think>");
        for s in &segs {
            assert!(!s.as_str().is_empty());
        }
        let fin = sp.finish();
        for s in &fin {
            assert!(!s.as_str().is_empty());
        }
    }

    /// Round-trip invariant, both regimes: for a genuinely leading
    /// stream, concatenating every emitted segment reproduces the input
    /// minus only complete delimiters. For a NON-leading stream (tags
    /// preceded by other text), the position rule means nothing is ever
    /// classified as Think, so concatenation reproduces the input
    /// byte-for-byte, delimiters included — no loss either way.
    #[test]
    fn round_trip_invariant_holds() {
        fn assert_round_trip(input: &str, split_at: &[usize], expected: &str) {
            let mut sp = InlineThinkSplitter::new();
            let mut segs = Vec::new();
            let mut last = 0;
            for &i in split_at {
                segs.extend(sp.push(&input[last..i]));
                last = i;
            }
            segs.extend(sp.push(&input[last..]));
            segs.extend(sp.finish());
            let got: String = concat_segments(&segs);
            assert_eq!(got, expected, "round-trip mismatch for {input:?}");
        }

        // Leading: first block's delimiters are stripped; a later
        // "second block" is literal (position rule), so ITS delimiters
        // survive in the output.
        assert_round_trip(
            "<think>d</think>c<think>d2</think>",
            &[1, 5, 10, 20],
            "dc<think>d2</think>",
        );
        assert_round_trip("no tags here at all", &[3, 7], "no tags here at all");
        assert_round_trip("<think>only think</think>", &[], "only think");

        // Non-leading: text precedes the tag, so nothing is ever
        // classified as Think — every byte, including delimiters,
        // survives verbatim.
        assert_round_trip(
            "a<think>b</think>c<think>d</think>",
            &[1, 5, 10, 20],
            "a<think>b</think>c<think>d</think>",
        );
    }
}
