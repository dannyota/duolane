use std::path::Path;
use std::sync::{Arc, RwLock};

use rustls::pki_types::CertificateDer;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde_json::json;
use time::{Duration, OffsetDateTime};
use tokio::sync::Notify;

use super::certs::{self, Pair};
use super::files::{self, CA_FILE, KEY_FILE, LEAF_FILE, PENDING_KEY_FILE, PENDING_LEAF_FILE};
use super::{INTERCEPTED_HOST, lock, manifest};
use crate::logging::create_logger;

const RENEW_BEFORE: Duration = Duration::days(14);
const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

type Loaded = Option<(Pair, CertificateDer<'static>)>;

#[derive(Debug)]
struct Current {
    key: Arc<CertifiedKey>,
    expiry: Option<OffsetDateTime>,
}

impl Current {
    fn new(pair: Pair) -> anyhow::Result<Self> {
        Ok(Self {
            key: Arc::new(CertifiedKey::from_der(
                vec![pair.leaf],
                pair.key,
                &rustls::crypto::ring::default_provider(),
            )?),
            expiry: pair.expiry,
        })
    }
}

#[derive(Debug)]
pub(super) struct Served {
    current: RwLock<Current>,
    expired: Notify,
}

impl Served {
    pub(super) fn new(pair: Pair) -> anyhow::Result<Self> {
        Ok(Self {
            current: RwLock::new(Current::new(pair)?),
            expired: Notify::new(),
        })
    }

    fn leaf(&self) -> Option<CertificateDer<'static>> {
        self.current.read().ok()?.key.cert.first().cloned()
    }

    fn expiry(&self) -> Option<OffsetDateTime> {
        self.current.read().ok()?.expiry
    }

    fn replace(&self, pair: Pair) -> anyhow::Result<bool> {
        if self.leaf().as_ref() == Some(&pair.leaf) {
            return Ok(false);
        }
        let current = Current::new(pair)?;
        let mut served = self
            .current
            .write()
            .map_err(|_| anyhow::anyhow!("the served intercept certificate is unavailable"))?;
        *served = current;
        Ok(true)
    }
}

impl ResolvesServerCert for Served {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let current = self.current.read().ok()?;
        if current
            .expiry
            .is_some_and(|expiry| expiry < OffsetDateTime::now_utc())
        {
            self.expired.notify_one();
        }
        Some(current.key.clone())
    }
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
        (Some((pair, ca)), Some((next, _))) if next.leaf == pair.leaf => {
            finish_promotion(dir, &ca)?;
            pair
        }
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
            copy(dir, PENDING_KEY_FILE, KEY_FILE)?;
            copy(dir, PENDING_LEAF_FILE, LEAF_FILE)?;
            finish_promotion(dir, &ca)?;
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

fn copy(dir: &Path, from: &str, to: &str) -> anyhow::Result<()> {
    let bytes = files::read(&dir.join(from))?
        .ok_or_else(|| anyhow::anyhow!("{from} is missing in {}", dir.display()))?;
    files::write(&dir.join(to), &bytes)
}

fn finish_promotion(dir: &Path, ca: &CertificateDer<'_>) -> anyhow::Result<()> {
    files::write(&dir.join(CA_FILE), certs::pem(ca).as_bytes())?;
    files::remove(&dir.join(PENDING_LEAF_FILE))?;
    files::remove(&dir.join(PENDING_KEY_FILE))?;
    Ok(())
}

pub(super) fn rotate(dir: &Path, now: OffsetDateTime, served: &Served) -> anyhow::Result<bool> {
    let _lock = lock::acquire(dir, lock::WAIT)?;
    if !dir.is_dir() {
        return Ok(false);
    }
    served.replace(refresh(dir, now)?)
}

fn delay(now: OffsetDateTime, expiry: Option<OffsetDateTime>) -> std::time::Duration {
    let Some(expiry) = expiry else {
        return CHECK_EVERY;
    };
    match std::time::Duration::try_from(expiry.saturating_add(Duration::SECOND) - now) {
        Ok(until) if !until.is_zero() => until.min(CHECK_EVERY),
        _ => RETRY_AFTER,
    }
}

pub(super) async fn run(served: Arc<Served>) {
    loop {
        let wait = delay(OffsetDateTime::now_utc(), served.expiry());
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = served.expired.notified() => {}
        }
        let checking = served.clone();
        let checked = tokio::task::spawn_blocking(move || {
            rotate(
                &super::intercept_dir(),
                OffsetDateTime::now_utc(),
                &checking,
            )
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

    fn renewing(root: &Path, now: OffsetDateTime) -> std::path::PathBuf {
        let dir = root.join("intercept");
        refresh(&dir, now - Duration::days(77)).unwrap();
        refresh(&dir, now).unwrap();
        assert!(dir.join(PENDING_LEAF_FILE).exists());
        assert_eq!(ca_count(&dir), 2);
        dir
    }

    fn crash_copy(dir: &Path, from: &str, to: &str) {
        std::fs::copy(dir.join(from), dir.join(to)).unwrap();
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
        let root = tempfile::TempDir::new().unwrap();
        let dir = &root.path().join("intercept");
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

    #[test]
    fn checks_hourly_and_just_after_the_served_leaf_expires() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(delay(now, Some(now + Duration::days(30))), CHECK_EVERY);
        let soon = delay(now, Some(now + Duration::minutes(10)));
        assert_eq!(soon, std::time::Duration::from_secs(601));
        assert_eq!(delay(now, Some(now - Duration::minutes(10))), RETRY_AFTER);
    }

    #[test]
    fn a_promotion_interrupted_after_the_key_was_copied_completes() {
        let root = tempfile::TempDir::new().unwrap();
        let now = OffsetDateTime::now_utc();
        let dir = renewing(root.path(), now);
        let pending_leaf = std::fs::read(dir.join(PENDING_LEAF_FILE)).unwrap();
        let pending_key = std::fs::read(dir.join(PENDING_KEY_FILE)).unwrap();
        crash_copy(&dir, PENDING_KEY_FILE, KEY_FILE);

        let promoted = refresh(&dir, now + Duration::days(14)).unwrap();
        assert!(promoted.leaf == CertificateDer::from_pem_slice(&pending_leaf).unwrap());
        assert!(std::fs::read(dir.join(LEAF_FILE)).unwrap() == pending_leaf);
        assert!(std::fs::read(dir.join(KEY_FILE)).unwrap() == pending_key);
        assert_eq!(ca_count(&dir), 1);
        assert!(!dir.join(PENDING_LEAF_FILE).exists());
        assert!(!dir.join(PENDING_KEY_FILE).exists());
    }

    #[test]
    fn a_pending_pair_equal_to_the_current_one_finishes_its_promotion() {
        let root = tempfile::TempDir::new().unwrap();
        let now = OffsetDateTime::now_utc();
        let dir = renewing(root.path(), now);
        crash_copy(&dir, PENDING_KEY_FILE, KEY_FILE);
        crash_copy(&dir, PENDING_LEAF_FILE, LEAF_FILE);

        refresh(&dir, now).unwrap();
        assert_eq!(ca_count(&dir), 1);
        assert!(!dir.join(PENDING_LEAF_FILE).exists());
        assert!(!dir.join(PENDING_KEY_FILE).exists());
    }
}
