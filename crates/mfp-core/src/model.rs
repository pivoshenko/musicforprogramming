use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// One episode of musicforprogramming.net. The first six fields come from the
/// authoritative feed; the rest are `Option` because enrichment is best-effort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    pub title: String,

    pub link: String,

    /// The audio URL, and the episode's identity: enrichment records join to episodes by
    /// exact string equality against it.
    pub enclosure_url: String,

    pub byte_len: u64,

    /// Whole seconds as the feed declares them. An approximation, never a measured length.
    pub duration_secs: u64,

    pub published_at: i64,

    #[serde(default)]
    pub slug: Option<String>,

    #[serde(default)]
    pub order: Option<u32>,

    #[serde(default)]
    pub tracklist: Option<String>,

    #[serde(default)]
    pub body: Option<String>,

    #[serde(default)]
    pub links: Option<String>,

    #[serde(default)]
    pub bundle_title: Option<String>,

    #[serde(default)]
    pub special: bool,
}

impl Episode {
    /// The stable filesystem-safe identifier naming this episode's on-disk artifacts: the
    /// enrichment slug where there is one, a derivation from the enclosure URL otherwise.
    pub fn id(&self) -> Cow<'_, str> {
        match &self.slug {
            Some(slug) => Cow::Borrowed(slug),
            None => Cow::Owned(format!(
                "ep-{:016x}",
                fnv1a64(self.enclosure_url.as_bytes())
            )),
        }
    }
}

impl Episode {
    /// The title as the site writes it, which the feed's `Episode ` prefix is stripped for.
    pub fn site_title(&self) -> &str {
        match &self.bundle_title {
            Some(title) => title,
            None => site_title(&self.title),
        }
    }
}

/// Normalises a feed title to the site's form, so the two presentations cannot drift.
pub fn site_title(title: &str) -> &str {
    title.strip_prefix("Episode ").unwrap_or(title)
}

/// FNV-1a over 64 bits, written out rather than taken from `DefaultHasher`, whose output
/// std does not guarantee across releases: on-disk names must survive a toolchain bump.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfoPage {
    pub slug: String,

    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub episodes: Vec<Episode>,

    #[serde(default)]
    /// Kept out of `episodes` so every count and next-or-previous computation is over
    /// episodes alone by construction rather than by remembering to filter.
    pub info: Vec<InfoPage>,

    /// When this catalog was built from the network, in whole seconds since the Unix epoch.
    pub fetched_at: i64,

    pub enriched: bool,
}

impl Catalog {
    pub fn get(&self, id: &str) -> Option<&Episode> {
        self.episodes.iter().find(|episode| episode.id() == id)
    }

    pub fn position(&self, id: &str) -> Option<usize> {
        self.episodes.iter().position(|episode| episode.id() == id)
    }

    /// How long ago this catalog was fetched, in seconds, clamped at zero.
    pub fn age_secs(&self, now: i64) -> i64 {
        (now - self.fetched_at).max(0)
    }

    pub fn info_page(&self, slug: &str) -> Option<&InfoPage> {
        self.info.iter().find(|page| page.slug == slug)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(slug: Option<&str>) -> Episode {
        Episode {
            bundle_title: None,
            special: false,
            title: "Episode 79".into(),
            link: "https://musicforprogramming.net/seventynine".into(),
            enclosure_url: "https://datashat.net/music_for_programming_79.mp3".into(),
            byte_len: 441_000_000,
            duration_secs: 14_400,
            published_at: 1_700_000_000,
            slug: slug.map(str::to_owned),
            order: None,
            tracklist: None,
            body: None,
            links: None,
        }
    }

    #[test]
    fn an_un_enriched_episode_reports_its_optional_fields_absent() {
        let episode = episode(None);
        assert!(episode.slug.is_none());
        assert!(episode.order.is_none());
        assert!(episode.tracklist.is_none());
        assert!(episode.body.is_none());
        assert!(episode.links.is_none());
    }

    #[test]
    fn an_enriched_episode_is_identified_by_its_slug() {
        assert_eq!(episode(Some("seventynine")).id(), "seventynine");
    }

    #[test]
    fn an_enriched_identifier_borrows_its_slug_rather_than_copying_it() {
        let episode = episode(Some("seventynine"));
        assert!(matches!(episode.id(), Cow::Borrowed(_)));
    }

    #[test]
    fn an_un_enriched_identifier_is_derived_from_the_enclosure_url() {
        let episode = episode(None);
        let id = episode.id();
        assert_eq!(id, episode.id());
        assert!(id.starts_with("ep-"));
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{id} is not filesystem-safe"
        );
    }

    #[test]
    fn different_enclosure_urls_get_different_identifiers() {
        let mut other = episode(None);
        other.enclosure_url = "https://datashat.net/music_for_programming_78.mp3".into();
        assert_ne!(episode(None).id(), other.id());
    }

    #[test]
    fn an_episode_round_trips_through_serde() {
        let mut original = episode(Some("seventynine"));
        original.order = Some(79);
        original.tracklist = Some("A & B\nC".into());
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(serde_json::from_str::<Episode>(&json).unwrap(), original);
    }

    #[test]
    fn optional_fields_default_to_absent_when_the_document_omits_them() {
        let json = r#"{
            "title": "Episode 79",
            "link": "https://musicforprogramming.net/seventynine",
            "enclosure_url": "https://datashat.net/music_for_programming_79.mp3",
            "byte_len": 441000000,
            "duration_secs": 14400,
            "published_at": 1700000000
        }"#;
        assert_eq!(
            serde_json::from_str::<Episode>(json).unwrap(),
            episode(None)
        );
    }

    #[test]
    fn the_site_title_is_the_bundles_where_enrichment_supplied_one() {
        let mut episode = episode(Some("seventynine"));
        episode.title = "Episode 79: Corticyte".into();
        episode.bundle_title = Some("79: Corticyte".into());
        assert_eq!(episode.site_title(), "79: Corticyte");
    }

    #[test]
    fn an_un_enriched_site_title_drops_the_feeds_episode_prefix() {
        let mut episode = episode(None);
        episode.title = "Episode 79: Corticyte".into();
        assert_eq!(episode.site_title(), "79: Corticyte");
    }

    #[test]
    fn a_title_without_the_prefix_is_left_alone() {
        let mut episode = episode(None);
        episode.title = "79: Corticyte".into();
        assert_eq!(episode.site_title(), "79: Corticyte");
    }

    #[test]
    fn an_episode_is_not_special_unless_the_flag_says_so() {
        assert!(!episode(None).special);
    }

    #[test]
    fn a_document_omitting_the_special_flag_reads_it_as_unset() {
        let json = r#"{
            "title": "Episode 79",
            "link": "https://musicforprogramming.net/seventynine",
            "enclosure_url": "https://datashat.net/music_for_programming_79.mp3",
            "byte_len": 441000000,
            "duration_secs": 14400,
            "published_at": 1700000000
        }"#;
        assert!(!serde_json::from_str::<Episode>(json).unwrap().special);
    }

    #[test]
    fn information_pages_are_found_by_slug_and_kept_out_of_the_episode_list() {
        let catalog = Catalog {
            episodes: vec![episode(Some("seventynine"))],
            info: vec![
                InfoPage {
                    slug: "about".into(),
                    title: "About".into(),
                    body: "Through years of trial and error".into(),
                },
                InfoPage {
                    slug: "credits".into(),
                    title: "Credits".into(),
                    body: "Music For Programming is maintained by".into(),
                },
            ],
            fetched_at: 0,
            enriched: true,
        };

        assert_eq!(catalog.info_page("about").unwrap().title, "About");
        assert_eq!(catalog.info_page("credits").unwrap().title, "Credits");
        assert!(catalog.info_page("seventynine").is_none());

        assert_eq!(catalog.episodes.len(), 1);
        assert!(catalog.get("about").is_none());
        assert!(catalog.position("credits").is_none());
    }

    #[test]
    fn a_catalog_without_information_pages_is_still_a_catalog() {
        let json = r#"{"episodes":[],"fetched_at":0,"enriched":false}"#;
        let catalog: Catalog = serde_json::from_str(json).unwrap();
        assert!(catalog.info.is_empty());
        assert!(catalog.info_page("about").is_none());
    }

    #[test]
    fn a_catalog_finds_episodes_and_reports_its_age() {
        let catalog = Catalog {
            info: Vec::new(),
            episodes: vec![episode(Some("seventyeight")), episode(Some("seventynine"))],
            fetched_at: 1_000,
            enriched: true,
        };
        assert_eq!(catalog.position("seventynine"), Some(1));
        assert_eq!(catalog.get("seventynine").unwrap().id(), "seventynine");
        assert!(catalog.get("nope").is_none());
        assert_eq!(catalog.age_secs(4_600), 3_600);
        assert_eq!(catalog.age_secs(500), 0);
    }
}
