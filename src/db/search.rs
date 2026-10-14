//! A query (see `query.rs`) becomes common table expressions ending in
//! `matches(id, score)`: the rowids of the matching videos, and a score where
//! lower is better. Each token goes to the index that can answer it:
//!
//! * `search_trigrams` uses the trigram tokenizer, so a token of three characters
//!   or more is a substring match in any script. That's what makes a fragment
//!   of a Japanese caption, which has no spaces to split on, findable.
//! * `search_words` uses unicode61 and holds whole words. A trigram index can't
//!   answer a token shorter than three characters, so "4" in "toy story 4"
//!   goes here.
//! * A short token in a script written without spaces (CJK, Thai) is neither a
//!   word nor a trigram, so it's a plain substring test on the title.
//!
//! The operators are matched in two steps. The driver searches the words the
//! query needs outright, including the longer words of quoted text and of a
//! pattern: that finds a superset of the answer and ranks it with bm25. Each
//! operator then narrows it with a condition of its own, chosen so that what
//! it means survives how captions are written:
//!
//! * Quoted text is those exact characters, punctuation and spaces included,
//!   which the trigram index answers as a substring, ignoring case and accents.
//! * An unquoted exclusion is a whole word, or for `-spider-man` the words it
//!   spells side by side across any separator, in `search_words`. It's never a
//!   substring.
//! * A pattern that ends a word with a star is a prefix query on
//!   `search_words`. Any other is a GLOB on the lowercased text, which, unlike
//!   the indexes, doesn't fold accents or non-ASCII case.
//!
//! Every token is quoted into MATCH, so FTS5's own query syntax (`NOT`,
//! `col:`, parentheses) is just text. Nothing the user types reaches the SQL
//! itself: it's all parameters.

use crate::query::{self, Atom, Index, Query};

/// SQL and the values for its `?`s, in order.
pub(super) type Sql = (String, Vec<String>);

/// bm25's weights for the title, author and description: a word in the title
/// beats the same word in a channel name, which beats it buried in a
/// description.
const RANK: &str = "10.0, 5.0, 1.0";

/// `search_text`'s columns, which both indexes have too.
const TEXT_COLUMNS: [&str; 3] = ["title", "author", "description"];

/// The CTEs ending in `matches(id, score)`, or None when there's nothing to
/// search for. Without `rank` every score is 0.0 and bm25 never runs, for a
/// count or an order by a column.
pub(super) fn search_matches(query: &Query, rank: bool) -> Option<Sql> {
    if query.is_empty() {
        return None;
    }
    let tokens = driver_tokens(query);
    let strict = strict(query);
    let driver = if tokens.is_empty() {
        alternatives(query)
    } else {
        Some(text_matches(&tokens, query.typing, rank))
    };
    Some(match driver {
        Some(driver) => narrowed(driver, strict),
        None => every_video(strict),
    })
}

/// Every video is a candidate, so the operators are tested as `search_text`
/// is read, rather than on a copy of all its ids joined back to it.
fn every_video(strict: Option<Sql>) -> Sql {
    let (condition, params) = strict.unwrap_or_else(|| ("TRUE".into(), Vec::new()));
    (
        format!(
            "matches(id, score) AS (SELECT t.id, 0.0 AS score FROM search_text t WHERE {condition})"
        ),
        params,
    )
}

fn narrowed((driver, mut params): Sql, strict: Option<Sql>) -> Sql {
    // The operators read the text only for a short substring or a pattern.
    // Every other one tests the id against an index, so the candidates, which
    // hold it, stand in for `search_text t`.
    let (from, u, condition) = match strict {
        None => ("candidates AS u", "u", String::new()),
        Some((sql, strict_params)) => {
            params.extend(strict_params);
            if TEXT_COLUMNS
                .iter()
                .any(|column| sql.contains(&format!("t.{column}")))
            {
                (
                    "candidates AS u JOIN search_text t ON t.id = u.id",
                    "u",
                    format!("WHERE {sql}"),
                )
            } else {
                ("candidates AS t", "t", format!("WHERE {sql}"))
            }
        }
    };
    // Materialized before the operators join it: flattened into that join,
    // bm25 loses the FTS5 query it scores and refuses to run.
    (
        format!(
            "candidates(id, score) AS MATERIALIZED ({driver}), \
             matches(id, score) AS (SELECT {u}.id, {u}.score FROM {from} {condition})"
        ),
        params,
    )
}

/// The tokens the query needs outright: every plain word, and the trigrams of
/// quoted text and patterns. A trigram is a substring, which takes in any
/// quoted text or pattern it's part of; a whole word wouldn't.
fn driver_tokens(query: &Query) -> Vec<(Index, &str)> {
    query
        .clauses
        .iter()
        .filter_map(|clause| match clause.as_slice() {
            [atom] => Some(atom_driver_tokens(atom)),
            _ => None,
        })
        .flatten()
        .collect()
}

fn atom_driver_tokens(atom: &Atom) -> Vec<(Index, &str)> {
    match atom {
        Atom::Words(text) => query::tokens(text),
        Atom::Phrase(text) | Atom::Wildcard(text) => query::tokens(text)
            .into_iter()
            .filter(|(index, _)| *index == Index::Trigram)
            .collect(),
    }
}

/// With no word it needs outright, a query starts from what any alternative
/// of one clause finds, when each has a token an index can find: everything
/// the clause allows is in there, and the operators then hold it to the rest.
/// A clause with an alternative only a scan can answer, such as `x*` or a
/// short unspaced word, and a query of nothing but exclusions, start from
/// every video (None).
fn alternatives(query: &Query) -> Option<Sql> {
    query.clauses.iter().find_map(|clause| {
        let atom_tokens: Vec<_> = clause.iter().map(atom_driver_tokens).collect();
        let indexed = atom_tokens
            .iter()
            .all(|tokens| tokens.iter().any(|(index, _)| *index != Index::Substring));
        indexed.then(|| {
            let parts: Vec<Sql> = atom_tokens
                .iter()
                .map(|tokens| text_matches(tokens, false, false))
                .collect();
            let sql = parts
                .iter()
                .map(|(sql, _)| sql.as_str())
                .collect::<Vec<_>>()
                .join(" UNION ");
            let params = parts.into_iter().flat_map(|(_, params)| params).collect();
            (format!("SELECT id, score FROM ({sql})"), params)
        })
    })
}

/// The videos with every token. One index drives the query and scores it;
/// every other kind of token narrows it. The trigram index drives when it
/// can, because its bm25 sees the longest and most specific tokens.
fn text_matches(tokens: &[(Index, &str)], typing: bool, rank: bool) -> Sql {
    let of = |wanted| -> Vec<&str> {
        tokens
            .iter()
            .filter(|(index, _)| *index == wanted)
            .map(|(_, token)| *token)
            .collect()
    };
    let (trigrams, words) = (of(Index::Trigram), of(Index::Word));
    // Lowercased to match `lower(title)`. SQLite's lower() folds only ASCII,
    // which is all a short token in an unspaced script can mix in ("a版").
    let mut substrings: Vec<String> = of(Index::Substring)
        .into_iter()
        .map(str::to_lowercase)
        .collect();
    // The token still being typed is a prefix, which only a word query
    // needs: a trigram phrase is already a substring match.
    let prefix = typing && matches!(tokens.last(), Some((Index::Word, _)));

    let (mut sql, mut params, words) = if !trigrams.is_empty() {
        (
            fts("search_trigrams", rank),
            vec![phrases(&trigrams, false)],
            words,
        )
    } else if !words.is_empty() {
        (
            fts("search_words", rank),
            vec![phrases(&words, prefix)],
            Vec::new(),
        )
    } else {
        let first = substrings.remove(0);
        (
            format!(
                "SELECT id, 0.0 AS score FROM search_text WHERE instr({}, ?) > 0",
                folded("title", &first)
            ),
            vec![first],
            Vec::new(),
        )
    };
    // `+rowid`, not `rowid`: given a plain `rowid IN (…)`, SQLite hands each
    // id in the list to FTS5, which then runs its whole MATCH once per id.
    // The unary plus keeps the list out of FTS5, so its MATCH runs once.
    if !words.is_empty() {
        sql.push_str(" AND +rowid IN (SELECT rowid FROM search_words WHERE search_words MATCH ?)");
        params.push(phrases(&words, prefix));
    }
    let narrowing = substrings
        .iter()
        .map(|substring| {
            format!(
                " AND +rowid IN (SELECT id FROM search_text WHERE instr({}, ?) > 0)",
                folded("title", substring)
            )
        })
        .collect::<Vec<_>>()
        .concat();
    params.extend(substrings);
    (sql + &narrowing, params)
}

/// bm25's weights must be literals. The table is named in full because MATCH
/// doesn't accept an alias.
fn fts(table: &str, rank: bool) -> String {
    let score = if rank {
        format!("bm25({table}, {RANK})")
    } else {
        "0.0".into()
    };
    format!("SELECT rowid AS id, {score} AS score FROM {table} WHERE {table} MATCH ?")
}

fn phrases(tokens: &[&str], prefix: bool) -> String {
    let mut quoted: Vec<String> = tokens.iter().map(|token| quote_fts(token)).collect();
    if prefix && let Some(last) = quoted.last_mut() {
        last.push('*');
    }
    quoted.join(" ")
}

/// A string FTS5 reads as text, never as its query syntax.
fn quote_fts(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// Everything the driver can't say: quoted text, patterns, alternatives and
/// exclusions, as one condition on `search_text t`, or None when there's
/// nothing to hold the candidates to. Plain words need nothing here: the
/// driver already answered for them.
fn strict(query: &Query) -> Option<Sql> {
    let required = query
        .clauses
        .iter()
        .filter(|clause| !matches!(clause.as_slice(), [Atom::Words(_)]))
        .map(|clause| any(clause.iter().map(atom_condition).collect()));
    let excluded = query
        .excludes
        .iter()
        .map(|atom| negate(excluded_condition(atom)));
    let conditions: Vec<Sql> = required.chain(excluded).collect();
    (!conditions.is_empty()).then(|| all(conditions))
}

fn atom_condition(atom: &Atom) -> Sql {
    match atom {
        // Plain words as an alternative mean what they mean in the driver.
        Atom::Words(text) => all(query::tokens(text)
            .into_iter()
            .map(|(index, token)| match index {
                Index::Trigram => in_index("search_trigrams", token),
                Index::Word => in_index("search_words", token),
                Index::Substring => contains(token),
            })
            .collect()),
        Atom::Phrase(text) => literal_condition(text),
        Atom::Wildcard(text) => wildcard_condition(text),
    }
}

/// An excluded word is a whole word, not a substring, or "-cat" would take
/// out every "catalog". One with punctuation in it, such as "-spider-man", is
/// the phrase it spells.
fn excluded_condition(atom: &Atom) -> Sql {
    match atom {
        Atom::Words(text) => phrase_condition(text),
        atom => atom_condition(atom),
    }
}

/// Quoted text is exactly those characters, punctuation and spaces and all.
/// The trigram index holds every character, so it answers that as a
/// substring, ignoring case and accents. Text too short for a trigram is
/// compared lowercased, which folds only ASCII.
fn literal_condition(text: &str) -> Sql {
    if text.chars().count() >= 3 {
        in_fts("search_trigrams", quote_fts(text))
    } else {
        contains(text)
    }
}

/// The word index splits text the way `query::tokens` does, so a phrase
/// there is its words in order, across any separator. A script written
/// without spaces is one long word to that index, and its words are matched
/// as substrings instead, which loses their order.
fn phrase_condition(text: &str) -> Sql {
    let words: Vec<&str> = query::tokens(text).into_iter().map(|(_, w)| w).collect();
    if words.iter().any(|word| query::unspaced(word)) {
        all(words
            .into_iter()
            .map(|word| {
                if query::unspaced(word) {
                    substring_condition(word)
                } else {
                    in_index("search_words", word)
                }
            })
            .collect())
    } else {
        in_fts("search_words", quote_fts(&words.join(" ")))
    }
}

/// A pattern of one word that a star ends is the start of a word, which the
/// word index answers. Any other is a GLOB on the lowercased text, with
/// punctuation and stars alike matching any run of characters. Its pieces are
/// only letters, digits and marks, so nothing in it is GLOB syntax.
fn wildcard_condition(text: &str) -> Sql {
    let anchored = text
        .split('*')
        .next()
        .is_some_and(|head| !query::tokens(head).is_empty());
    let words: Vec<&str> = query::tokens(text).into_iter().map(|(_, w)| w).collect();
    let starts_word = anchored && words.first().is_some_and(|word| !query::unspaced(word));
    match words.as_slice() {
        [word] if starts_word => in_fts("search_words", format!("{}*", quote_fts(word))),
        [word] => substring_condition(word),
        _ => {
            let pieces: Vec<String> = words.iter().map(|word| word.to_lowercase()).collect();
            let pattern = format!("*{}*", pieces.join("*"));
            let glob = text_condition(|column| format!("{column} GLOB ?"), &pattern);
            match words.first() {
                Some(first) if starts_word => all(vec![
                    in_fts("search_words", format!("{}*", quote_fts(first))),
                    glob,
                ]),
                _ => glob,
            }
        }
    }
}

fn substring_condition(word: &str) -> Sql {
    if word.chars().count() >= 3 {
        in_index("search_trigrams", word)
    } else {
        contains(word)
    }
}

fn in_index(table: &str, token: &str) -> Sql {
    in_fts(table, quote_fts(token))
}

fn in_fts(table: &str, expression: String) -> Sql {
    (
        format!("t.id IN (SELECT rowid FROM {table} WHERE {table} MATCH ?)"),
        vec![expression],
    )
}

fn contains(token: &str) -> Sql {
    text_condition(
        |column| format!("instr({column}, ?) > 0"),
        &token.to_lowercase(),
    )
}

/// SQLite's `lower()` folds only ASCII letters, so for text without one it
/// changes nothing the text could match, and is skipped.
fn folded(column: &str, text: &str) -> String {
    if text.chars().any(|c| c.is_ascii_alphabetic()) {
        format!("lower({column})")
    } else {
        column.into()
    }
}

/// `condition` over each text column of `search_text t`, any of which may
/// match. It turns a column into SQL with one `?`, which is `param`. A NULL
/// column reads as empty: NULL would make `NOT` unknown and drop the row from
/// an exclusion's results.
fn text_condition(condition: impl Fn(&str) -> String, param: &str) -> Sql {
    any(TEXT_COLUMNS
        .iter()
        .map(|column| {
            let column = format!("coalesce(t.{column}, '')");
            (condition(&folded(&column, param)), vec![param.into()])
        })
        .collect())
}

fn negate((sql, params): Sql) -> Sql {
    (format!("NOT {sql}"), params)
}

fn all(conditions: Vec<Sql>) -> Sql {
    join(conditions, " AND ")
}

fn any(conditions: Vec<Sql>) -> Sql {
    join(conditions, " OR ")
}

fn join(mut conditions: Vec<Sql>, operator: &str) -> Sql {
    if conditions.len() == 1 {
        return conditions.remove(0);
    }
    let sql = conditions
        .iter()
        .map(|(sql, _)| sql.as_str())
        .collect::<Vec<_>>()
        .join(operator);
    let params = conditions
        .into_iter()
        .flat_map(|(_, params)| params)
        .collect();
    (format!("({sql})"), params)
}

#[cfg(test)]
mod tests;
