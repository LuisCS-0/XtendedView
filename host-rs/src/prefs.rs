//! Ajustes que se eligen en el panel del ordenador y se guardan entre sesiones
//! (certs/ajustes.json). Los dispositivos solo muestran la pantalla.
//!
//! - Generales: qué conexiones están activas y si se mide la latencia.
//! - Por dispositivo (cada uno con su identificador, que guarda su navegador): modo, lado,
//!   monitor, fotogramas, calidad, resolución, pantalla completa y estadísticas. Un dispositivo
//!   nuevo empieza con los valores por defecto.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Formas de conectar un dispositivo.
pub const CONNECTIONS: [&str; 3] = ["local", "own", "cable"];

/// Un dispositivo bloqueado: con qué nombre y desde dónde pidió conectarse, y cuándo.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Blocked {
    pub name: String,
    pub ip: String,
    pub since: String,
}

/// Temas del panel.
pub const THEMES: [&str; 3] = ["system", "light", "dark"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DevicePrefs {
    /// Nombre con el que se vio por última vez ("SM-F971B · Android · Chrome").
    pub name: String,
    /// "extend": pantalla adicional. "mirror": espejo de un monitor.
    pub mode: String,
    /// Lado del ordenador en el que se coloca la primera vez: "left", "right", "top", "bottom".
    pub side: String,
    /// Posición elegida arrastrando su pantalla en el panel (esquina superior izquierda, en
    /// píxeles del escritorio de Windows). Sin ella, se coloca en `side`.
    pub position: Option<[i32; 2]>,
    /// Monitor que se duplica en modo espejo.
    pub monitor: usize,
    /// Fotogramas por segundo; 0 = los de la pantalla del dispositivo.
    pub fps: u32,
    /// Calidad en Mbps.
    pub bitrate: u32,
    /// Resolución respecto a la pantalla del dispositivo (1, 0.75, 0.5).
    pub scale: f64,
    /// Escala de Windows de su pantalla adicional, en % (iconos, textos y barras más grandes sin
    /// bajar la resolución). 0 = la que recomiende Windows.
    pub dpi: u32,
    /// Pantalla completa en el dispositivo.
    pub fullscreen: bool,
    /// Estadísticas encima de la imagen, en el dispositivo.
    pub stats: bool,
}

impl Default for DevicePrefs {
    fn default() -> Self {
        Self {
            name: String::new(),
            mode: "extend".into(),
            side: "right".into(),
            position: None,
            monitor: 0,
            fps: 0,
            bitrate: 20,
            scale: 1.0,
            dpi: 0,
            fullscreen: true,
            stats: false,
        }
    }
}

impl DevicePrefs {
    /// Corrige valores fuera de rango (por ejemplo, si se editó el fichero a mano).
    pub fn sanitized(mut self) -> Self {
        let d = DevicePrefs::default();
        if !["extend", "mirror"].contains(&self.mode.as_str()) {
            self.mode = d.mode;
        }
        if !["left", "right", "top", "bottom"].contains(&self.side.as_str()) {
            self.side = d.side;
        }
        if ![0, 30, 60, 90, 120].contains(&self.fps) {
            self.fps = 0;
        }
        if self.position.is_some_and(|p| p.iter().any(|v| v.abs() > 32_000)) {
            self.position = None;
        }
        self.bitrate = self.bitrate.clamp(2, 80);
        if ![1.0, 0.75, 0.5].contains(&self.scale) {
            self.scale = 1.0;
        }
        if ![0, 100, 125, 150, 175, 200, 225, 250, 300].contains(&self.dpi) {
            self.dpi = 0;
        }
        self.name = self.name.chars().take(80).collect();
        self
    }

    /// Aplica un cambio del panel. Devuelve false si el valor no es válido.
    pub fn set(&mut self, key: &str, v: &serde_json::Value) -> bool {
        let before = self.clone();
        match key {
            "mode" => self.mode = v.as_str().unwrap_or("").into(),
            "side" => self.side = v.as_str().unwrap_or("").into(),
            // [x, y], o null para volver a colocarlo solo.
            "position" => {
                self.position = match v.as_array().map(|a| a.iter().map(|n| n.as_i64()).collect::<Vec<_>>()) {
                    Some(xy) if xy.len() == 2 && xy.iter().all(Option::is_some) => {
                        Some([xy[0].unwrap_or(0) as i32, xy[1].unwrap_or(0) as i32])
                    }
                    _ if v.is_null() => None,
                    _ => return false,
                }
            }
            "fps" => self.fps = v.as_u64().unwrap_or(99) as u32,
            "bitrate" => self.bitrate = v.as_u64().unwrap_or(0) as u32,
            "scale" => self.scale = v.as_f64().unwrap_or(0.0),
            "dpi" => self.dpi = v.as_u64().unwrap_or(1) as u32,
            "fullscreen" => self.fullscreen = v.as_bool().unwrap_or(self.fullscreen),
            "stats" => self.stats = v.as_bool().unwrap_or(self.stats),
            "monitor" => self.monitor = v.as_u64().unwrap_or(0) as usize,
            _ => return false,
        }
        if self.clone().sanitized() != *self {
            *self = before; // valor inválido: se deja como estaba
            return false;
        }
        true
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Conexiones activas: "local" (la red del ordenador), "own" (red Wi-Fi propia) y "cable"
    /// (anclaje USB o Ethernet directo). Puede haber varias a la vez.
    pub connections: Vec<String>,
    /// Medir la latencia total con la sonda (en todos los dispositivos).
    pub measure: bool,
    /// «Imagen más suave» para los dispositivos conectados por Wi-Fi (la red del ordenador o la
    /// red propia): el visor retiene unos milisegundos de imágenes para tapar los parones cortos.
    /// Uno de los niveles de `SMOOTHING`.
    pub smoothing: String,
    /// Tema del panel: "system" (el de Windows), "light" u "dark".
    pub theme: String,
    /// Al cerrar el panel, apagar (quitar monitores virtuales, cerrar la red propia) y salir del
    /// todo. Si no, sigue junto al reloj.
    pub quit_on_close: bool,
    /// Dispositivos bloqueados desde el panel, por su identificador: no pueden ni pedir conexión.
    pub blocked: BTreeMap<String, Blocked>,
    /// Ajustes de cada dispositivo, por su identificador.
    pub devices: BTreeMap<String, DevicePrefs>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            connections: vec!["local".into()],
            measure: false,
            smoothing: "off".into(),
            theme: "system".into(),
            quit_on_close: true,
            blocked: BTreeMap::new(),
            devices: BTreeMap::new(),
        }
    }
}

/// Niveles de «Imagen más suave» y cuántos milisegundos de imágenes retiene el visor en cada uno:
/// cuanto más, más largos los parones de la Wi-Fi que tapa, y más retraso.
/// Más de 100 ms ya se nota demasiado al usar el ratón.
pub const SMOOTHING: [(&str, u32); 4] = [("off", 0), ("low", 25), ("medium", 50), ("high", 100)];

impl Prefs {
    /// Milisegundos que retiene el visor con el nivel elegido (0 si está desactivado).
    pub fn smoothing_ms(&self) -> u32 {
        SMOOTHING.iter().find(|(name, _)| *name == self.smoothing).map_or(0, |(_, ms)| *ms)
    }

    pub fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        serde_json::from_str::<Prefs>(&text).unwrap_or_default().sanitized()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, s);
        }
    }

    pub fn sanitized(mut self) -> Self {
        self.connections.retain(|c| CONNECTIONS.contains(&c.as_str()));
        self.connections.dedup();
        self.devices = std::mem::take(&mut self.devices).into_iter().map(|(k, v)| (k, v.sanitized())).collect();
        self
    }

    pub fn has(&self, connection: &str) -> bool {
        self.connections.iter().any(|c| c == connection)
    }

    /// Activa o desactiva una conexión. Devuelve false si no existe.
    pub fn set_connection(&mut self, connection: &str, enabled: bool) -> bool {
        if !CONNECTIONS.contains(&connection) {
            return false;
        }
        self.connections.retain(|c| c != connection);
        if enabled {
            self.connections.push(connection.into());
            // Siempre en el mismo orden, para el panel.
            self.connections.sort_by_key(|c| CONNECTIONS.iter().position(|k| k == c));
        }
        true
    }

    /// Ajustes de un dispositivo: los suyos o, si es nuevo, los de partida (los guardados con
    /// clave vacía, si los hay; si no, los de serie).
    pub fn device(&self, id: &str) -> DevicePrefs {
        self.devices
            .get(id)
            .or_else(|| self.devices.get(""))
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_device(&mut self, id: &str, key: &str, v: &serde_json::Value) -> bool {
        let mut d = self.device(id);
        if !d.set(key, v) {
            return false;
        }
        self.devices.insert(id.into(), d);
        true
    }

    /// Guarda el nombre con el que se ha visto el dispositivo (y crea sus ajustes si es nuevo).
    pub fn remember_device(&mut self, id: &str, name: &str, side: Option<&str>) {
        let mut d = self.device(id);
        let new = !self.devices.contains_key(id);
        d.name = name.chars().take(80).collect();
        if let (true, Some(s)) = (new, side) {
            d.side = s.into(); // un lado libre para que no se tapen
        }
        self.devices.insert(id.into(), d);
    }
}

pub fn path(data: &Path) -> PathBuf {
    data.join("ajustes.json")
}

/// Código de emparejamiento (certs/pairing.txt). Con `renew` se genera uno nuevo.
pub fn pairing_key(data: &Path, renew: bool) -> anyhow::Result<String> {
    use rand::Rng;
    let path = data.join("pairing.txt");
    if !renew {
        if let Ok(k) = std::fs::read_to_string(&path) {
            let k = k.trim().to_string();
            if k.len() == 8 {
                return Ok(k);
            }
        }
    }
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let key: String = (0..8).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect();
    std::fs::write(&path, &key)?;
    Ok(key)
}

pub fn pretty_key(key: &str) -> String {
    if key.len() == 8 {
        format!("{}-{}", &key[..4], &key[4..])
    } else {
        key.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_keep_their_own_settings() {
        let mut p = Prefs::default();
        assert!(p.set_device("a", "mode", &serde_json::json!("mirror")));
        assert!(p.set_device("b", "side", &serde_json::json!("left")));
        assert!(!p.set_device("b", "fps", &serde_json::json!(75)));
        assert_eq!(p.device("a").mode, "mirror");
        assert_eq!(p.device("b").mode, "extend");
        assert_eq!(p.device("b").side, "left");
        assert!(p.set_connection("own", true));
        assert!(p.set_connection("local", false));
        assert_eq!(p.connections, vec!["own".to_string()]);
        // Posición arrastrada en el panel, y vuelta a la automática.
        assert!(p.set_device("a", "position", &serde_json::json!([2880, -120])));
        assert_eq!(p.device("a").position, Some([2880, -120]));
        assert!(!p.set_device("a", "position", &serde_json::json!(["x", 1])));
        assert!(!p.set_device("a", "position", &serde_json::json!([99999, 0])));
        assert!(p.set_device("a", "position", &serde_json::Value::Null));
        assert_eq!(p.device("a").position, None);
    }

    #[test]
    fn smoothing_levels_and_unknown_options() {
        // Un ajuste que no conoce (de otra versión, o escrito a mano) se ignora sin perder lo demás.
        let p: Prefs = serde_json::from_str(r#"{"measure":true,"smooth_wifi":true,"devices":{"a":{"mode":"mirror"}}}"#).unwrap();
        assert!(p.measure);
        assert_eq!(p.device("a").mode, "mirror");
        assert_eq!((p.smoothing.as_str(), p.smoothing_ms()), ("off", 0));
        let ms = |level: &str| Prefs { smoothing: level.into(), ..Prefs::default() }.smoothing_ms();
        assert_eq!((ms("low"), ms("medium"), ms("high"), ms("raro")), (25, 50, 100, 0));
    }
}
