const ALPHABET: &[u8; 48] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ~@$%`(){}[]_";

pub fn encode_hash(bytes: &[u8]) -> Option<String> {
    if bytes.len() != 16 {
        return None;
    }
    let mut limbs = [0u16; 8];
    for (idx, chunk) in bytes.chunks_exact(2).enumerate() {
        limbs[idx] = u16::from_le_bytes([chunk[0], chunk[1]]);
    }
    let mut digits = [0u8; 23];
    for digit in &mut digits {
        let mut rem = 0u32;
        for idx in (0..limbs.len()).rev() {
            let cur = (rem << 16) | limbs[idx] as u32;
            limbs[idx] = (cur / 48) as u16;
            rem = cur % 48;
        }
        *digit = rem as u8;
    }
    if limbs.iter().any(|v| *v != 0) {
        return None;
    }
    Some(
        digits
            .iter()
            .map(|idx| ALPHABET[*idx as usize] as char)
            .collect(),
    )
}

pub fn decode_hash(text: &str) -> Option<[u8; 16]> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() != 23 {
        return None;
    }
    let mut digits = [0u16; 23];
    for (idx, ch) in chars.into_iter().enumerate() {
        let upper = if ch.is_ascii_lowercase() {
            ch.to_ascii_uppercase()
        } else {
            ch
        };
        let value = ALPHABET.iter().position(|v| *v as char == upper)?;
        digits[idx] = value as u16;
    }
    let mut out = [0u8; 16];
    for byte in &mut out {
        let mut rem = 0u32;
        for idx in (0..digits.len()).rev() {
            let cur = rem * 48 + digits[idx] as u32;
            digits[idx] = (cur / 256) as u16;
            rem = cur % 256;
        }
        *byte = rem as u8;
    }
    if digits.iter().any(|v| *v != 0) {
        return None;
    }
    Some(out)
}

pub fn decode_hex_16(hex: &str) -> Option<[u8; 16]> {
    let clean: String = hex
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace() && *ch != ':' && *ch != '-')
        .collect();
    if clean.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for idx in 0..16 {
        out[idx] = u8::from_str_radix(&clean[idx * 2..idx * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_known_group_hashes() {
        for hex in [
            "29d046bcf6a1b429d4e431806c87b813",
            "aaa7c24ef8a810567c1b1449a581f6de",
            "612717e8cec12abd5e9224101b9e2a68",
        ] {
            let hash = decode_hex_16(hex).unwrap();
            let encoded = encode_hash(&hash).unwrap();
            assert_eq!(decode_hash(&encoded).unwrap(), hash);
            assert_eq!(encoded.chars().count(), 23);
        }
    }

    #[test]
    fn decodes_lowercase_like_common_dll() {
        let hash = decode_hex_16("29d046bcf6a1b429d4e431806c87b813").unwrap();
        let encoded = encode_hash(&hash).unwrap();
        assert_eq!(decode_hash(&encoded.to_ascii_lowercase()).unwrap(), hash);
    }
}
