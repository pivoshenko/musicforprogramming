//! The on-disk catalog cache.
//!
//! A cache younger than [`TTL_SECS`] is served without touching the network. An older one
//! triggers a refresh, and a failed refresh falls back to the stale copy - the caller
//! tells staleness from [`crate::model::Catalog::age_secs`]. Only a cache miss with no
//! reachable network is an error.

use std::future::Future;
use std::path::Path;

use crate::error::{Error, Result};
use crate::model::Catalog;

/// How long a cached catalog stays fresh.
pub const TTL_SECS: i64 = 6 * 60 * 60;

/// Returns the catalog, from cache where it is fresh and from the network otherwise.
///
/// `force_refresh` ignores freshness; a forced refresh that fails leaves the cache file
/// intact and returns the previously cached catalog.
pub async fn load(client: &reqwest::Client, force_refresh: bool) -> Result<Catalog> {
    let path = crate::paths::catalog_cache_file()?;
    resolve(&path, now_secs(), force_refresh, || refresh(client)).await
}

/// Builds a catalog from the network: the authoritative feed, then best-effort enrichment.
async fn refresh(client: &reqwest::Client) -> Result<Catalog> {
    let mut episodes = super::feed::fetch(client).await?;
    let enrichment = super::enrich::enrich(client, &mut episodes).await;
    Ok(Catalog {
        episodes,
        info: enrichment.info,
        fetched_at: now_secs(),
        enriched: enrichment.applied,
    })
}

/// The cache policy, over an injected clock and refresh so it can be exercised offline.
async fn resolve<F, Fut>(path: &Path, now: i64, force_refresh: bool, refresh: F) -> Result<Catalog>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Catalog>>,
{
    let cached = read(path);

    if !force_refresh
        && let Some(catalog) = &cached
        && catalog.age_secs(now) < TTL_SECS
    {
        return Ok(catalog.clone());
    }

    match refresh().await {
        Ok(catalog) => {
            // a catalog that cannot be cached is still a catalog
            if let Err(error) = write(path, &catalog) {
                tracing::debug!("the catalog cache could not be written: {error}");
            }
            Ok(catalog)
        }
        // the cache file is left exactly as it was, so a failed refresh costs nothing
        Err(error) => match cached {
            Some(catalog) => {
                tracing::warn!(
                    "serving a catalog {} seconds old: {error}",
                    catalog.age_secs(now)
                );
                Ok(catalog)
            }
            None => Err(Error::CatalogUnavailable(format!(
                "{error}, and no cached catalog is available"
            ))),
        },
    }
}

/// Reads the cache file. An unreadable or unparseable file is a miss, not an error.
pub fn read(path: &Path) -> Option<Catalog> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(catalog) => Some(catalog),
        Err(error) => {
            tracing::debug!("{} could not be parsed: {error}", path.display());
            None
        }
    }
}

/// Writes the catalog as a normalised JSON document.
///
/// The document is written beside the cache file and renamed over it, so a write cut
/// short leaves the previous catalog rather than half of this one.
pub fn write(path: &Path, catalog: &Catalog) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let document = serde_json::to_string_pretty(catalog).map_err(|error| {
        Error::Internal(format!("the catalog could not be serialised: {error}"))
    })?;
    let partial = path.with_extension("json.tmp");
    std::fs::write(&partial, document)?;
    std::fs::rename(&partial, path)?;
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::model::Episode;

    const NOW: i64 = 1_800_000_000;

    fn catalog(fetched_at: i64, title: &str) -> Catalog {
        Catalog {
            info: Vec::new(),
            episodes: vec![Episode {
                bundle_title: None,
                special: false,
                title: title.into(),
                link: "https://musicforprogramming.net/one".into(),
                enclosure_url: "https://datashat.net/one.mp3".into(),
                byte_len: 1,
                duration_secs: 2,
                published_at: 3,
                slug: None,
                order: None,
                tracklist: None,
                body: None,
                links: None,
            }],
            fetched_at,
            enriched: false,
        }
    }

    /// A refresh that records whether it ran, so a test can assert whether the network was
    /// reached.
    struct Network<'a> {
        result: Result<Catalog>,
        calls: &'a Cell<usize>,
    }

    impl Network<'_> {
        fn run(self) -> impl Future<Output = Result<Catalog>> {
            self.calls.set(self.calls.get() + 1);
            std::future::ready(self.result)
        }
    }

    fn unreachable() -> Error {
        Error::CatalogUnavailable("the feed is unreachable: connection refused".into())
    }

    #[tokio::test]
    async fn a_fresh_cache_is_served_without_touching_the_network() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("catalog.json");
        write(&path, &catalog(NOW - 3_600, "cached")).unwrap();
        let calls = Cell::new(0);

        let resolved = resolve(&path, NOW, false, || {
            Network {
                result: Ok(catalog(NOW, "network")),
                calls: &calls,
            }
            .run()
        })
        .await
        .unwrap();

        assert_eq!(resolved.episodes[0].title, "cached");
        assert_eq!(calls.get(), 0);
    }

    #[tokio::test]
    async fn a_stale_cache_triggers_a_refresh_that_overwrites_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("catalog.json");
        write(&path, &catalog(NOW - 7 * 3_600, "cached")).unwrap();
        let calls = Cell::new(0);

        let resolved = resolve(&path, NOW, false, || {
            Network {
                result: Ok(catalog(NOW, "network")),
                calls: &calls,
            }
            .run()
        })
        .await
        .unwrap();

        assert_eq!(resolved.episodes[0].title, "network");
        assert_eq!(calls.get(), 1);
        assert_eq!(read(&path).unwrap().episodes[0].title, "network");
    }

    #[tokio::test]
    async fn the_ttl_boundary_is_six_hours() {
        assert_eq!(TTL_SECS, 6 * 60 * 60);
        let root = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);

        for (age, expected_calls) in [(TTL_SECS - 1, 0), (TTL_SECS, 1)] {
            let path = root.path().join(format!("catalog-{age}.json"));
            write(&path, &catalog(NOW - age, "cached")).unwrap();
            calls.set(0);

            resolve(&path, NOW, false, || {
                Network {
                    result: Ok(catalog(NOW, "network")),
                    calls: &calls,
                }
                .run()
            })
            .await
            .unwrap();

            assert_eq!(calls.get(), expected_calls, "at age {age}");
        }
    }

    #[tokio::test]
    async fn a_stale_cache_is_served_when_the_refresh_fails() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("catalog.json");
        let fetched_at = NOW - 3 * 86_400;
        write(&path, &catalog(fetched_at, "cached")).unwrap();
        let calls = Cell::new(0);

        let resolved = resolve(&path, NOW, false, || {
            Network {
                result: Err(unreachable()),
                calls: &calls,
            }
            .run()
        })
        .await
        .unwrap();

        assert_eq!(resolved.episodes[0].title, "cached");
        assert_eq!(resolved.age_secs(NOW), 3 * 86_400);
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn a_refresh_failure_with_no_cache_names_the_feed_as_unreachable() {
        let root = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);

        let error = resolve(&root.path().join("catalog.json"), NOW, false, || {
            Network {
                result: Err(unreachable()),
                calls: &calls,
            }
            .run()
        })
        .await
        .unwrap_err();

        assert_eq!(error.code(), crate::error::ErrorCode::CatalogUnavailable);
        assert!(error.to_string().contains("unreachable"));
        assert!(error.to_string().contains("no cached catalog is available"));
    }

    #[tokio::test]
    async fn a_forced_refresh_ignores_a_fresh_cache() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("catalog.json");
        write(&path, &catalog(NOW - 600, "cached")).unwrap();
        let calls = Cell::new(0);

        let resolved = resolve(&path, NOW, true, || {
            Network {
                result: Ok(catalog(NOW, "network")),
                calls: &calls,
            }
            .run()
        })
        .await
        .unwrap();

        assert_eq!(resolved.episodes[0].title, "network");
        assert_eq!(calls.get(), 1);
        assert_eq!(read(&path).unwrap().episodes[0].title, "network");
    }

    #[tokio::test]
    async fn a_failed_forced_refresh_leaves_the_cache_file_untouched() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("catalog.json");
        write(&path, &catalog(NOW - 600, "cached")).unwrap();
        let before = std::fs::read(&path).unwrap();
        let calls = Cell::new(0);

        let resolved = resolve(&path, NOW, true, || {
            Network {
                result: Err(unreachable()),
                calls: &calls,
            }
            .run()
        })
        .await
        .unwrap();

        assert_eq!(resolved.episodes[0].title, "cached");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn a_corrupt_cache_file_is_a_miss_rather_than_an_error() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("catalog.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        let calls = Cell::new(0);

        assert!(read(&path).is_none());

        let resolved = resolve(&path, NOW, false, || {
            Network {
                result: Ok(catalog(NOW, "network")),
                calls: &calls,
            }
            .run()
        })
        .await
        .unwrap();

        assert_eq!(resolved.episodes[0].title, "network");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn a_missing_cache_file_is_a_miss_rather_than_an_error() {
        let root = tempfile::tempdir().unwrap();
        assert!(read(&root.path().join("nothing.json")).is_none());
    }

    /// A truncated cache file reads as a miss, which costs a network fetch, so the write
    /// must never be observable half-done.
    #[test]
    fn a_written_catalog_replaces_the_last_one_and_leaves_no_partial_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("catalog.json");

        write(&path, &catalog(NOW, "first")).unwrap();
        write(&path, &catalog(NOW, "second")).unwrap();

        assert_eq!(read(&path).unwrap().episodes[0].title, "second");
        let entries: Vec<String> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["catalog.json"]);
    }

    #[test]
    fn a_written_catalog_round_trips_through_the_cache_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested").join("catalog.json");
        let original = catalog(NOW, "cached");

        write(&path, &original).unwrap();

        assert_eq!(read(&path).unwrap(), original);
    }
}
