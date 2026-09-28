//! Prompt-box draft classification (#149).
//!
//! Given a pane's plain-text and ANSI-styled screen snapshots, decide whether the agent's
//! input box currently holds real user-typed text, or is empty / showing only the agent's own
//! placeholder hint. This module is read-only: it never mutates the target's input box, and it
//! never guesses — anything it cannot line up between the two snapshots comes back as
//! [`PromptBoxState::Unreadable`], which callers must treat the same as "not confirmed empty".
//!
//! The two agent UIs shape their prompt box differently, so each gets its own locator:
//! Claude Code draws a box between two full-width `─` rules with a `❯` marker inside it;
//! Codex has no border, just a lone `›` marker line, ending at the next blank row.
//!
//! Placeholder hint text and a real draft are textually indistinguishable (both TUIs rotate the
//! hint through a pool of example prompts). The one reliable signal is the SGR "faint"/"dim"
//! (`\x1b[2m`) attribute both TUIs wrap placeholder text in and never apply to real input —
//! confirmed by hand for both Claude Code and Codex during the #149 investigation.
//!
//! Default action once a box is classified (user-approved, #149): empty/placeholder sends as
//! usual; a real draft is sent together with the caller's own text, prefixed by
//! [`DRAFT_NOTICE_LINE`], without touching the box; an unreadable box refuses to send. There is
//! no wait or opt-in override — the caller either gets a plan or a refusal, immediately.

/// What a prompt-box read found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptBoxState {
    /// No prompt box could be located, or the plain/ANSI snapshots didn't line up. Callers must
    /// not treat this as "confirmed empty" (AC-2).
    Unreadable,
    /// The box is empty, or shows only the agent's own placeholder hint text.
    EmptyOrPlaceholder,
    /// The box holds real user-typed text (verbatim, marker and indentation stripped).
    Draft(String),
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed.chars().all(|ch| ch == '─')
}

/// Claude Code: box between two `─` rules, `❯` marks the (possibly multi-line) prompt.
pub fn classify_claude_prompt_box(
    plain_screen: &str,
    ansi_screen: &str,
) -> PromptBoxState {
    let plain_lines: Vec<&str> =
        plain_screen.lines().collect();
    let ansi_lines: Vec<&str> =
        ansi_screen.lines().collect();
    if plain_lines.len() != ansi_lines.len() {
        return PromptBoxState::Unreadable;
    }
    let Some(top) = plain_lines
        .iter()
        .position(|line| is_horizontal_rule(line))
    else {
        return PromptBoxState::Unreadable;
    };
    let Some(marker_offset) =
        plain_lines[top + 1..].iter().position(|line| {
            line.trim_start().starts_with('❯')
        })
    else {
        return PromptBoxState::Unreadable;
    };
    let marker = top + 1 + marker_offset;
    let end = plain_lines[marker + 1..]
        .iter()
        .position(|line| is_horizontal_rule(line))
        .map(|relative| marker + 1 + relative)
        .unwrap_or(plain_lines.len());
    classify_body(
        &plain_lines[marker..end],
        &ansi_lines[marker..end],
        '❯',
    )
}

/// Codex: no border. `›` marks the current prompt line; the block ends at the next blank row.
pub fn classify_codex_prompt_box(
    plain_screen: &str,
    ansi_screen: &str,
) -> PromptBoxState {
    let plain_lines: Vec<&str> =
        plain_screen.lines().collect();
    let ansi_lines: Vec<&str> =
        ansi_screen.lines().collect();
    if plain_lines.len() != ansi_lines.len() {
        return PromptBoxState::Unreadable;
    }
    let Some(marker) =
        plain_lines.iter().position(|line| {
            line.trim() == "›"
                || line.trim_start().starts_with("› ")
        })
    else {
        return PromptBoxState::Unreadable;
    };
    let end = plain_lines[marker + 1..]
        .iter()
        .position(|line| line.trim().is_empty())
        .map(|relative| marker + 1 + relative)
        .unwrap_or(plain_lines.len());
    classify_body(
        &plain_lines[marker..end],
        &ansi_lines[marker..end],
        '›',
    )
}

fn classify_body(
    plain_rows: &[&str],
    ansi_rows: &[&str],
    marker: char,
) -> PromptBoxState {
    if plain_rows.is_empty()
        || ansi_rows.len() != plain_rows.len()
    {
        return PromptBoxState::Unreadable;
    }
    let text = plain_rows
        .iter()
        .map(|line| {
            line.trim_start_matches(marker).trim_start()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return PromptBoxState::EmptyOrPlaceholder;
    }
    let all_faint =
        ansi_rows.iter().enumerate().all(|(index, row)| {
            if index == 0 {
                row_is_all_faint(strip_leading_marker(
                    row, marker,
                ))
            } else {
                row_is_all_faint(row)
            }
        });
    if all_faint {
        PromptBoxState::EmptyOrPlaceholder
    } else {
        PromptBoxState::Draft(trimmed.to_string())
    }
}

/// Drops everything up to and including the first occurrence of `marker`, so the marker glyph
/// itself (which some TUIs style independently of the text that follows it) never counts toward
/// the faint check. Escape sequences before the marker are irrelevant either way since the scan
/// below tracks faint state from scratch.
fn strip_leading_marker(
    ansi_row: &str,
    marker: char,
) -> &str {
    for (index, ch) in ansi_row.char_indices() {
        if ch == marker {
            return &ansi_row[index + ch.len_utf8()..];
        }
    }
    ansi_row
}

/// True when every visible, non-whitespace character in this ANSI-styled row sits inside an
/// active SGR "faint" (`2`) span. A row with no visible characters at all counts as faint too
/// (it can't contradict the placeholder hypothesis).
fn row_is_all_faint(ansi_row: &str) -> bool {
    let mut faint = false;
    let mut chars = ansi_row.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            let mut params = String::new();
            let mut terminator = None;
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic()
                    || c == '@'
                    || c == '~'
                {
                    terminator = Some(c);
                    break;
                }
                params.push(c);
            }
            if terminator == Some('m') {
                apply_sgr_params(&params, &mut faint);
            }
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        if !faint {
            return false;
        }
    }
    true
}

/// Applies one SGR parameter string (the part between `\x1b[` and `m`) to `faint`. Extended
/// color selectors (`38`/`48` followed by `5;n` or `2;r;g;b`) must have their sub-parameters
/// consumed as a unit — otherwise the literal `2` inside a 24-bit background color (very common
/// in both TUIs' prompt-box highlight) is misread as the dim attribute.
fn apply_sgr_params(raw: &str, faint: &mut bool) {
    let params: Vec<&str> = raw.split(';').collect();
    let mut index = 0;
    while index < params.len() {
        match params[index] {
            "0" | "" => *faint = false,
            "2" => *faint = true,
            "22" => *faint = false,
            "38" | "48" => {
                match params.get(index + 1).copied() {
                    Some("5") => index += 2,
                    Some("2") => index += 4,
                    _ => {}
                }
            }
            _ => {}
        }
        index += 1;
    }
}

/// The agent kinds this module has a prompt-box locator for. Any other agent kind bypasses the
/// guard entirely (see [`plan_send`]) — the #149 investigation only verified the two TUIs below,
/// and refusing to send to every other agent kind for lack of a classifier would be a much
/// larger, unreviewed behavior change than this issue asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedAgent {
    Claude,
    Codex,
}

/// What the caller should do about the message it was about to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptDraftAction {
    /// The box was empty or only showed placeholder chrome; send the caller's text unchanged.
    SendAsIs,
    /// A real draft is present. Prepend [`DRAFT_NOTICE_LINE`] to the caller's own text and send
    /// both together, without touching the box (no clearing, no rewriting).
    IncludeDraftNotice,
    /// The box's state could not be confirmed; the caller must refuse to send and report why.
    Unreadable,
}

/// Prepended, on its own line, before the caller's text when a draft is detected (#149, user
/// decision 2026-09-28: default is to submit the draft together with the message, not to wait
/// or ask).
pub const DRAFT_NOTICE_LINE: &str = "前面是用户未发出的草稿，原样一并提交";

/// Classifies the target's prompt box and says what the caller should do next. This is the
/// single entry point send paths should call; it never reads or writes anything itself, only
/// judges the two snapshots the caller already has.
pub fn plan_send(plain_screen: &str, ansi_screen: &str, agent: SupportedAgent) -> PromptDraftAction {
    let state = match agent {
        SupportedAgent::Claude => classify_claude_prompt_box(plain_screen, ansi_screen),
        SupportedAgent::Codex => classify_codex_prompt_box(plain_screen, ansi_screen),
    };
    match state {
        PromptBoxState::Unreadable => PromptDraftAction::Unreadable,
        PromptBoxState::EmptyOrPlaceholder => PromptDraftAction::SendAsIs,
        PromptBoxState::Draft(_) => PromptDraftAction::IncludeDraftNotice,
    }
}

/// Builds the text to actually submit once [`plan_send`] returned [`PromptDraftAction::IncludeDraftNotice`].
pub fn text_with_draft_notice(caller_text: &str) -> String {
    format!("\n{DRAFT_NOTICE_LINE}\n{caller_text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_screen(
        plain_body: &str,
        ansi_body: &str,
    ) -> (String, String) {
        let plain = format!(
            " ▐▛███▛█   Claude Code v2.1.283\n\n{}\n{}\n{}\n  status line",
            "─".repeat(40),
            plain_body,
            "─".repeat(40),
        );
        let ansi = format!(
            "\u{1b}[0m ▐▛███▛█   Claude Code v2.1.283\n\n{}\n{}\n{}\n  status line",
            "─".repeat(40),
            ansi_body,
            "─".repeat(40),
        );
        (plain, ansi)
    }

    #[test]
    fn claude_placeholder_is_not_a_draft() {
        let (plain, ansi) = claude_screen(
            "❯ Try \"fix lint errors\"",
            "❯\u{a0}\u{1b}[0m\u{1b}[2mTry \"fix lint errors\"\u{1b}[0m",
        );
        assert_eq!(
            classify_claude_prompt_box(&plain, &ansi),
            PromptBoxState::EmptyOrPlaceholder
        );
    }

    #[test]
    fn claude_real_single_line_draft_is_detected() {
        let (plain, ansi) = claude_screen(
            "❯ 已经 switch 了，复验吧",
            "❯\u{a0}已经 switch 了，复验吧",
        );
        assert_eq!(
            classify_claude_prompt_box(&plain, &ansi),
            PromptBoxState::Draft(
                "已经 switch 了，复验吧".to_string()
            )
        );
    }

    #[test]
    fn claude_multi_line_draft_is_detected_in_full() {
        let (plain, ansi) = claude_screen(
            "❯ 第一行草稿\n  第二行草稿",
            "❯\u{a0}第一行草稿\n  第二行草稿",
        );
        assert_eq!(
            classify_claude_prompt_box(&plain, &ansi),
            PromptBoxState::Draft(
                "第一行草稿\n第二行草稿".to_string()
            )
        );
    }

    #[test]
    fn claude_true_color_background_is_not_mistaken_for_faint(
    ) {
        // A `48;2;r;g;b` truecolor background contains a literal "2" token that must not be
        // read as the SGR dim code on its own.
        let (plain, ansi) = claude_screen(
            "❯ 已经 switch 了",
            "❯\u{a0}\u{1b}[48;2;57;57;71m已经 switch 了\u{1b}[0m",
        );
        assert_eq!(
            classify_claude_prompt_box(&plain, &ansi),
            PromptBoxState::Draft(
                "已经 switch 了".to_string()
            )
        );
    }

    #[test]
    fn claude_no_box_found_is_unreadable() {
        let plain = " ▐▛███▛█   Claude Code v2.1.283\n  Accessing workspace:\n\n  /tmp";
        let ansi = plain;
        assert_eq!(
            classify_claude_prompt_box(plain, ansi),
            PromptBoxState::Unreadable
        );
    }

    #[test]
    fn claude_mismatched_snapshots_are_unreadable() {
        let (plain, ansi) =
            claude_screen("❯ 草稿", "❯\u{a0}草稿");
        // Force a mismatch: one extra ANSI line the plain snapshot doesn't have.
        let ansi =
            format!("{ansi}\nextra line only in ansi");
        assert_eq!(
            classify_claude_prompt_box(&plain, &ansi),
            PromptBoxState::Unreadable
        );
    }

    fn codex_screen(
        plain_body: &str,
        ansi_body: &str,
    ) -> (String, String) {
        let plain = format!(
            "╭──╮\n│ >_ OpenAI Codex │\n╰──╯\n\n{plain_body}\n\n  Ready · status"
        );
        let ansi = format!(
            "╭──╮\n│ >_ OpenAI Codex │\n╰──╯\n\n{ansi_body}\n\n  Ready · status"
        );
        (plain, ansi)
    }

    #[test]
    fn codex_placeholder_is_not_a_draft() {
        let (plain, ansi) = codex_screen(
            "› Ask Codex to do anything",
            "\u{1b}[1m\u{1b}[48;2;57;57;71m›\u{1b}[0m\u{1b}[48;2;57;57;71m \u{1b}[0m\u{1b}[2m\u{1b}[48;2;57;57;71mAsk Codex to do anything\u{1b}[0m",
        );
        assert_eq!(
            classify_codex_prompt_box(&plain, &ansi),
            PromptBoxState::EmptyOrPlaceholder
        );
    }

    #[test]
    fn codex_real_draft_is_detected() {
        let (plain, ansi) = codex_screen(
            "› 钥匙串解锁了",
            "\u{1b}[1m\u{1b}[48;2;57;57;71m›\u{1b}[0m\u{1b}[48;2;57;57;71m 钥匙串解锁了\u{1b}[0m",
        );
        assert_eq!(
            classify_codex_prompt_box(&plain, &ansi),
            PromptBoxState::Draft(
                "钥匙串解锁了".to_string()
            )
        );
    }

    #[test]
    fn codex_multi_line_draft_is_detected_in_full() {
        let (plain, ansi) = codex_screen(
            "› 第一行\n  第二行",
            "› 第一行\n  第二行",
        );
        assert_eq!(
            classify_codex_prompt_box(&plain, &ansi),
            PromptBoxState::Draft(
                "第一行\n第二行".to_string()
            )
        );
    }

    #[test]
    fn codex_no_marker_found_is_unreadable() {
        let plain = "╭──╮\n│ >_ OpenAI Codex │\n╰──╯\n  Ready · status";
        assert_eq!(
            classify_codex_prompt_box(plain, plain),
            PromptBoxState::Unreadable
        );
    }

    #[test]
    fn plan_send_empty_box_sends_as_is() {
        let (plain, ansi) = claude_screen(
            "❯ Try \"fix lint errors\"",
            "❯\u{a0}\u{1b}[0m\u{1b}[2mTry \"fix lint errors\"\u{1b}[0m",
        );
        assert_eq!(
            plan_send(&plain, &ansi, SupportedAgent::Claude),
            PromptDraftAction::SendAsIs
        );
    }

    #[test]
    fn plan_send_draft_includes_notice() {
        let (plain, ansi) = codex_screen(
            "› 钥匙串解锁了",
            "\u{1b}[1m\u{1b}[48;2;57;57;71m›\u{1b}[0m\u{1b}[48;2;57;57;71m 钥匙串解锁了\u{1b}[0m",
        );
        assert_eq!(
            plan_send(&plain, &ansi, SupportedAgent::Codex),
            PromptDraftAction::IncludeDraftNotice
        );
    }

    #[test]
    fn plan_send_unreadable_box_refuses() {
        let plain = "╭──╮\n│ >_ OpenAI Codex │\n╰──╯\n  Ready · status";
        assert_eq!(
            plan_send(plain, plain, SupportedAgent::Codex),
            PromptDraftAction::Unreadable
        );
    }

    #[test]
    fn text_with_draft_notice_prepends_notice_line() {
        assert_eq!(
            text_with_draft_notice("原始消息"),
            "\n前面是用户未发出的草稿，原样一并提交\n原始消息"
        );
    }
}
