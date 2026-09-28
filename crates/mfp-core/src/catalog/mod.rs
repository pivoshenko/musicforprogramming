//! The episode catalog: [`feed`] is authoritative and [`enrich`] is best-effort, both
//! behind the disk cache in [`cache`]. Nothing here fetches on a timer or in the background.

pub mod cache;
pub mod enrich;
pub mod feed;
pub mod query;

pub const FEED_URL: &str = "https://musicforprogramming.net/rss.xml";

/// The shell document the content-hashed client bundle path is discovered from.
pub const SITE_URL: &str = "https://musicforprogramming.net/";

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
