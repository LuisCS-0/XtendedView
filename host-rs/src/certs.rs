//! Certificados HTTPS locales.
//!
//! - Una autoridad (CA) propia que se crea una sola vez y se instala en cada dispositivo.
//! - Un certificado de servidor firmado por esa CA con las IP actuales del portátil; se
//!   vuelve a emitir si aparece una IP nueva (por ejemplo, al activar el hotspot).
//!
//! Es compatible con los ficheros que creó la versión de Python (carpeta `certs/`), así que
//! los dispositivos que ya confiaban en esa CA siguen funcionando.
use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyIdMethod,
    KeyPair, KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::BTreeSet;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::path::{Path, PathBuf};

/// IP habitual del portátil cuando comparte su propia red Wi-Fi.
pub const HOTSPOT_IPS: [Ipv4Addr; 1] = [Ipv4Addr::new(192, 168, 137, 1)];

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into())
}

pub fn local_ipv4s() -> Vec<Ipv4Addr> {
    let mut set = BTreeSet::new();
    if let Ok(addrs) = (hostname().as_str(), 0).to_socket_addrs() {
        for a in addrs {
            if let IpAddr::V4(v4) = a.ip() {
                set.insert(v4);
            }
        }
    }
    set.into_iter().collect()
}

/// IP a las que tiene sentido conectarse desde otro dispositivo.
pub fn usable_ips() -> Vec<Ipv4Addr> {
    local_ipv4s()
        .into_iter()
        .filter(|ip| !ip.is_loopback() && !ip.is_link_local())
        .collect()
}

fn is_dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// Identificador de clave al estilo de OpenSSL ("hash"): SHA-1 de la clave pública.
fn sha1_key_id(key: &KeyPair) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, key.public_key_raw())
        .as_ref()
        .to_vec()
}

pub struct CertManager {
    pub ca_crt: PathBuf,
    ca_key: PathBuf,
    crt: PathBuf,
    key: PathBuf,
    ips_file: PathBuf,
}

impl CertManager {
    pub fn new(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            ca_crt: dir.join("ca.crt"),
            ca_key: dir.join("ca.key"),
            crt: dir.join("server.crt"),
            key: dir.join("server.key"),
            ips_file: dir.join("server.ips"),
        })
    }

    fn wanted_ips(&self) -> BTreeSet<Ipv4Addr> {
        let mut s: BTreeSet<Ipv4Addr> = local_ipv4s().into_iter().collect();
        s.insert(Ipv4Addr::LOCALHOST);
        s.extend(HOTSPOT_IPS);
        s
    }

    /// Crea lo que falte. Devuelve true si el certificado del servidor ha cambiado.
    pub fn ensure(&self) -> Result<bool> {
        if !self.ca_crt.exists() || !self.ca_key.exists() {
            self.create_ca()?;
            let _ = fs::remove_file(&self.crt);
        }
        let current: BTreeSet<Ipv4Addr> = fs::read_to_string(&self.ips_file)
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        let wanted = self.wanted_ips();
        if self.crt.exists() && self.key.exists() && wanted.is_subset(&current) {
            return Ok(false);
        }
        let all: BTreeSet<Ipv4Addr> = wanted.union(&current).copied().collect();
        self.create_leaf(&all)?;
        Ok(true)
    }

    fn validity(p: &mut CertificateParams, days: i64) {
        let now = time::OffsetDateTime::now_utc();
        p.not_before = now - time::Duration::days(1);
        p.not_after = now + time::Duration::days(days);
    }

    fn create_ca(&self) -> Result<()> {
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::default();
        p.distinguished_name
            .push(DnType::CommonName, format!("Pantalla PoC CA ({})", hostname()));
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        p.key_identifier_method = KeyIdMethod::PreSpecified(sha1_key_id(&key));
        Self::validity(&mut p, 3650);
        let cert = p.self_signed(&key)?;
        fs::write(&self.ca_crt, cert.pem())?;
        fs::write(&self.ca_key, key.serialize_pem())?;
        Ok(())
    }

    fn create_leaf(&self, ips: &BTreeSet<Ipv4Addr>) -> Result<()> {
        let ca_key = KeyPair::from_pem(&fs::read_to_string(&self.ca_key)?)
            .context("no se pudo leer certs/ca.key")?;
        let mut ca_params = CertificateParams::from_ca_cert_pem(&fs::read_to_string(&self.ca_crt)?)
            .context("no se pudo leer certs/ca.crt")?;
        // Mismo identificador de clave que pone OpenSSL, para encadenar con la CA de Python.
        ca_params.key_identifier_method = KeyIdMethod::PreSpecified(sha1_key_id(&ca_key));
        let ca = ca_params.self_signed(&ca_key)?;

        let key = KeyPair::generate()?;
        let mut p = CertificateParams::default();
        p.distinguished_name.push(DnType::CommonName, "pantalla-poc");
        p.subject_alt_names.push(SanType::DnsName("localhost".try_into()?));
        let host = hostname();
        if is_dns_label(&host) {
            p.subject_alt_names.push(SanType::DnsName(host.clone().try_into()?));
            p.subject_alt_names.push(SanType::DnsName(format!("{host}.local").try_into()?));
        }
        for ip in ips {
            p.subject_alt_names.push(SanType::IpAddress(IpAddr::V4(*ip)));
        }
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.use_authority_key_identifier_extension = true;
        Self::validity(&mut p, 397);
        let cert = p.signed_by(&key, &ca, &ca_key)?;
        fs::write(&self.crt, cert.pem())?;
        fs::write(&self.key, key.serialize_pem())?;
        let list: Vec<String> = ips.iter().map(|ip| ip.to_string()).collect();
        fs::write(&self.ips_file, list.join(" "))?;
        Ok(())
    }

    pub fn load(&self) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
        let certs = rustls_pemfile::certs(&mut fs::read(&self.crt)?.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .context("certificado del servidor ilegible")?;
        let key = rustls_pemfile::private_key(&mut fs::read(&self.key)?.as_slice())?
            .context("clave del servidor ilegible")?;
        Ok((certs, key))
    }

    /// Huella SHA-256 de la CA, para comprobarla en el dispositivo al instalarla.
    pub fn fingerprint(&self) -> Result<String> {
        let der = rustls_pemfile::certs(&mut fs::read(&self.ca_crt)?.as_slice())
            .next()
            .context("CA vacía")??;
        let digest = ring::digest::digest(&ring::digest::SHA256, der.as_ref());
        Ok(digest
            .as_ref()
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":"))
    }
}
