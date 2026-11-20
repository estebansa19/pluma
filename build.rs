//! Embeds the Pluma icon in the executable (what Explorer shows for Pluma.exe).
//!
//! A resource compiler (rc.exe / windres) usually does this, but the minimal GNU Rust toolchain
//! doesn't ship one. So this script draws the icon (src/icon.rs) at several sizes and writes a
//! COFF object containing the `.rsrc` section itself; the linker takes it as is.

#[path = "src/icon.rs"]
mod icon;

use std::path::PathBuf;

/// Sizes Windows picks from (Explorer views and DPI scales). 256 is stored as PNG.
const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
const RT_ICON: u32 = 3;
const RT_GROUP_ICON: u32 = 14;
const LANG_EN_US: u32 = 0x0409;
const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const IMAGE_REL_AMD64_ADDR32NB: u16 = 0x0003;
/// IMAGE_SCN_CNT_INITIALIZED_DATA | IMAGE_SCN_ALIGN_4BYTES | IMAGE_SCN_MEM_READ
const RSRC_FLAGS: u32 = 0x0000_0040 | 0x0030_0000 | 0x4000_0000;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    if var("CARGO_CFG_TARGET_OS") != "windows" || var("CARGO_CFG_TARGET_ARCH") != "x86_64" {
        return;
    }

    let images: Vec<Vec<u8>> = SIZES
        .iter()
        .map(|&s| {
            let rgba = icon::icon_rgba(s);
            if s >= 256 { png(s, &rgba) } else { dib(s, &rgba) }
        })
        .collect();
    let group = group_icon_dir(&images);

    let mut resources: Vec<Resource> = images
        .into_iter()
        .enumerate()
        .map(|(i, data)| Resource { kind: RT_ICON, id: i as u32 + 1, data })
        .collect();
    resources.push(Resource { kind: RT_GROUP_ICON, id: 1, data: group });

    let out = PathBuf::from(var("OUT_DIR")).join("pluma_icon.o");
    std::fs::write(&out, coff_object(&resources)).expect("write pluma_icon.o");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

struct Resource {
    kind: u32,
    id: u32,
    data: Vec<u8>,
}

// ---------- icon images ----------

/// Icon image as a 32-bit DIB: BITMAPINFOHEADER + bottom-up BGRA pixels + AND mask.
fn dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mask_row = size.div_ceil(32) * 4;
    let mut d = Vec::new();
    d.extend(40u32.to_le_bytes()); // biSize
    d.extend((size as i32).to_le_bytes()); // biWidth
    d.extend((2 * size as i32).to_le_bytes()); // biHeight: color + mask
    d.extend(1u16.to_le_bytes()); // biPlanes
    d.extend(32u16.to_le_bytes()); // biBitCount
    d.extend(0u32.to_le_bytes()); // biCompression = BI_RGB
    d.extend((size * size * 4 + mask_row * size).to_le_bytes()); // biSizeImage
    d.extend([0u8; 16]); // resolution and palette: unused
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            d.extend([rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    // AND mask all zeros: transparency comes from the alpha channel.
    d.resize(d.len() + (mask_row * size) as usize, 0);
    d
}

fn png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let row = (size * 4) as usize;
    let mut raw = Vec::with_capacity((row + 1) * size as usize);
    for line in rgba.chunks_exact(row) {
        raw.push(0); // filter: none
        raw.extend_from_slice(line);
    }
    let mut ihdr = Vec::new();
    ihdr.extend(size.to_be_bytes());
    ihdr.extend(size.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]); // 8 bits, RGBA, deflate, no filter, no interlace
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 10));
    png_chunk(&mut out, b"IEND", &[]);
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(kind);
    out.extend(data);
    let crc = crc32(&out[start..]);
    out.extend(crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// GRPICONDIR: points each size to its RT_ICON resource.
fn group_icon_dir(images: &[Vec<u8>]) -> Vec<u8> {
    let mut d = Vec::new();
    d.extend(0u16.to_le_bytes()); // reserved
    d.extend(1u16.to_le_bytes()); // type: icon
    d.extend((images.len() as u16).to_le_bytes());
    for (i, (img, &s)) in images.iter().zip(SIZES.iter()).enumerate() {
        let side = if s >= 256 { 0 } else { s as u8 }; // 0 means 256
        d.extend([side, side, 0, 0]); // width, height, colors, reserved
        d.extend(1u16.to_le_bytes()); // planes
        d.extend(32u16.to_le_bytes()); // bits per pixel
        d.extend((img.len() as u32).to_le_bytes());
        d.extend((i as u16 + 1).to_le_bytes()); // RT_ICON id
    }
    d
}

// ---------- COFF object with a .rsrc section ----------

fn dir_header(buf: &mut Vec<u8>, id_entries: usize) {
    buf.extend([0u8; 12]); // characteristics, timestamp, version
    buf.extend(0u16.to_le_bytes()); // named entries
    buf.extend((id_entries as u16).to_le_bytes());
}

fn dir_entry(buf: &mut Vec<u8>, id: u32, offset: usize, subdir: bool) {
    buf.extend(id.to_le_bytes());
    buf.extend((offset as u32 | if subdir { 0x8000_0000 } else { 0 }).to_le_bytes());
}

/// Resource tree (type → id → language → data) followed by the data, plus one relocation per
/// data entry so the linker turns section offsets into RVAs.
fn coff_object(resources: &[Resource]) -> Vec<u8> {
    let mut kinds: Vec<u32> = resources.iter().map(|r| r.kind).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let by_kind = |k: u32| resources.iter().enumerate().filter(move |(_, r)| r.kind == k);

    // Layout: root dir, one id dir per type, one language dir per resource, data entries, data.
    let mut off = 16 + 8 * kinds.len();
    let mut kind_dir = Vec::new();
    for &k in &kinds {
        kind_dir.push(off);
        off += 16 + 8 * by_kind(k).count();
    }
    let lang_dir: Vec<usize> = (0..resources.len()).map(|i| off + i * 24).collect();
    off += 24 * resources.len();
    let data_entry: Vec<usize> = (0..resources.len()).map(|i| off + i * 16).collect();
    off += 16 * resources.len();
    let mut data_at = Vec::new();
    for r in resources {
        off = off.next_multiple_of(8);
        data_at.push(off);
        off += r.data.len();
    }

    let mut sec = Vec::with_capacity(off);
    dir_header(&mut sec, kinds.len());
    for (k, &at) in kinds.iter().zip(&kind_dir) {
        dir_entry(&mut sec, *k, at, true);
    }
    for &k in &kinds {
        let mut items: Vec<(usize, &Resource)> = by_kind(k).collect();
        items.sort_by_key(|(_, r)| r.id);
        dir_header(&mut sec, items.len());
        for (i, r) in items {
            dir_entry(&mut sec, r.id, lang_dir[i], true);
        }
    }
    for i in 0..resources.len() {
        dir_header(&mut sec, 1);
        dir_entry(&mut sec, LANG_EN_US, data_entry[i], false);
    }
    for (i, r) in resources.iter().enumerate() {
        sec.extend((data_at[i] as u32).to_le_bytes()); // relocated to an RVA by the linker
        sec.extend((r.data.len() as u32).to_le_bytes());
        sec.extend([0u8; 8]); // code page, reserved
    }
    for (i, r) in resources.iter().enumerate() {
        sec.resize(data_at[i], 0);
        sec.extend(&r.data);
    }
    sec.resize(sec.len().next_multiple_of(4), 0);

    let raw_ptr = 20 + 40;
    let reloc_ptr = raw_ptr + sec.len();
    let sym_ptr = reloc_ptr + 10 * resources.len();
    let name = *b".rsrc\0\0\0";

    let mut o = Vec::new();
    // File header
    o.extend(IMAGE_FILE_MACHINE_AMD64.to_le_bytes());
    o.extend(1u16.to_le_bytes()); // sections
    o.extend(0u32.to_le_bytes()); // timestamp
    o.extend((sym_ptr as u32).to_le_bytes());
    o.extend(2u32.to_le_bytes()); // symbols: section symbol + its aux record
    o.extend(0u16.to_le_bytes()); // optional header size
    o.extend(0u16.to_le_bytes()); // characteristics
    // Section header
    o.extend(name);
    o.extend(0u32.to_le_bytes()); // virtual size
    o.extend(0u32.to_le_bytes()); // virtual address
    o.extend((sec.len() as u32).to_le_bytes());
    o.extend((raw_ptr as u32).to_le_bytes());
    o.extend((reloc_ptr as u32).to_le_bytes());
    o.extend(0u32.to_le_bytes()); // line numbers
    o.extend((resources.len() as u16).to_le_bytes());
    o.extend(0u16.to_le_bytes());
    o.extend(RSRC_FLAGS.to_le_bytes());
    // Section data
    o.extend(&sec);
    // Relocations: every data entry's OffsetToData, relative to the section symbol
    for &at in &data_entry {
        o.extend((at as u32).to_le_bytes());
        o.extend(0u32.to_le_bytes()); // symbol index
        o.extend(IMAGE_REL_AMD64_ADDR32NB.to_le_bytes());
    }
    // Symbol table: the section symbol and its section-definition aux record
    o.extend(name);
    o.extend(0u32.to_le_bytes()); // value
    o.extend(1i16.to_le_bytes()); // section number
    o.extend(0u16.to_le_bytes()); // type
    o.push(3); // IMAGE_SYM_CLASS_STATIC
    o.push(1); // aux records
    o.extend((sec.len() as u32).to_le_bytes());
    o.extend((resources.len() as u16).to_le_bytes());
    o.extend([0u8; 12]); // line numbers, checksum, number, selection, padding
    // Empty string table
    o.extend(4u32.to_le_bytes());
    o
}
