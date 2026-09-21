//! Concatenate codec-config buffers into one Annex-B blob.

use droidmirror_proto::to_annex_b;

pub fn csd_annex_b(parts: &[impl AsRef<[u8]>]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        let b = part.as_ref();
        if b.is_empty() {
            continue;
        }
        out.extend_from_slice(&to_annex_b(b));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_raw_sps_pps() {
        let blob = csd_annex_b(&[&[0x67, 0x42][..], &[0x68, 0xce][..]]);
        assert_eq!(
            blob,
            vec![0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce]
        );
    }
}
