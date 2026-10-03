//! Brief amendment watcher (RT-09, D-10).
//!
//! Polls a child's brief file and forwards each new `## Amendment N`
//! section as a [`SteerMessage::Steering`]. The agent loop drains the
//! steer channel only at iteration boundaries, so an amendment is never
//! injected in the middle of a tool call.
//!
//! Torn-read guard (T-01-15): the file is only parsed once its size is
//! unchanged across two consecutive polls, and an unterminated final
//! section is dropped (`parse_amendments_with(.., false)`).

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::agent::brief;
use crate::event::SteerMessage;

/// Spawn the watcher. `initial_last` is the highest amendment number
/// already folded into the initial task; those are never re-sent.
/// The task exits when the receiving side of `tx` is dropped.
pub fn spawn_brief_watcher(
    path: PathBuf,
    initial_last: u32,
    tx: mpsc::Sender<SteerMessage>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = initial_last;
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut prev_size: Option<u64> = None;
        let mut processed_size: Option<u64> = None;
        loop {
            ticker.tick().await;
            if tx.is_closed() {
                return;
            }
            let Ok(meta) = tokio::fs::metadata(&path).await else {
                prev_size = None;
                continue;
            };
            let size = meta.len();
            if prev_size != Some(size) {
                // Changed (or first sighting): wait for a stable poll.
                prev_size = Some(size);
                continue;
            }
            if processed_size == Some(size) {
                continue;
            }
            let Ok(content) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            processed_size = Some(size);
            let mut items = brief::parse_amendments_with(&content, false);
            items.sort_by_key(|(n, _)| *n);
            for (n, text) in items {
                if n <= last {
                    continue;
                }
                last = n;
                let msg = SteerMessage::Steering {
                    text: format!("Amendment {n} to your brief:\n\n{text}"),
                };
                if tx.send(msg).await.is_err() {
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const TICK: Duration = Duration::from_millis(20);

    fn tmp_brief(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nanopi-brief-watch-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("brief.md");
        let spec = brief::BriefSpec {
            task: "task".into(),
            ..Default::default()
        };
        std::fs::write(&p, brief::render_brief(&spec)).unwrap();
        p
    }

    fn texts(rx: &mut mpsc::Receiver<SteerMessage>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(SteerMessage::Steering { text }) = rx.try_recv() {
            out.push(text);
        }
        out
    }

    async fn settle() {
        tokio::time::sleep(TICK * 8).await;
    }

    #[tokio::test]
    async fn initial_amendments_are_not_resent() {
        let p = tmp_brief("initial");
        brief::append_amendment(&p, 1, "old one").unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let h = spawn_brief_watcher(p.clone(), 1, tx, TICK);
        settle().await;
        assert!(texts(&mut rx).is_empty());
        drop(rx);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn appended_amendment_is_sent_exactly_once() {
        let p = tmp_brief("once");
        let (tx, mut rx) = mpsc::channel(8);
        let h = spawn_brief_watcher(p.clone(), 0, tx, TICK);
        settle().await;
        brief::append_amendment(&p, 1, "Also do X").unwrap();
        settle().await;
        settle().await;
        let got = texts(&mut rx);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].starts_with("Amendment 1 to your brief:"));
        assert!(got[0].contains("Also do X"));
        drop(rx);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn half_written_append_waits_until_complete() {
        let p = tmp_brief("torn");
        let (tx, mut rx) = mpsc::channel(8);
        let h = spawn_brief_watcher(p.clone(), 0, tx, TICK);
        settle().await;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"\n## Amendment 1\n\npartial").unwrap();
        settle().await;
        assert!(texts(&mut rx).is_empty(), "torn section must not be sent");
        f.write_all(b" rest\n").unwrap();
        settle().await;
        let got = texts(&mut rx);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("partial rest"));
        drop(rx);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn duplicates_ignored_and_order_ascending() {
        let p = tmp_brief("dups");
        let (tx, mut rx) = mpsc::channel(8);
        let h = spawn_brief_watcher(p.clone(), 0, tx, TICK);
        settle().await;
        brief::append_amendment(&p, 1, "one").unwrap();
        brief::append_amendment(&p, 2, "two").unwrap();
        settle().await;
        brief::append_amendment(&p, 2, "two again").unwrap();
        settle().await;
        let got = texts(&mut rx);
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].contains("one") && got[1].contains("two"));
        drop(rx);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn watcher_stops_when_receiver_dropped() {
        let p = tmp_brief("stop");
        let (tx, rx) = mpsc::channel(8);
        let h = spawn_brief_watcher(p, 0, tx, TICK);
        drop(rx);
        tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .expect("watcher must exit")
            .unwrap();
    }
}
