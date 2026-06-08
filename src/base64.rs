pub(crate) fn decode_base64(value: &str) -> std::result::Result<Vec<u8>, &'static str> {
    let cleaned = value
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if cleaned.len() % 4 != 0 {
        return Err("invalid base64 length");
    }

    let mut output = Vec::with_capacity(cleaned.len() / 4 * 3);
    let chunk_count = cleaned.len() / 4;
    for (index, chunk) in cleaned.chunks(4).enumerate() {
        let final_chunk = index + 1 == chunk_count;
        let padding = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        if padding > 2 {
            return Err("invalid base64 padding");
        }
        if padding > 0 && !final_chunk {
            return Err("invalid base64 padding");
        }
        if chunk[..4 - padding].contains(&b'=') {
            return Err("invalid base64 padding");
        }

        let first = u32::from(base64_value(chunk[0]).ok_or("invalid base64 input")?);
        let second = u32::from(base64_value(chunk[1]).ok_or("invalid base64 input")?);
        let third = if padding == 2 {
            0
        } else {
            u32::from(base64_value(chunk[2]).ok_or("invalid base64 input")?)
        };
        let fourth = if padding > 0 {
            0
        } else {
            u32::from(base64_value(chunk[3]).ok_or("invalid base64 input")?)
        };

        let packed = (first << 18) | (second << 12) | (third << 6) | fourth;
        output.push(((packed >> 16) & 0xff) as u8);
        if padding < 2 {
            output.push(((packed >> 8) & 0xff) as u8);
        }
        if padding == 0 {
            output.push((packed & 0xff) as u8);
        }
    }

    Ok(output)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}
