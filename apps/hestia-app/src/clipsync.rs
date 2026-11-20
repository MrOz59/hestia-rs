//! Clipboard sync with the host, as decisions (the controller does the
//! I/O): what to send, what to take, and never echo back what just came
//! from the other side.
//!
//! Each round reads the local clipboard (on Android only after it changed,
//! see `platform::clipboard_stamp`) and the host's. Local news goes to the
//! host first, so the device's clipboard wins when a stream starts (as in
//! Artemis); otherwise news from the host lands locally.

/// The text both sides agree on, and what is known of the local clipboard.
#[derive(Debug, Default)]
pub struct ClipSync {
    synced: Option<String>,
    stamp: Option<i64>,
    max_bytes: usize,
    too_big_noted: bool,
}

impl ClipSync {
    /// `max_bytes`: the most the host accepts.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            ..Self::default()
        }
    }

    /// Whether to read the local clipboard this round, given its change
    /// stamp (`None`: the platform can't tell, read every round).
    pub fn should_read_local(&mut self, stamp: Option<i64>) -> bool {
        match stamp {
            None => true,
            Some(s) if s > 0 && self.stamp != Some(s) => {
                self.stamp = Some(s);
                true
            }
            Some(_) => false,
        }
    }

    /// The local clipboard holds `text`: what to send to the host, if
    /// anything. Call [`sent`](Self::sent) once it went through.
    pub fn local(&mut self, text: Option<String>) -> Option<String> {
        let text = text.filter(|t| !t.is_empty() && self.synced.as_deref() != Some(t.as_str()))?;
        if text.len() > self.max_bytes {
            if !self.too_big_noted {
                self.too_big_noted = true;
                log::info!(
                    "clipboard not sent: {} KiB, the host takes up to {} KiB",
                    text.len() / 1024,
                    self.max_bytes / 1024
                );
            }
            return None;
        }
        Some(text)
    }

    pub fn sent(&mut self, text: String) {
        self.synced = Some(text);
    }

    /// Sending failed: read the local clipboard again next round.
    pub fn send_failed(&mut self) {
        self.stamp = None;
    }

    /// The host's clipboard holds `text`: what to put on the local one.
    /// Call [`taken`](Self::taken) once it's there.
    pub fn host(&self, text: String) -> Option<String> {
        (!text.is_empty() && self.synced.as_deref() != Some(text.as_str())).then_some(text)
    }

    /// `text` is on the local clipboard now; `stamp` is its new change
    /// stamp (our own write, not news to send back).
    pub fn taken(&mut self, text: String, stamp: Option<i64>) {
        self.synced = Some(text);
        if let Some(s) = stamp {
            self.stamp = Some(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_clipboard_goes_first_then_host_news_comes_back() {
        let mut s = ClipSync::new(1024);
        // Stream starts: the phone holds "a", the host "b".
        assert!(s.should_read_local(Some(10)));
        let send = s.local(Some("a".into())).unwrap();
        s.sent(send);
        assert_eq!(s.host("a".into()), None, "what we sent isn't news");
        // Nothing changed locally: not read again (no Android paste notice).
        assert!(!s.should_read_local(Some(10)));
        // Copied on the PC.
        let take = s.host("b".into()).unwrap();
        s.taken(take, Some(11));
        assert!(!s.should_read_local(Some(11)), "our own write isn't read back");
        assert_eq!(s.host("b".into()), None);
        // Copied on the phone afterwards.
        assert!(s.should_read_local(Some(12)));
        assert_eq!(s.local(Some("c".into())).as_deref(), Some("c"));
    }

    #[test]
    fn empty_unknown_and_too_big() {
        let mut s = ClipSync::new(4);
        assert!(!s.should_read_local(Some(0)), "empty / no focus");
        assert!(s.should_read_local(None), "desktop reads every round");
        assert_eq!(s.local(Some(String::new())), None);
        assert_eq!(s.local(Some("too long".into())), None);
        assert_eq!(s.host(String::new()), None, "an empty host clipboard doesn't wipe ours");
    }

    #[test]
    fn a_failed_send_is_retried() {
        let mut s = ClipSync::new(1024);
        assert!(s.should_read_local(Some(5)));
        assert!(s.local(Some("x".into())).is_some());
        s.send_failed();
        assert!(s.should_read_local(Some(5)), "read again after a failure");
        let send = s.local(Some("x".into())).unwrap();
        s.sent(send);
        assert_eq!(s.local(Some("x".into())), None);
    }
}
