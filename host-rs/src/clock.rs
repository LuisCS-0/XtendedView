//! Reloj monotónico común: lo usan las marcas de los frames y las respuestas a los pings,
//! así el visor puede calcular la latencia de red con su propio reloj sincronizado.
use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

pub fn init() {
    START.get_or_init(Instant::now);
}

/// Milisegundos desde que arrancó el proceso.
pub fn now_ms() -> f64 {
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}
