//! Retrieval and parsing of the RSS feed, which is the sole authority on which episodes
//! exist.
//!
//! Every episode corresponds to an `<item>` carrying an `<enclosure>` of type
//! `audio/mpeg`, and the enclosure URL is the episode's identity. A retrieval failure and
//! a parse failure are distinguishable, and neither yields a partial list.

use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::error::{Error, Result};
use crate::model::Episode;

/// Fails rather than returning a partial list when the feed is unreachable, returns a
/// non-success status, or cannot be parsed as RSS.
pub async fn fetch(client: &reqwest::Client) -> Result<Vec<Episode>> {
    let response = super::request(client, super::FEED_URL)
        .send()
        .await
        .map_err(|error| unreachable(&error.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(unreachable(&format!("It returned HTTP {status}")));
    }

    let body = response
        .text()
        .await
        .map_err(|error| unreachable(&error.to_string()))?;

    parse(&body)
}

fn unreachable(reason: &str) -> Error {
    Error::CatalogUnavailable(format!(
        "The feed at {} is unreachable: {reason}",
        super::FEED_URL
    ))
}

fn malformed(reason: &str) -> Error {
    Error::Internal(format!(
        "The feed at {} is not a valid RSS document: {reason}",
        super::FEED_URL
    ))
}

/// Parses an RSS document into episodes, keeping only items with an `audio/mpeg`
/// enclosure and preserving feed order.
pub fn parse(xml: &str) -> Result<Vec<Episode>> {
    // text is not trimmed by the reader: an entity arrives as its own event, so trimming
    // each run would silently eat the spaces around it
    let mut reader = Reader::from_str(xml);

    let mut episodes = Vec::new();
    let mut saw_rss = false;
    let mut item: Option<PartialItem> = None;
    let mut text = String::new();
    let mut path: Vec<String> = Vec::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|error| malformed(&error.to_string()))?;

        match event {
            Event::Eof => break,
            Event::Start(start) => {
                let name = local_name(start.name().as_ref()).to_owned();
                if name == "rss" || name == "channel" {
                    saw_rss = true;
                }
                if name == "item" {
                    item = Some(PartialItem::default());
                }
                path.push(name);
                text.clear();
            }
            Event::Empty(empty) => {
                // enclosure is the only empty tag in the feed carrying data we need
                if local_name(empty.name().as_ref()) == "enclosure"
                    && let Some(partial) = item.as_mut()
                {
                    let mut url = None;
                    let mut length = None;
                    let mut mime = None;
                    for attribute in empty.attributes().flatten() {
                        // `value` is the raw attribute text; RSS writes `&` in an
                        // enclosure URL as `&amp;`, and that URL is both the download
                        // target and the episode's identity
                        let Ok(value) = attribute.normalized_value(XmlVersion::Implicit1_0) else {
                            continue;
                        };
                        match local_name(attribute.key.as_ref()) {
                            "url" => url = Some(value.into_owned()),
                            "length" => length = value.parse::<u64>().ok(),
                            "type" => mime = Some(value.into_owned()),
                            _ => {}
                        }
                    }
                    if mime.as_deref() == Some("audio/mpeg")
                        && let Some(url) = url
                    {
                        partial.enclosure_url = Some(url);
                        partial.byte_len = length.unwrap_or(0);
                    }
                }
            }
            Event::Text(chunk) => text.push_str(&chunk.xml10_content()),
            Event::CData(chunk) => text.push_str(&chunk.xml10_content()),
            Event::GeneralRef(reference) => {
                if let Some(character) = reference
                    .resolve_char_ref()
                    .map_err(|error| malformed(&error.to_string()))?
                {
                    text.push(character);
                } else if let Some(resolved) = resolve_predefined_entity(&reference) {
                    text.push_str(resolved);
                }
            }
            Event::End(end) => {
                let name = local_name(end.name().as_ref()).to_owned();
                path.pop();
                // only the direct children of an <item> carry episode data, so text from
                // the channel-level elements of the same name is ignored
                let in_item = path.last().map(String::as_str) == Some("item");
                if in_item && let Some(partial) = item.as_mut() {
                    match name.as_str() {
                        "title" => partial.title = Some(text.trim().to_owned()),
                        "link" => partial.link = Some(text.trim().to_owned()),
                        "duration" => partial.duration_secs = parse_duration(&text),
                        "pubDate" => partial.published_at = parse_pub_date(&text),
                        _ => {}
                    }
                }
                if name == "item"
                    && let Some(episode) = item.take().and_then(PartialItem::into_episode)
                {
                    episodes.push(episode);
                }
                text.clear();
            }
            _ => {}
        }
    }

    if !saw_rss {
        return Err(malformed("It has no <rss> or <channel> element"));
    }
    // an error page rendered as RSS parses, and a catalog built from it would be cached
    // over the good one for the whole of the cache's lifetime
    if episodes.is_empty() {
        return Err(malformed("It lists no episode with an audio enclosure"));
    }

    Ok(episodes)
}

/// Strips any namespace prefix, so `itunes:duration` reads as `duration`.
fn local_name(raw: &str) -> &str {
    match raw.rsplit_once(':') {
        Some((_, local)) => local,
        None => raw,
    }
}

#[derive(Debug, Default, Clone)]
struct PartialItem {
    title: Option<String>,
    link: Option<String>,
    enclosure_url: Option<String>,
    byte_len: u64,
    duration_secs: Option<u64>,
    published_at: Option<i64>,
}

impl PartialItem {
    /// An item without an `audio/mpeg` enclosure is not an episode and is dropped; every
    /// other field falls back to a neutral value rather than dropping an episode the feed
    /// does list.
    fn into_episode(self) -> Option<Episode> {
        Some(Episode {
            title: self.title.unwrap_or_default(),
            link: self.link.unwrap_or_default(),
            enclosure_url: self.enclosure_url?,
            byte_len: self.byte_len,
            duration_secs: self.duration_secs.unwrap_or(0),
            published_at: self.published_at.unwrap_or(0),
            slug: None,
            bundle_title: None,
            order: None,
            tracklist: None,
            body: None,
            links: None,
            special: false,
        })
    }
}

/// Parses `itunes:duration`, which the feed writes as `H:MM:SS` but which the format also
/// permits as `MM:SS` or as whole seconds.
fn parse_duration(raw: &str) -> Option<u64> {
    let mut total = 0u64;
    for part in raw.trim().split(':') {
        total = total
            .checked_mul(60)?
            .checked_add(part.trim().parse().ok()?)?;
    }
    Some(total)
}

/// The years a `pubDate` may name.
///
/// Anything outside overflows the arithmetic below, which wraps in a release build and
/// panics in a debug one, and no real feed carries it.
const PUB_DATE_YEARS: std::ops::RangeInclusive<i64> = 1..=9999;

/// Parses an RFC 2822 `pubDate` into whole seconds since the Unix epoch.
fn parse_pub_date(raw: &str) -> Option<i64> {
    let rest = match raw.trim().split_once(',') {
        Some((_weekday, rest)) => rest,
        None => raw.trim(),
    };
    let mut fields = rest.split_whitespace();

    let day: i64 = fields.next()?.parse().ok()?;
    let month = month_number(fields.next()?)?;
    let year: i64 = fields.next()?.parse().ok()?;
    if !PUB_DATE_YEARS.contains(&year) || !(1..=31).contains(&day) {
        return None;
    }

    let mut clock = fields.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next().unwrap_or("0").parse().ok()?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }

    let offset = fields.next().and_then(zone_offset_secs).unwrap_or(0);

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

fn month_number(name: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| name.eq_ignore_ascii_case(month))
        .map(|index| index as i64 + 1)
}

/// Seconds east of UTC for a `+HHMM`, `-HHMM`, or named zone.
///
/// RFC 5322 section 4.3 gives the North American zone names numeric offsets; everything
/// else it leaves unknown, which reads as UTC.
fn zone_offset_secs(zone: &str) -> Option<i64> {
    let (sign, digits) = match zone.split_at_checked(1)? {
        ("+", digits) => (1, digits),
        ("-", digits) => (-1, digits),
        _ => return Some(named_zone_offset_secs(zone)),
    };
    if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(0);
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3_600 + minutes * 60))
}

/// Days between 1970-01-01 and the given civil date, by Howard Hinnant's algorithm.
/// Seconds east of UTC for one of the zone names RFC 5322 section 4.3 defines, and zero
/// for anything else.
fn named_zone_offset_secs(zone: &str) -> i64 {
    let hours = match zone.to_ascii_uppercase().as_str() {
        "EDT" => -4,
        "EST" | "CDT" => -5,
        "CST" | "MDT" => -6,
        "MST" | "PDT" => -7,
        "PST" => -8,
        _ => 0,
    };
    hours * 3_600
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = include_str!("../../tests/fixtures/rss.xml");

    #[test]
    fn the_checked_in_feed_parses_into_every_episode() {
        let episodes = parse(FEED).unwrap();
        assert_eq!(episodes.len(), 79);
    }

    #[test]
    fn every_episode_carries_its_guaranteed_fields() {
        for episode in parse(FEED).unwrap() {
            assert!(!episode.title.is_empty(), "{episode:?}");
            assert!(!episode.link.is_empty(), "{episode:?}");
            assert!(
                episode.enclosure_url.ends_with(".mp3"),
                "{}",
                episode.enclosure_url
            );
            assert!(episode.byte_len > 0, "{episode:?}");
            assert!(episode.duration_secs > 0, "{episode:?}");
            assert!(episode.published_at > 0, "{episode:?}");
        }
    }

    #[test]
    fn every_episode_carries_only_feed_fields_before_enrichment() {
        for episode in parse(FEED).unwrap() {
            assert!(episode.slug.is_none());
            assert!(episode.order.is_none());
            assert!(episode.tracklist.is_none());
            assert!(episode.body.is_none());
            assert!(episode.links.is_none());
        }
    }

    #[test]
    fn enclosure_urls_are_unique_so_they_can_identify_an_episode() {
        let episodes = parse(FEED).unwrap();
        let mut urls: Vec<&str> = episodes
            .iter()
            .map(|episode| episode.enclosure_url.as_str())
            .collect();
        urls.sort_unstable();
        urls.dedup();
        assert_eq!(urls.len(), episodes.len());
    }

    #[test]
    fn feed_order_is_preserved() {
        let episodes = parse(FEED).unwrap();
        assert_eq!(episodes[0].title, "Episode 79: Corticyte");
        assert_eq!(episodes[78].title, "Episode 01: Datassette");
        assert_eq!(
            episodes[0].enclosure_url,
            "https://datashat.net/music_for_programming_79-corticyte.mp3"
        );
        assert_eq!(episodes[0].byte_len, 441_077_163);
        assert_eq!(episodes[0].duration_secs, 4 * 3_600);
    }

    #[test]
    fn a_body_that_is_not_rss_is_a_parse_failure() {
        let error = parse("<html><body>not a feed</body></html>").unwrap_err();
        assert!(error.to_string().contains("not a valid RSS document"));
    }

    #[test]
    fn a_body_that_is_not_xml_is_a_parse_failure() {
        assert!(parse("<rss><channel><item></rss>").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn a_parse_failure_is_distinguishable_from_a_network_failure() {
        let parse_failure = parse("<html></html>").unwrap_err();
        let network_failure = unreachable("connection refused");
        assert_ne!(parse_failure.code(), network_failure.code());
        assert_eq!(
            network_failure.code(),
            crate::error::ErrorCode::CatalogUnavailable
        );
    }

    #[test]
    fn a_non_success_status_names_the_feed_as_unreachable() {
        let error = unreachable("it returned HTTP 503 Service Unavailable");

        assert_eq!(error.code(), crate::error::ErrorCode::CatalogUnavailable);
        assert!(error.to_string().contains(super::super::FEED_URL));
        assert!(error.to_string().contains("unreachable"));
        assert!(error.to_string().contains("503"));
    }

    #[test]
    fn items_without_an_audio_enclosure_are_not_episodes() {
        let xml = r#"<rss><channel>
            <item><title>No audio</title><link>a</link>
                <enclosure url="a.jpg" length="1" type="image/jpeg"/></item>
            <item><title>No enclosure at all</title><link>b</link></item>
            <item><title>Audio</title><link>c</link>
                <pubDate>Tue, 22 Feb 2011 17:17:58 GMT</pubDate>
                <itunes:duration>1:02:16</itunes:duration>
                <enclosure url="c.mp3" length="7" type="audio/mpeg"/></item>
        </channel></rss>"#;

        let episodes = parse(xml).unwrap();

        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].enclosure_url, "c.mp3");
        assert_eq!(episodes[0].byte_len, 7);
        assert_eq!(episodes[0].duration_secs, 3_736);
    }

    #[test]
    fn channel_level_elements_do_not_leak_into_an_episode() {
        let xml = r#"<rss><channel>
            <title>Music For Programming</title>
            <link>https://musicforprogramming.net/</link>
            <itunes:duration>9:99:99</itunes:duration>
            <item><title>Episode 01</title><link>ep</link>
                <enclosure url="c.mp3" length="7" type="audio/mpeg"/></item>
        </channel></rss>"#;

        let episodes = parse(xml).unwrap();

        assert_eq!(episodes[0].title, "Episode 01");
        assert_eq!(episodes[0].link, "ep");
        assert_eq!(episodes[0].duration_secs, 0);
    }

    #[test]
    fn entities_in_a_title_are_decoded() {
        let xml = r#"<rss><channel><item>
            <title>A &amp; B &#66;</title><link>x</link>
            <enclosure url="c.mp3" length="1" type="audio/mpeg"/>
        </item></channel></rss>"#;

        assert_eq!(parse(xml).unwrap()[0].title, "A & B B");
    }

    /// A five-digit year overflowed the day arithmetic, which panics a debug build and
    /// wraps a release one into a date that sorts as nonsense.
    #[test]
    fn a_date_too_far_out_to_compute_is_refused_rather_than_overflowing() {
        assert_eq!(
            parse_pub_date("Mon, 01 Jan 999999999999999 00:00:00 GMT"),
            None
        );
        assert_eq!(
            parse_pub_date("Mon, 01 Jan -999999999999 00:00:00 GMT"),
            None
        );
        assert_eq!(parse_pub_date("Mon, 99 Jan 2024 00:00:00 GMT"), None);
        assert_eq!(parse_pub_date("Mon, 01 Jan 2024 99:00:00 GMT"), None);
    }

    /// The item is dropped rather than the whole feed: the enclosure is what makes an
    /// episode, and a date that will not parse falls back to the epoch.
    #[test]
    fn an_item_whose_date_cannot_be_computed_still_yields_an_episode() {
        let xml = r#"<rss><channel><item>
            <title>Episode 01</title><link>ep</link>
            <pubDate>Mon, 01 Jan 999999999999999 00:00:00 GMT</pubDate>
            <enclosure url="c.mp3" length="7" type="audio/mpeg"/>
        </item></channel></rss>"#;

        let episodes = parse(xml).unwrap();

        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].published_at, 0);
    }

    #[test]
    fn a_feed_listing_no_episode_is_a_parse_failure() {
        let error = parse("<rss><channel><title>Music For Programming</title></channel></rss>")
            .unwrap_err();

        assert_eq!(error.code(), crate::error::ErrorCode::Internal);
        assert!(error.to_string().contains("lists no episode"));
    }

    /// RSS writes `&` in a URL as `&amp;`, and the enclosure URL is both the download
    /// target and the episode's identity.
    #[test]
    fn an_escaped_enclosure_url_is_decoded() {
        let xml = r#"<rss><channel><item>
            <title>Episode 01</title><link>ep</link>
            <enclosure url="https://datashat.net/one.mp3?a=1&amp;b=2" length="7" type="audio/mpeg"/>
        </item></channel></rss>"#;

        assert_eq!(
            parse(xml).unwrap()[0].enclosure_url,
            "https://datashat.net/one.mp3?a=1&b=2"
        );
    }

    #[test]
    fn the_zone_names_rfc_5322_defines_are_not_read_as_utc() {
        assert_eq!(
            parse_pub_date("Tue, 22 Feb 2011 12:17:58 EST"),
            parse_pub_date("Tue, 22 Feb 2011 17:17:58 GMT")
        );
        assert_eq!(
            parse_pub_date("Tue, 22 Feb 2011 09:17:58 PST"),
            parse_pub_date("Tue, 22 Feb 2011 17:17:58 GMT")
        );
        // an unknown name stays UTC, which is what an unknown zone means
        assert_eq!(
            parse_pub_date("Tue, 22 Feb 2011 17:17:58 XYZ"),
            parse_pub_date("Tue, 22 Feb 2011 17:17:58 GMT")
        );
    }

    #[test]
    fn durations_parse_in_every_permitted_shape() {
        assert_eq!(parse_duration("4:00:00"), Some(14_400));
        assert_eq!(parse_duration("51:15"), Some(3_075));
        assert_eq!(parse_duration("90"), Some(90));
        assert_eq!(parse_duration("not a duration"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn pub_dates_parse_to_the_unix_epoch() {
        assert_eq!(parse_pub_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_pub_date("Tue, 22 Feb 2011 17:17:58 GMT"),
            Some(1_298_395_078)
        );
        assert_eq!(
            parse_pub_date("Mon, 24 Aug 2026 17:18:00 GMT"),
            Some(1_787_591_880)
        );
        assert_eq!(
            parse_pub_date("24 Aug 2026 17:18:00 +0000"),
            parse_pub_date("Mon, 24 Aug 2026 12:18:00 -0500")
        );
        assert_eq!(parse_pub_date("nonsense"), None);
    }
}
