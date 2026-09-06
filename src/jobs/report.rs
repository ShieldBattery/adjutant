//! A bounded, readable Discord preview for diagnostic reports.

use std::fmt::Write as _;

const SUMMARY_LIMIT: usize = 500;
const NEXT_CHECKS_LIMIT: usize = 600;
const DETAILS_LIMIT: usize = 500;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct DiscordReport {
    pub(super) content: String,
    pub(super) attach_full_report: bool,
}

#[derive(Clone, Copy)]
enum Section {
    Summary,
    Confidence,
    Evidence,
    LikelyCause,
    NextChecks,
    Other,
}

#[derive(Default)]
struct ParsedReport {
    summary: String,
    confidence: String,
    evidence: String,
    likely_cause: String,
    next_checks: String,
    other: String,
    has_known_sections: bool,
}

/// Formats only the Discord preview; the caller retains and attaches the original report.
pub(super) fn render(report: &str) -> DiscordReport {
    let mut parsed = parse(report);
    if !parsed.has_known_sections {
        report.trim().clone_into(&mut parsed.summary);
        parsed.other.clear();
    }
    // The Discord reply links to the request, so there is no need to echo its title or URLs.
    let details = detail_preview(&parsed);
    let mut content = String::new();
    let mut attach_full_report = false;
    for (heading, value, limit) in [
        ("summary", parsed.summary.trim(), SUMMARY_LIMIT),
        ("next checks", parsed.next_checks.trim(), NEXT_CHECKS_LIMIT),
        ("details", details.as_str(), DETAILS_LIMIT),
    ] {
        if !value.is_empty() {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            let (preview, attach) = public_preview(value, limit);
            let _ = write!(content, "**{heading}**\n{preview}");
            attach_full_report |= attach;
        }
    }
    if content.is_empty() {
        content.push_str("no diagnostic details were returned.");
        attach_full_report = !report.trim().is_empty();
    }
    if attach_full_report {
        content.push_str("\n\n_the complete diagnosis is attached._");
    }
    debug_assert!(content.encode_utf16().count() < 2_000);
    DiscordReport {
        content,
        attach_full_report,
    }
}

fn parse(report: &str) -> ParsedReport {
    let mut parsed = ParsedReport::default();
    let mut section = Section::Other;
    let mut fence: Option<(char, usize)> = None;
    for line in report.lines() {
        if let Some((delimiter, length)) = fence {
            let trimmed = line.trim_start();
            let run = trimmed.chars().take_while(|&ch| ch == delimiter).count();
            if run >= length && trimmed[run..].trim().is_empty() {
                fence = None;
            }
        } else if let Some(opening) = opening_fence(line) {
            fence = Some(opening);
        } else if let Some(next_section) = familiar_heading(line) {
            parsed.has_known_sections = true;
            section = next_section;
            continue;
        }
        let buffer = match section {
            Section::Summary => &mut parsed.summary,
            Section::Confidence => &mut parsed.confidence,
            Section::Evidence => &mut parsed.evidence,
            Section::LikelyCause => &mut parsed.likely_cause,
            Section::NextChecks => &mut parsed.next_checks,
            Section::Other => &mut parsed.other,
        };
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(line);
    }
    parsed
}

fn opening_fence(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let delimiter = trimmed.chars().next()?;
    if !matches!(delimiter, '`' | '~') {
        return None;
    }
    let length = trimmed.chars().take_while(|&ch| ch == delimiter).count();
    (length >= 3).then_some((delimiter, length))
}

fn familiar_heading(line: &str) -> Option<Section> {
    let mut heading = line.trim();
    let hashes = heading.bytes().take_while(|&byte| byte == b'#').count();
    if hashes > 0 {
        if hashes > 6 || !heading[hashes..].starts_with(char::is_whitespace) {
            return None;
        }
        heading = heading[hashes..].trim_start();
    }
    heading = heading.strip_suffix(':').unwrap_or(heading).trim_end();
    if let Some(stripped) = heading
        .strip_prefix("**")
        .and_then(|s| s.strip_suffix("**"))
    {
        heading = stripped.trim();
    }
    heading = heading.strip_suffix(':').unwrap_or(heading).trim_end();
    match heading.to_ascii_lowercase().as_str() {
        "summary" => Some(Section::Summary),
        "confidence" => Some(Section::Confidence),
        "evidence" => Some(Section::Evidence),
        "likely cause" => Some(Section::LikelyCause),
        "recommended next checks" | "next checks" => Some(Section::NextChecks),
        _ => None,
    }
}

fn detail_preview(parsed: &ParsedReport) -> String {
    let mut details = String::new();
    for (heading, value) in [
        ("confidence", &parsed.confidence),
        ("evidence", &parsed.evidence),
        ("likely cause", &parsed.likely_cause),
        ("other diagnostic details", &parsed.other),
    ] {
        let value = value.trim();
        if !value.is_empty() {
            if !details.is_empty() {
                details.push_str("\n\n");
            }
            let _ = write!(details, "{heading}: {value}");
        }
    }
    details
}

fn public_preview(value: &str, limit: usize) -> (String, bool) {
    if value.encode_utf16().count() > limit
        || has_complex_markdown(value)
        || !value.matches('`').count().is_multiple_of(2)
    {
        // Flatten complex/clipped public Markdown so an unfinished delimiter cannot affect
        // the next section. Simple lists, emphasis, and complete inline code stay formatted.
        let (preview, _) = escaped_preview(value, limit);
        (preview, true)
    } else {
        (value.to_owned(), false)
    }
}

fn has_complex_markdown(value: &str) -> bool {
    value.contains("||")
        || value.contains('\\')
        || value.lines().any(|line| opening_fence(line).is_some())
}

fn needs_escape(character: char) -> bool {
    matches!(
        character,
        '\\' | '|' | '`' | '*' | '_' | '~' | '#' | '[' | ']' | '<' | '>'
    )
}

fn escaped_preview(value: &str, limit: usize) -> (String, bool) {
    let escaped_len: usize = value
        .chars()
        .map(|ch| ch.len_utf16() + usize::from(needs_escape(ch)))
        .sum();
    let truncated = escaped_len > limit;
    let budget = limit.saturating_sub(usize::from(truncated));
    let mut result = String::new();
    let mut used = 0;
    for character in value.chars() {
        let units = character.len_utf16() + usize::from(needs_escape(character));
        if used + units > budget {
            break;
        }
        if needs_escape(character) {
            result.push('\\');
        }
        result.push(character);
        used += units;
    }
    if truncated && limit > 0 {
        result.push('\u{2026}');
    }
    (result, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn familiar_sections_place_next_checks_before_details() {
        let report = "**summary**\nThe match desynced after reconnecting.\n\n**confidence**\nmedium\n\n**evidence**\n- client log ends at `frame 8`\n\n**likely cause**\nreconnect state was stale\n\n**recommended next checks**\n- inspect the reconnect trace";

        let rendered = render(report);

        let next_checks = rendered.content.find("**next checks**").unwrap();
        let details = rendered.content.find("**details**").unwrap();
        assert!(rendered.content.starts_with("**summary**"));
        assert!(next_checks < details);
        assert!(rendered.content.contains("confidence: medium"));
        assert!(rendered.content.contains("- client log ends at `frame 8`"));
        assert!(!rendered.content.contains("||"));
        assert!(!rendered.attach_full_report);
    }

    #[test]
    fn headings_inside_fenced_code_do_not_split_the_summary() {
        let report = "## Summary\nObserved this output:\n```text\n## Evidence\nnot a real section\n```\n\n### Next checks\n- rerun the report";

        let rendered = render(report);

        assert!(
            parse(report)
                .summary
                .contains("## Evidence\nnot a real section")
        );
        assert!(
            rendered
                .content
                .contains("**next checks**\n- rerun the report")
        );
        assert!(rendered.attach_full_report);
    }

    #[test]
    fn long_details_do_not_crowd_out_next_checks() {
        let report = format!(
            "summary\nshort summary\n\nnext checks\n- collect the replay\n\nevidence\n{}",
            "detail ".repeat(300)
        );

        let rendered = render(&report);

        assert!(
            rendered
                .content
                .contains("**next checks**\n- collect the replay")
        );
        assert!(rendered.content.contains("**details**\nevidence"));
        assert!(rendered.attach_full_report);
        assert!(
            rendered
                .content
                .contains("\u{2026}\n\n_the complete diagnosis is attached._")
        );
    }

    #[test]
    fn missing_structured_sections_do_not_invent_a_diagnosis() {
        let rendered = render("evidence\n- the supplied log is incomplete");
        assert_eq!(
            rendered.content,
            "**details**\nevidence: - the supplied log is incomplete"
        );
        assert!(!rendered.attach_full_report);
    }

    #[test]
    fn complex_detail_markdown_stays_visible_and_attached() {
        let report =
            "summary\nshort\n\nevidence\n```text\n||hidden marker||\n```\n\nnext checks\n- retry";

        let rendered = render(report);

        assert!(!rendered.content.contains("||"));
        assert!(rendered.content.contains(r"\|\|hidden marker\|\|"));
        assert!(rendered.content.contains(r"\`\`\`text"));
        assert!(rendered.attach_full_report);
    }

    #[test]
    fn astral_unicode_is_never_split_and_the_message_stays_under_discord_limit() {
        let report = format!(
            "summary\n{}\n\nnext checks\n{}\n\nevidence\n{}",
            "😀".repeat(400),
            "🚀".repeat(400),
            "🛰️".repeat(400),
        );

        let rendered = render(&report);

        assert!(rendered.content.encode_utf16().count() < 2_000);
        assert!(rendered.content.is_char_boundary(rendered.content.len()));
        assert!(rendered.attach_full_report);
    }

    #[test]
    fn unstructured_short_reports_stay_inline_without_an_attachment() {
        let rendered = render("The bug report has no diagnostic structure yet.");

        assert!(
            rendered
                .content
                .contains("**summary**\nThe bug report has no diagnostic structure yet.")
        );
        assert!(!rendered.attach_full_report);
    }

    #[test]
    fn unstructured_long_reports_are_trimmed_and_attached() {
        let rendered = render(&"plain report ".repeat(100));

        assert!(rendered.content.contains("plain report"));
        assert!(rendered.attach_full_report);
    }

    #[test]
    fn absent_sections_do_not_generate_empty_details_or_placeholder_prose() {
        let rendered = render("## summary\nblocked\n\n## next checks\n- retry");
        assert_eq!(
            rendered.content,
            "**summary**\nblocked\n\n**next checks**\n- retry"
        );
        let plain = render("brief answer");
        assert_eq!(plain.content, "**summary**\nbrief answer");
    }

    #[test]
    fn heading_only_reports_still_produce_a_sendable_message() {
        let rendered = render("## summary\n\n## next checks");
        assert!(
            rendered
                .content
                .starts_with("no diagnostic details were returned.")
        );
        assert!(rendered.attach_full_report);
    }

    #[test]
    fn heading_variants_and_nested_fence_markers_preserve_sections() {
        let report = "# **Summary**:\nshort\n## evidence\n````text\n```\n## next checks\nstill code\n````\n**next checks:**\n- real check";
        let parsed = parse(report);
        assert_eq!(parsed.summary.trim(), "short");
        assert!(parsed.evidence.contains("## next checks\nstill code"));
        assert_eq!(parsed.next_checks.trim(), "- real check");
        let tilde = parse("## evidence\n~~~text\n## summary\ncode\n~~~\n## summary\nreal summary");
        assert_eq!(tilde.summary.trim(), "real summary");
    }

    #[test]
    fn literal_pipes_backslashes_and_clipped_escapes_stay_visible() {
        for (ending, expected) in [
            ("|", "|"),
            ("\\", r"\\"),
            ("`", r"\`"),
            ("||", r"\|\|"),
            ("\\|", r"\\\|"),
        ] {
            let result = render(&format!("## evidence\nends in {ending}"));
            assert!(!result.content.contains("||"));
            assert!(
                result
                    .content
                    .contains(&format!("**details**\nevidence: ends in {expected}"))
            );
            for limit in 0..8 {
                let (escaped, _) = escaped_preview(&ending.repeat(20), limit);
                assert!(escaped.encode_utf16().count() <= limit);
                assert!(!escaped.contains("||"));
                assert_eq!(
                    escaped.chars().rev().take_while(|&ch| ch == '\\').count() % 2,
                    0
                );
            }
        }
    }

    #[test]
    fn clipped_public_code_cannot_consume_following_sections() {
        let source = format!(
            "## summary\n`{}\n## next checks\n- retry\n## evidence\nsomething",
            "x".repeat(600)
        );
        let result = render(&source);
        assert!(result.content.contains("**summary**\n\\`"));
        assert!(result.content.contains("**next checks**\n- retry"));
        assert!(result.content.contains("**details**\nevidence: something"));
        assert!(result.attach_full_report);
    }
}
