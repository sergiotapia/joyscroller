//! What the user typed into the list's search box, read as a query: words,
//! quoted text, exclusions, alternatives and patterns. The operators are
//! magnetissimo's (`lib/magnetissimo/torrents/query.ex`), so they work the
//! same in both apps; `db/search.rs` turns a `Query` into SQL against the archive's
//! two FTS5 indexes and says how each part is matched.
//!
//! * `cat piano` finds both words, each anywhere, in any order.
//! * `"exact text"` is exactly what's between the quotes, punctuation and
//!   spaces included, ignoring only case and accents. So `"spider man"`
//!   doesn't find "Spider-Man"; unquoted, it does.
//! * `-cat` excludes the whole word, so it leaves "catalog" alone. `-"exact
//!   text"` excludes that text and `-*cat` a pattern.
//! * `cute|funny` is either. A bar binds tighter than the space between
//!   words, so `dog cute|funny` is dog, and cute or funny.
//! * `prefix*` and `*anywhere` are patterns, where a star is any run of
//!   characters. One the word ends with makes it the start of a word; one it
//!   starts with lets it begin anywhere.
//!
//! Every operator has to be typed unambiguously, because captions are full
//! of the same characters. A dash is an exclusion only at the start of a word
//! ("spider-man" is a word), and only the ASCII hyphen: a phone turns `--`
//! into an em dash. Curly quotes are quotes, for the same phone. A bar next to
//! an exclusion is ignored, since "this or not that" means nothing useful.
//! Anything with no letters or digits in it is dropped.

use std::sync::LazyLock;

use regex::Regex;

/// One thing to match: plain words as typed, the text between quotes, or a
/// pattern with its stars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Atom {
    Words(String),
    Phrase(String),
    Wildcard(String),
}

impl Atom {
    /// Two atoms with the same key match the same videos.
    fn key(&self) -> (u8, String) {
        match self {
            Atom::Words(text) => (0, normalize(text)),
            Atom::Phrase(text) => (1, normalize(text)),
            Atom::Wildcard(text) => (2, normalize(text)),
        }
    }
}

/// A parsed search. Every clause must match, and each is a list of
/// alternatives of which one must. No atom in `excludes` may match.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub clauses: Vec<Vec<Atom>>,
    pub excludes: Vec<Atom>,
    /// Whether the last thing in the input is a plain word, which may still be
    /// being typed, so its last token is searched as a prefix.
    pub typing: bool,
}

impl Query {
    /// Whether there's nothing to search for.
    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty() && self.excludes.is_empty()
    }
}

/// Past this, a query isn't something a person typed, and every atom past a
/// plain word costs the database a subquery. The rest of the input is ignored.
pub const MAX_ATOMS: usize = 32;

/// A pattern below, compiled once.
#[expect(
    clippy::expect_used,
    reason = "the patterns are constants, and the tests compile each one"
)]
fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid pattern")
}

/// A phrase, which may follow dashes and may never be closed; a bar; or a run
/// of anything else up to whitespace, a quote or a bar. The quotes are the
/// straight one and the curly ones phones type.
static LEXER: LazyLock<Regex> =
    LazyLock::new(|| compile(r#"(-*)["“”„]([^"“”„]*)["“”„]?|(\|)|([^\s"“”„|]+)"#));

/// What `unicode61` splits text on: anything that isn't a letter, digit or
/// mark.
static SEPARATORS: LazyLock<Regex> = LazyLock::new(|| compile(r"[^\p{L}\p{N}\p{M}]+"));

/// Scripts written without spaces between words.
static UNSPACED: LazyLock<Regex> =
    LazyLock::new(|| compile(r"[\p{Han}\p{Hiragana}\p{Katakana}\p{Hangul}\p{Thai}]"));

enum Lexeme {
    Positive(Atom),
    Negative(Atom),
    Or,
}

/// Reads the operators in `text`.
pub fn parse(text: &str) -> Query {
    let mut lexemes = Vec::new();
    let mut atoms = 0;
    for caps in LEXER.captures_iter(text) {
        let lexeme = if let Some(phrase) = caps.get(2) {
            let dashes = caps.get(1).map_or("", |m| m.as_str());
            phrase_lexeme(dashes, phrase.as_str())
        } else if caps.get(3).is_some() {
            Some(Lexeme::Or)
        } else {
            caps.get(4).and_then(|chunk| chunk_lexeme(chunk.as_str()))
        };
        match lexeme {
            None => {}
            Some(Lexeme::Or) => lexemes.push(Lexeme::Or),
            Some(_) if atoms >= MAX_ATOMS => break,
            Some(atom) => {
                atoms += 1;
                lexemes.push(atom);
            }
        }
    }
    build(lexemes)
}

/// A dash, quote or star with nothing searchable around it is no lexeme.
fn phrase_lexeme(dashes: &str, phrase: &str) -> Option<Lexeme> {
    let phrase = phrase.trim();
    searchable(phrase).then(|| with_polarity(dashes, Atom::Phrase(phrase.into())))
}

fn chunk_lexeme(chunk: &str) -> Option<Lexeme> {
    let body = chunk.trim_start_matches('-');
    let dashes = &chunk[..chunk.len() - body.len()];
    let atom = if body.contains('*') {
        Atom::Wildcard(body.into())
    } else {
        Atom::Words(body.into())
    };
    searchable(body).then(|| with_polarity(dashes, atom))
}

fn with_polarity(dashes: &str, atom: Atom) -> Lexeme {
    if dashes.is_empty() {
        Lexeme::Positive(atom)
    } else {
        Lexeme::Negative(atom)
    }
}

fn searchable(text: &str) -> bool {
    !tokens(text).is_empty()
}

/// What came last while building: a positive atom, which a bar can join to
/// the next; a bar right after one; or anything else.
#[derive(PartialEq)]
enum After {
    Atom,
    Or,
    Nothing,
}

fn build(lexemes: Vec<Lexeme>) -> Query {
    let mut clauses: Vec<Vec<Atom>> = Vec::new();
    let mut excludes = Vec::new();
    let mut after = After::Nothing;
    // The last positive atom, or None after an exclusion.
    let mut last = None;
    for lexeme in lexemes {
        match lexeme {
            Lexeme::Or => {
                if after == After::Atom {
                    after = After::Or;
                }
            }
            Lexeme::Negative(atom) => {
                excludes.push(atom);
                after = After::Nothing;
                last = None;
            }
            Lexeme::Positive(atom) => {
                last = Some(atom.clone());
                match clauses.last_mut() {
                    Some(clause) if after == After::Or => clause.push(atom),
                    _ => clauses.push(vec![atom]),
                }
                after = After::Atom;
            }
        }
    }
    let clauses: Vec<Vec<Atom>> = clauses.into_iter().map(uniq).collect();
    let typing = match (&last, clauses.last()) {
        (Some(atom @ Atom::Words(_)), Some(clause)) => clause.as_slice() == [atom.clone()],
        _ => false,
    };
    Query {
        clauses,
        excludes: uniq(excludes),
        typing,
    }
}

/// The atoms, each kept once, the first time it appears.
fn uniq(atoms: Vec<Atom>) -> Vec<Atom> {
    let mut seen = Vec::new();
    let mut kept = Vec::new();
    for atom in atoms {
        let key = atom.key();
        if !seen.contains(&key) {
            seen.push(key);
            kept.push(atom);
        }
    }
    kept
}

/// Lowercase, single spaces, trimmed: "Cat  Piano" and "cat piano" are the
/// same text.
pub fn normalize(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Which index answers a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Index {
    /// Three characters or more: a substring, in the trigram index.
    Trigram,
    /// Shorter: a whole word, in the `unicode61` index.
    Word,
    /// Shorter, in a script written without spaces, which `unicode61` reads
    /// as one long word: a plain substring test.
    Substring,
}

/// Splits text into the tokens a search asks for, the way `unicode61` splits
/// what it indexes, each tagged with the index that answers it.
pub fn tokens(text: &str) -> Vec<(Index, &str)> {
    SEPARATORS
        .split(text)
        .filter(|token| !token.is_empty())
        .map(|token| {
            // Characters, not bytes or graphemes: a trigram is three code points.
            let index = if token.chars().count() >= 3 {
                Index::Trigram
            } else if unspaced(token) {
                Index::Substring
            } else {
                Index::Word
            };
            (index, token)
        })
        .collect()
}

/// Whether `word` is in a script written without spaces (CJK, Thai).
pub fn unspaced(word: &str) -> bool {
    UNSPACED.is_match(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str) -> Atom {
        Atom::Words(text.into())
    }
    fn p(text: &str) -> Atom {
        Atom::Phrase(text.into())
    }
    fn s(text: &str) -> Atom {
        Atom::Wildcard(text.into())
    }
    fn clauses(text: &str) -> Vec<Vec<Atom>> {
        parse(text).clauses
    }
    fn excludes(text: &str) -> Vec<Atom> {
        parse(text).excludes
    }

    #[test]
    fn each_plain_word_is_its_own_clause_as_typed() {
        assert_eq!(
            clauses("Cat piano solo"),
            [vec![w("Cat")], vec![w("piano")], vec![w("solo")]]
        );
        assert_eq!(parse("  cat \t  piano "), parse("cat piano"));
    }

    #[test]
    fn punctuation_inside_a_word_is_part_of_it() {
        assert_eq!(
            clauses("spider-man x265- ubuntu:24.04 S.H.I.E.L.D."),
            [
                vec![w("spider-man")],
                vec![w("x265-")],
                vec![w("ubuntu:24.04")],
                vec![w("S.H.I.E.L.D.")]
            ]
        );
    }

    #[test]
    fn fts5_operators_are_just_words() {
        assert_eq!(
            clauses("NOT AND OR NEAR"),
            [
                vec![w("NOT")],
                vec![w("AND")],
                vec![w("OR")],
                vec![w("NEAR")]
            ]
        );
    }

    #[test]
    fn a_chunk_with_nothing_searchable_is_dropped() {
        assert_eq!(clauses("cat : ... ( ) !"), [vec![w("cat")]]);
    }

    #[test]
    fn nothing_to_search_for_is_empty() {
        for text in [
            "", "   ", "\t\n", ":", "...", "-", "--", "- -", "|", "||", " | | ", "*", "**", "-*",
            r#""""#, r#""   ""#, r#"-"""#, r#""--""#, r#"""#, "“”",
        ] {
            assert!(parse(text).is_empty(), "expected {text:?} to be empty");
        }
        assert!(!parse("cat").is_empty());
        assert!(!parse("-cat").is_empty());
    }

    #[test]
    fn quotes_make_a_phrase() {
        assert_eq!(
            clauses(r#"cat "piano solo""#),
            [vec![w("cat")], vec![p("piano solo")]]
        );
        // An unclosed quote runs to the end of the input.
        assert_eq!(
            clauses(r#"cat "piano solo"#),
            [vec![w("cat")], vec![p("piano solo")]]
        );
        // Curly quotes from a phone keyboard are quotes too.
        assert_eq!(
            clauses("“piano solo” cat"),
            [vec![p("piano solo")], vec![w("cat")]]
        );
        assert_eq!(clauses("„piano solo“"), [vec![p("piano solo")]]);
    }

    #[test]
    fn a_quote_ends_the_word_before_it() {
        assert_eq!(
            clauses(r#"cat"piano solo"2024"#),
            [vec![w("cat")], vec![p("piano solo")], vec![w("2024")]]
        );
    }

    #[test]
    fn operators_inside_a_phrase_are_text() {
        assert_eq!(clauses(r#""-cat | dog* x""#), [vec![p("-cat | dog* x")]]);
    }

    #[test]
    fn empty_phrases_are_dropped_and_phrases_trimmed() {
        assert_eq!(clauses(r#"cat "" "  " "--""#), [vec![w("cat")]]);
        assert_eq!(clauses(r#""  piano solo  ""#), [vec![p("piano solo")]]);
    }

    #[test]
    fn a_leading_dash_excludes_a_word() {
        assert_eq!(clauses("cat -dog"), [vec![w("cat")]]);
        assert_eq!(excludes("cat -dog"), [w("dog")]);
        // Several dashes are one.
        assert_eq!(excludes("cat --dog"), [w("dog")]);
    }

    #[test]
    fn a_dash_on_its_own_is_ignored() {
        assert_eq!(clauses("cat - dog"), [vec![w("cat")], vec![w("dog")]]);
        assert_eq!(excludes("cat - dog"), Vec::<Atom>::new());
    }

    #[test]
    fn only_an_ascii_hyphen_excludes() {
        assert_eq!(excludes("cat –dog —dog"), Vec::<Atom>::new());
        assert_eq!(
            clauses("cat –dog —dog"),
            [vec![w("cat")], vec![w("–dog")], vec![w("—dog")]]
        );
    }

    #[test]
    fn a_dash_excludes_a_phrase_or_a_pattern() {
        assert_eq!(excludes(r#"cat -"piano solo""#), [p("piano solo")]);
        assert_eq!(excludes("cat -dog*"), [s("dog*")]);
        // An exclusion keeps its inner punctuation.
        assert_eq!(excludes("cat -spider-man"), [w("spider-man")]);
    }

    #[test]
    fn only_exclusions_is_still_a_query() {
        let query = parse("-cat -dog");
        assert_eq!(query.clauses, Vec::<Vec<Atom>>::new());
        assert_eq!(query.excludes, [w("cat"), w("dog")]);
        assert!(!query.is_empty());
    }

    #[test]
    fn the_same_exclusion_twice_is_kept_once() {
        assert_eq!(excludes("-dog cat -DOG"), [w("dog")]);
    }

    #[test]
    fn a_bar_joins_words_into_one_clause() {
        let expected = [vec![w("dog")], vec![w("cute"), w("funny")]];
        for text in [
            "dog cute|funny",
            "dog cute | funny",
            "dog cute |funny",
            "dog cute| funny",
            "dog cute || funny",
            "dog cute | | funny",
        ] {
            assert_eq!(clauses(text), expected, "for {text:?}");
        }
    }

    #[test]
    fn a_bar_binds_tighter_than_a_space() {
        assert_eq!(
            clauses("dog cute|funny video"),
            [
                vec![w("dog")],
                vec![w("cute"), w("funny")],
                vec![w("video")]
            ]
        );
        assert_eq!(clauses("a1|b2|c3"), [vec![w("a1"), w("b2"), w("c3")]]);
    }

    #[test]
    fn phrases_and_patterns_can_be_alternatives() {
        assert_eq!(
            clauses(r#""part two"|"part 2"|s01e*"#),
            [vec![p("part two"), p("part 2"), s("s01e*")]]
        );
    }

    #[test]
    fn a_bar_with_nothing_on_one_side_is_ignored() {
        assert_eq!(clauses("| cat |"), [vec![w("cat")]]);
        assert_eq!(clauses("cat | * | 4k"), [vec![w("cat"), w("4k")]]);
    }

    #[test]
    fn an_exclusion_never_joins_an_alternative() {
        for text in ["cat | -dog", "-dog | cat", "cat|-dog"] {
            assert_eq!(clauses(text), [vec![w("cat")]], "for {text:?}");
            assert_eq!(excludes(text), [w("dog")], "for {text:?}");
        }
        assert_eq!(clauses("cat | -dog | 4k"), [vec![w("cat")], vec![w("4k")]]);
    }

    #[test]
    fn the_same_alternative_twice_is_kept_once() {
        assert_eq!(clauses("Cat|cat"), [vec![w("Cat")]]);
        assert_eq!(clauses("4k|2160p|4K"), [vec![w("4k"), w("2160p")]]);
    }

    #[test]
    fn bars_in_unspaced_scripts() {
        assert_eq!(clauses("進撃|拉拉熊"), [vec![w("進撃"), w("拉拉熊")]]);
        // A full-width bar is punctuation, not an operator.
        assert_eq!(clauses("進撃｜拉拉熊"), [vec![w("進撃｜拉拉熊")]]);
    }

    #[test]
    fn a_star_anywhere_makes_a_pattern() {
        assert_eq!(
            clauses("cat s05e1* *265 s0*e14"),
            [
                vec![w("cat")],
                vec![s("s05e1*")],
                vec![s("*265")],
                vec![s("s0*e14")]
            ]
        );
        assert_eq!(clauses("cat * ** ***"), [vec![w("cat")]]);
    }

    #[test]
    fn typing_while_the_last_thing_is_a_plain_word() {
        assert!(parse("cat 4").typing);
        assert!(parse("cat |").typing);
        assert!(!parse(r#"cat "4""#).typing);
        assert!(!parse("cat -4").typing);
        assert!(!parse("cat 2160p|4").typing);
        assert!(!parse("cat s01e*").typing);
        assert!(!parse("").typing);
    }

    #[test]
    fn a_query_too_long_to_be_typed_is_cut() {
        let words: Vec<String> = (1..=500).map(|n| format!("w{n}")).collect();
        let query = parse(&format!("{} -cat", words.join(" | ")));
        assert_eq!(query.clauses.len(), 1);
        assert_eq!(query.clauses[0].len(), MAX_ATOMS);
        assert_eq!(query.excludes, Vec::<Atom>::new());
    }

    #[test]
    fn tokens_split_like_unicode61_and_pick_an_index() {
        assert_eq!(
            tokens("Spider.Man 4 é 進撃 拉拉熊 de"),
            [
                (Index::Trigram, "Spider"),
                (Index::Trigram, "Man"),
                (Index::Word, "4"),
                (Index::Word, "é"),
                (Index::Substring, "進撃"),
                (Index::Trigram, "拉拉熊"),
                (Index::Word, "de"),
            ]
        );
        // Marks stay in their word: a Devanagari vowel sign isn't a separator.
        assert_eq!(tokens("नमस्ते"), [(Index::Trigram, "नमस्ते")]);
        assert_eq!(tokens("🔥 -- ... |"), Vec::<(Index, &str)>::new());
    }

    #[test]
    fn normalizes_case_and_spaces() {
        assert_eq!(normalize("  Cat \t PIANO "), "cat piano");
    }
}
