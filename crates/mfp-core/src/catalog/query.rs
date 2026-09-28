use crate::model::{Catalog, Episode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Published,
    Title,
    Duration,

    Order,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

/// Case-insensitive substring of the title or of any enriched text. An empty query
/// matches everything.
pub fn matches(episode: &Episode, query: &str) -> bool {
    matches_needle(episode, &needle(query))
}

fn needle(query: &str) -> String {
    query.trim().to_lowercase()
}

fn matches_needle(episode: &Episode, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }

    let optional = [&episode.slug, &episode.body, &episode.tracklist];
    contains(&episode.title, needle)
        || optional
            .into_iter()
            .flatten()
            .any(|field| contains(field, needle))
}

/// Folds one character at a time rather than lowercasing a copy: this runs over every
/// track listing on every keystroke, where the copies were the bulk of the cost.
fn contains(haystack: &str, lowercase_needle: &str) -> bool {
    haystack.char_indices().any(|(offset, _)| {
        let mut folded = haystack[offset..].chars().flat_map(char::to_lowercase);
        let mut wanted = lowercase_needle.chars();
        loop {
            match (wanted.next(), folded.next()) {
                (None, _) => return true,
                (Some(_), None) => return false,
                (Some(left), Some(right)) if left != right => return false,
                _ => {}
            }
        }
    })
}

pub fn search<'a>(catalog: &'a Catalog, query: &str) -> Vec<&'a Episode> {
    let needle = needle(query);
    catalog
        .episodes
        .iter()
        .filter(|episode| matches_needle(episode, &needle))
        .collect()
}

/// The predicate belongs to the caller: only the daemon knows what the download cache holds.
pub fn filter<'a>(episodes: &[&'a Episode], keep: impl Fn(&Episode) -> bool) -> Vec<&'a Episode> {
    episodes
        .iter()
        .copied()
        .filter(|episode| keep(episode))
        .collect()
}

/// Total and stable: equal episodes keep their relative order, and one missing the sort
/// key is ordered deterministically rather than dropped.
pub fn sort(episodes: &mut [&Episode], key: SortKey, direction: SortDirection) {
    match key {
        SortKey::Published => sort_by_key(episodes, direction, |episode| episode.published_at),
        SortKey::Title => sort_by_key(episodes, direction, |episode| episode.title.to_lowercase()),
        SortKey::Duration => sort_by_key(episodes, direction, |episode| episode.duration_secs),
        SortKey::Order => sort_by_key(episodes, direction, |episode| episode.order),
    }
}

fn sort_by_key<K: Ord>(
    episodes: &mut [&Episode],
    direction: SortDirection,
    key: impl Fn(&Episode) -> K,
) {
    let mut decorated: Vec<(K, &Episode)> = episodes
        .iter()
        .map(|episode| (key(episode), *episode))
        .collect();
    decorated.sort_by(|left, right| {
        let ordering = left.0.cmp(&right.0);
        match direction {
            SortDirection::Ascending => ordering,
            SortDirection::Descending => ordering.reverse(),
        }
    });
    for (slot, (_, episode)) in episodes.iter_mut().zip(decorated) {
        *slot = episode;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Statistics {
    pub episodes: usize,

    pub tracks: usize,

    /// Raw seconds. The site's own hour figure is a full day too high, so presentation divides
    /// this rather than copying that arithmetic.
    pub duration_secs: u64,
}

pub fn statistics(catalog: &Catalog) -> Statistics {
    Statistics {
        episodes: catalog.episodes.len(),

        tracks: catalog
            .episodes
            .iter()
            .filter_map(|episode| episode.tracklist.as_deref())
            .flat_map(str::lines)
            .filter(|line| !line.trim().is_empty())
            .count(),
        duration_secs: catalog
            .episodes
            .iter()
            .map(|episode| episode.duration_secs)
            .sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = include_str!("../../tests/fixtures/rss.xml");
    const BUNDLE: &str = include_str!("../../tests/fixtures/client.js");

    fn enriched_catalog() -> Catalog {
        let mut episodes = super::super::feed::parse(FEED).unwrap();
        let enrichment = super::super::enrich::enrich_from(BUNDLE, &mut episodes);
        Catalog {
            info: enrichment.info,
            episodes,
            fetched_at: 0,
            enriched: enrichment.applied,
        }
    }

    fn episode(title: &str, published_at: i64, duration_secs: u64) -> Episode {
        Episode {
            bundle_title: None,
            special: false,
            title: title.into(),
            link: String::new(),
            enclosure_url: format!("https://datashat.net/{title}.mp3"),
            byte_len: 0,
            duration_secs,
            published_at,
            slug: None,
            order: None,
            tracklist: None,
            body: None,
            links: None,
        }
    }

    fn titles(episodes: &[&Episode]) -> Vec<String> {
        episodes
            .iter()
            .map(|episode| episode.title.clone())
            .collect()
    }

    #[test]
    fn a_query_matches_a_title_in_any_letter_case() {
        let catalog = enriched_catalog();

        let lower = titles(&search(&catalog, "corticyte"));
        let upper = titles(&search(&catalog, "CORTICYTE"));
        let mixed = titles(&search(&catalog, "CoRtIcYtE"));

        assert!(lower.contains(&"Episode 79: Corticyte".to_owned()));
        assert_eq!(lower, upper);
        assert_eq!(lower, mixed);
    }

    #[test]
    fn a_query_matches_a_title_whose_letters_are_not_ascii() {
        let episode = episode("Épisode ÜBER Straße", 1, 1);

        assert!(matches(&episode, "épisode"));
        assert!(matches(&episode, "ÜBER"));
        assert!(matches(&episode, "straße"));
        assert!(!matches(&episode, "strasse"));
    }

    #[test]
    fn every_episode_whose_title_carries_the_query_is_returned() {
        let catalog = enriched_catalog();

        let matched = titles(&search(&catalog, "seventy"));

        assert_eq!(matched.len(), 10);
        assert!(matched.contains(&"Episode 70: THINGS DISAPPEAR".to_owned()));
    }

    #[test]
    fn a_query_matches_text_that_only_a_track_listing_carries() {
        let catalog = enriched_catalog();

        let matched = search(&catalog, "frog pocket");

        assert!(!matched.is_empty());
        for episode in &matched {
            assert!(
                !episode.title.to_lowercase().contains("frog pocket"),
                "{} matched on its title, not its track listing",
                episode.title
            );
            assert!(
                episode
                    .tracklist
                    .as_deref()
                    .unwrap()
                    .to_lowercase()
                    .contains("frog pocket")
            );
        }
    }

    #[test]
    fn searching_un_enriched_episodes_matches_on_the_title_and_does_not_fail() {
        let catalog = Catalog {
            info: Vec::new(),
            episodes: vec![episode("Episode 79: Corticyte", 3, 10)],
            fetched_at: 0,
            enriched: false,
        };

        assert_eq!(search(&catalog, "corticyte").len(), 1);
        assert_eq!(search(&catalog, "frog pocket").len(), 0);
    }

    #[test]
    fn a_query_matching_nothing_returns_an_empty_result() {
        let catalog = enriched_catalog();
        assert!(search(&catalog, "zzzz no such text zzzz").is_empty());
    }

    #[test]
    fn an_empty_query_returns_every_episode() {
        let catalog = enriched_catalog();
        assert_eq!(search(&catalog, "").len(), 79);
        assert_eq!(search(&catalog, "   ").len(), 79);
    }

    #[test]
    fn a_query_matches_a_slug() {
        let catalog = enriched_catalog();
        assert_eq!(
            titles(&search(&catalog, "seventynine")),
            ["Episode 79: Corticyte"]
        );
    }

    #[test]
    fn search_preserves_the_catalogs_current_order() {
        let catalog = enriched_catalog();
        let matched = search(&catalog, "datassette");
        let expected: Vec<String> = catalog
            .episodes
            .iter()
            .filter(|episode| matches(episode, "datassette"))
            .map(|episode| episode.title.clone())
            .collect();
        assert_eq!(titles(&matched), expected);
    }

    #[test]
    fn sorting_by_publication_date_descending_puts_the_newest_first() {
        let catalog = enriched_catalog();
        let mut episodes = search(&catalog, "");

        sort(&mut episodes, SortKey::Published, SortDirection::Descending);

        assert_eq!(episodes.len(), 79);
        assert_eq!(episodes[0].title, "Episode 79: Corticyte");
        assert_eq!(episodes[78].title, "Episode 01: Datassette");
        assert!(
            episodes
                .windows(2)
                .all(|pair| pair[0].published_at >= pair[1].published_at)
        );
    }

    #[test]
    fn sorting_by_title_and_duration_covers_both_directions() {
        let catalog = enriched_catalog();

        for (key, direction) in [
            (SortKey::Title, SortDirection::Ascending),
            (SortKey::Title, SortDirection::Descending),
            (SortKey::Duration, SortDirection::Ascending),
            (SortKey::Duration, SortDirection::Descending),
            (SortKey::Published, SortDirection::Ascending),
            (SortKey::Order, SortDirection::Ascending),
            (SortKey::Order, SortDirection::Descending),
        ] {
            let mut episodes = search(&catalog, "");
            sort(&mut episodes, key, direction);
            assert_eq!(episodes.len(), 79, "{key:?} {direction:?} dropped episodes");
        }

        let mut episodes = search(&catalog, "");
        sort(&mut episodes, SortKey::Duration, SortDirection::Ascending);
        assert!(
            episodes
                .windows(2)
                .all(|pair| pair[0].duration_secs <= pair[1].duration_secs)
        );

        sort(&mut episodes, SortKey::Title, SortDirection::Ascending);
        assert_eq!(episodes[0].title, "Episode 01: Datassette");
    }

    #[test]
    fn sorting_is_stable_for_episodes_that_compare_equal() {
        let all = [
            episode("first", 10, 5),
            episode("second", 10, 5),
            episode("third", 10, 5),
        ];
        let mut episodes: Vec<&Episode> = all.iter().collect();

        sort(&mut episodes, SortKey::Published, SortDirection::Ascending);
        assert_eq!(titles(&episodes), ["first", "second", "third"]);

        sort(&mut episodes, SortKey::Published, SortDirection::Descending);
        assert_eq!(titles(&episodes), ["first", "second", "third"]);
    }

    #[test]
    fn episodes_missing_the_sort_key_are_ordered_deterministically_not_dropped() {
        let mut with_order = episode("enriched", 1, 1);
        with_order.order = Some(7);
        let all = [episode("bare-a", 1, 1), with_order, episode("bare-b", 1, 1)];
        let mut episodes: Vec<&Episode> = all.iter().collect();

        sort(&mut episodes, SortKey::Order, SortDirection::Ascending);
        assert_eq!(titles(&episodes), ["bare-a", "bare-b", "enriched"]);

        sort(&mut episodes, SortKey::Order, SortDirection::Descending);
        assert_eq!(titles(&episodes), ["enriched", "bare-a", "bare-b"]);
    }

    #[test]
    fn filtering_keeps_only_the_locally_available_episodes() {
        let catalog = enriched_catalog();
        let episodes = search(&catalog, "");
        let downloaded = ["seventynine", "one", "two"];

        let local = filter(&episodes, |episode| {
            downloaded.contains(&episode.id().as_ref())
        });

        assert_eq!(local.len(), 3);
        assert_eq!(
            local.iter().map(|episode| episode.id()).collect::<Vec<_>>(),
            ["seventynine", "two", "one"]
        );
    }

    #[test]
    fn the_totals_match_the_site_for_the_current_catalog() {
        let catalog = enriched_catalog();

        let statistics = statistics(&catalog);

        assert_eq!(statistics.episodes, 79);
        assert_eq!(statistics.tracks, 1380);
        assert_eq!(statistics.duration_secs, 347_111);

        assert_eq!(statistics.duration_secs / 3_600, 96);
        assert_eq!(statistics.duration_secs % 3_600 / 60, 25);
        assert_eq!(statistics.duration_secs % 60, 11);
    }

    #[test]
    fn statistics_without_enrichment_report_no_tracks_and_still_count_and_sum() {
        let catalog = Catalog {
            info: Vec::new(),
            episodes: vec![episode("one", 1, 100), episode("two", 2, 250)],
            fetched_at: 0,
            enriched: false,
        };

        let statistics = statistics(&catalog);

        assert_eq!(statistics.tracks, 0);
        assert_eq!(statistics.episodes, 2);
        assert_eq!(statistics.duration_secs, 350);
    }

    #[test]
    fn the_information_pages_are_available_and_change_no_count_or_navigation() {
        let catalog = enriched_catalog();

        assert_eq!(catalog.info_page("about").unwrap().title, "About");
        assert_eq!(catalog.info_page("credits").unwrap().title, "Credits");
        assert!(catalog.info_page("nothing").is_none());

        assert_eq!(statistics(&catalog).episodes, 79);
        assert_eq!(search(&catalog, "").len(), 79);
        assert_eq!(catalog.episodes[0].title, "Episode 79: Corticyte");
        assert!(catalog.get("about").is_none());
        assert!(catalog.position("credits").is_none());
        let position = catalog.position("sixtytwo").unwrap();
        assert_eq!(catalog.episodes[position - 1].order, Some(63));
        assert_eq!(catalog.episodes[position + 1].order, Some(61));
    }

    #[test]
    fn filtering_with_nothing_downloaded_returns_nothing() {
        let catalog = enriched_catalog();
        let episodes = search(&catalog, "");
        assert!(filter(&episodes, |_| false).is_empty());
        assert_eq!(filter(&episodes, |_| true).len(), 79);
    }
}
