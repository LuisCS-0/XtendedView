//! Red Wi-Fi directa del portátil, sin router ni internet.
//!
//! Usa Wi-Fi Direct en "modo legacy": el portátil emite una red normal (nombre + WPA2) a la
//! que puede unirse cualquier dispositivo, también un iPad, que no admite Wi-Fi Direct. La
//! red existe mientras exista este objeto. Si el portátil sigue conectado a otra Wi-Fi, la
//! tarjeta comparte canal con ella.
//!
//! Nombre y contraseña son aleatorios y nuevos cada vez que se crea la red: el nombre se emite
//! a todo el que esté cerca, así que no dice de quién es el ordenador ni deja seguirlo de un
//! sitio a otro, y una contraseña vista en la pantalla no sirve la siguiente vez.
use anyhow::{bail, Context, Result};
use rand::Rng;
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Devices::WiFiDirect::{WiFiDirectAdvertisementPublisher, WiFiDirectAdvertisementPublisherStatus};
use windows::Security::Credentials::PasswordCredential;

pub struct Hotspot {
    publisher: WiFiDirectAdvertisementPublisher,
    pub ssid: String,
    pub password: String,
}

fn random(alphabet: &[u8], n: usize) -> String {
    let mut rng = rand::thread_rng(); // generador criptográfico del sistema
    (0..n).map(|_| alphabet[rng.gen_range(0..alphabet.len())] as char).collect()
}

/// Nombre y contraseña nuevos.
///
/// - Nombre con el formato habitual de Wi-Fi Direct ("DIRECT-…", como impresoras y teles):
///   no revela el nombre del equipo ni el del programa.
/// - Contraseña de 16 caracteres al azar entre 32 (80 bits): inviable de adivinar aunque
///   alguien capture la conexión y la ataque sin conexión. Va en grupos de 4 para teclearla.
fn credentials() -> (String, String) {
    const UPPER: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    const LOWER: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let ssid = format!("DIRECT-{}-{}", random(UPPER, 2), random(UPPER, 6));
    let password = (0..4).map(|_| random(LOWER, 4)).collect::<Vec<_>>().join("-");
    (ssid, password)
}

impl Hotspot {
    pub fn start() -> Result<Self> {
        let (ssid, password) = credentials();
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
