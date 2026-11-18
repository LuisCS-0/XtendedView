//! Certificados HTTPS locales.
//!
//! - Una autoridad (CA) propia que se crea una sola vez y se instala en cada dispositivo.
//! - Un certificado de servidor firmado por esa CA con las IP actuales del portátil; se
//!   vuelve a emitir si aparece una IP nueva (por ejemplo, al activar el hotspot).
//!
//! La CA está limitada ("name constraints", marcadas como críticas) a direcciones privadas y a
//! `localhost`: aunque alguien robara su clave de `certs/`, los dispositivos que confían en
//! ella no aceptarían un certificado suyo para ninguna web de internet. Tampoco puede crear
//! otras CA (longitud de cadena 0) ni lleva el nombre del equipo, que vería cualquiera que se
//! conecte. Una CA guardada sin estas restricciones no se usa: se rehace al arrancar.
use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa,
    KeyIdMethod, KeyPair, KeyUsagePurpose, NameConstraints, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::BTreeSet;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::path::{Path, PathBuf};

/// Nombre de la CA tal como aparece en la lista de certificados del dispositivo (le sigue un
/// identificador al azar).
const CA_NAME: &str = "XtendedView - CA local";

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

/// Redes para las que vale la CA: privadas, de enlace local, compartidas (VPN tipo Tailscale)
/// y el propio equipo. Ninguna existe en internet.
const PERMITTED_V4: [([u8; 4], u8); 6] = [
    ([10, 0, 0, 0], 8),
    ([172, 16, 0, 0], 12),
    ([192, 168, 0, 0], 16),
    ([100, 64, 0, 0], 10),
    ([169, 254, 0, 0], 16),
    ([127, 0, 0, 0], 8),
];

/// Si la IP cabe en las redes permitidas (solo esas pueden ir en el certificado del servidor:
/// una sola fuera de ellas haría que los dispositivos lo rechazaran entero).
pub fn ip_permitted(ip: Ipv4Addr) -> bool {
    let ip = u32::from(ip);
    PERMITTED_V4.iter().any(|(net, prefix)| {
        let mask = u32::MAX.checked_shl(32 - *prefix as u32).unwrap_or(0);
        ip & mask == u32::from(Ipv4Addr::from(*net)) & mask
    })
}

fn name_constraints() -> NameConstraints {
    let mut permitted: Vec<GeneralSubtree> = PERMITTED_V4
        .iter()
        .map(|(net, prefix)| GeneralSubtree::IpAddress(CidrSubnet::from_v4_prefix(*net, *prefix)))
        .collect();
    permitted.push(GeneralSubtree::DnsName("localhost".into()));
    NameConstraints {
        permitted_subtrees: permitted,
        // No se usa IPv6: se excluye entero para que no quede ningún resquicio.
        excluded_subtrees: vec![GeneralSubtree::IpAddress(CidrSubnet::from_v6_prefix([0; 16], 0))],
    }
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
        s.retain(|ip| ip_permitted(*ip));
        s
    }

    /// La CA guardada está limitada a direcciones privadas (si no, no se usa).
    fn ca_is_current(&self) -> bool {
        fs::read_to_string(&self.ca_crt)
            .ok()
            .and_then(|pem| CertificateParams::from_ca_cert_pem(&pem).ok())
            .is_some_and(|p| p.name_constraints.is_some())
    }

    /// Crea lo que falte. Devuelve true si el certificado del servidor ha cambiado.
    pub fn ensure(&self) -> Result<bool> {
        let exists = self.ca_crt.exists() && self.ca_key.exists();
        if !exists || !self.ca_is_current() {
            if exists {
                log::warn!("La CA guardada no está limitada a direcciones privadas: se crea otra (hay que instalarla en los dispositivos)");
            }
            self.create_ca()?;
            let _ = fs::remove_file(&self.crt);
            let _ = fs::remove_file(&self.ips_file);
        }
        let current: BTreeSet<Ipv4Addr> = fs::read_to_string(&self.ips_file)
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .filter(|ip| ip_permitted(*ip))
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
        use rand::Rng;
        let key = KeyPair::generate()?;
        let mut p = CertificateParams::default();
        // Un identificador al azar para distinguirla en la lista de certificados del dispositivo
        // si hay varios ordenadores, sin decir de quién es.
        const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
        let mut rng = rand::thread_rng();
        let id: String = (0..4).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect();
        p.distinguished_name.push(DnType::CommonName, format!("{CA_NAME} {id}"));
        p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        p.name_constraints = Some(name_constraints());
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
        // El mismo identificador de clave con el que se creó la CA, para que el del servidor
        // encadene con ella.
        ca_params.key_identifier_method = KeyIdMethod::PreSpecified(sha1_key_id(&ca_key));
        let ca = ca_params.self_signed(&ca_key)?;

        let key = KeyPair::generate()?;
        let mut p = CertificateParams::default();
        // Solo direcciones (y localhost): el nombre del equipo no, porque se ve al conectarse y
        // porque la CA no permite otros nombres.
        p.distinguished_name.push(DnType::CommonName, "XtendedView");
        p.subject_alt_names.push(SanType::DnsName("localhost".try_into()?));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtendedview-certs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn only_private_ips_are_permitted() {
        for ip in ["192.168.0.15", "192.168.137.1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "100.64.0.1", "127.0.0.1", "169.254.1.1"] {
            assert!(ip_permitted(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "172.32.0.1", "100.128.0.1", "193.168.0.1", "11.0.0.1"] {
            assert!(!ip_permitted(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn new_ca_is_constrained_and_anonymous() {
        let dir = temp_dir("nueva");
        let cm = CertManager::new(&dir).unwrap();
        assert!(cm.ensure().unwrap());
        assert!(cm.ca_is_current());
        let ca = CertificateParams::from_ca_cert_pem(&fs::read_to_string(&cm.ca_crt).unwrap()).unwrap();
        assert!(matches!(ca.is_ca, IsCa::Ca(BasicConstraints::Constrained(0))));
        assert_eq!(ca.name_constraints, Some(name_constraints()));
        let ca_name = format!("{:?}", ca.distinguished_name);
        assert!(ca_name.contains(CA_NAME) && !ca_name.contains(&hostname()), "{ca_name}");
        let leaf = CertificateParams::from_ca_cert_pem(&fs::read_to_string(&cm.crt).unwrap()).unwrap();
        for san in &leaf.subject_alt_names {
            match san {
                SanType::DnsName(n) => assert_eq!(n.as_str(), "localhost"),
                SanType::IpAddress(IpAddr::V4(ip)) => assert!(ip_permitted(*ip), "{ip}"),
                other => panic!("nombre inesperado en el certificado: {other:?}"),
            }
        }
        assert!(!cm.ensure().unwrap(), "no debe rehacerse si nada ha cambiado");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unconstrained_ca_is_replaced() {
        let dir = temp_dir("sin-limites");
        let cm = CertManager::new(&dir).unwrap();
        // Una CA que podría firmar cualquier web: no se usa.
        let key = KeyPair::generate().unwrap();
        let mut p = CertificateParams::default();
        p.distinguished_name.push(DnType::CommonName, "CA sin restricciones");
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let old = p.self_signed(&key).unwrap();
        fs::write(&cm.ca_crt, old.pem()).unwrap();
        fs::write(&cm.ca_key, key.serialize_pem()).unwrap();
        assert!(!cm.ca_is_current());
        assert!(cm.ensure().unwrap());
        assert!(cm.ca_is_current());
        assert_ne!(fs::read_to_string(&cm.ca_crt).unwrap(), old.pem());
        let _ = fs::remove_dir_all(&dir);
    }
}
