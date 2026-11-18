//! Ajustes que se eligen en el panel del ordenador y se guardan entre sesiones
//! (certs/ajustes.json). El dispositivo conectado solo muestra la pantalla.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// "local": la red a la que ya está conectado el ordenador. "own": red Wi-Fi propia.
    pub network: String,
    /// "extend": pantalla adicional. "mirror": espejo de un monitor.
    pub mode: String,
    /// Lado del ordenador en el que está el dispositivo: "left", "right", "top", "bottom".
    pub side: String,
    /// Fotogramas por segundo; 0 = los de la pantalla del dispositivo.
    pub fps: u32,
    /// Calidad en Mbps.
    pub bitrate: u32,
    /// Resolución respecto a la pantalla del dispositivo (1, 0.75, 0.5).
    pub scale: f64,
    /// Pantalla completa en el dispositivo.
    pub fullscreen: bool,
    /// Estadísticas encima de la imagen, en el dispositivo.
    pub stats: bool,
    /// Medir la latencia total con la sonda.
    pub measure: bool,
    /// Monitor que se duplica en modo espejo.
    pub monitor: usize,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            network: "local".into(),
            mode: "extend".into(),
            side: "right".into(),
            fps: 0,
            bitrate: 20,
            scale: 1.0,
            fullscreen: true,
            stats: false,
            measure: false,
            monitor: 0,
        }
    }
}

impl Prefs {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Prefs>(&s).ok())
            .map(Prefs::sanitized)
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, s);
        }
    }

    /// Corrige valores fuera de rango (fichero editado a mano, versiones antiguas…).
    pub fn sanitized(mut self) -> Self {
        let d = Prefs::default();
        if !["local", "own"].contains(&self.network.as_str()) {
            self.network = d.network;
        }
        if !["extend", "mirror"].contains(&self.mode.as_str()) {
            self.mode = d.mode;
        }
        if !["left", "right", "top", "bottom"].contains(&self.side.as_str()) {
            self.side = d.side;
        }
        if ![0, 30, 60, 90, 120].contains(&self.fps) {
            self.fps = 0;
        }
        self.bitrate = self.bitrate.clamp(2, 80);
        if ![1.0, 0.75, 0.5].contains(&self.scale) {
            self.scale = 1.0;
        }
        self
    }

    /// Aplica un cambio enviado por el panel. Devuelve false si el valor no es válido.
    pub fn set(&mut self, key: &str, v: &serde_json::Value) -> bool {
        let before = self.clone();
        match key {
            "network" => self.network = v.as_str().unwrap_or("").into(),
            "mode" => self.mode = v.as_str().unwrap_or("").into(),
            "side" => self.side = v.as_str().unwrap_or("").into(),
            "fps" => self.fps = v.as_u64().unwrap_or(99) as u32,
            "bitrate" => self.bitrate = v.as_u64().unwrap_or(0) as u32,
            "scale" => self.scale = v.as_f64().unwrap_or(0.0),
            "fullscreen" => self.fullscreen = v.as_bool().unwrap_or(self.fullscreen),
            "stats" => self.stats = v.as_bool().unwrap_or(self.stats),
            "measure" => self.measure = v.as_bool().unwrap_or(self.measure),
            "monitor" => self.monitor = v.as_u64().unwrap_or(0) as usize,
            _ => return false,
        }
        let fixed = self.clone().sanitized();
        if fixed != *self {
            *self = before; // valor inválido: se deja como estaba
            return false;
        }
        true
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
