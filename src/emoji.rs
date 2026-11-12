//! Curated emoji table and its search.
//!
//! Suckless on purpose: a static slice, a linear scan, no dependency, no build
//! script, no generated code. A full Unicode table would add ~40 KB of source
//! for names nobody types; a couple of hundred well-known glyphs with short
//! keyword lists cover ordinary chat.

/// One emoji: the glyph, a canonical name and extra search words.
pub(crate) struct Emoji {
    /// The glyph as it is sent to Discord.
    pub ch: &'static str,
    /// Canonical name, `snake_case`, English: also the primary search key.
    pub name: &'static str,
    /// Space-separated synonyms, English and Russian, lowercase.
    pub keys: &'static str,
}

/// Most matches the picker shows; typing more than this narrows the query.
pub(crate) const MAX_RESULTS: usize = 64;

/// The table, ordered by how often each glyph is used in chat.
pub(crate) static TABLE: &[Emoji] = &[
    // Faces
    Emoji { ch: "😂", name: "joy", keys: "lol laugh tears funny смех слёзы" },
    Emoji { ch: "😭", name: "sob", keys: "cry sad tears плач" },
    Emoji { ch: "🥰", name: "smiling_hearts", keys: "love adore любовь мило" },
    Emoji { ch: "😍", name: "heart_eyes", keys: "love crush влюблён" },
    Emoji { ch: "😊", name: "blush", keys: "smile shy улыбка" },
    Emoji { ch: "😁", name: "beaming", keys: "grin happy радость" },
    Emoji { ch: "😄", name: "smile", keys: "grin happy улыбка" },
    Emoji { ch: "😀", name: "grinning", keys: "smile happy grin улыбка" },
    Emoji { ch: "🙂", name: "slight_smile", keys: "smile улыбка" },
    Emoji { ch: "😉", name: "wink", keys: "flirt подмигивание" },
    Emoji { ch: "😎", name: "sunglasses", keys: "cool крутой" },
    Emoji { ch: "🤔", name: "thinking", keys: "hmm думаю" },
    Emoji { ch: "😅", name: "sweat_smile", keys: "lol nervous смех пот" },
    Emoji { ch: "🙃", name: "upside_down", keys: "silly перевёрнутый" },
    Emoji { ch: "😐", name: "neutral", keys: "meh нейтрально" },
    Emoji { ch: "😬", name: "grimace", keys: "yikes неловко" },
    Emoji { ch: "🫠", name: "melting", keys: "melting таю" },
    Emoji { ch: "🙄", name: "roll_eyes", keys: "annoyed глаза закатить" },
    Emoji { ch: "😳", name: "flushed", keys: "shy shock смущение" },
    Emoji { ch: "😱", name: "scream", keys: "fear panic крик" },
    Emoji { ch: "🤯", name: "mind_blown", keys: "wow шок" },
    Emoji { ch: "😴", name: "sleeping", keys: "sleep спать" },
    Emoji { ch: "😢", name: "cry", keys: "sad tear грусть" },
    Emoji { ch: "😡", name: "rage", keys: "angry mad злость" },
    Emoji { ch: "😤", name: "triumph", keys: "huff пыхтеть" },
    Emoji { ch: "😌", name: "relieved", keys: "calm облегчение" },
    Emoji { ch: "🥺", name: "pleading", keys: "please умоляю" },
    Emoji { ch: "🤗", name: "hug", keys: "обнять" },
    Emoji { ch: "🤫", name: "shush", keys: "quiet тихо" },
    Emoji { ch: "🥳", name: "party_face", keys: "celebrate праздник" },
    Emoji { ch: "😋", name: "yum", keys: "tasty вкусно" },
    Emoji { ch: "🥶", name: "cold_face", keys: "cold холод" },
    Emoji { ch: "🥵", name: "hot_face", keys: "hot жарко" },
    Emoji { ch: "🤒", name: "sick", keys: "ill болею" },
    Emoji { ch: "🤓", name: "nerd", keys: "glasses очки" },
    Emoji { ch: "🤖", name: "robot", keys: "bot робот" },
    Emoji { ch: "👻", name: "ghost", keys: "boo призрак" },
    Emoji { ch: "💀", name: "skull", keys: "dead череп" },
    Emoji { ch: "👽", name: "alien", keys: "ufo инопланетянин" },
    Emoji { ch: "🎃", name: "pumpkin", keys: "halloween тыква" },
    // Hands and people
    Emoji { ch: "👍", name: "thumbs_up", keys: "like ok yes палец вверх" },
    Emoji { ch: "👎", name: "thumbs_down", keys: "dislike no палец вниз" },
    Emoji { ch: "👌", name: "ok_hand", keys: "ok отлично" },
    Emoji { ch: "✌️", name: "victory", keys: "peace два" },
    Emoji { ch: "🤞", name: "crossed_fingers", keys: "luck удача" },
    Emoji { ch: "🤝", name: "handshake", keys: "deal рукопожатие" },
    Emoji { ch: "👏", name: "clap", keys: "applause хлопать" },
    Emoji { ch: "🙌", name: "raised_hands", keys: "hooray ура" },
    Emoji { ch: "🙏", name: "pray", keys: "thanks please спасибо молиться" },
    Emoji { ch: "👋", name: "wave_hand", keys: "hi hello привет" },
    Emoji { ch: "💪", name: "muscle", keys: "strong сила" },
    Emoji { ch: "🫡", name: "salute", keys: "respect честь" },
    Emoji { ch: "🤙", name: "call_me", keys: "call звони" },
    Emoji { ch: "👀", name: "eyes", keys: "look смотрю" },
    Emoji { ch: "🧠", name: "brain", keys: "mind мозг" },
    // Hearts and symbols
    Emoji { ch: "❤️", name: "heart", keys: "love red сердце" },
    Emoji { ch: "🧡", name: "orange_heart", keys: "love сердце" },
    Emoji { ch: "💛", name: "yellow_heart", keys: "love сердце" },
    Emoji { ch: "💚", name: "green_heart", keys: "love сердце" },
    Emoji { ch: "💙", name: "blue_heart", keys: "love сердце" },
    Emoji { ch: "💜", name: "purple_heart", keys: "love сердце" },
    Emoji { ch: "🖤", name: "black_heart", keys: "love сердце" },
    Emoji { ch: "🤍", name: "white_heart", keys: "love сердце" },
    Emoji { ch: "💔", name: "broken_heart", keys: "sad разбитое" },
    Emoji { ch: "💖", name: "sparkling_heart", keys: "love блеск" },
    Emoji { ch: "🔥", name: "fire", keys: "hot огонь" },
    Emoji { ch: "✨", name: "sparkles", keys: "shiny блеск" },
    Emoji { ch: "💯", name: "hundred", keys: "100 сто" },
    Emoji { ch: "✅", name: "check", keys: "yes done галочка" },
    Emoji { ch: "❌", name: "x", keys: "no cross крестик" },
    Emoji { ch: "❗", name: "exclamation", keys: "important важно" },
    Emoji { ch: "❓", name: "question", keys: "вопрос" },
    Emoji { ch: "⚠️", name: "warning", keys: "careful внимание" },
    Emoji { ch: "⛔", name: "no_entry", keys: "stop стоп" },
    Emoji { ch: "🚫", name: "prohibited", keys: "forbidden запрещено" },
    Emoji { ch: "🏳️", name: "white_flag", keys: "surrender белый флаг" },
    // Celebration and objects
    Emoji { ch: "🎉", name: "tada", keys: "party celebrate праздник" },
    Emoji { ch: "🎊", name: "confetti", keys: "ура" },
    Emoji { ch: "🎁", name: "gift", keys: "present подарок" },
    Emoji { ch: "🏆", name: "trophy", keys: "winner кубок" },
    Emoji { ch: "🥇", name: "first_place", keys: "gold первое" },
    Emoji { ch: "🎯", name: "dart", keys: "target цель" },
    Emoji { ch: "🚀", name: "rocket", keys: "launch ракета" },
    Emoji { ch: "💡", name: "bulb", keys: "idea идея" },
    Emoji { ch: "🔔", name: "bell", keys: "notification звонок" },
    Emoji { ch: "🔕", name: "no_bell", keys: "mute без звука" },
    Emoji { ch: "📌", name: "pin", keys: "закрепить" },
    Emoji { ch: "📎", name: "paperclip", keys: "attach скрепка" },
    Emoji { ch: "📝", name: "memo", keys: "note заметка" },
    Emoji { ch: "📖", name: "book", keys: "read книга" },
    Emoji { ch: "🖥️", name: "desktop", keys: "computer компьютер" },
    Emoji { ch: "💻", name: "laptop", keys: "ноутбук" },
    Emoji { ch: "📱", name: "phone", keys: "телефон" },
    Emoji { ch: "⌨️", name: "keyboard", keys: "клавиатура" },
    Emoji { ch: "🖱️", name: "mouse", keys: "мышь" },
    Emoji { ch: "💾", name: "floppy", keys: "save сохранение" },
    Emoji { ch: "🗑️", name: "wastebasket", keys: "trash delete корзина" },
    Emoji { ch: "⚙️", name: "gear", keys: "settings настройки" },
    Emoji { ch: "🔍", name: "magnify", keys: "search лупа поиск" },
    Emoji { ch: "🕐", name: "clock", keys: "time время" },
    Emoji { ch: "⏰", name: "alarm_clock", keys: "alarm будильник" },
    Emoji { ch: "⌛", name: "hourglass", keys: "wait песочные часы" },
    Emoji { ch: "📅", name: "calendar", keys: "date календарь" },
    Emoji { ch: "🔒", name: "lock", keys: "замок" },
    Emoji { ch: "🔑", name: "key", keys: "ключ" },
    Emoji { ch: "✂️", name: "scissors", keys: "cut ножницы" },
    Emoji { ch: "🛠️", name: "tools", keys: "fix инструменты" },
    Emoji { ch: "🐛", name: "bug", keys: "баг" },
    Emoji { ch: "🧹", name: "broom", keys: "clean уборка" },
    Emoji { ch: "🧲", name: "magnet", keys: "магнит" },
    Emoji { ch: "🧪", name: "test_tube", keys: "test тест" },
    Emoji { ch: "🪄", name: "magic_wand", keys: "magic магия" },
    Emoji { ch: "🧊", name: "ice", keys: "cold лёд" },
    // Weather and nature
    Emoji { ch: "☀️", name: "sun", keys: "weather солнце" },
    Emoji { ch: "🌙", name: "moon", keys: "night луна" },
    Emoji { ch: "⛅", name: "partly_sunny", keys: "cloud облако" },
    Emoji { ch: "🌧️", name: "rain", keys: "дождь" },
    Emoji { ch: "❄️", name: "snowflake", keys: "snow снег" },
    Emoji { ch: "⚡", name: "zap", keys: "lightning молния" },
    Emoji { ch: "🌈", name: "rainbow", keys: "радуга" },
    Emoji { ch: "🌊", name: "ocean_wave", keys: "sea волна" },
    Emoji { ch: "🌲", name: "evergreen", keys: "tree дерево" },
    Emoji { ch: "🌸", name: "blossom", keys: "flower цветок" },
    Emoji { ch: "🌻", name: "sunflower", keys: "flower подсолнух" },
    Emoji { ch: "🍀", name: "four_leaf_clover", keys: "luck клевер" },
    Emoji { ch: "🍁", name: "maple_leaf", keys: "autumn осень" },
    // Animals
    Emoji { ch: "🐶", name: "dog", keys: "puppy собака" },
    Emoji { ch: "🐱", name: "cat", keys: "kitten кот" },
    Emoji { ch: "🦊", name: "fox", keys: "лиса" },
    Emoji { ch: "🐻", name: "bear", keys: "медведь" },
    Emoji { ch: "🐼", name: "panda", keys: "панда" },
    Emoji { ch: "🐸", name: "frog", keys: "жаба" },
    Emoji { ch: "🐵", name: "monkey", keys: "обезьяна" },
    Emoji { ch: "🦄", name: "unicorn", keys: "единорог" },
    Emoji { ch: "🐝", name: "bee", keys: "пчела" },
    Emoji { ch: "🦋", name: "butterfly", keys: "бабочка" },
    Emoji { ch: "🐙", name: "octopus", keys: "осьминог" },
    Emoji { ch: "🐢", name: "turtle", keys: "черепаха" },
    // Food and drink
    Emoji { ch: "🍕", name: "pizza", keys: "пицца" },
    Emoji { ch: "🍔", name: "burger", keys: "бургер" },
    Emoji { ch: "🍟", name: "fries", keys: "картошка" },
    Emoji { ch: "🍜", name: "ramen", keys: "noodles лапша" },
    Emoji { ch: "🍣", name: "sushi", keys: "суши" },
    Emoji { ch: "🌮", name: "taco", keys: "тако" },
    Emoji { ch: "🧀", name: "cheese", keys: "сыр" },
    Emoji { ch: "🥑", name: "avocado", keys: "авокадо" },
    Emoji { ch: "🍰", name: "cake", keys: "торт" },
    Emoji { ch: "🍫", name: "chocolate", keys: "шоколад" },
    Emoji { ch: "🍎", name: "apple", keys: "яблоко" },
    Emoji { ch: "🍌", name: "banana", keys: "банан" },
    Emoji { ch: "🍓", name: "strawberry", keys: "клубника" },
    Emoji { ch: "🍺", name: "beer", keys: "пиво" },
    Emoji { ch: "☕", name: "coffee", keys: "кофе" },
    Emoji { ch: "🍵", name: "tea", keys: "чай" },
    Emoji { ch: "🥤", name: "cup", keys: "drink напиток" },
    // Places and travel
    Emoji { ch: "🏠", name: "house", keys: "home дом" },
    Emoji { ch: "🏢", name: "office", keys: "офис" },
    Emoji { ch: "🚗", name: "car", keys: "машина" },
    Emoji { ch: "🚲", name: "bike", keys: "велосипед" },
    Emoji { ch: "✈️", name: "airplane", keys: "самолёт" },
    Emoji { ch: "🚂", name: "train", keys: "поезд" },
    Emoji { ch: "🚢", name: "ship", keys: "корабль" },
    // Media
    Emoji { ch: "🎮", name: "video_game", keys: "game игра" },
    Emoji { ch: "🎧", name: "headphones", keys: "music наушники" },
    Emoji { ch: "🎵", name: "note", keys: "music нота" },
    Emoji { ch: "🎸", name: "guitar", keys: "гитара" },
    Emoji { ch: "🎬", name: "clapper", keys: "movie кино" },
    Emoji { ch: "📷", name: "camera", keys: "photo фото" },
    Emoji { ch: "🎨", name: "art", keys: "palette рисование" },
    Emoji { ch: "💤", name: "zzz", keys: "sleep сон" },
    Emoji { ch: "💬", name: "speech", keys: "chat сообщение" },
];

/// Score for one query term against one emoji; zero means no match.
///
/// Exact glyph and exact name beat a name prefix, which beats a word prefix in
/// the synonyms. Prefixes, not substrings: "art" must not match "party".
fn term_score(emoji: &Emoji, term: &str) -> u8 {
    if emoji.ch == term {
        return 4;
    }
    if emoji.name == term {
        return 3;
    }
    if emoji.name.starts_with(term) {
        return 2;
    }
    let words = emoji.name.split('_').chain(emoji.keys.split(' '));
    u8::from(words.into_iter().any(|word| word.starts_with(term)))
}

/// Emoji matching `query`, best first, at most `limit`.
///
/// Every whitespace-separated term must match somewhere, so `red heart` cannot
/// drag in every heart. An empty query returns the head of the table, which is
/// ordered by how often the glyphs are used.
pub(crate) fn search(query: &str, limit: usize) -> Vec<&'static Emoji> {
    let query = query.trim().to_lowercase();
    let terms: Vec<&str> = query.split_whitespace().collect();
    if terms.is_empty() {
        return TABLE.iter().take(limit).collect();
    }
    let mut hits: Vec<(u8, usize, &'static Emoji)> = Vec::new();
    for (index, emoji) in TABLE.iter().enumerate() {
        let mut score = 0u8;
        let mut matched = true;
        for term in &terms {
            match term_score(emoji, term) {
                0 => {
                    matched = false;
                    break;
                }
                points => score = score.saturating_add(points),
            }
        }
        if matched {
            hits.push((score, index, emoji));
        }
    }
    // Stable by construction: score first, table order breaks ties, so the
    // common glyphs stay in front instead of the sort shuffling equal hits.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.into_iter().take(limit).map(|(_, _, emoji)| emoji).collect()
}

#[cfg(test)]
mod tests {
    use super::{search, term_score, Emoji, MAX_RESULTS, TABLE};

    #[test]
    fn every_glyph_appears_once() {
        let mut seen = std::collections::HashSet::new();
        for emoji in TABLE {
            assert!(seen.insert(emoji.ch), "глиф {} повторяется в таблице", emoji.ch);
        }
    }

    #[test]
    fn names_are_unique_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for emoji in TABLE {
            assert!(seen.insert(emoji.name), "имя {} повторяется", emoji.name);
            assert_eq!(emoji.name, emoji.name.to_lowercase(), "имя должно быть в нижнем регистре");
            assert!(emoji.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "имя {} должно быть snake_case", emoji.name);
            assert!(!emoji.ch.is_empty(), "пустой глиф у {}", emoji.name);
        }
    }

    #[test]
    fn keywords_are_lowercase_words() {
        for emoji in TABLE {
            assert_eq!(emoji.keys, emoji.keys.to_lowercase(),
                "ключевые слова {} должны быть в нижнем регистре", emoji.name);
            for word in emoji.keys.split(' ') {
                assert!(!word.is_empty(), "пустое ключевое слово у {}", emoji.name);
            }
        }
    }

    #[test]
    fn search_matches_a_name_prefix() {
        let hits = search("thu", 8);
        assert_eq!(hits.first().map(|e| e.name), Some("thumbs_up"),
            "«thu» должен вести к большому пальцу");
    }

    #[test]
    fn search_matches_russian_keywords() {
        let heart = search("сердце", 8);
        assert!(heart.iter().any(|e| e.name == "heart"), "«сердце» должно находить красное сердце");
        let party = search("праздник", 8);
        assert!(party.iter().any(|e| e.name == "tada"), "«праздник» должно находить хлопушку");
    }

    #[test]
    fn every_term_must_match() {
        let hits = search("red heart", 8);
        assert_eq!(hits.len(), 1, "должно остаться одно красное сердце");
        assert_eq!(hits[0].name, "heart");
    }

    #[test]
    fn the_glyph_itself_finds_its_entry() {
        let hits = search("🔥", 4);
        assert_eq!(hits.first().map(|e| e.name), Some("fire"),
            "вставленный эмодзи должен находить себя");
    }

    #[test]
    fn empty_query_returns_the_head_of_the_table() {
        let hits = search("   ", 5);
        assert_eq!(hits.len(), 5, "пустой запрос отдаёт начало таблицы");
        assert_eq!(hits[0].name, TABLE[0].name);
        assert_eq!(hits[4].name, TABLE[4].name);
    }

    #[test]
    fn limit_caps_the_result() {
        assert_eq!(search("", 3).len(), 3);
        assert!(search("heart", MAX_RESULTS).len() <= MAX_RESULTS);
    }

    #[test]
    fn unknown_query_returns_nothing() {
        assert!(search("zzzz-not-an-emoji", 8).is_empty(), "неизвестный запрос пуст");
    }

    #[test]
    fn prefix_beats_synonym() {
        let named = Emoji { ch: "x", name: "heart", keys: "love" };
        let keyed = Emoji { ch: "y", name: "orange_heart", keys: "love" };
        assert!(term_score(&named, "hea") > term_score(&keyed, "hea"),
            "совпадение с именем должно весить больше, чем с синонимом");
    }
}
