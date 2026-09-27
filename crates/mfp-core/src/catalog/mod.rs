//! The episode catalog: the authoritative feed, its best-effort enrichment, the disk
//! cache that lets the player start without a network, and query over the result.
//!
//! Resolution order is [`feed`] then [`enrich`], behind the cache in [`cache`]. A feed
//! failure fails the catalog; any enrichment failure still yields it, without track
//! listings.
//!
//! Nothing fetches on a timer or in the background: every request here is reached from
//! [`cache::load`], called only when a user action needs data the cache cannot satisfy.

pub mod cache;
pub mod enrich;
pub mod feed;
pub mod query;

/// The authoritative episode list.
pub const FEED_URL: &str = "https://musicforprogramming.net/rss.xml";

/// The shell document the content-hashed client bundle path is discovered from.
pub const SITE_URL: &str = "https://musicforprogramming.net/";

/// A GET carrying the User-Agent every upstream request identifies itself with.
///
/// Set per request rather than on the client, so the header holds whatever client the
/// caller supplies.
pub(crate) fn request(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header(reqwest::header::USER_AGENT, crate::USER_AGENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_upstream_request_names_the_application() {
        let client = reqwest::Client::new();
        for url in [
            FEED_URL,
            SITE_URL,
            "https://musicforprogramming.net/client/client.a.js",
        ] {
            let built = request(&client, url).build().unwrap();
            let agent = built
                .headers()
                .get(reqwest::header::USER_AGENT)
                .expect("no User-Agent")
                .to_str()
                .unwrap();
            assert_eq!(agent, crate::USER_AGENT);
            assert!(
                agent.contains("mfp"),
                "{agent} does not name the application"
            );
        }
    }
}
