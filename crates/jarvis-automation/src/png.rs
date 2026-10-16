//! Códec PNG mínimo para las capturas (HU-38).
//!
//! Los backends de captura (portal, spectacle, grim) entregan PNG y los
//! modelos de visión lo consumen. Se implementa aquí sobre `flate2` (ya en el
//! árbol) en lugar de sumar el crate `png`: solo 8 bits por canal, sin
//! entrelazado, en escala de grises, RGB y RGBA (con o sin alfa), con los cinco
//! filtros de fila. Paleta, 16 bits y Adam7 se rechazan con error explícito.

use std::io::{Read, Write};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::{Compression, Crc};

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// Tope de píxeles aceptados al decodificar (8K × 8K): una cabecera hostil no
/// puede forzar una reserva de memoria gigantesca.
const MAX_PIXELS: u64 = 8192 * 8192;

/// Imagen RGBA8 decodificada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

/// Codifica RGBA8 (`width × height × 4` bytes) como PNG con filtro `None`.
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let stride = width as usize * 4;
    debug_assert_eq!(rgba.len(), stride * height as usize);
    let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
    for row in rgba.chunks_exact(stride.max(1)).take(height as usize) {
        // Escribir en un Vec en memoria no puede fallar.
        let _ = z.write_all(&[0]);
        let _ = z.write_all(row);
    }
    let idat = z.finish().unwrap_or_default();

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 bits, RGBA, deflate, filtro 0, sin entrelazado

    let mut out = Vec::with_capacity(idat.len() + 64);
    out.extend_from_slice(&SIGNATURE);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let pa = (p - i16::from(a)).abs();
    let pb = (p - i16::from(b)).abs();
    let pc = (p - i16::from(c)).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Decodifica un PNG de 8 bits sin entrelazado a RGBA8.
pub fn decode_rgba(data: &[u8]) -> Result<RgbaImage, String> {
    if data.len() < 8 || data[..8] != SIGNATURE {
        return Err("no es un PNG".into());
    }
    let mut pos = 8;
    let mut header: Option<(u32, u32, u8)> = None;
    let mut idat = Vec::new();
    while pos + 8 <= data.len() {
        let len = be32(&data[pos..]) as usize;
        let kind = &data[pos + 4..pos + 8];
        let start = pos + 8;
        let end = start
            .checked_add(len)
            .filter(|e| e + 4 <= data.len())
            .ok_or("chunk PNG truncado")?;
        let body = &data[start..end];
        match kind {
            b"IHDR" => {
                if body.len() != 13 {
                    return Err("IHDR inválido".into());
                }
                let (w, h) = (be32(body), be32(&body[4..]));
                let (depth, color, interlace) = (body[8], body[9], body[12]);
                if depth != 8 {
                    return Err(format!("profundidad {depth} no soportada (solo 8 bits)"));
                }
                if interlace != 0 {
                    return Err("PNG entrelazado no soportado".into());
                }
                if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
                    return Err(format!("dimensiones fuera de rango: {w}×{h}"));
                }
                header = Some((w, h, color));
            }
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        pos = end + 4;
    }
    let (width, height, color) = header.ok_or("PNG sin IHDR")?;
    let channels = match color {
        0 => 1, // gris
        2 => 3, // RGB
        4 => 2, // gris + alfa
        6 => 4, // RGBA
        3 => return Err("PNG con paleta no soportado".into()),
        c => return Err(format!("tipo de color {c} inválido")),
    };
    let stride = width as usize * channels;
    let expected = (stride + 1) * height as usize;
    let mut raw = Vec::with_capacity(expected);
    ZlibDecoder::new(idat.as_slice())
        .take(expected as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| format!("IDAT corrupto: {e}"))?;
    if raw.len() < expected {
        return Err("IDAT truncado".into());
    }

    let mut pixels = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
        let filter = raw[y * (stride + 1)];
        let src = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        let (done, rest) = pixels.split_at_mut(y * stride);
        let prev = if y == 0 {
            None
        } else {
            Some(&done[(y - 1) * stride..])
        };
        let cur = &mut rest[..stride];
        for i in 0..stride {
            let a = if i >= channels { cur[i - channels] } else { 0 };
            let b = prev.map_or(0, |p| p[i]);
            let c = if i >= channels {
                prev.map_or(0, |p| p[i - channels])
            } else {
                0
            };
            cur[i] = match filter {
                0 => src[i],
                1 => src[i].wrapping_add(a),
                2 => src[i].wrapping_add(b),
                3 => src[i].wrapping_add(((u16::from(a) + u16::from(b)) / 2) as u8),
                4 => src[i].wrapping_add(paeth(a, b, c)),
                f => return Err(format!("filtro de fila {f} inválido")),
            };
        }
    }

    let rgba = match channels {
        4 => pixels,
        3 => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|[r, g, b]| [*r, *g, *b, 255])
            .collect(),
        2 => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|[g, a]| [*g, *g, *g, *a])
            .collect(),
        _ => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
    };
    Ok(RgbaImage {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(w: u32, h: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [(x * 7) as u8, (y * 13) as u8, (x ^ y) as u8, 200]
            })
            .collect()
    }

    #[test]
    fn roundtrip_rgba() {
        let rgba = pattern(37, 21);
        let png = encode_rgba(37, 21, &rgba);
        assert_eq!(&png[..8], &SIGNATURE);
        let img = decode_rgba(&png).unwrap();
        assert_eq!((img.width, img.height), (37, 21));
        assert_eq!(img.rgba, rgba);
    }

    /// PNG RGB con los filtros Sub, Up, Average y Paeth, construido a mano.
    #[test]
    fn decodes_rgb_with_all_filters() {
        let (w, h) = (3u32, 5u32);
        let rows: Vec<Vec<u8>> = (0..h)
            .map(|y| (0..w * 3).map(|i| (i as u8) * 10 + y as u8).collect())
            .collect();
        let stride = (w * 3) as usize;
        let mut raw = Vec::new();
        for (y, row) in rows.iter().enumerate() {
            let filter = y as u8; // 0..=4
            raw.push(filter);
            for i in 0..stride {
                let a = if i >= 3 { row[i - 3] } else { 0 };
                let b = if y > 0 { rows[y - 1][i] } else { 0 };
                let c = if y > 0 && i >= 3 {
                    rows[y - 1][i - 3]
                } else {
                    0
                };
                let pred = match filter {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                    _ => paeth(a, b, c),
                };
                raw.push(row[i].wrapping_sub(pred));
            }
        }
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(&raw).unwrap();
        let idat = z.finish().unwrap();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut png = SIGNATURE.to_vec();
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &idat);
        chunk(&mut png, b"IEND", &[]);

        let img = decode_rgba(&png).unwrap();
        for (y, row) in rows.iter().enumerate() {
            for x in 0..w as usize {
                let px = &img.rgba[(y * w as usize + x) * 4..][..4];
                assert_eq!(px, &[row[x * 3], row[x * 3 + 1], row[x * 3 + 2], 255]);
            }
        }
    }

    #[test]
    fn rejects_garbage_and_huge_headers() {
        assert!(decode_rgba(b"hola").is_err());
        let mut png = SIGNATURE.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&100_000u32.to_be_bytes());
        ihdr.extend_from_slice(&100_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        assert!(decode_rgba(&png).unwrap_err().contains("fuera de rango"));
    }
}
