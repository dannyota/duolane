use std::path::Path;
use std::sync::{Arc, RwLock};

use rustls::pki_types::CertificateDer;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tokio::time::MissedTickBehavior;

use super::certs::{self, Pair};
use super::files::{self, CA_FILE, KEY_FILE, LEAF_FILE, PENDING_KEY_FILE, PENDING_LEAF_FILE};
use super::{INTERCEPTED_HOST, manifest};
use crate::logging::create_logger;

const RENEW_BEFORE: Duration = Duration::days(14);
const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);

type Loaded = Option<(Pair, CertificateDer<'static>)>;

#[derive(Debug)]
pub(super) struct Served(RwLock<Arc<CertifiedKey>>);

impl Served {
    pub(super) fn new(pair: Pair) -> anyhow::Result<Self> {
        Ok(Self(RwLock::new(certified(pair)?)))
    }

    fn leaf(&self) -> Option<CertificateDer<'static>> {
        self.0.read().ok()?.cert.first().cloned()
    }

    fn replace(&self, pair: Pair) -> anyhow::Result<bool> {
        if self.leaf().as_ref() == Some(&pair.leaf) {
            return Ok(false);
        }
        let key = certified(pair)?;
        let mut served = self
            .0
            .write()
            .map_err(|_| anyhow::anyhow!("the served intercept certificate is unavailable"))?;
        *served = key;
        Ok(true)
    }
}

impl ResolvesServerCert for Served {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.0.read().ok().map(|key| key.clone())
    }
}

fn certified(pair: Pair) -> anyhow::Result<Arc<CertifiedKey>> {
    Ok(Arc::new(CertifiedKey::from_der(
        vec![pair.leaf],
        pair.key,
        &rustls::crypto::ring::default_provider(),
    )?))
}

pub(super) fn server_config(served: Arc<Served>) -> anyhow::Result<rustls::ServerConfig> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_cert_resolver(served);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

pub(super) fn refresh(dir: &Path, now: OffsetDateTime) -> anyhow::Result<Pair> {
    let authorities = certs::authorities(&files::read(&dir.join(CA_FILE))?.unwrap_or_default());
    let current = load(dir, LEAF_FILE, KEY_FILE, &authorities, now)?;
    let pending = load(dir, PENDING_LEAF_FILE, PENDING_KEY_FILE, &authorities, now)?;
    let pair = match (current, pending) {
        (Some((pair, ca)), pending) => {
            let lasting = certs::verify(
                &ca,
                &pair.leaf,
                INTERCEPTED_HOST,
                now.saturating_add(RENEW_BEFORE),
            );
            if lasting.is_err() && pending.is_none() {
                let created = certs::create(now)?;
                files::write(&dir.join(PENDING_KEY_FILE), created.key_pem.as_bytes())?;
                files::write(&dir.join(PENDING_LEAF_FILE), created.leaf_pem.as_bytes())?;
                let both = format!("{}{}", certs::pem(&ca), created.ca_pem);
                files::write(&dir.join(CA_FILE), both.as_bytes())?;
            }
            pair
        }
        (None, Some((pair, ca))) => {
            rename(dir, PENDING_LEAF_FILE, LEAF_FILE)?;
            rename(dir, PENDING_KEY_FILE, KEY_FILE)?;
            files::write(&dir.join(CA_FILE), certs::pem(&ca).as_bytes())?;
            pair
        }
        (None, None) => {
            let created = certs::create(now)?;
            files::write(&dir.join(KEY_FILE), created.key_pem.as_bytes())?;
            files::write(&dir.join(LEAF_FILE), created.leaf_pem.as_bytes())?;
            files::write(&dir.join(CA_FILE), created.ca_pem.as_bytes())?;
            files::remove(&dir.join(PENDING_LEAF_FILE))?;
            files::remove(&dir.join(PENDING_KEY_FILE))?;
            let authorities = certs::authorities(created.ca_pem.as_bytes());
            load(dir, LEAF_FILE, KEY_FILE, &authorities, now)?
                .map(|(pair, _)| pair)
                .ok_or_else(|| anyhow::anyhow!("the new intercept certificate does not verify"))?
        }
    };
    manifest::refresh_files(dir)?;
    Ok(pair)
}

fn load(
    dir: &Path,
    leaf: &str,
    key: &str,
    authorities: &[CertificateDer<'static>],
    now: OffsetDateTime,
) -> anyhow::Result<Loaded> {
    let leaf = files::read(&dir.join(leaf))?;
    let key = files::read(&dir.join(key))?;
    let (Some(leaf), Some(key)) = (leaf, key) else {
        return Ok(None);
    };
    Ok(certs::usable(authorities, &leaf, &key, now))
}

fn rename(dir: &Path, from: &str, to: &str) -> anyhow::Result<()> {
    std::fs::rename(dir.join(from), dir.join(to))
        .map_err(|error| anyhow::anyhow!("cannot rename {from} to {to}: {error}"))
}

pub(super) fn rotate(dir: &Path, now: OffsetDateTime, served: &Served) -> anyhow::Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    served.replace(refresh(dir, now)?)
}

pub(super) async fn run(served: Arc<Served>) {
    let start = tokio::time::Instant::now() + CHECK_EVERY;
    let mut ticks = tokio::time::interval_at(start, CHECK_EVERY);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let served = served.clone();
        let checked = tokio::task::spawn_blocking(move || {
            rotate(&super::intercept_dir(), OffsetDateTime::now_utc(), &served)
        })
        .await;
        let logger = create_logger("intercept");
        match checked {
            Ok(Ok(true)) => logger.info("serving the renewed intercept certificate", None),
            Ok(Err(error)) => logger.warn(
                "intercept certificate check failed",
                Some(serde_json::Map::from_iter([(
                    "error".to_string(),
                    json!(error.to_string()),
                )])),
            ),
            Ok(Ok(false)) | Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::ServerName;
    use rustls::pki_types::pem::PemObject;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    fn ca_count(dir: &Path) -> usize {
        certs::authorities(&std::fs::read(dir.join(CA_FILE)).unwrap()).len()
    }

    async fn served_leaf(served: Arc<Served>, ca: &[u8]) -> CertificateDer<'static> {
        let acceptor = TlsAcceptor::from(Arc::new(server_config(served).unwrap()));
        let mut roots = rustls::RootCertStore::empty();
        for authority in certs::authorities(ca) {
            roots.add(authority).unwrap();
        }
        let client = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let (client_side, server_side) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move { acceptor.accept(server_side).await.map(drop) });
        let tls = TlsConnector::from(Arc::new(client))
            .connect(ServerName::try_from(INTERCEPTED_HOST).unwrap(), client_side)
            .await
            .unwrap();
        server.await.unwrap().unwrap();
        tls.get_ref().1.peer_certificates().unwrap()[0].clone()
    }

    #[tokio::test]
    async fn pending_pair_appears_before_expiry_and_is_served_once_the_current_one_expires() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir = dir.path();
        let now = OffsetDateTime::now_utc();
        let created = now - Duration::days(77);

        let first = refresh(dir, created).unwrap();
        let served =
            Arc::new(Served::new(refresh(dir, created + Duration::days(60)).unwrap()).unwrap());
        assert!(served.leaf() == Some(first.leaf.clone()));
        assert!(!dir.join(PENDING_LEAF_FILE).exists());
        assert_eq!(ca_count(dir), 1);

        assert!(!rotate(dir, now, &served).unwrap());
        assert!(dir.join(PENDING_LEAF_FILE).exists());
        assert!(dir.join(PENDING_KEY_FILE).exists());
        assert_eq!(ca_count(dir), 2);
        let pending = std::fs::read(dir.join(PENDING_LEAF_FILE)).unwrap();
        let pending = CertificateDer::from_pem_slice(&pending).unwrap();
        let both = std::fs::read(dir.join(CA_FILE)).unwrap();
        assert!(served_leaf(served.clone(), &both).await == first.leaf);

        assert!(rotate(dir, now + Duration::days(14), &served).unwrap());
        assert!(!dir.join(PENDING_LEAF_FILE).exists());
        assert_eq!(ca_count(dir), 1);
        let single = std::fs::read(dir.join(CA_FILE)).unwrap();
        assert!(served_leaf(served, &single).await == pending);
    }
}
