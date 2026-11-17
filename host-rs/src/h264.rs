//! Utilidades mínimas de H.264 en formato Annex B.

/// Unidades NAL (sin el código de inicio) de un bloque Annex B.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
        while end > s && data[end - 1] == 0 {
            end -= 1; // ceros del código de inicio de 4 bytes siguiente
        }
        out.push(&data[s..end]);
    }
    out
}

/// Cadena de códec para WebCodecs ("avc1.PPCCLL") a partir del SPS.
pub fn codec_string(data: &[u8]) -> Option<String> {
    nal_units(data)
        .into_iter()
        .find(|n| n.len() >= 4 && n[0] & 0x1f == 7)
        .map(|sps| format!("avc1.{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_sps() {
        let data = [0, 0, 0, 1, 0x67, 0x64, 0x00, 0x33, 0xAA, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 1, 0x65, 1, 2];
        let nals = nal_units(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[1], &[0x68, 0xEE]);
        assert_eq!(codec_string(&data).as_deref(), Some("avc1.640033"));
    }
}
