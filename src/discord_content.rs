//! Bounded formatting for untrusted Discord evidence.

pub(crate) const MAX_FORWARDED_SNAPSHOTS: usize = 1;
pub(crate) const MAX_EMBEDS: usize = 10;
pub(crate) const MAX_EMBED_FIELDS: usize = 25;
pub(crate) const MAX_ATTACHMENTS: usize = 20;

/// Builds a bounded, UTF-8-safe plain-text representation of Discord evidence.
pub(crate) struct EvidenceText {
    text: String,
    max_bytes: usize,
    truncated: bool,
}

impl EvidenceText {
    #[must_use]
    pub(crate) fn new(content: &str, max_bytes: usize) -> Self {
        let mut text = Self {
            text: String::with_capacity(content.len().min(max_bytes)),
            max_bytes,
            truncated: false,
        };
        text.append(content);
        text
    }

    /// Appends one labelled section when its value is present and capacity remains.
    pub(crate) fn push(&mut self, label: &str, value: &str) {
        if value.is_empty() || self.truncated {
            return;
        }
        if !self.text.is_empty() {
            self.append("\n");
        }
        self.append(label);
        self.append(": ");
        self.append(value);
    }

    #[must_use]
    pub(crate) fn finish(self) -> String {
        self.text
    }

    fn append(&mut self, value: &str) {
        if self.truncated || value.is_empty() {
            return;
        }
        let remaining = self.max_bytes.saturating_sub(self.text.len());
        if value.len() <= remaining {
            self.text.push_str(value);
            return;
        }

        let marker = truncation_marker(self.max_bytes);
        let content_limit = self.max_bytes.saturating_sub(marker.len());
        self.truncate_to(content_limit);
        let available = content_limit.saturating_sub(self.text.len());
        let prefix_len = utf8_prefix_len(value, available);
        self.text.push_str(&value[..prefix_len]);
        self.text.push_str(marker);
        self.truncated = true;
    }

    fn truncate_to(&mut self, max_bytes: usize) {
        if self.text.len() <= max_bytes {
            return;
        }
        let end = utf8_prefix_len(&self.text, max_bytes);
        self.text.truncate(end);
    }
}

fn truncation_marker(max_bytes: usize) -> &'static str {
    match max_bytes {
        0 => "",
        1 => ".",
        2 => "..",
        _ => "...",
    }
}

fn utf8_prefix_len(value: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::EvidenceText;

    #[test]
    fn preserves_plain_content_that_fits() {
        assert_eq!(
            EvidenceText::new("plain message", 20).finish(),
            "plain message"
        );
    }

    #[test]
    fn bounds_sections_without_splitting_unicode() {
        let mut text = EvidenceText::new(&"\u{1f600}".repeat(2), 13);
        text.push("embed", "evidence");
        assert_eq!(text.finish(), "\u{1f600}\u{1f600}\ne...");
    }

    #[test]
    fn marks_tiny_truncations() {
        assert_eq!(EvidenceText::new("abcdef", 0).finish(), "");
        assert_eq!(EvidenceText::new("abcdef", 1).finish(), ".");
        assert_eq!(EvidenceText::new("abcdef", 2).finish(), "..");
    }
}
