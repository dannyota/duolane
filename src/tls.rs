#![allow(clippy::disallowed_methods)]

pub fn client_builder() -> reqwest::ClientBuilder {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
}
