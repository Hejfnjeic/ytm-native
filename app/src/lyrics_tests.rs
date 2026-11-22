//! Tests for lyrics parsing and matching. Inputs are made up; no real songs.

use super::*;

#[test]
fn times() {
    assert_eq!(parse_time("24.365"), Some(24.365));
    assert_eq!(parse_time("0:31.136"), Some(31.136));
    assert_eq!(parse_time("1:02:03.5"), Some(3723.5));
    assert_eq!(parse_time("12.5s"), Some(12.5));
    assert_eq!(parse_time("500ms"), Some(0.5));
}

#[test]
fn ttml_words_and_spaces() {
    let t = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:itunes="http://music.apple.com/lyric-ttml-internal" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" itunes:timing="Word"><body><div>
        <p begin="1.0" end="3.0" ttm:agent="v1"><span begin="1.0" end="1.5">Hel</span><span begin="1.5" end="2.0">lo</span> <span begin="2.0" end="3.0">world</span><span ttm:role="x-bg"><span begin="2.5" end="3.0">(yeah)</span></span></p>
        <p begin="4.0" end="5.0" ttm:agent="v2"><span begin="4.0" end="5.0">duet</span></p></div></body></tt>"#;
    let l = parse_ttml(t, "x").unwrap();
    assert_eq!(l.sync, Sync::Syllable);
    assert_eq!(l.lines.len(), 3);
    let w: Vec<&str> = l.lines[0].words.iter().map(|w| w.text.as_str()).collect();
    assert_eq!(w, ["Hel", "lo ", "world"]);
    assert!(l.lines[1].background);
    assert!(l.lines[2].opposite);
}

#[test]
fn lrc_lines_enhanced_and_credits() {
    let l = parse_lrc("[00:00.50]作词 : Someone\n[00:01.00]first line\n[00:03.50]second\n[00:05.00]", "x");
    assert_eq!(l.sync, Sync::Line);
    assert_eq!(l.lines.len(), 2, "leading credit line is dropped");
    assert_eq!(l.lines[0].end, 3.5);
    let e = parse_lrc("[00:01.00]<00:01.00>Hel<00:01.50>lo <00:02.00>world\n[00:03.00]x", "x");
    assert_eq!(e.sync, Sync::Syllable);
    assert_eq!(e.lines[0].words.len(), 3);
    assert_eq!(e.lines[0].words[2].start, 2.0);
}

#[test]
fn translation_detection() {
    let en = parse_lrc("[00:01.00]This line is written in English\n[00:03.00]and so is this one here", "x");
    let ja = parse_lrc("[00:01.00]これは日本語の歌詞です\n[00:03.00]二行目も日本語で書いてある", "x");
    assert!(looks_translated("日本語の題名 - Romaji Title", &en));
    assert!(!looks_translated("日本語の題名 - Romaji Title", &ja));
    assert!(!looks_translated("An English Title", &en));
}

#[test]
fn variants() {
    let v = title_variants("日本語の題名 - Romaji Title (feat. Someone)");
    assert_eq!(v[0], "日本語の題名 - Romaji Title");
    assert!(v.contains(&"日本語の題名".to_string()) && v.contains(&"Romaji Title".to_string()));
    assert_eq!(main_artist("Artist A & Artist B / Label"), "Artist A");
    assert_eq!(main_artist("Artist A feat. Artist B"), "Artist A");
}

/// Live lookups against the real sources (network). Song list comes from a file outside the
/// repo, one per line: `title<TAB>artist<TAB>album<TAB>seconds`.
///   YTM_LYRICS_CASES=/path/cases.tsv cargo test -p ytm-native -- --ignored lyrics_live --nocapture
#[tokio::test]
#[ignore]
async fn lyrics_live() {
    let path = std::env::var("YTM_LYRICS_CASES").expect("set YTM_LYRICS_CASES to a TSV file");
    let f = Finder::new().unwrap();
    for line in std::fs::read_to_string(path).unwrap().lines().filter(|l| !l.trim().is_empty()) {
        let c: Vec<&str> = line.split('\t').collect();
        let (t, a, al, d) = (c[0], c.get(1).copied().unwrap_or(""), c.get(2).copied().unwrap_or(""), c.get(3).and_then(|d| d.parse().ok()).unwrap_or(0.0));
        let t0 = std::time::Instant::now();
        match f.find(t, a, al, d).await {
            Some(l) => println!("{t}: {:?} from {} ({} lines) in {:?}; first: {:?}", l.sync, l.source, l.lines.len(), t0.elapsed(),
                l.lines.first().map(|x| x.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>())),
            None => println!("{t}: none ({:?})", t0.elapsed()),
        }
    }
}

#[test]
fn units_keep_words_and_split_cjk() {
    let l = parse_lrc("[00:01.00]日本語、です Hello world\n[00:03.00]x", "x");
    let units: Vec<String> = line_units(&l.lines[0]).iter().map(|u| u.iter().map(|c| c.0.as_str()).collect()).collect();
    assert_eq!(units, ["日", "本", "語、", "で", "す ", "Hello ", "world"]);
}

#[test]
fn ttml_translations_follow_line_keys() {
    let ttml = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:lrc="http://lrc.red/lyric-ttml-internal" xmlns:ttm="http://www.w3.org/ns/ttml#metadata" lrc:timing="Line">
      <head><metadata><sourceMetadata xmlns="http://lrc.red/lyric-ttml-internal">
        <translations>
          <translation type="subtitle" xml:lang="de"><text for="L1">Erste Zeile</text></translation>
          <translation type="subtitle" xml:lang="en-US"><text for="L1">First line</text><text for="L3">Third line</text></translation>
        </translations>
        <transliterations><transliteration xml:lang="ja-Latn"><text for="L1"><span begin="1" end="2">romaji</span></text></transliteration></transliterations>
      </sourceMetadata></metadata></head>
      <body><div>
        <p begin="1.0" end="2.0" lrc:key="L1">Zeile eins</p>
        <p begin="2.0" end="3.0" lrc:key="L2">Zeile zwei</p>
        <p begin="3.0" end="4.0" lrc:key="L3">Zeile drei</p>
      </div></body></tt>"#;
    let l = parse_ttml(ttml, "test").unwrap();
    let tr: Vec<Option<&str>> = l.lines.iter().map(|x| x.translation.as_deref()).collect();
    // English is preferred; lines without one stay empty; romanization is never used.
    assert_eq!(tr, [Some("First line"), None, Some("Third line")]);
    assert!(l.has_translation());
    assert_eq!(l.lines.len(), 3);
}
