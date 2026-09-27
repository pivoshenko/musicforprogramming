//! The episode model and the catalog that holds it.
//!
//! An episode has two tiers of fields. The first six come from the authoritative RSS feed
//! and are present for every episode. The rest come from best-effort enrichment out of the
//! site's client bundle, and are `Option` so an absent value cannot be mistaken for a real
//! one.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// One episode of musicforprogramming.net.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    pub title: String,
    /// The episode's page on the site.
    pub link: String,
    /// The audio enclosure URL, and the episode's stable identity: enrichment records join
    /// to episodes by exact string equality against it.
    pub enclosure_url: String,
    /// The byte length the feed declares for the enclosure.
    pub byte_len: u64,
    /// The duration the feed declares in `itunes:duration`, in seconds.
    ///
    /// An approximate total, never a measured audio length. Consumers must present it as
    /// approximate.
    pub duration_secs: u64,
    /// Publication time as whole seconds since the Unix epoch.
    pub published_at: i64,

    /// Site slug, when enrichment supplied one.
    #[serde(default)]
    pub slug: Option<String>,
    /// The site's own catalog ordering, when enrichment supplied one.
    #[serde(default)]
    pub order: Option<u32>,
    /// Track listing as plain text, one track per line, when enrichment supplied one.
    #[serde(default)]
    pub tracklist: Option<String>,
    /// Descriptive body as plain text, when enrichment supplied one.
    #[serde(default)]
    pub body: Option<String>,
    /// Related links as plain text, one per line, when enrichment supplied any.
    #[serde(default)]
    pub links: Option<String>,
    /// The site's own title for this episode, when enrichment supplied one.
    ///
    /// The feed says `Episode 79: Corticyte` where the site says `79: Corticyte`, and
    /// anything replicating the site's presentation wants the latter, which
    /// [`Episode::site_title`] resolves.
    #[serde(default)]
    pub bundle_title: Option<String>,
    /// Whether the site flags this episode for distinct colouring.
    ///
    /// Exactly one episode carries it today; absent enrichment every episode reports
    /// `false`, so an unknown flag is never mistaken for a set one. Presentation only:
    /// ordering, playback, and download all ignore it.
    #[serde(default)]
    pub special: bool,
}

impl Episode {
    /// The episode's stable filesystem-safe identifier, used to name its on-disk artifacts
    /// and as the `slug` clients pass over the wire.
    ///
    /// The enrichment slug where one exists, a deterministic derivation from the enclosure
    /// URL otherwise, so the same episode resolves to the same name across runs whether or
    /// not enrichment succeeded. Borrowed from the slug in the enriched case, so scanning
    /// the catalog for one identifier allocates nothing.
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
    /// The title as the site itself writes it, which every view replicating the site draws.
    ///
    /// Enrichment's title where there is one, otherwise the feed's with its `Episode `
    /// prefix stripped, since the feed writes `Episode 79: Corticyte` where the site writes
    /// `79: Corticyte` - so an un-enriched catalog degrades to the same shape rather than
    /// a visibly different one.
    pub fn site_title(&self) -> &str {
        match &self.bundle_title {
            Some(title) => title,
            None => site_title(&self.title),
        }
    }
}

/// The site's form of a title that may have come from the feed.
///
/// The feed writes `Episode 79: Corticyte` where the site writes `79: Corticyte`. Anything
/// rendering the site's presentation from a title it did not get from enrichment - the
/// marquee, which only ever sees the daemon's snapshot - normalises through this, so the
/// two cannot drift.
pub fn site_title(title: &str) -> &str {
    title.strip_prefix("Episode ").unwrap_or(title)
}

/// FNV-1a over 64 bits.
///
/// Written out rather than taken from `DefaultHasher`, whose output std explicitly does
/// not guarantee across releases, because on-disk names must survive a toolchain upgrade.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// One of the site's non-episode pages, kept so the interface can present it as the site
/// does.
///
/// Same enrichment source as track listings, but with no audio to join to, which is why
/// these live beside [`Catalog::episodes`] rather than in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfoPage {
    /// Site slug, which is this page's identity: `about` or `credits`.
    pub slug: String,
    /// Display title as the bundle gives it.
    pub title: String,
    pub body: String,
}

/// The resolved episode list together with when it was built.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    /// Every episode the feed listed, in catalog order.
    pub episodes: Vec<Episode>,
    /// The site's information pages, keyed by their slug.
    ///
    /// Deliberately separate from `episodes`: every count, ordering, and next-or-previous
    /// computation is over episodes alone, and keeping these out of that list makes it true
    /// by construction rather than by remembering to filter.
    #[serde(default)]
    pub info: Vec<InfoPage>,
    /// When this catalog was built from the network, as whole seconds since the Unix epoch.
    /// A catalog read back from disk keeps its fetch time, so callers can tell how stale it
    /// is.
    pub fetched_at: i64,
    /// Whether the client bundle enriched this catalog. False means every episode carries
    /// feed fields only.
    pub enriched: bool,
}

impl Catalog {
    pub fn get(&self, id: &str) -> Option<&Episode> {
        self.episodes.iter().find(|episode| episode.id() == id)
    }

    /// The index of the episode with this identifier, for `next` and `previous`.
    pub fn position(&self, id: &str) -> Option<usize> {
        self.episodes.iter().position(|episode| episode.id() == id)
    }

    /// How long ago this catalog was fetched, in seconds, relative to `now`.
    pub fn age_secs(&self, now: i64) -> i64 {
        (now - self.fetched_at).max(0)
    }

    /// The information page with this slug, if enrichment supplied one.
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
        // the feed writes `Episode 79: Corticyte` where the site writes `79: Corticyte`,
        // so an un-enriched catalog must not render a visibly different shape
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

        // the whole point of the separate collection: counting and navigating never see them
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
