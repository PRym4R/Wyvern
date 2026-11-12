//! Local message search behind Ctrl+F.
//!
//! Pure text work, no egui: the overlay only paints what this returns, so the
//! matching rules are testable without a frame. Only messages already loaded
//! in the open channel are searched — the REST search is a later step.

use std::sync::Arc;

use crate::models::ChatMessage;

/// Chars shown per result row: enough for the hit to sit in context, short
/// enough to keep the list scannable.
pub(crate) const SNIPPET_CHARS: usize = 110;
/// Chars of context kept before the hit, so the eye lands on it.
const CONTEXT_BEFORE: usize = 28;
/// Results kept per query; the rest are behind the scroll anyway.
pub(crate) const MAX_HITS: usize = 60;

/// One message body cut around the match, in three pieces so the overlay can
/// paint the hit itself in the accent colour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snippet {
    pub(crate) before: String,
    pub(crate) hit: String,
    pub(crate) after: String,
}

/// A message that matched, ready for the result list.
#[derive(Clone, Debug)]
pub(crate) struct Hit {
    pub(crate) message_id: String,
    pub(crate) author: String,
    pub(crate) snippet: Snippet,
}

/// Char index of `needle` in `text`, compared case-insensitively.
///
/// `needle` is expected lowercase (the caller lowers it once per query).
/// Lowercasing is tracked char by char because it can change the char count
/// (`İ` becomes two chars): a byte offset into the lowered text would land
/// inside a different character of the original.
pub(crate) fn find_at(text: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let mut lowered = String::with_capacity(text.len());
    // Original char index for every lowered char.
    let mut origin: Vec<usize> = Vec::with_capacity(text.len());
    for (i, ch) in text.chars().enumerate() {
        for low in ch.to_lowercase() {
            lowered.push(low);
            origin.push(i);
        }
    }
    let byte = lowered.find(needle)?;
    let lowered_index = lowered[..byte].chars().count();
    origin.get(lowered_index).copied()
}

/// Folds every whitespace break into a space: a result row is one line, and a
/// newline inside a button would stretch it.
fn one_line(chars: &[char]) -> String {
    chars
        .iter()
        .map(|c| if c.is_whitespace() { ' ' } else { *c })
        .collect()
}

/// `text` cut to a window around the match at char `at`, which is `len` chars
/// long. `…` marks a clipped edge.
pub(crate) fn snippet(text: &str, at: usize, len: usize) -> Snippet {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let at = at.min(total);
    let start = at.saturating_sub(CONTEXT_BEFORE);
    let end = (start + SNIPPET_CHARS).max(at + len).min(total);
    let mut before = one_line(&chars[start..at]);
    if start > 0 {
        before.insert(0, '…');
    }
    let hit = one_line(&chars[at..(at + len).min(end)]);
    let mut after = one_line(&chars[(at + len).min(end)..end]);
    if end < total {
        after.push('…');
    }
    Snippet { before, hit, after }
}

/// Messages whose body contains `query`, newest first.
///
/// Newest first because a search usually looks for something recent; the order
/// also makes Enter jump to the freshest hit. Optimistic echoes are skipped:
/// they have no id yet, so the view could not jump to them.
pub(crate) fn hits(msgs: &[Arc<ChatMessage>], query: &str) -> Vec<Hit> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    let len = query.chars().count();
    let mut out = Vec::new();
    for msg in msgs.iter().rev() {
        if msg.id.is_empty() {
            continue;
        }
        let Some(at) = find_at(&msg.content, &needle) else {
            continue;
        };
        out.push(Hit {
            message_id: msg.id.clone(),
            author: msg
                .nickname
                .clone()
                .unwrap_or_else(|| msg.author_name.clone()),
            snippet: snippet(&msg.content, at, len),
        });
        if out.len() == MAX_HITS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{find_at, hits, snippet, MAX_HITS, SNIPPET_CHARS};
    use crate::models::ChatMessage;

    fn message(i: usize, content: &str) -> Arc<ChatMessage> {
        Arc::new(ChatMessage {
            id: format!("m{i:04}"),
            channel_id: "c0".into(),
            author_id: "u0".into(),
            author_name: "user0".into(),
            author_avatar: None,
            nickname: None,
            content: content.into(),
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            attachments: vec![],
            embeds: vec![],
            reactions: vec![],
            reply: None,
            is_own: false,
        })
    }

    #[test]
    fn finds_a_case_insensitive_substring() {
        assert_eq!(find_at("Hello World", "world"), Some(6), "регистр не должен мешать");
        assert_eq!(find_at("Привет, мир", "мир"), Some(8), "кириллица тоже ищется");
        assert_eq!(find_at("abc", "zzz"), None, "несовпадающий запрос пуст");
    }

    #[test]
    fn empty_needle_matches_nothing() {
        assert_eq!(find_at("anything", ""), None, "пустая игла ничего не находит");
    }

    #[test]
    fn a_lowercase_expansion_keeps_the_index() {
        // `İ` lowercases to two chars; the char index must stay in step.
        assert_eq!(find_at("İx", "x"), Some(1), "индекс должен указывать на исходный символ");
    }

    #[test]
    fn snippet_keeps_context_and_marks_the_hit() {
        let text = "начало сообщения, а внутри needle и хвост";
        let at = text.chars().position(|c| c == 'n').unwrap();
        let s = snippet(text, at, 6);
        assert_eq!(s.hit, "needle", "совпадение выделяется отдельно");
        assert!(s.before.ends_with("а внутри "), "контекст перед совпадением: {:?}", s.before);
        assert!(s.after.starts_with(" и хвост"), "контекст после совпадения: {:?}", s.after);
        assert!(!s.before.starts_with('…'), "короткий текст не обрезается");
        assert!(!s.after.ends_with('…'), "короткий текст не обрезается");
    }

    #[test]
    fn snippet_clips_long_text_with_ellipses() {
        let text = "x".repeat(400) + "needle" + &"y".repeat(400);
        let at = 400;
        let s = snippet(text.as_str(), at, 6);
        assert_eq!(s.hit, "needle");
        assert!(s.before.starts_with('…'), "левое поле обрезано");
        assert!(s.after.ends_with('…'), "правое поле обрезано");
        let shown = s.before.chars().count() + s.hit.chars().count() + s.after.chars().count();
        assert!(shown <= SNIPPET_CHARS + 6, "строка не должна разрастаться: {shown}");
    }

    #[test]
    fn snippet_folds_newlines_into_spaces() {
        let s = snippet("первая\nвторая строка", 8, 7);
        assert!(!s.before.contains('\n') && !s.hit.contains('\n') && !s.after.contains('\n'),
            "переводы строк заменяются пробелами");
    }

    #[test]
    fn hits_are_newest_first() {
        let msgs = vec![
            message(0, "старое needle"),
            message(1, "мимо"),
            message(2, "новое needle"),
        ];
        let got = hits(&msgs, "needle");
        assert_eq!(got.len(), 2, "оба сообщения должны найтись");
        assert_eq!(got[0].message_id, "m0002", "сначала новое совпадение");
        assert_eq!(got[1].message_id, "m0000");
    }

    #[test]
    fn hits_skip_optimistic_echoes() {
        let mut echo = (*message(0, "needle")).clone();
        echo.id.clear();
        let msgs = vec![Arc::new(echo)];
        assert!(hits(&msgs, "needle").is_empty(), "эхо без id пропускается");
    }

    #[test]
    fn hits_trim_the_query_and_stop_at_the_cap() {
        let msgs: Vec<Arc<ChatMessage>> =
            (0..MAX_HITS + 5).map(|i| message(i, "needle")).collect();
        assert_eq!(hits(&msgs, "  needle  ").len(), MAX_HITS, "выдача ограничена");
        assert!(hits(&msgs, "   ").is_empty(), "пустой запрос ничего не ищет");
    }

    #[test]
    fn nickname_wins_over_account_name() {
        let mut msg = (*message(0, "needle")).clone();
        msg.nickname = Some("Ник".into());
        let got = hits(&[Arc::new(msg)], "needle");
        assert_eq!(got[0].author, "Ник", "в выдаче показывается ник");
    }
}
