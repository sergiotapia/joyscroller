use super::*;
use crate::db::test_support::*;
use crate::db::*;

// ---- Search ----

/// Titles as they really come, borrowed from magnetissimo's search tests:
/// dotted names, a run of spaces and a dash in front, CJK with no spaces,
/// an accent, quotes and a bar inside a title.
const CORPUS: &[(&str, &str)] = &[
    (
        "dune_uhd",
        "Dune Part Two 2024 2160p UHD BluRay x265-MeGusta",
    ),
    (
        "dune_web",
        "Dune.Part.Two.2024.1080p.WEB-DL.DDP5.1.H.264-FLUX",
    ),
    ("dune_hdcam", "Dune 2021 HDCAM x264-GAP"),
    ("dune_cam", "Dune Part Two 2024 CAM 720p x264"),
    (
        "prophecy3",
        "Dune Prophecy S01E03 Sisterhood Above All 1080p MAX WEB-DL DDP5 1 Atmos H 264-FLUX",
    ),
    ("prophecy4", "Dune Prophecy S01E04 Twice Born 2160p WEB-DL"),
    ("dunes", "Dunes of Arrakis Soundtrack FLAC"),
    (
        "seinfeld_s05e11",
        "-    Seinfeld S05E11 The Conversion 1080p HEVC x265-MeGusta",
    ),
    (
        "seinfeld_s05e16",
        "-    Seinfeld S05E16 The Stand-In 1080p PLAY WEB-DL AAC2 0 H 264-FLUX",
    ),
    (
        "seinfeld_s03e14",
        "Seinfeld s03e14 the pez dispenser dvdrip x264 digg mkv",
    ),
    ("seinfeld_disc", "Seinfeld season 5 disc 4 dvdr"),
    ("bb_s05e13", "Breaking-Bad-S05E13-HDTVx264EVOLVE[ettv]"),
    ("bb_s05e14", "Breaking Bad S05E14 Ozymandias 720p"),
    (
        "el_camino",
        "El Camino - A Breaking Bad Movie 2019 (720p - WEB-DL) [DUBLADO]",
    ),
    (
        "cabin_woods",
        "The.Cabin.In.The.Woods.2011.DVDRip.XviD-Ac3.Feel-Free",
    ),
    ("cabin", "The Cabin (2018)"),
    ("shingeki", "進撃の巨人 The Final Season 2160p"),
    (
        "rilakkuma",
        "[ANi] 拉拉熊 - 27 [1080P][Baha][WEB-DL][AAC AVC][CHT] mp4",
    ),
    ("pikachu", "Pokémon Detective Pikachu 2019 1080p"),
    ("toy4", "Toy.Story.4.2019.1080p"),
    ("toy3", "Toy Story 3 (2010)"),
    ("f9", "F9 2021 720p HDCAM x265-GAP"),
    ("camera", "Camera Obscura 2017 1080p WEB"),
    (
        "weird_al",
        r#""Weird Al" Yankovic - Mandatory Fun (2014) FLAC"#,
    ),
    ("cyberpunk", "Cyberpunk | Edgerunners S01 1080p"),
    ("not_okay", "Not Okay (2022)"),
    ("up", "Up (2009) 4K HDR"),
];

const DUNE: &[&str] = &[
    "dune_uhd",
    "dune_web",
    "dune_hdcam",
    "dune_cam",
    "prophecy3",
    "prophecy4",
    "dunes",
];
const SEINFELD: &[&str] = &[
    "seinfeld_s05e11",
    "seinfeld_s05e16",
    "seinfeld_s03e14",
    "seinfeld_disc",
];

/// The corpus, plus one video by a TikTok creator whose display name
/// differs from their handle. Only "cabin" has a description, from its
/// download, so a match on "couple" is in the description.
fn search_db() -> Connection {
    let mut conn = memory_db();
    let mut videos: Vec<NewVideo> = CORPUS
        .iter()
        .map(|&(id, title)| NewVideo {
            title: Some(title),
            ..new_video(id, true)
        })
        .collect();
    videos.push(NewVideo {
        title: None,
        author_name: Some("zicktoonz"),
        listing_json: r#"{"author": {"nickname": "Zick Toonz"}}"#,
        ..new_video("creator", true)
    });
    insert_new(&mut conn, "tiktok", &videos).unwrap();
    let info = r#"{"description": "A couple escape to a cabin in the woods."}"#;
    record_download(
        &mut conn,
        "tiktok",
        "cabin",
        &Downloaded {
            folder: "tiktok/cabin",
            files: &[],
            size_bytes: 0,
            formats: None,
            metadata_json: Some(info),
            posted_at: None,
        },
    )
    .unwrap();
    conn
}

fn sorted(ids: &[&str]) -> Vec<String> {
    let mut ids: Vec<String> = ids.iter().map(|&id| id.into()).collect();
    ids.sort();
    ids
}

fn all_but(except: &[&str]) -> Vec<String> {
    let mut ids: Vec<&str> = CORPUS.iter().map(|(id, _)| *id).collect();
    ids.push("creator");
    ids.retain(|id| !except.contains(id));
    sorted(&ids)
}

fn without(ids: &[&str], except: &[&str]) -> Vec<String> {
    sorted(
        &ids.iter()
            .copied()
            .filter(|id| !except.contains(id))
            .collect::<Vec<_>>(),
    )
}

/// Asserts each case's query finds what it expects in `search_db`.
fn assert_finds(cases: &[(&str, &str, Vec<String>)]) {
    let conn = search_db();
    for (label, text, expected) in cases {
        assert_eq!(&found(&conn, text), expected, "{label}: {text:?}");
    }
}

#[test]
fn search_quotes() {
    assert_finds(&[
        // Quotes match exactly what's between them, punctuation and
        // spaces and all, ignoring only case and accents.
        ("quotes keep brackets", r#""[ani]""#, sorted(&["rilakkuma"])),
        ("quotes keep a dot", r#""h.264""#, sorted(&["dune_web"])),
        (
            "quotes keep a space",
            r#""h 264""#,
            sorted(&["prophecy3", "seinfeld_s05e16"]),
        ),
        (
            "quotes keep a hyphen",
            r#""web-dl""#,
            sorted(&[
                "dune_web",
                "prophecy3",
                "prophecy4",
                "seinfeld_s05e16",
                "el_camino",
                "rilakkuma",
            ]),
        ),
        ("a hyphen is not a dot", r#""web.dl""#, sorted(&[])),
        (
            "a space is not a dot",
            r#""part two""#,
            sorted(&["dune_uhd", "dune_cam"]),
        ),
        (
            "a dot is not a space",
            r#""part.two""#,
            sorted(&["dune_web"]),
        ),
        ("quoted text keeps its order", r#""two part""#, sorted(&[])),
        (
            "quoted text keeps its words together",
            r#""dune two""#,
            sorted(&[]),
        ),
        (
            "quoted text matches inside a word",
            r#""s05e1""#,
            sorted(&[
                "seinfeld_s05e11",
                "seinfeld_s05e16",
                "bb_s05e13",
                "bb_s05e14",
            ]),
        ),
        (
            "a quoted word is a substring too",
            r#""dune""#,
            sorted(DUNE),
        ),
        (
            "quoted text across a dash",
            r#""x265-megusta""#,
            sorted(&["dune_uhd", "seinfeld_s05e11"]),
        ),
        (
            "quoted text with parentheses",
            r#""(2018)""#,
            sorted(&["cabin"]),
        ),
    ]);
}

#[test]
fn search_quotes_in_any_column_and_script() {
    assert_finds(&[
        (
            "quoted text in a description",
            r#""cabin in the woods""#,
            sorted(&["cabin"]),
        ),
        (
            "quotes in a title are characters",
            r#""weird al""#,
            sorted(&["weird_al"]),
        ),
        (
            "quoted text ignores case and accents",
            r#""POKEMON""#,
            sorted(&["pikachu"]),
        ),
        (
            "quoted text in an unspaced script",
            r#""進撃の巨人""#,
            sorted(&["shingeki"]),
        ),
        (
            "short quoted text in an unspaced script",
            r#""巨人""#,
            sorted(&["shingeki"]),
        ),
        ("short quoted text", r#""拉拉""#, sorted(&["rilakkuma"])),
        ("a short quoted number", r#"up "4""#, sorted(&["up"])),
        (
            "dots in a title are not spaces",
            r#""toy story 4""#,
            sorted(&[]),
        ),
        (
            "an unclosed quote",
            r#"dune "part two"#,
            sorted(&["dune_uhd", "dune_cam"]),
        ),
        (
            "curly quotes",
            "“part two”",
            sorted(&["dune_uhd", "dune_cam"]),
        ),
    ]);
}

#[test]
fn search_exclusions() {
    assert_finds(&[
        // Exclusions remove whole words, so "-cam" keeps El Camino and HDCAM.
        (
            "an exclusion is a whole word",
            "dune -cam",
            without(DUNE, &["dune_cam"]),
        ),
        (
            "two exclusions",
            "dune -cam -hdcam",
            without(DUNE, &["dune_cam", "dune_hdcam"]),
        ),
        (
            "a pattern excludes inside words",
            "dune -*cam",
            without(DUNE, &["dune_cam", "dune_hdcam"]),
        ),
        (
            "only an exclusion is everything else",
            "-cam",
            all_but(&["dune_cam"]),
        ),
        (
            "an exclusion of a longer word",
            "breaking bad -camino",
            sorted(&["bb_s05e13", "bb_s05e14"]),
        ),
        (
            "an exclusion doesn't cut into words",
            "breaking bad -cam",
            sorted(&["bb_s05e13", "bb_s05e14", "el_camino"]),
        ),
        (
            "an exclusion after a dash",
            "seinfeld -x265",
            without(SEINFELD, &["seinfeld_s05e11"]),
        ),
        (
            "an excluded word with a hyphen is a phrase",
            "seinfeld -web-dl",
            without(SEINFELD, &["seinfeld_s05e16"]),
        ),
        (
            "an excluded quote",
            r#"seinfeld -"web-dl""#,
            without(SEINFELD, &["seinfeld_s05e16"]),
        ),
        (
            "an excluded quote is exact too",
            r#"seinfeld -"web dl""#,
            sorted(SEINFELD),
        ),
        (
            "a lone dash is not an exclusion",
            "seinfeld - x265",
            sorted(&["seinfeld_s05e11"]),
        ),
        (
            "two dashes are one",
            "seinfeld --x265",
            without(SEINFELD, &["seinfeld_s05e11"]),
        ),
        (
            "a substring survives its whole word's exclusion",
            "dune -dune",
            sorted(&["dunes"]),
        ),
        (
            "an exclusion ignores accents",
            "pikachu -pokemon",
            sorted(&[]),
        ),
        (
            "an exclusion in an unspaced script",
            "2160p -巨人",
            sorted(&["dune_uhd", "prophecy4"]),
        ),
        (
            "an exclusion reads descriptions",
            "cabin -couple",
            sorted(&["cabin_woods"]),
        ),
        ("exclusions of nothing", "-xyzzy", all_but(&[])),
    ]);
}

#[test]
fn search_alternatives() {
    assert_finds(&[
        // A bar is OR, and binds tighter than the space between words.
        (
            "alternatives",
            "dune 2160p|1080p",
            sorted(&["dune_uhd", "dune_web", "prophecy3", "prophecy4"]),
        ),
        (
            "a doubled bar",
            "dune 2160p||1080p",
            sorted(&["dune_uhd", "dune_web", "prophecy3", "prophecy4"]),
        ),
        (
            "short alternatives are whole words",
            "toy story 3|4",
            sorted(&["toy3", "toy4"]),
        ),
        ("a short alternative", "up 4k|8k", sorted(&["up"])),
        ("no alternative matches", "up 8k|16k", sorted(&[])),
        (
            "alternatives as the whole query",
            "dune|seinfeld",
            sorted(&[DUNE, SEINFELD].concat()),
        ),
        (
            "a bar binds tighter than a space",
            "dune|seinfeld 1080p",
            sorted(&[
                "dune_web",
                "prophecy3",
                "seinfeld_s05e11",
                "seinfeld_s05e16",
            ]),
        ),
        (
            "a quote as an alternative",
            r#""part two"|prophecy"#,
            sorted(&["dune_uhd", "dune_cam", "prophecy3", "prophecy4"]),
        ),
        (
            "a bar next to an exclusion is ignored",
            "dune 2160p|-cam",
            sorted(&["dune_uhd", "prophecy4"]),
        ),
        ("stray bars", "| dune |", sorted(DUNE)),
        (
            "episodes as alternatives",
            "s05e13|s05e14",
            sorted(&["bb_s05e13", "bb_s05e14"]),
        ),
        (
            "alternatives of different shapes",
            "breaking bad s05e13|camino",
            sorted(&["bb_s05e13", "el_camino"]),
        ),
        (
            "unspaced alternatives",
            "進撃|拉拉熊",
            sorted(&["shingeki", "rilakkuma"]),
        ),
        (
            "short unspaced alternatives",
            "進撃|拉拉",
            sorted(&["shingeki", "rilakkuma"]),
        ),
        (
            "an alternative matching nothing",
            "dune xyzzy|2160p",
            sorted(&["dune_uhd", "prophecy4"]),
        ),
        (
            "a bar inside a title",
            "cyberpunk|edgerunners",
            sorted(&["cyberpunk"]),
        ),
    ]);
}

#[test]
fn search_patterns() {
    assert_finds(&[
        // A star is any run of characters. A word it ends is a prefix of
        // a word; one it starts can begin anywhere.
        (
            "a trailing star is a word prefix",
            "s05e1*",
            sorted(&[
                "seinfeld_s05e11",
                "seinfeld_s05e16",
                "bb_s05e13",
                "bb_s05e14",
            ]),
        ),
        (
            "a prefix with other words",
            "seinfeld s05e1*",
            sorted(&["seinfeld_s05e11", "seinfeld_s05e16"]),
        ),
        (
            "a prefix starts a word",
            "cam*",
            sorted(&["dune_cam", "el_camino", "camera"]),
        ),
        (
            "a leading star starts anywhere",
            "*cam",
            sorted(&["dune_hdcam", "dune_cam", "el_camino", "camera", "f9"]),
        ),
        (
            "a star in the middle",
            "s0*e14",
            sorted(&["seinfeld_s03e14", "bb_s05e14"]),
        ),
        (
            "a star across words",
            "breaking*bad",
            sorted(&["bb_s05e13", "bb_s05e14", "el_camino"]),
        ),
        ("a short prefix", "toy story 4*", sorted(&["toy4"])),
        (
            "a prefix of three characters",
            "x26*",
            sorted(&[
                "dune_uhd",
                "dune_hdcam",
                "dune_cam",
                "seinfeld_s05e11",
                "seinfeld_s03e14",
                "f9",
            ]),
        ),
        (
            "prefixes as alternatives",
            "s05e1*|s03e*",
            sorted(&[
                "seinfeld_s05e11",
                "seinfeld_s05e16",
                "seinfeld_s03e14",
                "bb_s05e13",
                "bb_s05e14",
            ]),
        ),
        (
            "an excluded prefix",
            "dune -s01e*",
            without(DUNE, &["prophecy3", "prophecy4"]),
        ),
        (
            "a prefix in an unspaced script",
            "進撃*",
            sorted(&["shingeki"]),
        ),
        (
            "stars around an unspaced word",
            "*巨人*",
            sorted(&["shingeki"]),
        ),
        ("a prefix ignores accents", "poke*", sorted(&["pikachu"])),
        (
            "a prefix reads descriptions",
            "cabin wood*",
            sorted(&["cabin_woods", "cabin"]),
        ),
        ("a lone star next to a word", "dune *", sorted(DUNE)),
    ]);
}

#[test]
fn search_plain_words_and_combinations() {
    assert_finds(&[
        // No operator, no change.
        ("plain words are substrings", "dune", sorted(DUNE)),
        ("FTS5 operators are words", "NOT", sorted(&["not_okay"])),
        (
            "a trailing hyphen is punctuation",
            "x265-",
            sorted(&["dune_uhd", "seinfeld_s05e11", "f9"]),
        ),
        (
            "a short last word is a prefix while typed",
            "up 4",
            sorted(&["up"]),
        ),
        (
            "a bar inside a title isn't needed to find it",
            "cyberpunk edgerunners",
            sorted(&["cyberpunk"]),
        ),
        (
            "a title with a dash in front",
            "seinfeld conversion",
            sorted(&["seinfeld_s05e11"]),
        ),
        ("descriptions", "couple", sorted(&["cabin"])),
        ("handles", "zicktoonz", sorted(&["creator"])),
        ("display names", "zick toonz", sorted(&["creator"])),
        // Everything at once.
        (
            "quote, alternative, exclusion and prefix",
            r#""part.two" 2160p|1080p -cam -x26*"#,
            sorted(&["dune_web"]),
        ),
        (
            "alternatives of exclusions don't exist",
            "dune -cam|-hdcam",
            without(DUNE, &["dune_cam", "dune_hdcam"]),
        ),
    ]);
}

#[test]
fn titles_rank_above_authors_and_descriptions() {
    let mut conn = memory_db();
    insert_new(
        &mut conn,
        "youtube",
        &[
            NewVideo {
                title: Some("Something else"),
                author_name: Some("Piano Guy"),
                ..new_video("author", true)
            },
            NewVideo {
                title: Some("Piano solo"),
                ..new_video("title", true)
            },
            NewVideo {
                title: Some("Quiet"),
                ..new_video("description", true)
            },
        ],
    )
    .unwrap();
    let info = r#"{"description": "a piano piece"}"#;
    let done = Downloaded {
        folder: "youtube/description",
        files: &[],
        size_bytes: 0,
        formats: None,
        metadata_json: Some(info),
        posted_at: None,
    };
    record_download(&mut conn, "youtube", "description", &done).unwrap();
    let best = Order {
        sort: Sort::Best,
        dir: Dir::Asc,
    };
    let ranked = ids(videos_page(&conn, None, &query::parse("piano"), best, 10, 0).unwrap());
    assert_eq!(ranked, ["title", "author", "description"]);
    // A platform narrows a search, and its count.
    let query = query::parse("piano");
    assert_eq!(
        ids(videos_page(&conn, Some("tiktok"), &query, best, 10, 0).unwrap()),
        Vec::<String>::new()
    );
    assert_eq!(videos_count(&conn, Some("youtube"), &query).unwrap(), 3);
    assert_eq!(videos_count(&conn, Some("tiktok"), &query).unwrap(), 0);
}

#[test]
fn input_that_could_break_the_sql_or_fts5_is_searched_safely() {
    let conn = search_db();
    let long_words: Vec<String> = (1..=500).map(|n| format!("w{n}")).collect();
    let excluded: Vec<String> = (1..=500).map(|n| format!("-w{n}")).collect();
    let phrases: Vec<String> = (1..=200).map(|n| format!("\"w{n} x\"")).collect();
    let patterns: Vec<String> = (1..=200).map(|n| format!("w{n}*z")).collect();
    let hostile = [
        r#"""#.to_string(),
        r#"""""#.into(),
        r#"-""#.into(),
        r#"|"|"#.into(),
        r#"*"*"#.into(),
        "\\".into(),
        "'".into(),
        "' OR 1=1 --".into(),
        "NEAR(a b)".into(),
        "{title}: dune".into(),
        "title:dune".into(),
        "description : dune".into(),
        "dune^".into(),
        "(".into(),
        ")".into(),
        r#""a" OR "b""#.into(),
        "-(".into(),
        "*-*".into(),
        "||||".into(),
        "-|-".into(),
        "dune\u{0}cam".into(),
        "🔥 dune 🔥".into(),
        "é".into(),
        "دون".into(),
        "a".repeat(10_000),
        "-".repeat(1000),
        long_words.join("|"),
        excluded.join(" "),
        phrases.join(" "),
        patterns.join(" "),
        "%".into(),
        "_".into(),
        "[a-z]*".into(),
        "?".into(),
        "dune [".into(),
        "dune ]*".into(),
        "s0?e1*".into(),
        "**a**b**".into(),
    ];
    for text in &hostile {
        found(&conn, text);
    }
}

#[test]
fn glob_characters_in_a_pattern_are_punctuation() {
    let mut conn = memory_db();
    insert_new(
        &mut conn,
        "youtube",
        &[NewVideo {
            title: Some("Question? Mark [Remastered]"),
            ..new_video("q", true)
        }],
    )
    .unwrap();
    assert_eq!(found(&conn, "q*ion"), ["q"]);
    assert_eq!(found(&conn, "ques*[remastered]"), ["q"]);
    // As a GLOB class, "[a-z]" would match the "q" of "Question".
    assert_eq!(found(&conn, "[a-z]uestion*"), Vec::<String>::new());
}

#[test]
fn finds_x_posts_by_display_name_and_quoted_post() {
    let mut conn = memory_db();
    let quoting = r#"{"core": {"user_results": {"result": {"core": {"name": "Poster Person"}}}},
        "quoted_status_result": {"result": {
            "core": {"user_results": {"result": {"core": {"screen_name": "konami", "name": "Konami"}}}},
            "legacy": {"full_text": "Castlevania out now"}}}}"#;
    let long = r#"{"quoted_status_result": {"result": {
        "note_tweet": {"note_tweet_results": {"result": {"text": "a long post about turtles"}}},
        "legacy": {"full_text": "a long post about…"}}}}"#;
    insert_new(
        &mut conn,
        "x",
        &[
            NewVideo {
                title: Some("look at this"),
                author_name: Some("poster"),
                listing_json: quoting,
                ..new_video("1", true)
            },
            NewVideo {
                title: Some("wow"),
                listing_json: long,
                ..new_video("2", true)
            },
        ],
    )
    .unwrap();
    assert_eq!(found(&conn, "\"poster person\""), ["1"]);
    assert_eq!(found(&conn, "castlevania"), ["1"]);
    assert_eq!(found(&conn, "konami"), ["1"]);
    assert_eq!(found(&conn, "turtles"), ["2"]);
    // A download changes nothing: an X post's description is its quote.
    download(&mut conn, "x", "1");
    assert_eq!(found(&conn, "castlevania"), ["1"]);
}
