//! Minimal EDID parsing: manufacturer, model name and serial.

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Edid {
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
}

pub fn parse(data: &[u8]) -> Option<Edid> {
    const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
    if data.len() < 128 || data[..8] != HEADER {
        return None;
    }
    let mut edid = Edid::default();

    // Three 5-bit letters packed big-endian into bytes 8–9.
    let id = u16::from_be_bytes([data[8], data[9]]);
    let letters: String = [(id >> 10) & 0x1f, (id >> 5) & 0x1f, id & 0x1f]
        .iter()
        .filter(|&&c| (1..=26).contains(&c))
        .map(|&c| (b'A' + c as u8 - 1) as char)
        .collect();
    if letters.len() == 3 {
        edid.make = Some(letters);
    }

    for desc in data[54..126].chunks(18) {
        if desc[0..3] != [0, 0, 0] {
            continue;
        }
        let text = String::from_utf8_lossy(&desc[5..18])
            .split('\n')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() {
            continue;
        }
        match desc[3] {
            0xfc => edid.model = Some(text),
            0xff => edid.serial = Some(text),
            _ => {}
        }
    }

    if edid.serial.is_none() {
        let n = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
        if n != 0 {
            edid.serial = Some(n.to_string());
        }
    }
    Some(edid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut d = vec![0u8; 128];
        d[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        // "BNQ": B=2, N=14, Q=17
        let id: u16 = (2 << 10) | (14 << 5) | 17;
        d[8..10].copy_from_slice(&id.to_be_bytes());
        let mut put = |off: usize, tag: u8, s: &str| {
            d[off + 3] = tag;
            let mut text = s.as_bytes().to_vec();
            text.push(b'\n');
            text.resize(13, b' ');
            d[off + 5..off + 18].copy_from_slice(&text);
        };
        put(72, 0xff, "T1J05492019");
        put(90, 0xfc, "BenQ GW2270");
        d
    }

    #[test]
    fn parses_descriptors() {
        let e = parse(&sample()).unwrap();
        assert_eq!(e.make.as_deref(), Some("BNQ"));
        assert_eq!(e.model.as_deref(), Some("BenQ GW2270"));
        assert_eq!(e.serial.as_deref(), Some("T1J05492019"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(&[0u8; 128]).is_none());
        assert!(parse(&[]).is_none());
    }
}
