use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;

use rustls::pki_types::CertificateDer;
use time::{Duration, OffsetDateTime};

use super::certs::{self, Pair};
use super::files::{self, CA_FILE, KEY_FILE, LEAF_FILE, MANIFEST_FILE};
use super::{INTERCEPTED_HOST, lock, manifest};
use crate::logging::create_logger;

const RENEW_BEFORE: Duration = Duration::days(14);
const WARN_EVERY: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
const PORT_CHECK: std::time::Duration = std::time::Duration::from_secs(1);
const PAIR_FILES: [&str; 3] = [KEY_FILE, LEAF_FILE, CA_FILE];

pub(super) const SOON: &str = "the intercept certificate expires within 14 days; stop `duolane serve`, run `duolane intercept renew`, start `duolane serve --intercept`, then restart Claude Code sessions";
pub(super) const EXPIRED: &str = "the intercept certificate has expired or is not valid; restart `duolane serve --intercept`, then restart Claude Code sessions";
const REPLACED: &str = "replaced the intercept certificate because it had expired or could not be read; restart Claude Code sessions so they trust the new one";
const SOON_NOTICE: &str =
    "intercept certificate expires within 14 days: run duolane intercept renew";
const REPLACED_NOTICE: &str = "intercept certificate replaced: restart Claude Code sessions";
const NOTHING_TO_RENEW: &str = "no intercept certificate to renew; start `duolane serve --intercept` or run `duolane intercept setup` first";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Due {
    Fine,
    Soon,
    Expired,
}

pub(super) struct Loaded {
    pub(super) pair: Pair,
    pub(super) ca: CertificateDer<'static>,
    pub(super) replaced: bool,
}

pub(super) fn due(
    authorities: &[CertificateDer<'_>],
    leaf: &CertificateDer<'_>,
    now: OffsetDateTime,
) -> Due {
    let lasts = |at| {
        authorities
            .iter()
            .any(|ca| certs::verify(ca, leaf, INTERCEPTED_HOST, at).is_ok())
    };
    if !lasts(now) {
        Due::Expired
    } else if !lasts(now.saturating_add(RENEW_BEFORE)) {
        Due::Soon
    } else {
        Due::Fine
    }
}

pub(super) fn load(
    dir: &Path,
    now: OffsetDateTime,
    port_check: Option<u16>,
) -> anyhow::Result<Loaded> {
    let ca = files::read(&dir.join(CA_FILE))?;
    let leaf = files::read(&dir.join(LEAF_FILE))?;
    let key = files::read(&dir.join(KEY_FILE))?;
    let existed = ca.is_some() || leaf.is_some() || key.is_some();
    if let (Some(ca), Some(leaf), Some(key)) = (ca, leaf, key)
        && let Some((pair, ca)) = certs::usable(&certs::authorities(&ca), &leaf, &key, now)
    {
        return Ok(Loaded {
            pair,
            ca,
            replaced: false,
        });
    }
    if existed && let Some(port) = port_check {
        check_port_free(port)?;
    }
    let (pair, ca) = write_pair(dir, now)?;
    Ok(Loaded {
        pair,
        ca,
        replaced: existed,
    })
}

fn write_pair(dir: &Path, now: OffsetDateTime) -> anyhow::Result<(Pair, CertificateDer<'static>)> {
    let created = certs::create(now)?;
    files::write(&dir.join(KEY_FILE), created.key_pem.as_bytes())?;
    files::write(&dir.join(LEAF_FILE), created.leaf_pem.as_bytes())?;
    files::write(&dir.join(CA_FILE), created.ca_pem.as_bytes())?;
    certs::usable(
        &certs::authorities(created.ca_pem.as_bytes()),
        created.leaf_pem.as_bytes(),
        created.key_pem.as_bytes(),
        now,
    )
    .ok_or_else(|| anyhow::anyhow!("the new intercept certificate does not verify"))
}

pub(super) fn notices(due: Due, replaced: bool) -> Vec<&'static str> {
    let mut notices = Vec::new();
    if replaced {
        notices.push(REPLACED_NOTICE);
    }
    if due == Due::Soon {
        notices.push(SOON_NOTICE);
    }
    notices
}

pub(super) fn report_replaced(loaded: &Loaded) {
    if loaded.replaced {
        create_logger("intercept").warn(REPLACED, None);
    }
}

pub(super) async fn watch(ca: CertificateDer<'static>, leaf: CertificateDer<'static>) {
    let logger = create_logger("intercept");
    loop {
        match due(std::slice::from_ref(&ca), &leaf, OffsetDateTime::now_utc()) {
            Due::Soon => logger.warn(SOON, None),
            Due::Expired => logger.warn(EXPIRED, None),
            Due::Fine => {}
        }
        tokio::time::sleep(WARN_EVERY).await;
    }
}

pub fn renew() -> anyhow::Result<()> {
    let dir = super::intercept_dir();
    let _lock = lock::acquire(&dir, lock::WAIT)?;
    let mut present = false;
    for name in PAIR_FILES {
        present |= exists(&dir.join(name))?;
    }
    if !present {
        anyhow::bail!(NOTHING_TO_RENEW);
    }
    check_port_free(super::port())?;
    files::ensure_dir(&dir)?;
    write_pair(&dir, OffsetDateTime::now_utc())?;
    let manifest = manifest::refresh_files(&dir)?;
    for name in PAIR_FILES {
        println!("wrote {name}");
    }
    if manifest {
        println!("wrote {MANIFEST_FILE}");
    }
    println!("Start `duolane serve --intercept`, then restart Claude Code sessions.");
    Ok(())
}

fn check_port_free(port: u16) -> anyhow::Result<()> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    if TcpStream::connect_timeout(&address, PORT_CHECK).is_ok() {
        anyhow::bail!(
            "something accepts connections on {address}; stop `duolane serve --intercept`, or whatever holds that port, then run `duolane intercept renew` again"
        );
    }
    Ok(())
}

fn exists(path: &Path) -> anyhow::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => anyhow::bail!("cannot inspect {}: {error}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::pem::PemObject;

    #[test]
    fn renewal_is_due_two_weeks_before_expiry_and_overdue_after_it() {
        let created = OffsetDateTime::now_utc();
        let pair = certs::create(created).unwrap();
        let authorities = certs::authorities(pair.ca_pem.as_bytes());
        let leaf = CertificateDer::from_pem_slice(pair.leaf_pem.as_bytes()).unwrap();
        let at = |days| due(&authorities, &leaf, created + Duration::days(days));
        assert_eq!(at(0), Due::Fine);
        assert_eq!(at(77), Due::Soon);
        assert_eq!(at(91), Due::Expired);
    }

    #[test]
    fn the_dashboard_header_names_a_replacement_and_a_near_expiry() {
        assert!(notices(Due::Fine, false).is_empty());
        assert_eq!(notices(Due::Soon, false), [SOON_NOTICE]);
        assert_eq!(notices(Due::Fine, true), [REPLACED_NOTICE]);
    }

    #[test]
    fn loading_keeps_a_usable_pair_and_replaces_an_expired_one() {
        let root = tempfile::TempDir::new().unwrap();
        let dir = root.path().join("intercept");
        files::ensure_dir(&dir).unwrap();
        let now = OffsetDateTime::now_utc();
        let first = load(&dir, now - Duration::days(100), None).unwrap();
        assert!(!first.replaced);

        let kept = load(&dir, now - Duration::days(30), None).unwrap();
        assert!(!kept.replaced);
        assert!(kept.pair.leaf == first.pair.leaf);

        let renewed = load(&dir, now, None).unwrap();
        assert!(renewed.replaced);
        assert!(renewed.pair.leaf != first.pair.leaf);
        let ca = std::fs::read(dir.join(CA_FILE)).unwrap();
        assert_eq!(
            due(&certs::authorities(&ca), &renewed.pair.leaf, now),
            Due::Fine
        );
    }
}
