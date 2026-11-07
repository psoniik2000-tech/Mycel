//! Media blobs (DESIGN.md §8): chunked file encryption and photo preparation.

use anyhow::{Result, anyhow};
use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305};
use image::{
    DynamicImage, ImageDecoder, ImageReader, codecs::jpeg::JpegEncoder, imageops::FilterType,
};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

/// Plaintext chunk size (§8.1); each ciphertext chunk is 16 bytes longer (Poly1305 tag).
pub const CHUNK: usize = 256 << 10;
// A fresh key per file, so a fixed nonce prefix is safe; the STREAM counter and last-chunk flag stop reordering and truncation.
const NONCE: [u8; 19] = [0; 19];

fn read_chunk(r: &mut impl Read, n: usize) -> Result<Vec<u8>> {
    let mut v = Vec::with_capacity(n);
    r.by_ref().take(n as u64).read_to_end(&mut v)?;
    Ok(v)
}

/// Encrypts `src` into `dst` with a new random key (XChaCha20-Poly1305 STREAM, 256 KiB chunks).
pub fn encrypt(src: &Path, dst: &Path) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|e| anyhow!("rng: {e}"))?;
    let mut enc = EncryptorBE32::from_aead(XChaCha20Poly1305::new(&key.into()), &NONCE.into());
    let mut r = BufReader::new(File::open(src)?);
    let mut w = BufWriter::new(File::create(dst)?);
    let mut cur = read_chunk(&mut r, CHUNK)?;
    loop {
        let next = read_chunk(&mut r, CHUNK)?;
        if next.is_empty() {
            let ct = enc
                .encrypt_last(cur.as_slice())
                .map_err(|_| anyhow!("encrypt"))?;
            w.write_all(&ct)?;
            break;
        }
        w.write_all(
            &enc.encrypt_next(cur.as_slice())
                .map_err(|_| anyhow!("encrypt"))?,
        )?;
        cur = next;
    }
    w.flush()?;
    Ok(key)
}

/// Decrypts a blob made by [`encrypt`]; fails on any tampered, reordered or missing chunk.
pub fn decrypt(src: &Path, dst: &Path, key: &[u8; 32]) -> Result<()> {
    let mut dec = DecryptorBE32::from_aead(XChaCha20Poly1305::new(key.into()), &NONCE.into());
    let mut r = BufReader::new(File::open(src)?);
    let mut w = BufWriter::new(File::create(dst)?);
    let n = CHUNK + 16;
    let mut cur = read_chunk(&mut r, n)?;
    loop {
        let next = read_chunk(&mut r, n)?;
        let bad = |_| anyhow!("blob does not decrypt");
        if next.is_empty() {
            w.write_all(&dec.decrypt_last(cur.as_slice()).map_err(bad)?)?;
            break;
        }
        w.write_all(&dec.decrypt_next(cur.as_slice()).map_err(bad)?)?;
        cur = next;
    }
    w.flush()?;
    Ok(())
}

/// A photo re-encoded for sending: no EXIF (re-encoding drops it), upright, at most 2048 px.
pub struct Photo {
    pub jpeg: Vec<u8>,
    pub dims: (u32, u32),
    pub blurhash: String,
}

// shortcut: JPEG q82 instead of WebP — the image crate writes WebP lossless only; add libwebp if size matters.
pub fn photo(src: &Path) -> Result<Photo> {
    let mut dec = ImageReader::open(src)?
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = dec.orientation()?;
    let mut img = DynamicImage::from_decoder(dec)?;
    img.apply_orientation(orientation);
    if img.width().max(img.height()) > 2048 {
        img = img.resize(2048, 2048, FilterType::Lanczos3);
    }
    let mut jpeg = vec![];
    JpegEncoder::new_with_quality(&mut jpeg, 82).encode_image(&img.to_rgb8())?;
    let small = img.thumbnail(32, 32).to_rgba8();
    let blurhash = blurhash::encode(4, 3, small.width(), small.height(), small.as_raw())
        .map_err(|e| anyhow!("blurhash: {e:?}"))?;
    Ok(Photo {
        jpeg,
        dims: (img.width(), img.height()),
        blurhash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tamper() {
        let d = tempfile::tempdir().unwrap();
        let p = |n: &str| d.path().join(n);
        for len in [0, 10, CHUNK, 2 * CHUNK + 7] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
            std::fs::write(p("a"), &data).unwrap();
            let key = encrypt(&p("a"), &p("b")).unwrap();
            let ct = std::fs::read(p("b")).unwrap();
            assert_eq!(ct.len(), len + 16 * len.div_ceil(CHUNK).max(1));
            decrypt(&p("b"), &p("c"), &key).unwrap();
            assert_eq!(std::fs::read(p("c")).unwrap(), data);
            if len > CHUNK {
                // Truncated to whole chunks: the last-chunk flag catches it.
                std::fs::write(p("t"), &ct[..CHUNK + 16]).unwrap();
                assert!(decrypt(&p("t"), &p("c"), &key).is_err());
            }
        }
    }

    #[test]
    fn photo_strips_and_shrinks() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("x.png");
        image::RgbImage::from_fn(3000, 1000, |x, _| image::Rgb([(x % 256) as u8, 0, 0]))
            .save(&src)
            .unwrap();
        let ph = photo(&src).unwrap();
        assert_eq!(ph.dims, (2048, 683));
        assert_eq!(&ph.jpeg[..2], [0xFF, 0xD8]);
        assert!(!ph.blurhash.is_empty());
    }
}
