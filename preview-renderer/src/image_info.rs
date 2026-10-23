// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! PNG signature and header checks, used by the renderer, the `smoke` command and tests.

/// The eight bytes every PNG starts with.
pub const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

/// `(width, height)` in pixels from the `IHDR` chunk, which must directly follow the signature.
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || !bytes.starts_with(&PNG_SIGNATURE) || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ihdr_dimensions() {
        let mut header = PNG_SIGNATURE.to_vec();
        header.extend_from_slice(&13u32.to_be_bytes());
        header.extend_from_slice(b"IHDR");
        header.extend_from_slice(&2400u32.to_be_bytes());
        header.extend_from_slice(&1260u32.to_be_bytes());
        assert_eq!(png_dimensions(&header), Some((2400, 1260)));
        assert_eq!(png_dimensions(&header[..20]), None);
        assert_eq!(png_dimensions(b"GIF89a..................."), None);
    }
}
