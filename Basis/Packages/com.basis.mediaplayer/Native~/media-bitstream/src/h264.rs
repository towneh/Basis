//! H.264 SPS dimension extraction (exp-Golomb walk).

/// Bit reader that freezes past end-of-data instead of erroring: a
/// truncated SPS reads zeros until the dimension range check rejects it.
struct BitReader<'a> {
    data: &'a [u8],
    bitpos: usize,
}

impl BitReader<'_> {
    fn u(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.bitpos >> 3;
            let bit = 7 - (self.bitpos & 7);
            let b = if byte < self.data.len() {
                (self.data[byte] >> bit) & 1
            } else {
                0
            };
            v = (v << 1) | u32::from(b);
            if byte < self.data.len() {
                // Freeze past the end; never overflow bitpos.
                self.bitpos += 1;
            }
        }
        v
    }

    fn ue(&mut self) -> u32 {
        let mut zeros = 0u32;
        // Cap at 31: a ue(v) with >=31 leading zeros is malformed, and a
        // 32-bit shift would overflow.
        while self.u(1) == 0 && zeros < 31 {
            zeros += 1;
        }
        (1u32 << zeros) - 1 + self.u(zeros)
    }

    fn se(&mut self) -> i32 {
        let ue = self.ue();
        if ue & 1 == 1 {
            ((ue + 1) >> 1) as i32
        } else {
            -((ue >> 1) as i32)
        }
    }
}

/// Width and height from an SPS NAL (header byte included), or `None` when
/// the SPS is malformed or the result lands outside (0, 8192].
pub fn sps_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    if sps.len() < 4 {
        return None;
    }
    // Strip the NAL header byte and emulation-prevention 0x03 bytes. 512
    // bytes of RBSP is plenty for everything up to the frame dimensions.
    let mut rbsp = Vec::with_capacity(512.min(sps.len()));
    let mut zeros = 0u32;
    for &b in &sps[1..] {
        if rbsp.len() >= 512 {
            break;
        }
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        rbsp.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }

    let mut g = BitReader {
        data: &rbsp,
        bitpos: 0,
    };
    let profile_idc = g.u(8);
    g.u(8); // constraint flags + reserved
    g.u(8); // level_idc
    g.ue(); // seq_parameter_set_id
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128
    ) {
        let chroma = g.ue();
        if chroma == 3 {
            g.u(1);
        }
        g.ue();
        g.ue();
        g.u(1);
        if g.u(1) == 1 { /* scaling matrix — skip (uncommon in SPS) */ }
    }
    g.ue(); // log2_max_frame_num_minus4
    let poc_type = g.ue();
    if poc_type == 0 {
        g.ue();
    } else if poc_type == 1 {
        g.u(1);
        g.se();
        g.se();
        let n = g.ue();
        if n > 255 {
            return None; // malformed; H.264 caps this cycle at 255
        }
        for _ in 0..n {
            g.se();
        }
    }
    g.ue(); // max_num_ref_frames
    g.u(1); // gaps_in_frame_num_value_allowed
    let width_mbs = i64::from(g.ue()) + 1;
    let height_map_units = i64::from(g.ue()) + 1;
    let frame_mbs_only = i64::from(g.u(1));
    if frame_mbs_only == 0 {
        g.u(1);
    }
    g.u(1); // direct_8x8_inference
    let (mut cl, mut cr, mut ct, mut cb) = (0i64, 0i64, 0i64, 0i64);
    if g.u(1) == 1 {
        cl = i64::from(g.ue());
        cr = i64::from(g.ue());
        ct = i64::from(g.ue());
        cb = i64::from(g.ue());
    }

    // Crop and MB counts are attacker-controlled ue(v); compute in i64 so a
    // malformed SPS overflows nothing before the range check rejects it.
    let w = width_mbs * 16 - (cl + cr) * 2;
    let h = height_map_units * 16 * (2 - frame_mbs_only) - (ct + cb) * 2;
    if w <= 0 || h <= 0 || w > 8192 || h > 8192 {
        return None;
    }
    Some((w as u32, h as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_fixture_sps() {
        // The repo's 640x360 fixture's SPS (High profile, with cropping and
        // emulation-prevention bytes in the VUI).
        let sps = [
            0x67, 0x64, 0x00, 0x1e, 0xac, 0xd9, 0x40, 0xa0, 0x2f, 0xf9, 0x70, 0x11, 0x00, 0x00,
            0x03, 0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x3c, 0x0f, 0x16, 0x2d, 0x96,
        ];
        assert_eq!(sps_dimensions(&sps), Some((640, 360)));
    }

    #[test]
    fn rejects_truncated() {
        assert_eq!(sps_dimensions(&[]), None);
        assert_eq!(sps_dimensions(&[0x67, 0xFF]), None);
        // A truncated High-profile SPS freezes the bit reader at zero and
        // fails the dimension range check instead of overrunning.
        assert_eq!(sps_dimensions(&[0x67, 0x64, 0x00]), None);
    }
}
