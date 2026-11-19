use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints, PKCS_ECDSA_P256_SHA256,
};
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use time::{Duration, OffsetDateTime};

use super::INTERCEPTED_HOST;

const CA_NAME: &str = "duolane intercept CA";
const BACKDATE: Duration = Duration::hours(1);
const VALIDITY: Duration = Duration::days(90);

pub(super) struct Pair {
    pub(super) leaf: CertificateDer<'static>,
    pub(super) key: PrivateKeyDer<'static>,
}

pub(super) struct Created {
    pub(super) ca_pem: String,
    pub(super) leaf_pem: String,
    pub(super) key_pem: String,
}

pub(super) fn authorities(pem: &[u8]) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem)
        .filter_map(Result::ok)
        .collect()
}

pub(super) fn usable(
    authorities: &[CertificateDer<'static>],
    leaf: &[u8],
    key: &[u8],
    now: OffsetDateTime,
) -> Option<(Pair, CertificateDer<'static>)> {
    let leaf = CertificateDer::from_pem_slice(leaf).ok()?;
    let key = PrivateKeyDer::from_pem_slice(key).ok()?;
    let ca = authorities
        .iter()
        .find(|ca| verify(ca, &leaf, INTERCEPTED_HOST, now).is_ok())?
        .clone();
    rustls::sign::CertifiedKey::from_der(
        vec![leaf.clone()],
        key.clone_key(),
        &rustls::crypto::ring::default_provider(),
    )
    .ok()?;
    Some((Pair { leaf, key }, ca))
}

pub(super) fn verify(
    ca: &CertificateDer<'_>,
    leaf: &CertificateDer<'_>,
    name: &str,
    at: OffsetDateTime,
) -> anyhow::Result<()> {
    let seconds = u64::try_from(at.unix_timestamp()).unwrap_or(0);
    let at = UnixTime::since_unix_epoch(std::time::Duration::from_secs(seconds));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.clone().into_owned())?;
    let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .build()?;
    let name = ServerName::try_from(name.to_string())?;
    verifier.verify_server_cert(leaf, &[], &name, &[], at)?;
    Ok(())
}

pub(super) fn create(now: OffsetDateTime) -> anyhow::Result<Created> {
    build(now).map_err(|error| anyhow::anyhow!("cannot create the intercept certificate: {error}"))
}

fn build(now: OffsetDateTime) -> Result<Created, rcgen::Error> {
    let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let (ca_pem, leaf_pem) = {
        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let params = ca_params(now);
        let ca = params.self_signed(&ca_key)?;
        let issuer = Issuer::new(params, ca_key);
        let leaf = leaf_params(INTERCEPTED_HOST, now)?.signed_by(&leaf_key, &issuer)?;
        (ca.pem(), leaf.pem())
    };
    Ok(Created {
        ca_pem,
        leaf_pem,
        key_pem: leaf_key.serialize_pem(),
    })
}

fn ca_params(now: OffsetDateTime) -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = common_name(CA_NAME);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName(INTERCEPTED_HOST.to_string())],
        excluded_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::V4([0; 4], [0; 4])),
            GeneralSubtree::IpAddress(CidrSubnet::V6([0; 16], [0; 16])),
        ],
    });
    params.not_before = now - BACKDATE;
    params.not_after = now + VALIDITY;
    params
}

fn leaf_params(name: &str, now: OffsetDateTime) -> Result<CertificateParams, rcgen::Error> {
    let mut params = CertificateParams::new(vec![name.to_string()])?;
    params.distinguished_name = common_name(name);
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    params.not_before = now - BACKDATE;
    params.not_after = now + VALIDITY;
    Ok(params)
}

fn common_name(name: &str) -> DistinguishedName {
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::CommonName, name);
    distinguished_name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_verifies_for_the_intercepted_host_and_the_ca_signs_no_other_name() {
        let now = OffsetDateTime::now_utc();
        let created = create(now).unwrap();
        let ca = CertificateDer::from_pem_slice(created.ca_pem.as_bytes()).unwrap();
        let leaf = CertificateDer::from_pem_slice(created.leaf_pem.as_bytes()).unwrap();
        verify(&ca, &leaf, INTERCEPTED_HOST, now).unwrap();

        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let params = ca_params(now);
        let other_ca = params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(params, ca_key);
        let other_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let other_leaf = leaf_params("other.example", now)
            .unwrap()
            .signed_by(&other_key, &issuer)
            .unwrap();
        let refused = verify(other_ca.der(), other_leaf.der(), "other.example", now).unwrap_err();
        assert!(format!("{refused:?}").contains("NameConstraintViolation"));
    }
}
