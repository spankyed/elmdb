//! Vector quantization (`SQ8` default, `F16`, `F32`) and cosine similarity/distance kernels.
//!
//! Per-vector SQ8 stores `(min: f32, scale: f32, data: Vec<u8>)`:
//! - A 1536-dim SQ8 vector is 1,544 bytes -> full HNSW node + neighbors fits in `< 2 KiB`
//!   (2 nodes per 4 KiB LMDB leaf page, zero overflow pages, zero global calibration drift!).

use elmdb_wire::{ElmdbError, ErrorCode, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quantization {
    #[default]
    Sq8,
    F16,
    F32,
}

impl Quantization {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "sq8" | "SQ8" => Ok(Self::Sq8),
            "f16" | "F16" => Ok(Self::F16),
            "f32" | "F32" => Ok(Self::F32),
            other => Err(ElmdbError::new(
                ErrorCode::InvalidSchema,
                format!("unsupported vector quantization '{other}'; expected 'sq8', 'f16', or 'f32'"),
            )),
        }
    }

    pub fn tag(self) -> u8 {
        match self {
            Self::Sq8 => 1,
            Self::F16 => 2,
            Self::F32 => 3,
        }
    }

    pub fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            1 => Ok(Self::Sq8),
            2 => Ok(Self::F16),
            3 => Ok(Self::F32),
            other => Err(ElmdbError::new(ErrorCode::Corrupted, format!("unknown quantization tag {other}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum QuantVec {
    Sq8 { min: f32, scale: f32, data: Vec<u8> },
    F16(Vec<u16>),
    F32(Vec<f32>),
}

impl QuantVec {
    pub fn from_f32(vec: &[f32], mode: Quantization) -> Self {
        match mode {
            Quantization::Sq8 => {
                if vec.is_empty() {
                    return Self::Sq8 { min: 0.0, scale: 0.0, data: Vec::new() };
                }
                let mut min = f32::INFINITY;
                let mut max = f32::NEG_INFINITY;
                for &v in vec {
                    min = min.min(v);
                    max = max.max(v);
                }
                let span = max - min;
                if span <= 0.0 || !span.is_finite() {
                    return Self::Sq8 { min, scale: 0.0, data: vec![0u8; vec.len()] };
                }
                let scale = span / 255.0;
                let inv_scale = 255.0 / span;
                let mut data = Vec::with_capacity(vec.len());
                for &v in vec {
                    let q = ((v - min) * inv_scale).round().clamp(0.0, 255.0) as u8;
                    data.push(q);
                }
                Self::Sq8 { min, scale, data }
            }
            Quantization::F16 => {
                let mut out = Vec::with_capacity(vec.len());
                for &v in vec {
                    out.push(f32_to_f16(v));
                }
                Self::F16(out)
            }
            Quantization::F32 => Self::F32(vec.to_vec()),
        }
    }

    pub fn dims(&self) -> usize {
        match self {
            Self::Sq8 { data, .. } => data.len(),
            Self::F16(v) => v.len(),
            Self::F32(v) => v.len(),
        }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Sq8 { min, scale, data } => {
                out.push(Quantization::Sq8.tag());
                out.extend_from_slice(&min.to_le_bytes());
                out.extend_from_slice(&scale.to_le_bytes());
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                out.extend_from_slice(data);
            }
            Self::F16(vals) => {
                out.push(Quantization::F16.tag());
                out.extend_from_slice(&(vals.len() as u32).to_le_bytes());
                for &h in vals {
                    out.extend_from_slice(&h.to_le_bytes());
                }
            }
            Self::F32(vals) => {
                out.push(Quantization::F32.tag());
                out.extend_from_slice(&(vals.len() as u32).to_le_bytes());
                for &f in vals {
                    out.extend_from_slice(&f.to_le_bytes());
                }
            }
        }
    }

    pub fn decode(raw: &[u8], pos: &mut usize, expected_dims: usize) -> Result<Self> {
        if *pos >= raw.len() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated quant vec tag"));
        }
        let mode = Quantization::from_tag(raw[*pos])?;
        *pos += 1;
        match mode {
            Quantization::Sq8 => {
                if *pos + 12 > raw.len() {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated sq8 header"));
                }
                let min = f32::from_le_bytes(raw[*pos..*pos + 4].try_into().expect("4"));
                let scale = f32::from_le_bytes(raw[*pos + 4..*pos + 8].try_into().expect("4"));
                let dims = u32::from_le_bytes(raw[*pos + 8..*pos + 12].try_into().expect("4")) as usize;
                *pos += 12;
                if dims != expected_dims || !min.is_finite() || !scale.is_finite() || scale < 0.0 {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "invalid sq8 metadata"));
                }
                let end = pos
                    .checked_add(dims)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "sq8 length overflow"))?;
                if end > raw.len() {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated sq8 payload"));
                }
                let data = raw[*pos..end].to_vec();
                *pos = end;
                Ok(Self::Sq8 { min, scale, data })
            }
            Quantization::F16 => {
                if *pos + 4 > raw.len() {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated f16 len"));
                }
                let dims = u32::from_le_bytes(raw[*pos..*pos + 4].try_into().expect("4")) as usize;
                *pos += 4;
                if dims != expected_dims {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "f16 dims mismatch"));
                }
                let bytes = dims
                    .checked_mul(2)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "f16 byte len overflow"))?;
                let end =
                    pos.checked_add(bytes).ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "f16 pos overflow"))?;
                if end > raw.len() {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated f16 payload"));
                }
                let mut vals = Vec::with_capacity(dims);
                for &chunk in raw[*pos..end].as_chunks::<2>().0 {
                    vals.push(u16::from_le_bytes(chunk));
                }
                *pos = end;
                Ok(Self::F16(vals))
            }
            Quantization::F32 => {
                if *pos + 4 > raw.len() {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated f32 len"));
                }
                let dims = u32::from_le_bytes(raw[*pos..*pos + 4].try_into().expect("4")) as usize;
                *pos += 4;
                if dims != expected_dims {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "f32 dims mismatch"));
                }
                let bytes = dims
                    .checked_mul(4)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "f32 byte len overflow"))?;
                let end =
                    pos.checked_add(bytes).ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "f32 pos overflow"))?;
                if end > raw.len() {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated f32 payload"));
                }
                let mut vals = Vec::with_capacity(dims);
                for &chunk in raw[*pos..end].as_chunks::<4>().0 {
                    let f = f32::from_le_bytes(chunk);
                    if !f.is_finite() {
                        return Err(ElmdbError::new(ErrorCode::Corrupted, "non-finite f32 in vector"));
                    }
                    vals.push(f);
                }
                *pos = end;
                Ok(Self::F32(vals))
            }
        }
    }

    pub fn to_f32(&self) -> Vec<f32> {
        match self {
            Self::Sq8 { min, scale, data } => data.iter().map(|&q| *min + (*scale) * (q as f32)).collect(),
            Self::F16(vals) => vals.iter().map(|&h| f16_to_f32(h)).collect(),
            Self::F32(vals) => vals.clone(),
        }
    }
}

/// Exact cosine similarity in `[-1.0, 1.0]` between two `f32` vectors (`0.0` if either norm is 0).
#[inline]
pub fn cosine_similarity_f32(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (&x, &y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na * nb).sqrt();
    if denom <= 0.0 || !denom.is_finite() { 0.0 } else { (dot / denom).clamp(-1.0, 1.0) }
}

/// Cosine distance in `[0.0, 2.0]` (`1.0 - cosine_similarity`).
#[inline]
pub fn cosine_distance_f32(a: &[f32], b: &[f32]) -> f32 {
    (1.0 - cosine_similarity_f32(a, b)).clamp(0.0, 2.0)
}

/// Fast cosine distance between an `f32` query vector (with precomputed `q_norm`) and a `QuantVec`.
#[inline]
pub fn cosine_distance_query_vs_quant(query: &[f32], q_norm: f32, target: &QuantVec) -> f32 {
    if q_norm <= 0.0 {
        return 1.0;
    }
    match target {
        QuantVec::Sq8 { min, scale, data } => {
            let mut dot = 0.0f32;
            let mut nt = 0.0f32;
            let m = *min;
            let s = *scale;
            for (&qx, &tb) in query.iter().zip(data.iter()) {
                let tv = m + s * (tb as f32);
                dot += qx * tv;
                nt += tv * tv;
            }
            let denom = q_norm * nt.sqrt();
            if denom <= 0.0 || !denom.is_finite() {
                1.0
            } else {
                (1.0 - (dot / denom).clamp(-1.0, 1.0)).clamp(0.0, 2.0)
            }
        }
        QuantVec::F16(vals) => {
            let mut dot = 0.0f32;
            let mut nt = 0.0f32;
            for (&qx, &th) in query.iter().zip(vals.iter()) {
                let tv = f16_to_f32(th);
                dot += qx * tv;
                nt += tv * tv;
            }
            let denom = q_norm * nt.sqrt();
            if denom <= 0.0 || !denom.is_finite() {
                1.0
            } else {
                (1.0 - (dot / denom).clamp(-1.0, 1.0)).clamp(0.0, 2.0)
            }
        }
        QuantVec::F32(vals) => cosine_distance_f32(query, vals),
    }
}

/// Cosine distance between two `QuantVec`s (used during HNSW neighbor pruning).
pub fn cosine_distance_quant(a: &QuantVec, b: &QuantVec) -> f32 {
    let af = a.to_f32();
    let norm = l2_norm(&af);
    cosine_distance_query_vs_quant(&af, norm, b)
}

#[inline]
pub fn l2_norm(v: &[f32]) -> f32 {
    v.iter().map(|&x| x * x).sum::<f32>().sqrt()
}

/// Pure-Rust IEEE-754 binary32 -> binary16 conversion (round-to-nearest-even).
pub fn f32_to_f16(val: f32) -> u16 {
    let bits = val.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xFF) as i32;
    let mant = bits & 0x007F_FFFF;

    if exp == 0xFF {
        if mant == 0 {
            return sign | 0x7C00;
        }
        return sign | 0x7E00 | ((mant >> 13) as u16).max(1);
    }

    let unbiased = exp - 127;
    if unbiased > 15 {
        return sign | 0x7C00; // overflow -> +/- Inf
    }
    if unbiased < -24 {
        return sign; // underflow -> +/- 0
    }
    if unbiased < -14 {
        // Subnormal f16
        let shift = (-14 - unbiased) as u32;
        let full_mant = mant | 0x0080_0000;
        let half_mant = full_mant >> (shift + 13);
        let round_bit = (full_mant >> (shift + 12)) & 1;
        let sticky = (full_mant & ((1u32 << (shift + 12)) - 1)) != 0;
        let rounded = half_mant + u32::from(round_bit != 0 && (sticky || (half_mant & 1) != 0));
        return sign | (rounded as u16);
    }

    let h_exp = ((unbiased + 15) as u32) << 10;
    let half_mant = mant >> 13;
    let round_bit = (mant >> 12) & 1;
    let sticky = (mant & 0x0FFF) != 0;
    let rounded = (h_exp | half_mant) + u32::from(round_bit != 0 && (sticky || (half_mant & 1) != 0));
    sign | (rounded as u16)
}

/// Pure-Rust IEEE-754 binary16 -> binary32 conversion.
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h as u32) & 0x8000) << 16;
    let exp = ((h >> 10) & 0x1F) as u32;
    let mant = (h & 0x03FF) as u32;

    if exp == 0 {
        if mant == 0 {
            return f32::from_bits(sign);
        }
        // Normalize subnormal
        let mut m = mant;
        let mut e = -14i32;
        while (m & 0x0400) == 0 {
            m <<= 1;
            e -= 1;
        }
        m &= 0x03FF;
        let f_exp = ((e + 127) as u32) << 23;
        return f32::from_bits(sign | f_exp | (m << 13));
    }
    if exp == 0x1F {
        if mant == 0 {
            return f32::from_bits(sign | 0x7F80_0000);
        }
        return f32::from_bits(sign | 0x7FC0_0000 | (mant << 13));
    }
    let f_exp = (exp + 112) << 23;
    f32::from_bits(sign | f_exp | (mant << 13))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_roundtrip_and_sq8_1536_dim_fits_under_2kb() {
        for &v in &[0.0f32, -0.0, 1.0, -1.0, 0.5, -0.125, 3.25, 1e-4, 65504.0] {
            let h = f32_to_f16(v);
            let back = f16_to_f32(h);
            assert!((back - v).abs() <= v.abs() * 1e-3 + 1e-4, "v={v} back={back}");
        }

        // Verify 1536-dim SQ8 vector payload is strictly < 1.6 KiB (so node + neighbors < 2 KiB!).
        let v: Vec<f32> = (0..1536).map(|i| ((i as f32) * 0.013).sin()).collect();
        let q = QuantVec::from_f32(&v, Quantization::Sq8);
        let mut buf = Vec::new();
        q.encode(&mut buf);
        assert!(buf.len() < 1600, "1536-dim SQ8 payload {} B must stay < 1600 B", buf.len());
        let mut pos = 0;
        let dec = QuantVec::decode(&buf, &mut pos, 1536).unwrap();
        assert_eq!(dec, q);
        assert!(cosine_distance_query_vs_quant(&v, l2_norm(&v), &dec) < 1e-3);
    }
}
