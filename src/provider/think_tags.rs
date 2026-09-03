//! Splits `<think>…</think>` blocks inlined in OpenAI-compat `delta.content`
//! text into `Text` / `Think` segments, streaming-safe across arbitrary
//! chunk boundaries.
//!
//! Some vendors (Xiaomi MiMo, MiniMax) served over the OpenAI-compatible
//! wire inline their reasoning as literal `<think>…</think>` tags inside
//! `delta.content`, rather than using a separate `reasoning_content` field
//! or the Anthropic-native `thinking` block. Left alone, that text is
//! printed as ordinary reply text and accumulated into `assistant_text`,
//! polluting the context, the session transcript, and `--output json`.
//!
//! This splitter recognizes those tags across delta boundaries and
//! reclassifies the enclosed text as `Think`, everything else as `Text`.
//! It never drops a byte: an unclosed tag or a partial delimiter prefix at
//! end of stream is flushed via [`InlineThinkSplitter::finish`].

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

/// Streaming state machine that recognizes `<think>` / `</think>` across
/// arbitrary chunk boundaries.
///
/// Holds a `carry` buffer containing any bytes not yet safely classified
/// (either because they might be the start of a delimiter, or because a
/// full delimiter has not yet arrived). The carry is bounded by
/// `CLOSE.len()` — the longer of the two delimiters — so it never grows
/// with total stream length (see T-L1S-01).
#[derive(Debug, Default)]
pub struct InlineThinkSplitter {
    carry: String,
    inside: bool,
}

impl InlineThinkSplitter {
    pub fn new() -> Self {
        Self { carry: String::new(), inside: false }
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
        let out = self.drain(true);
        out
    }

    /// Core scan loop. When `flush_all` is true (end of stream), any
    /// remaining carry is emitted outright instead of held back as a
    /// possible partial delimiter prefix.
    fn drain(&mut self, flush_all: bool) -> Vec<Segment> {
        let mut out = Vec::new();
        loop {
            let delim = if self.inside { CLOSE } else { OPEN };
            if let Some(idx) = self.carry.find(delim) {
                // idx is a byte offset from `find`, guaranteed to land on
                // a char boundary since `delim` is ASCII and `find`
                // returns match-start offsets that are always on
                // boundaries relative to the haystack.
                let before = self.carry[..idx].to_string();
                if !before.is_empty() {
                    out.push(if self.inside {
                        Segment::Think(before)
                    } else {
                        Segment::Text(before)
                    });
                }
                let rest = self.carry[idx + delim.len()..].to_string();
                self.carry = rest;
                self.inside = !self.inside;
                continue;
            }

            // No full delimiter present. Figure out how much of the
            // carry might be a prefix of the delimiter we're scanning
            // for, and hold that much back (unless flushing at EOF).
            if flush_all {
                if !self.carry.is_empty() {
                    let text = std::mem::take(&mut self.carry);
                    out.push(if self.inside { Segment::Think(text) } else { Segment::Text(text) });
                }
            } else {
                let keep = longest_prefix_suffix(&self.carry, delim);
                let split_at = self.carry.len() - keep;
                if split_at > 0 {
                    let emit = self.carry[..split_at].to_string();
                    if !emit.is_empty() {
                        out.push(if self.inside { Segment::Think(emit) } else { Segment::Text(emit) });
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
    fn whole_tag_in_one_chunk() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("a<think>b</think>c");
        segs.extend(sp.finish());
        assert_eq!(
            segs,
            vec![
                Segment::Text("a".into()),
                Segment::Think("b".into()),
                Segment::Text("c".into()),
            ]
        );
    }

    /// The test that matters: split at EVERY char-boundary offset in the
    /// fixed input, feed the two halves separately, drain, and assert the
    /// concatenated result is identical across every split point.
    #[test]
    fn split_at_every_offset_produces_identical_result() {
        let input = "pre<think>mid</think>post";
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
        let input = "pre<think>mid</think>post";
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
        // case that motivated char-boundary-only slicing.
        let input = "前缀<think>推理内容</think>后缀";
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

    #[test]
    fn repeated_blocks_yield_two_think_one_text() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("<think>a</think>x<think>b</think>");
        segs.extend(sp.finish());
        assert_eq!(
            segs,
            vec![
                Segment::Think("a".into()),
                Segment::Text("x".into()),
                Segment::Think("b".into()),
            ]
        );
    }

    /// A nested opener while already inside is literal thinking content;
    /// the FIRST `</think>` closes. Pinned explicitly per D-plan.
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

    #[test]
    fn unclosed_tag_flushes_as_think_at_finish() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("x<think>partial");
        segs.extend(sp.finish());
        assert_eq!(
            segs,
            vec![Segment::Text("x".into()), Segment::Think("partial".into())]
        );
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

    /// Mere mention of the tag DOES split under the current design — the
    /// tag is indistinguishable from a real delimiter. Document the
    /// behavior, and assert no non-delimiter byte is lost.
    #[test]
    fn mere_mention_of_tag_splits_but_loses_no_other_bytes() {
        let mut sp = InlineThinkSplitter::new();
        let mut segs = sp.push("the <think> tag is used by mimo");
        segs.extend(sp.finish());
        let total: String = concat_segments(&segs);
        assert_eq!(total, "the  tag is used by mimo");
        assert_eq!(segs[0], Segment::Text("the ".into()));
        assert_eq!(segs[1], Segment::Think(" tag is used by mimo".into()));
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

    /// Round-trip invariant helper, used ad hoc above and reasserted here
    /// with a fresh case: concatenating every emitted segment reproduces
    /// the input, minus only complete delimiters.
    #[test]
    fn round_trip_invariant_holds() {
        fn assert_round_trip(input: &str, split_at: &[usize]) {
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
            let expected = input.replace(OPEN, "").replace(CLOSE, "");
            assert_eq!(got, expected);
        }

        assert_round_trip("a<think>b</think>c<think>d</think>", &[1, 5, 10, 20]);
        assert_round_trip("no tags here at all", &[3, 7]);
        assert_round_trip("<think>only think</think>", &[]);
    }
}
