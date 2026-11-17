//! Red Wi-Fi directa del portátil, sin router ni internet.
//!
//! Usa Wi-Fi Direct en "modo legacy": el portátil emite una red normal (nombre + WPA2) a la
//! que puede unirse cualquier dispositivo, también un iPad, que no admite Wi-Fi Direct. La
//! red existe mientras exista este objeto. Si el portátil sigue conectado a otra Wi-Fi, la
//! tarjeta comparte canal con ella.
use anyhow::{bail, Context, Result};
use rand::Rng;
use std::path::Path;
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Devices::WiFiDirect::{WiFiDirectAdvertisementPublisher, WiFiDirectAdvertisementPublisherStatus};
use windows::Security::Credentials::PasswordCredential;

pub struct Hotspot {
    publisher: WiFiDirectAdvertisementPublisher,
    pub ssid: String,
    pub password: String,
}

/// Nombre y contraseña guardados en certs/hotspot.txt (el mismo formato que
/// tools/hotspot.ps1), para que los dispositivos recuerden la red.
fn credentials(file: &Path) -> Result<(String, String)> {
    let saved = std::fs::read_to_string(file).unwrap_or_default();
    let get = |k: &str| saved.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(str::to_string);
    let ssid = get("ssid").unwrap_or_else(|| {
        let host: String = crate::certs::hostname().chars().take(23).collect();
        format!("Pantalla-{host}")
    });
    let password = get("password").filter(|p| p.len() >= 8).unwrap_or_else(|| {
        const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
        let mut rng = rand::thread_rng();
        (0..12).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
    });
    std::fs::write(file, format!("ssid={ssid}\npassword={password}\n"))?;
    Ok((ssid, password))
}

impl Hotspot {
    pub fn start(file: &Path) -> Result<Self> {
        let (ssid, password) = credentials(file)?;
        let publisher = WiFiDirectAdvertisementPublisher::new()?;
        let adv = publisher.Advertisement()?;
        adv.SetIsAutonomousGroupOwnerEnabled(true)?;
        let legacy = adv.LegacySettings()?;
        legacy.SetIsEnabled(true)?;
        legacy.SetSsid(&HSTRING::from(&ssid))?;
        let cred = PasswordCredential::new()?;
        cred.SetPassword(&HSTRING::from(&password))?;
        legacy.SetPassphrase(&cred)?;
        publisher.Start().context("no se pudo crear la red Wi-Fi directa")?;
        let start = Instant::now();
        loop {
            match publisher.Status()? {
                WiFiDirectAdvertisementPublisherStatus::Started => break,
                WiFiDirectAdvertisementPublisherStatus::Created if start.elapsed() < Duration::from_secs(10) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                s => bail!("la red Wi-Fi directa no ha arrancado (estado {s:?}). ¿Está activado el Wi-Fi?"),
            }
        }
        Ok(Self { publisher, ssid, password })
    }

    /// Texto del QR estándar de Wi-Fi (la cámara del móvil lo entiende sin apps).
    pub fn wifi_qr_text(&self) -> String {
        format!("WIFI:T:WPA;S:{};P:{};;", self.ssid, self.password)
    }

    /// La red funciona de verdad: Windows puede seguir diciendo "Started" después de que la red
    /// haya desaparecido (por ejemplo, al desconectar el portátil de otra Wi-Fi), así que se
    /// comprueba que el portátil tenga su dirección en esa red (192.168.137.x).
    pub fn healthy(&self) -> bool {
        let started = matches!(self.publisher.Status(), Ok(WiFiDirectAdvertisementPublisherStatus::Started));
        started && crate::certs::local_ipv4s().iter().any(|ip| ip.octets()[..3] == [192, 168, 137])
    }

    /// Para y vuelve a crear la red.
    pub fn restart(&self) -> Result<()> {
        let _ = self.publisher.Stop();
        std::thread::sleep(Duration::from_millis(500));
        self.publisher.Start()?;
        Ok(())
    }
}

impl Drop for Hotspot {
    fn drop(&mut self) {
        let _ = self.publisher.Stop();
    }
}
