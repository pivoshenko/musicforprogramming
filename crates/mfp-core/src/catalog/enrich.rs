//! Best-effort enrichment from the site's client bundle, the only source of track listings.
//! Nothing here may fail the catalog: every failure is logged at debug level and skipped.

use std::collections::HashMap;

use crate::model::{Episode, InfoPage};

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub slug: String,

    /// `"episode"` for an episode, `"info"` for the `about` and `credits` pages.
    pub kind: String,
    pub order: Option<u32>,

    pub title: Option<String>,

    pub special: bool,
    /// The join key: byte-identical to an episode's enclosure URL.
    pub file: String,

    pub tracklist: Option<String>,

    pub body: Option<String>,

    pub links: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Enrichment {
    pub applied: bool,

    pub info: Vec<InfoPage>,
}

/// Enriches `episodes` in place and returns what else the bundle carried. Never fails: an
/// unreachable or unreadable bundle yields [`Enrichment::default`].
pub async fn enrich(client: &reqwest::Client, episodes: &mut [Episode]) -> Enrichment {
    match fetch_bundle(client).await {
        Some(bundle) => enrich_from(&bundle, episodes),
        None => Enrichment::default(),
    }
}

pub(super) fn enrich_from(bundle_js: &str, episodes: &mut [Episode]) -> Enrichment {
    let records = extract_records(bundle_js);
    if records.is_empty() {
        tracing::debug!("the client bundle yielded no records; serving the feed alone");
        return Enrichment::default();
    }
    let info = info_pages(&records);
    Enrichment {
        applied: apply(records, episodes) > 0,
        info,
    }
}

fn info_pages(records: &[Record]) -> Vec<InfoPage> {
    records
        .iter()
        .filter(|record| record.kind == "info")
        .filter_map(|record| {
            Some(InfoPage {
                slug: record.slug.clone(),
                title: record.title.clone()?,
                body: record.body.clone()?,
            })
        })
        .collect()
}

async fn fetch_bundle(client: &reqwest::Client) -> Option<String> {
    let shell = fetch_text(client, super::SITE_URL).await?;

    let path = match find_bundle_url(&shell) {
        Some(path) => path,
        None => {
            let Some(target) = find_redirect_target(&shell) else {
                tracing::debug!("the site root references no client bundle");
                return None;
            };
            let redirected = fetch_text(client, &absolute(&target)).await?;
            match find_bundle_url(&redirected) {
                Some(path) => path,
                None => {
                    tracing::debug!("{target} references no client bundle");
                    return None;
                }
            }
        }
    };

    let url = absolute(&path);
    let body = fetch_text(client, &url).await?;

    if !is_script(&body) {
        tracing::debug!("{url} returned a document rather than a script");
        return None;
    }

    Some(body)
}

fn is_script(body: &str) -> bool {
    let body = body.trim_start();
    !body.is_empty() && !body.starts_with('<')
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = match super::request(client, url).send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!("{url} could not be retrieved: {error}");
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::debug!("{url} returned HTTP {}", response.status());
        return None;
    }
    match response.text().await {
        Ok(body) => Some(body),
        Err(error) => {
            tracing::debug!("{url} could not be read: {error}");
            None
        }
    }
}

fn absolute(path: &str) -> String {
    format!(
        "{}/{}",
        super::SITE_URL.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn apply(records: Vec<Record>, episodes: &mut [Episode]) -> usize {
    let mut by_file: HashMap<String, Record> = records
        .into_iter()
        .filter(|record| record.kind == "episode" && !record.file.is_empty())
        .map(|record| (record.file.clone(), record))
        .collect();

    let mut enriched = 0;
    for episode in episodes.iter_mut() {
        let Some(record) = by_file.remove(episode.enclosure_url.as_str()) else {
            continue;
        };
        episode.slug = Some(record.slug);
        episode.bundle_title = record.title;
        episode.order = record.order;
        episode.special = record.special;
        episode.tracklist = record.tracklist;
        episode.body = record.body;
        episode.links = record.links;
        enriched += 1;
    }
    enriched
}

/// The bundle's hash changes on every upstream deploy, so it is never hardcoded or
/// persisted, only read back out of the shell document.
pub fn find_bundle_url(shell_html: &str) -> Option<String> {
    const MARKER: &str = "client/client.";

    let mut search = 0;
    while let Some(offset) = shell_html[search..].find(MARKER) {
        let hash_start = search + offset + MARKER.len();
        let hash: String = shell_html[hash_start..]
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        let after_hash = hash_start + hash.len();
        if !hash.is_empty() && shell_html[after_hash..].starts_with(".js") {
            return Some(format!("/client/client.{hash}.js"));
        }
        search = hash_start;
    }
    None
}

fn find_redirect_target(html: &str) -> Option<String> {
    let rest = html.split_once("window.location.href")?.1;
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let delimiter = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let target: String = rest[delimiter.len_utf8()..]
        .chars()
        .take_while(|c| *c != delimiter)
        .collect();
    (!target.is_empty()).then_some(target)
}

/// Tolerates anything it does not recognise: a malformed record is skipped, and a bundle
/// whose shape has changed yields none, costing the track listings and nothing else.
pub fn extract_records(bundle_js: &str) -> Vec<Record> {
    const MARKER: &str = "{slug:";

    let mut records = Vec::new();
    let mut search = 0;
    while let Some(offset) = bundle_js[search..].find(MARKER) {
        let start = search + offset;
        match scan_object(bundle_js, start) {
            Some(end) => {
                if let Some(record) = parse_record(&bundle_js[start..end]) {
                    records.push(record);
                } else {
                    tracing::debug!("skipping an unparseable bundle record at byte {start}");
                }
                search = end;
            }

            None => search = start + MARKER.len(),
        }
    }
    records
}

fn scan_object(source: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (offset, character) in source[start..].char_indices() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' | '`' => quote = Some(character),
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(start + offset + character.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

enum Field<'a> {
    Text(String),

    Raw(&'a str),
}

impl Field<'_> {
    fn text(self) -> Option<String> {
        match self {
            Self::Text(text) => Some(text),
            Self::Raw(_) => None,
        }
    }
}

fn parse_record(object: &str) -> Option<Record> {
    let inner = object.strip_prefix('{')?.strip_suffix('}')?;

    let mut slug = None;
    let mut kind = None;
    let mut order = None;
    let mut title = None;
    let mut special = false;
    let mut file = None;
    let mut tracklist = None;
    let mut body = None;
    let mut links = None;

    for (key, value) in fields(inner) {
        match key {
            "slug" => slug = value.text(),
            "type" => kind = value.text(),
            "order" => {
                order = match value {
                    Field::Raw(raw) => raw.parse().ok(),
                    Field::Text(text) => text.parse().ok(),
                }
            }
            "title" => title = value.text(),
            "special" => {
                special = match value {
                    Field::Raw(raw) => raw == "!0" || raw == "true",
                    Field::Text(text) => text == "true",
                }
            }
            "file" => file = value.text(),
            "tracklist" => tracklist = value.text(),
            "body" => body = value.text(),
            "links" => links = value.text(),
            _ => {}
        }
    }

    let slug = slug.filter(|slug| is_filesystem_safe(slug))?;
    Some(Record {
        slug,
        kind: kind?,
        order,
        title: title.filter(|title| !title.is_empty()),
        special,
        file: file?,
        tracklist: as_text(tracklist),
        body: as_text(body),
        links: as_text(links),
    })
}

/// The slug becomes the episode's identifier and names `<slug>.mp3` in the audio cache, so
/// a separator or a parent reference is refused and the episode falls back to its hash.
fn is_filesystem_safe(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn as_text(fragment: Option<String>) -> Option<String> {
    fragment
        .map(|fragment| html_fragment_to_text(&fragment))
        .filter(|text| !text.is_empty())
}

fn fields(inner: &str) -> Vec<(&str, Field<'_>)> {
    let bytes = inner.as_bytes();
    let mut pairs = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        while index < bytes.len() && (bytes[index] == b',' || bytes[index].is_ascii_whitespace()) {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }

        let key_start = index;
        while index < bytes.len() && bytes[index] != b':' && bytes[index] != b',' {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b':' {
            break;
        }
        let key = inner[key_start..index]
            .trim()
            .trim_matches(|c| c == '"' || c == '\'');
        index += 1;

        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }

        let value = if matches!(bytes[index], b'"' | b'\'' | b'`') {
            let delimiter = bytes[index];
            let start = index + 1;
            index = start;
            let mut escaped = false;
            let mut end = None;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == delimiter {
                    end = Some(index - 1);
                    break;
                }
            }
            let Some(end) = end else { break };
            Field::Text(decode_js_string(&inner[start..end]))
        } else {
            let start = index;
            let mut depth = 0usize;
            let mut quote: Option<u8> = None;
            let mut escaped = false;
            while index < bytes.len() {
                let byte = bytes[index];
                if let Some(delimiter) = quote {
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == delimiter {
                        quote = None;
                    }
                } else {
                    match byte {
                        b'"' | b'\'' | b'`' => quote = Some(byte),
                        b'{' | b'[' | b'(' => depth += 1,
                        b'}' | b']' | b')' => depth = depth.saturating_sub(1),
                        b',' if depth == 0 => break,
                        _ => {}
                    }
                }
                index += 1;
            }
            Field::Raw(inner[start..index].trim())
        };

        pairs.push((key, value));
    }

    pairs
}

fn decode_js_string(raw: &str) -> String {
    let mut decoded = String::with_capacity(raw.len());
    let mut characters = raw.chars().peekable();

    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters.next() {
            None => break,
            Some('n') => decoded.push('\n'),
            Some('t') => decoded.push('\t'),
            Some('r') => decoded.push('\r'),
            Some('b') => decoded.push('\u{8}'),
            Some('f') => decoded.push('\u{c}'),
            Some('v') => decoded.push('\u{b}'),
            Some('0') => decoded.push('\0'),

            Some('\n') => {}
            Some('x') => match hex_scalar(&mut characters, 2) {
                Some(scalar) => decoded.push(scalar),
                None => decoded.push('\u{fffd}'),
            },
            Some('u') => match decode_unicode_escape(&mut characters) {
                Some(scalar) => decoded.push(scalar),
                None => decoded.push('\u{fffd}'),
            },

            Some(other) => decoded.push(other),
        }
    }

    decoded
}

fn decode_unicode_escape(
    characters: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Option<char> {
    if characters.peek() == Some(&'{') {
        characters.next();
        let mut digits = String::new();
        for character in characters.by_ref() {
            if character == '}' {
                break;
            }
            digits.push(character);
        }
        return char::from_u32(u32::from_str_radix(&digits, 16).ok()?);
    }

    let high = hex_value(characters, 4)?;
    if !(0xd800..0xdc00).contains(&high) {
        return char::from_u32(high);
    }

    let mut lookahead = characters.clone();
    if lookahead.next() != Some('\\') || lookahead.next() != Some('u') {
        return None;
    }
    let low = hex_value(&mut lookahead, 4)?;
    if !(0xdc00..0xe000).contains(&low) {
        return None;
    }
    *characters = lookahead;
    char::from_u32(0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00))
}

fn hex_value(
    characters: &mut std::iter::Peekable<std::str::Chars<'_>>,
    width: usize,
) -> Option<u32> {
    let mut digits = String::with_capacity(width);
    for _ in 0..width {
        digits.push(characters.next()?);
    }
    u32::from_str_radix(&digits, 16).ok()
}

fn hex_scalar(
    characters: &mut std::iter::Peekable<std::str::Chars<'_>>,
    width: usize,
) -> Option<char> {
    char::from_u32(hex_value(characters, width)?)
}

pub fn html_fragment_to_text(fragment: &str) -> String {
    let mut text = String::with_capacity(fragment.len());
    let mut rest = fragment;

    while let Some(open) = rest.find('<') {
        push_text(&mut text, &rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            push_text(&mut text, &rest[open..]);
            return finish(&text);
        };
        let tag = &rest[open + 1..open + close];
        if tag
            .trim_start_matches('/')
            .trim_end_matches('/')
            .trim()
            .eq_ignore_ascii_case("br")
        {
            text.push('\n');
        }
        rest = &rest[open + close + 1..];
    }
    push_text(&mut text, rest);

    finish(&text)
}

fn push_text(text: &mut String, run: &str) {
    for character in decode_entities(run).chars() {
        if character.is_whitespace() {
            if !matches!(text.chars().last(), None | Some(' ') | Some('\n')) {
                text.push(' ');
            }
        } else {
            text.push(character);
        }
    }
}

fn finish(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let start = lines.iter().position(|line| !line.is_empty());
    let Some(start) = start else {
        return String::new();
    };
    let end = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .unwrap_or(start);
    lines[start..=end].join("\n")
}

fn decode_entities(raw: &str) -> String {
    let mut decoded = String::with_capacity(raw.len());
    let mut rest = raw;

    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find(';').filter(|end| *end <= 12) {
            Some(end) => {
                let entity = &after[1..end];
                match resolve_entity(entity) {
                    Some(resolved) => decoded.push_str(&resolved),
                    None => decoded.push_str(&after[..=end]),
                }
                rest = &after[end + 1..];
            }
            None => {
                decoded.push('&');
                rest = &after[1..];
            }
        }
    }
    decoded.push_str(rest);

    decoded
}

fn resolve_entity(entity: &str) -> Option<String> {
    if let Some(digits) = entity.strip_prefix("#x").or(entity.strip_prefix("#X")) {
        return char::from_u32(u32::from_str_radix(digits, 16).ok()?).map(String::from);
    }
    if let Some(digits) = entity.strip_prefix('#') {
        return char::from_u32(digits.parse().ok()?).map(String::from);
    }
    let resolved = match entity {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        _ => return None,
    };
    Some(resolved.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUNDLE: &str = include_str!("../../tests/fixtures/client.js");
    const SHELL: &str = include_str!("../../tests/fixtures/shell.html");
    const ROOT: &str = include_str!("../../tests/fixtures/root.html");
    const FEED: &str = include_str!("../../tests/fixtures/rss.xml");

    fn feed_episodes() -> Vec<Episode> {
        super::super::feed::parse(FEED).unwrap()
    }

    fn head(length: usize) -> &'static str {
        let mut length = length.min(BUNDLE.len());
        while !BUNDLE.is_char_boundary(length) {
            length -= 1;
        }
        &BUNDLE[..length]
    }

    #[test]
    fn the_bundle_path_is_discovered_from_the_checked_in_shell() {
        assert_eq!(
            find_bundle_url(SHELL).unwrap(),
            "/client/client.e1462480.js"
        );
    }

    #[test]
    fn a_different_hash_is_followed_without_a_code_change() {
        let shell = SHELL.replace("client.e1462480.js", "client.abc123.js");
        assert_eq!(find_bundle_url(&shell).unwrap(), "/client/client.abc123.js");
    }

    #[test]
    fn a_shell_without_a_bundle_reference_yields_nothing() {
        assert!(find_bundle_url("<html><body>nothing here</body></html>").is_none());
        assert!(find_bundle_url("client/client..js").is_none());
        assert!(find_bundle_url("client/client.abc.css").is_none());
        assert!(find_bundle_url("").is_none());
    }

    #[test]
    fn the_site_root_redirect_is_followed_to_the_real_shell() {
        assert!(find_bundle_url(ROOT).is_none());
        assert_eq!(find_redirect_target(ROOT).unwrap(), "/latest");
        assert_eq!(
            absolute("/latest"),
            "https://musicforprogramming.net/latest"
        );
        assert_eq!(
            absolute("client/client.a.js"),
            "https://musicforprogramming.net/client/client.a.js"
        );
    }

    #[test]
    fn a_document_with_no_redirect_yields_no_target() {
        assert!(find_redirect_target("<html></html>").is_none());
        assert!(find_redirect_target("<script>window.location.href = \"\"</script>").is_none());
    }

    #[test]
    fn the_checked_in_bundle_yields_every_episode_plus_the_information_pages() {
        let records = extract_records(BUNDLE);

        let episodes: Vec<&Record> = records.iter().filter(|r| r.kind == "episode").collect();
        let info: Vec<&str> = records
            .iter()
            .filter(|r| r.kind != "episode")
            .map(|r| r.slug.as_str())
            .collect();

        assert_eq!(episodes.len(), 79);
        assert_eq!(info, ["about", "credits"]);
        assert_eq!(records.len(), 81);
    }

    #[test]
    fn a_record_carrying_an_unexpected_field_still_parses() {
        let records = extract_records(BUNDLE);
        let special = records
            .iter()
            .find(|record| record.slug == "sixtytwo")
            .expect("episode 62 is missing");

        assert_eq!(special.order, Some(62));
        assert_eq!(
            special.file,
            "https://datashat.net/music_for_programming_62-our_grey_lives.mp3"
        );
        assert!(special.tracklist.is_some());
    }

    #[test]
    fn every_bundle_record_decodes_its_join_key_and_order() {
        for record in extract_records(BUNDLE) {
            assert!(!record.slug.is_empty());
            if record.kind == "episode" {
                assert!(record.file.starts_with("https://"), "{record:?}");
                assert!(record.order.is_some(), "{record:?}");
                assert!(record.tracklist.is_some(), "{record:?}");
                assert!(!record.file.contains('\\'), "{record:?}");
            }
        }
    }

    #[test]
    fn every_feed_episode_joins_to_a_bundle_record() {
        let mut episodes = feed_episodes();
        let records = extract_records(BUNDLE);

        let enriched = apply(records, &mut episodes);

        assert_eq!(enriched, 79);
        assert_eq!(episodes.len(), 79);
        assert_eq!(episodes[78].slug.as_deref(), Some("one"));
        assert_eq!(episodes[78].order, Some(1));
        assert!(
            episodes[78]
                .tracklist
                .as_deref()
                .unwrap()
                .starts_with("Frog Pocket - Plinty\nTor Lundvall - Crooked")
        );
        assert_eq!(
            episodes[78].links.as_deref(),
            Some("http://datassette.net/")
        );

        assert!(episodes[78].body.is_none());
    }

    #[test]
    fn non_episode_and_unmatched_records_are_discarded() {
        let mut episodes = feed_episodes();
        let mut records = extract_records(BUNDLE);
        records.push(Record {
            slug: "ghost".into(),
            kind: "episode".into(),
            order: Some(900),
            title: Some("900: Ghost".into()),
            special: false,
            file: "https://datashat.net/not_in_the_feed.mp3".into(),
            tracklist: Some("nothing".into()),
            body: None,
            links: None,
        });

        apply(records, &mut episodes);

        assert_eq!(episodes.len(), 79);
        assert!(
            episodes
                .iter()
                .all(|episode| episode.slug.as_deref() != Some("ghost"))
        );
        assert!(
            episodes
                .iter()
                .all(|episode| episode.slug.as_deref() != Some("about"))
        );
        assert!(
            episodes
                .iter()
                .all(|episode| episode.slug.as_deref() != Some("credits"))
        );
    }

    #[test]
    fn an_html_fragment_becomes_plain_text() {
        assert_eq!(html_fragment_to_text("A &amp; B<br><i>C</i>"), "A & B\nC");
    }

    #[test]
    fn the_fragments_own_newlines_and_indentation_are_not_line_breaks() {
        let fragment = "\n\t\t\tFrog Pocket - Plinty<br>\n\t\t\tTor Lundvall - Crooked<br>\n\t\t";
        assert_eq!(
            html_fragment_to_text(fragment),
            "Frog Pocket - Plinty\nTor Lundvall - Crooked"
        );
    }

    #[test]
    fn a_paragraph_break_survives_conversion() {
        assert_eq!(html_fragment_to_text("A<br><br>B"), "A\n\nB");
    }

    #[test]
    fn every_variant_of_a_break_tag_is_a_line_break() {
        assert_eq!(
            html_fragment_to_text("A<br>B<br/>C<br />D<BR>E"),
            "A\nB\nC\nD\nE"
        );
    }

    #[test]
    fn entities_and_character_references_are_decoded() {
        assert_eq!(
            html_fragment_to_text("&amp;&lt;&gt;&quot;&#39;&#x41;&nbsp;B&unknown;"),
            "&<>\"'A B&unknown;"
        );
    }

    #[test]
    fn an_unterminated_tag_is_treated_as_text_rather_than_swallowing_the_rest() {
        assert_eq!(html_fragment_to_text("A<br>B <3 C"), "A\nB <3 C");
    }

    #[test]
    fn an_empty_or_markup_only_fragment_reads_as_absent() {
        assert_eq!(as_text(Some(String::new())), None);
        assert_eq!(as_text(Some("<br><br>".into())), None);
        assert_eq!(as_text(None), None);
        assert_eq!(as_text(Some("  x  ".into())), Some("x".into()));
    }

    #[test]
    fn js_escapes_are_decoded() {
        assert_eq!(decode_js_string(r"a\nb\tc"), "a\nb\tc");
        assert_eq!(
            decode_js_string(r#"Cecilia\'s \"Fruit\""#),
            "Cecilia's \"Fruit\""
        );
        assert_eq!(decode_js_string(r"back\\slash"), r"back\slash");
        assert_eq!(decode_js_string(r"A\u{1F600}\x41"), "A\u{1f600}A");
        assert_eq!(decode_js_string(r"😀"), "\u{1f600}");
        assert_eq!(decode_js_string(r"trailing\"), "trailing");
    }

    #[test]
    fn a_truncated_bundle_yields_the_records_it_can_and_never_panics() {
        for length in [0, 1, 200, 5_000, 60_000, 120_000, BUNDLE.len() - 1] {
            let records = extract_records(head(length));
            assert!(records.len() <= 81);
        }
    }

    #[test]
    fn a_single_malformed_record_is_skipped_and_the_rest_still_parse() {
        let bundle = concat!(
            r#"const x=[{slug:"a",type:"episode",order:1,file:"https://a.mp3",tracklist:"A<br>"},"#,
            r#"{slug:"broken",type:"episode",order:2,file:"https://b.mp3",tracklist:"unterminated,"#,
            r#"{slug:"c",type:"episode",order:3,file:"https://c.mp3",tracklist:"C<br>"}];"#
        );

        let records = extract_records(bundle);

        let slugs: Vec<&str> = records.iter().map(|r| r.slug.as_str()).collect();
        assert_eq!(slugs, ["a", "c"]);
    }

    #[test]
    fn a_record_whose_slug_could_name_a_file_outside_the_cache_is_skipped() {
        for slug in [
            r"../../../etc/authorized_keys",
            "/etc/passwd",
            "one/two",
            "one two",
            "one.mp3",
            "",
        ] {
            let bundle = format!(
                r#"[{{slug:"{slug}",type:"episode",file:"https://datashat.net/one.mp3"}}]"#
            );
            assert!(
                extract_records(&bundle).is_empty(),
                "{slug} was accepted as a slug"
            );
        }

        for slug in ["one", "seventynine", "a-b_C9"] {
            let bundle = format!(
                r#"[{{slug:"{slug}",type:"episode",file:"https://datashat.net/one.mp3"}}]"#
            );
            let records = extract_records(&bundle);
            assert_eq!(records.len(), 1, "{slug} was refused");
            assert_eq!(records[0].slug, slug);
        }
    }

    #[test]
    fn an_episode_whose_slug_is_refused_is_left_un_enriched() {
        let mut episodes = feed_episodes();
        let url = episodes[0].enclosure_url.clone();
        let bundle = format!(r#"[{{slug:"../escape",type:"episode",file:"{url}"}}]"#);

        let enrichment = enrich_from(&bundle, &mut episodes);

        assert!(!enrichment.applied);
        assert!(episodes[0].slug.is_none());
        assert!(episodes[0].id().starts_with("ep-"));
    }

    #[test]
    fn a_record_missing_a_required_field_is_skipped() {
        let bundle = concat!(
            r#"[{slug:"a",order:1,file:"https://a.mp3"},"#,
            r#"{slug:"",type:"episode",file:"https://b.mp3"},"#,
            r#"{slug:"c",type:"episode",file:"https://c.mp3"}]"#
        );

        let records = extract_records(bundle);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].slug, "c");
    }

    #[test]
    fn a_bundle_of_the_wrong_shape_yields_nothing_rather_than_failing() {
        for input in [
            "",
            "function t(){}",
            "<!doctype html><html><body>shell</body></html>",
            "{slug:",
            "{slug:{{{{",
            "[}]}]}",
            "{slug:\"a\"",
        ] {
            assert!(extract_records(input).is_empty(), "{input}");
        }
    }

    #[test]
    fn enriching_from_nothing_leaves_every_episode_untouched() {
        let mut episodes = feed_episodes();

        assert_eq!(apply(Vec::new(), &mut episodes), 0);

        assert_eq!(episodes.len(), 79);
        assert!(episodes.iter().all(|episode| episode.slug.is_none()));
        assert!(episodes.iter().all(|episode| episode.tracklist.is_none()));
    }

    #[test]
    fn the_checked_in_bundle_enriches_every_episode() {
        let mut episodes = feed_episodes();
        assert!(enrich_from(BUNDLE, &mut episodes).applied);
        assert!(episodes.iter().all(|episode| episode.slug.is_some()));
    }

    #[test]
    fn a_bundle_that_cannot_be_parsed_leaves_a_usable_un_enriched_catalog() {
        for bundle in ["", "function t(){}", SHELL, head(5_000)] {
            let mut episodes = feed_episodes();

            let enrichment = enrich_from(bundle, &mut episodes);

            assert_eq!(episodes.len(), 79);
            if !enrichment.applied {
                assert!(episodes.iter().all(|episode| episode.slug.is_none()));
                assert!(enrichment.info.is_empty());
            }
        }
    }

    const MALFORMED: &str = concat!(
        r#"[{slug:"a",type:"episode",order:1,file:"https://a.mp3",tracklist:"unterminated,"#,
        r#"{slug:"about",type:"info",order:0,title:"About""#
    );

    const EPISODES_ONLY: &str = r#"[{slug:"a",type:"episode",order:1,title:"1: A",file:"https://a.mp3",tracklist:"A<br>"}]"#;

    #[test]
    fn a_truncated_or_malformed_bundle_never_fails_and_yields_only_whole_pages() {
        let mut bundles: Vec<String> =
            vec![String::new(), "function t(){}".into(), MALFORMED.into()];
        bundles.extend(
            [0, 1, 200, 5_000, 60_000, BUNDLE.len() - 1].map(|length| head(length).to_owned()),
        );

        for bundle in bundles {
            let mut episodes = feed_episodes();

            let enrichment = enrich_from(&bundle, &mut episodes);

            assert_eq!(episodes.len(), 79);

            assert!(enrichment.info.iter().all(|page| !page.slug.is_empty()
                && !page.title.is_empty()
                && !page.body.is_empty()));
        }
    }

    #[test]
    fn a_bundle_carrying_no_information_record_reports_no_pages() {
        for bundle in [
            "",
            "function t(){}",
            SHELL,
            MALFORMED,
            EPISODES_ONLY,
            head(200),
        ] {
            let mut episodes = feed_episodes();

            let enrichment = enrich_from(bundle, &mut episodes);

            assert!(enrichment.info.is_empty(), "{bundle}");
            assert_eq!(episodes.len(), 79);
        }
    }

    #[test]
    fn both_information_pages_are_retained_with_their_title_and_body() {
        let mut episodes = feed_episodes();

        let enrichment = enrich_from(BUNDLE, &mut episodes);

        let slugs: Vec<&str> = enrichment
            .info
            .iter()
            .map(|page| page.slug.as_str())
            .collect();
        assert_eq!(slugs, ["about", "credits"]);
        assert_eq!(enrichment.info[0].title, "About");
        assert_eq!(enrichment.info[1].title, "Credits");
        assert!(enrichment.info.iter().all(|page| !page.body.is_empty()));

        assert!(enrichment.info.iter().all(|page| !page.body.contains('<')));

        assert_eq!(episodes.len(), 79);
        assert!(
            episodes
                .iter()
                .all(|episode| !matches!(episode.slug.as_deref(), Some("about") | Some("credits")))
        );
    }

    #[test]
    fn a_record_without_a_title_or_a_body_is_not_an_information_page() {
        let records = [
            Record {
                slug: "titleless".into(),
                kind: "info".into(),
                order: Some(0),
                title: None,
                special: false,
                file: String::new(),
                tracklist: None,
                body: Some("body".into()),
                links: None,
            },
            Record {
                slug: "bodyless".into(),
                kind: "info".into(),
                order: Some(0),
                title: Some("Bodyless".into()),
                special: false,
                file: String::new(),
                tracklist: None,
                body: None,
                links: None,
            },
        ];

        assert!(info_pages(&records).is_empty());
    }

    #[test]
    fn exactly_one_episode_carries_the_special_flag_and_it_is_order_62() {
        let mut episodes = feed_episodes();

        enrich_from(BUNDLE, &mut episodes);

        let flagged: Vec<&Episode> = episodes.iter().filter(|episode| episode.special).collect();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].order, Some(62));
        assert_eq!(flagged[0].slug.as_deref(), Some("sixtytwo"));
    }

    #[test]
    fn the_flag_is_unset_for_an_unflagged_record_and_without_enrichment() {
        let records = extract_records(BUNDLE);
        assert_eq!(
            records.iter().filter(|record| record.special).count(),
            1,
            "the bundle marks exactly one record"
        );

        let mut episodes = feed_episodes();
        assert!(episodes.iter().all(|episode| !episode.special));

        enrich_from(BUNDLE, &mut episodes);
        assert_eq!(episodes.iter().filter(|episode| episode.special).count(), 1);
    }

    #[test]
    fn the_flagged_episode_is_otherwise_an_ordinary_episode() {
        let mut episodes = feed_episodes();
        enrich_from(BUNDLE, &mut episodes);

        let position = episodes
            .iter()
            .position(|episode| episode.special)
            .expect("no flagged episode");
        let flagged = &episodes[position];

        assert_eq!(flagged.id(), "sixtytwo");
        assert!(flagged.enclosure_url.starts_with("https://"));
        assert!(flagged.duration_secs > 0);
        assert!(flagged.byte_len > 0);
        assert_eq!(episodes[position - 1].order, Some(63));
        assert_eq!(episodes[position + 1].order, Some(61));
    }

    #[tokio::test]
    async fn an_unreachable_site_leaves_a_usable_catalog_and_raises_no_error() {
        let client = reqwest::Client::builder()
            .resolve(
                "musicforprogramming.net",
                std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            )
            .build()
            .unwrap();
        let mut episodes = feed_episodes();

        let enrichment = enrich(&client, &mut episodes).await;

        assert_eq!(enrichment, Enrichment::default());
        assert_eq!(episodes.len(), 79);
        assert!(episodes.iter().all(|episode| episode.slug.is_none()));
        assert!(episodes.iter().all(|episode| episode.tracklist.is_none()));
        assert!(episodes.iter().all(|episode| !episode.special));
    }

    #[test]
    fn a_two_hundred_carrying_the_shell_is_not_evidence_the_bundle_exists() {
        assert!(!is_script(SHELL));
        assert!(!is_script(ROOT));
        assert!(!is_script("  \n<!doctype html>"));
        assert!(!is_script(""));
        assert!(is_script(BUNDLE));
        assert!(is_script("function t(){}"));
    }
}
