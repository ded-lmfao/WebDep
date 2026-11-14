use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

const BLOCK_SIZE: usize = 512;
const DEFAULT_SKIP_DIRS: &[&str] = &["node_modules", ".git"];

/// Formats an octal integer right-aligned in `buf` (NUL-terminated).
fn write_octal(buf: &mut [u8], val: u64) {
    for b in buf.iter_mut() {
        *b = b'0';
    }
    let width = buf.len().saturating_sub(1);
    if !buf.is_empty() {
        buf[width] = 0;
    }
    let formatted = format!("{val:o}");
    let bytes = formatted.as_bytes();
    if bytes.len() <= width {
        let start = width - bytes.len();
        buf[start..width].copy_from_slice(bytes);
    }
}

fn parse_octal(slice: &[u8]) -> Option<u64> {
    let s: String = slice
        .iter()
        .take_while(|&&b| b != 0 && b != b' ')
        .map(|&b| b as char)
        .collect::<String>()
        .trim()
        .to_string();
    if s.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(&s, 8).ok()
}

fn split_ustar_path(rel_unix: &str) -> Option<(String, String)> {
    if rel_unix.len() <= 100 {
        return Some((String::new(), rel_unix.to_string()));
    }
    if rel_unix.len() > 256 {
        return None;
    }
    // Split on '/' such that prefix <= 155 and name <= 100
    for (idx, ch) in rel_unix.char_indices().rev() {
        if ch == '/' {
            let prefix = &rel_unix[..idx];
            let name = &rel_unix[idx + 1..];
            if !name.is_empty() && name.len() <= 100 && prefix.len() <= 155 {
                return Some((prefix.to_string(), name.to_string()));
            }
        }
    }
    None
}

fn build_ustar_header(
    prefix: &str,
    name: &str,
    mode: u32,
    size: u64,
    mtime: u64,
    typeflag: u8,
) -> [u8; BLOCK_SIZE] {
    let mut hdr = [0u8; BLOCK_SIZE];
    let n_bytes = name.as_bytes();
    let n_copy = n_bytes.len().min(100);
    hdr[0..n_copy].copy_from_slice(&n_bytes[..n_copy]);

    write_octal(&mut hdr[100..108], (mode & 0o7777) as u64);
    write_octal(&mut hdr[108..116], 0);
    write_octal(&mut hdr[116..124], 0);

    let mut size_field = [b'0'; 12];
    write_octal(&mut size_field, size);
    hdr[124..136].copy_from_slice(&size_field);

    let mut mtime_field = [b'0'; 12];
    write_octal(&mut mtime_field, mtime);
    hdr[136..148].copy_from_slice(&mtime_field);

    hdr[148..156].fill(b' ');
    hdr[156] = typeflag;

    hdr[257..263].copy_from_slice(b"ustar\0");
    hdr[263..265].copy_from_slice(b"00");

    let p_bytes = prefix.as_bytes();
    let p_copy = p_bytes.len().min(155);
    hdr[345..345 + p_copy].copy_from_slice(&p_bytes[..p_copy]);

    let sum: u32 = hdr.iter().map(|&b| b as u32).sum();
    let chk_str = format!("{sum:06o}\0 ");
    hdr[148..156].copy_from_slice(chk_str.as_bytes());
    hdr
}

fn format_pax_record(key: &str, val: &str) -> Vec<u8> {
    let payload = format!(" {key}={val}\n");
    let mut total_len = payload.len() + 1;
    loop {
        let len_digits = total_len.to_string().len();
        let next = len_digits + payload.len();
        if next == total_len {
            break;
        }
        total_len = next;
    }
    format!("{total_len}{payload}").into_bytes()
}

fn write_tar_entry_header<W: Write>(
    writer: &mut W,
    rel_unix: &str,
    mode: u32,
    size: u64,
    mtime: u64,
    typeflag: u8,
) -> Result<(), String> {
    if let Some((prefix, name)) = split_ustar_path(rel_unix) {
        let hdr = build_ustar_header(&prefix, &name, mode, size, mtime, typeflag);
        writer
            .write_all(&hdr)
            .map_err(|e| format!("Failed writing tar header: {e}"))?;
    } else {
        // Emit PAX extended header ('x') for paths > 256 chars or un-splittable segments > 100 chars
        let pax = format_pax_record("path", rel_unix);
        let fallback_name = format!(
            "PaxHeader/{}",
            rel_unix.rsplit('/').next().unwrap_or("entry").chars().take(80).collect::<String>()
        );
        let pax_hdr = build_ustar_header("", &fallback_name, 0o644, pax.len() as u64, mtime, b'x');
        writer
            .write_all(&pax_hdr)
            .map_err(|e| format!("Failed writing PAX header: {e}"))?;
        writer
            .write_all(&pax)
            .map_err(|e| format!("Failed writing PAX payload: {e}"))?;
        let rem = pax.len() % BLOCK_SIZE;
        if rem != 0 {
            let pad = vec![0u8; BLOCK_SIZE - rem];
            writer
                .write_all(&pad)
                .map_err(|e| format!("Failed padding PAX payload: {e}"))?;
        }
        let short_name: String = rel_unix.chars().take(100).collect();
        let hdr = build_ustar_header("", &short_name, mode, size, mtime, typeflag);
        writer
            .write_all(&hdr)
            .map_err(|e| format!("Failed writing entry header: {e}"))?;
    }
    Ok(())
}

fn parse_pax_path(data: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(data).ok()?;
    for line in text.lines() {
        if let Some((_, kv)) = line.split_once(' ') {
            if let Some(v) = kv.strip_prefix("path=") {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn validate_relative_entry_path(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim_start_matches("./");
    if trimmed.is_empty() || trimmed == "." {
        return Ok(PathBuf::new());
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(format!("Unsafe absolute archive path rejected: {raw}"));
    }
    if raw.contains('\0') {
        return Err("Archive path contains NUL byte".to_string());
    }
    let normalized = trimmed.replace('\\', "/");
    let mut out = PathBuf::new();
    for seg in normalized.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains(':') {
            return Err(format!("Unsafe archive path traversal rejected: {raw}"));
        }
        let comp_check = Path::new(seg).components().next();
        match comp_check {
            Some(Component::Normal(_)) => out.push(seg),
            _ => return Err(format!("Unsafe archive component rejected: {raw}")),
        }
    }
    Ok(out)
}

fn append_dir_recursive<W: Write>(
    writer: &mut W,
    disk_dir: &Path,
    archive_prefix: &str,
    skip_names: &[&str],
) -> Result<u64, String> {
    let mut raw_bytes = 0u64;
    let Ok(entries) = fs::read_dir(disk_dir) else {
        return Ok(0);
    };
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|e| e.file_name());

    for entry in items {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if skip_names.contains(&name_str.as_ref()) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let rel = if archive_prefix.is_empty() {
            name_str.to_string()
        } else {
            format!("{archive_prefix}/{name_str}")
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if meta.is_dir() {
            let dir_entry = format!("{rel}/");
            write_tar_entry_header(writer, &dir_entry, 0o755, 0, mtime, b'5')?;
            raw_bytes += append_dir_recursive(writer, &path, &rel, skip_names)?;
        } else if meta.is_file() {
            let size = meta.len();
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o777
            };
            #[cfg(not(unix))]
            let mode = 0o644u32;

            write_tar_entry_header(writer, &rel, mode, size, mtime, b'0')?;
            let mut file = File::open(&path)
                .map_err(|e| format!("Failed reading {} for archive: {e}", path.display()))?;
            let mut remaining = size;
            let mut buf = [0u8; 65536];
            while remaining > 0 {
                let to_read = (remaining as usize).min(buf.len());
                let n = file
                    .read(&mut buf[..to_read])
                    .map_err(|e| format!("Failed streaming {} to archive: {e}", path.display()))?;
                if n == 0 {
                    break;
                }
                writer
                    .write_all(&buf[..n])
                    .map_err(|e| format!("Failed writing archive data: {e}"))?;
                remaining -= n as u64;
                raw_bytes += n as u64;
            }
            if remaining > 0 {
                let zeroes = vec![0u8; remaining.min(65536) as usize];
                while remaining > 0 {
                    let w = (remaining as usize).min(zeroes.len());
                    writer
                        .write_all(&zeroes[..w])
                        .map_err(|e| format!("Failed padding truncated file in archive: {e}"))?;
                    remaining -= w as u64;
                }
            }
            let pad = (BLOCK_SIZE - ((size as usize) % BLOCK_SIZE)) % BLOCK_SIZE;
            if pad > 0 {
                let zeroes = [0u8; BLOCK_SIZE];
                writer
                    .write_all(&zeroes[..pad])
                    .map_err(|e| format!("Failed writing tar block padding: {e}"))?;
            }
        }
    }
    Ok(raw_bytes)
}

/// Packs specified subdirectories (e.g. `current`, `containers`, `backups`) under `root`
/// into a standard POSIX `.tar.gz` archive, skipping `node_modules`/`.git` and symlinks.
/// If none of `subdirs` exist, packs `root` directly.
/// Returns `(compressed_bytes, raw_file_bytes)`.
pub fn pack_subdirs_to_tar_gz(
    root: &Path,
    subdirs: &[&str],
    dest_tar_gz: &Path,
    extra_skip: &[&str],
) -> Result<(u64, u64), String> {
    if let Some(parent) = dest_tar_gz.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed creating archive parent dir {}: {e}", parent.display()))?;
    }
    let out_file = File::create(dest_tar_gz)
        .map_err(|e| format!("Failed creating archive {}: {e}", dest_tar_gz.display()))?;
    let mut encoder = GzEncoder::new(BufWriter::new(out_file), Compression::fast());

    let mut skips: Vec<&str> = DEFAULT_SKIP_DIRS.to_vec();
    skips.extend_from_slice(extra_skip);

    let mut raw_bytes = 0u64;
    let existing_subs: Vec<&&str> = subdirs
        .iter()
        .filter(|&&sub| root.join(sub).exists())
        .collect();

    if existing_subs.is_empty() {
        raw_bytes += append_dir_recursive(&mut encoder, root, "", &skips)?;
    } else {
        for sub in existing_subs {
            let sub_path = root.join(sub);
            if sub_path.is_dir() {
                write_tar_entry_header(&mut encoder, &format!("{sub}/"), 0o755, 0, 0, b'5')?;
                raw_bytes += append_dir_recursive(&mut encoder, &sub_path, sub, &skips)?;
            }
        }
    }

    // Two 512-byte zero blocks terminate a POSIX tar archive
    encoder
        .write_all(&[0u8; BLOCK_SIZE * 2])
        .map_err(|e| format!("Failed writing tar EOF trailer: {e}"))?;
    let buf_writer = encoder
        .finish()
        .map_err(|e| format!("Failed finalizing gzip stream: {e}"))?;
    let _file = buf_writer
        .into_inner()
        .map_err(|e| format!("Failed flushing archive buffer: {e}"))?;

    let compressed_bytes = fs::metadata(dest_tar_gz).map(|m| m.len()).unwrap_or(0);
    Ok((compressed_bytes, raw_bytes))
}

/// Packs a single directory's contents at archive root into `.tar.gz`.
pub fn pack_dir_to_tar_gz(
    src_dir: &Path,
    dest_tar_gz: &Path,
    extra_skip: &[&str],
) -> Result<(u64, u64), String> {
    pack_subdirs_to_tar_gz(src_dir, &[], dest_tar_gz, extra_skip)
}

/// Safely unpacks a `.tar.gz` stream into `dest_dir`, strictly rejecting any traversal (`..`),
/// absolute paths, symlinks/hardlinks, or oversized payloads. Returns extracted file bytes.
pub fn unpack_tar_gz_safe(src_tar_gz: &Path, dest_dir: &Path) -> Result<u64, String> {
    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Failed creating destination {}: {e}", dest_dir.display()))?;
    let file = File::open(src_tar_gz)
        .map_err(|e| format!("Failed opening archive {}: {e}", src_tar_gz.display()))?;
    let mut decoder = GzDecoder::new(BufReader::new(file));

    let mut hdr = [0u8; BLOCK_SIZE];
    let mut pax_next_path: Option<String> = None;
    let mut total_extracted = 0u64;
    const MAX_EXTRACT_BYTES: u64 = 16 * 1024 * 1024 * 1024; // 16 GiB safety cap

    loop {
        match decoder.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("Failed reading tar header: {e}")),
        }

        if hdr.iter().all(|&b| b == 0) {
            break;
        }

        let stored_chk = parse_octal(&hdr[148..156])
            .ok_or_else(|| "Invalid tar header checksum field".to_string())?;
        let mut chk_hdr = hdr;
        chk_hdr[148..156].fill(b' ');
        let computed_chk: u64 = chk_hdr.iter().map(|&b| b as u64).sum();
        if stored_chk != computed_chk {
            return Err("Corrupt tar entry checksum".to_string());
        }

        let size = parse_octal(&hdr[124..136])
            .ok_or_else(|| "Invalid tar entry size field".to_string())?;
        let mode = parse_octal(&hdr[100..108]).unwrap_or(0o644) as u32;
        let typeflag = hdr[156];

        if total_extracted.saturating_add(size) > MAX_EXTRACT_BYTES {
            return Err("Archive exceeded maximum safe extraction size".to_string());
        }

        // Read payload (with padding) cleanly before handling control records
        let padded_size = size.div_ceil(BLOCK_SIZE as u64) * BLOCK_SIZE as u64;

        if typeflag == b'x' {
            let mut pax_buf = vec![0u8; padded_size as usize];
            decoder
                .read_exact(&mut pax_buf)
                .map_err(|e| format!("Failed reading PAX header payload: {e}"))?;
            pax_next_path = parse_pax_path(&pax_buf[..size as usize]);
            continue;
        }
        if typeflag == b'g' {
            let mut skip_buf = vec![0u8; padded_size as usize];
            decoder
                .read_exact(&mut skip_buf)
                .map_err(|e| format!("Failed reading global PAX header: {e}"))?;
            continue;
        }

        let raw_path = if let Some(p) = pax_next_path.take() {
            p
        } else {
            let name = String::from_utf8_lossy(
                hdr[0..100].split(|&b| b == 0).next().unwrap_or(&[]),
            )
            .to_string();
            let prefix = String::from_utf8_lossy(
                hdr[345..500].split(|&b| b == 0).next().unwrap_or(&[]),
            )
            .to_string();
            if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            }
        };

        if matches!(typeflag, b'1' | b'2') {
            return Err(format!("Symlinks/hardlinks are forbidden in shift archives ({raw_path})"));
        }

        let rel_path = validate_relative_entry_path(&raw_path)?;
        let target_path = if rel_path.as_os_str().is_empty() {
            dest_dir.to_path_buf()
        } else {
            dest_dir.join(&rel_path)
        };

        if typeflag == b'5' {
            fs::create_dir_all(&target_path)
                .map_err(|e| format!("Failed creating extracted dir {}: {e}", target_path.display()))?;
            if padded_size > 0 {
                let mut skip_buf = vec![0u8; padded_size as usize];
                decoder
                    .read_exact(&mut skip_buf)
                    .map_err(|e| format!("Failed skipping dir payload: {e}"))?;
            }
        } else if typeflag == b'0' || typeflag == 0 {
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed creating parent {}: {e}", parent.display()))?;
            }
            let mut out = File::create(&target_path)
                .map_err(|e| format!("Failed creating extracted file {}: {e}", target_path.display()))?;
            let mut remaining = size;
            let mut buf = [0u8; 65536];
            while remaining > 0 {
                let want = (remaining as usize).min(buf.len());
                decoder
                    .read_exact(&mut buf[..want])
                    .map_err(|e| format!("Failed reading extracted file payload: {e}"))?;
                out.write_all(&buf[..want])
                    .map_err(|e| format!("Failed writing extracted file: {e}"))?;
                remaining -= want as u64;
                total_extracted += want as u64;
            }
            let pad = (padded_size - size) as usize;
            if pad > 0 {
                let mut pad_buf = vec![0u8; pad];
                decoder
                    .read_exact(&mut pad_buf)
                    .map_err(|e| format!("Failed reading tar padding bytes: {e}"))?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&target_path, fs::Permissions::from_mode(mode & 0o777));
            }
            #[cfg(not(unix))]
            {
                let _ = mode;
            }
        } else {
            // Skip unknown metadata entry safely
            if padded_size > 0 {
                let mut skip_buf = vec![0u8; padded_size as usize];
                decoder
                    .read_exact(&mut skip_buf)
                    .map_err(|e| format!("Failed skipping entry payload: {e}"))?;
            }
        }
    }

    Ok(total_extracted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "{tag}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = fs::create_dir_all(&p);
        p
    }

    #[test]
    fn pack_and_unpack_roundtrip_and_skips_heavy_dirs() {
        let src = tmp_dir("webdep-archive-src");
        let dst = tmp_dir("webdep-archive-dst");
        let tgz = dst.join("bundle.tar.gz");

        fs::create_dir_all(src.join("current/nested/deep")).unwrap();
        fs::create_dir_all(src.join("current/node_modules/pkg")).unwrap();
        fs::create_dir_all(src.join("current/.git/objects")).unwrap();
        fs::create_dir_all(src.join("containers/app")).unwrap();
        fs::write(src.join("current/index.js"), b"console.log('ok');").unwrap();
        fs::write(src.join("current/nested/deep/data.json"), br#"{"hello":"world"}"#).unwrap();
        fs::write(src.join("current/node_modules/pkg/big.js"), b"ignored").unwrap();
        fs::write(src.join("current/.git/objects/abc"), b"ignored-git").unwrap();
        fs::write(src.join("containers/app/config.json"), br#"{"port":3000}"#).unwrap();

        // Also test >110 char relative path via ustar prefix / PAX
        let long_sub = format!("current/{}/{}", "a".repeat(90), "b".repeat(50));
        fs::create_dir_all(src.join(&long_sub)).unwrap();
        fs::write(src.join(&long_sub).join("long.txt"), b"long-path-ok").unwrap();

        let (compressed, raw) =
            pack_subdirs_to_tar_gz(&src, &["current", "containers"], &tgz, &[]).unwrap();
        assert!(compressed > 0);
        assert!(raw > 30);

        let unpacked_dir = dst.join("extracted");
        let extracted = unpack_tar_gz_safe(&tgz, &unpacked_dir).unwrap();
        assert_eq!(extracted, raw);
        assert_eq!(
            fs::read_to_string(unpacked_dir.join("current/index.js")).unwrap(),
            "console.log('ok');"
        );
        assert_eq!(
            fs::read_to_string(unpacked_dir.join("containers/app/config.json")).unwrap(),
            r#"{"port":3000}"#
        );
        assert_eq!(
            fs::read_to_string(unpacked_dir.join(&long_sub).join("long.txt")).unwrap(),
            "long-path-ok"
        );
        assert!(!unpacked_dir.join("current/node_modules").exists());
        assert!(!unpacked_dir.join("current/.git").exists());

        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }

    #[test]
    fn unpack_rejects_traversal_and_symlinks() {
        let dir = tmp_dir("webdep-archive-sec");
        let bad_tgz = dir.join("evil.tar.gz");
        let out_dir = dir.join("out");

        // 1. Forge path traversal tar
        {
            let f = File::create(&bad_tgz).unwrap();
            let mut enc = GzEncoder::new(f, Compression::fast());
            let hdr = build_ustar_header("", "../../etc/passwd", 0o644, 4, 0, b'0');
            enc.write_all(&hdr).unwrap();
            let mut block = [0u8; BLOCK_SIZE];
            block[0..4].copy_from_slice(b"evil");
            enc.write_all(&block).unwrap();
            enc.write_all(&[0u8; BLOCK_SIZE * 2]).unwrap();
            enc.finish().unwrap();
        }
        let err = unpack_tar_gz_safe(&bad_tgz, &out_dir).unwrap_err();
        assert!(err.contains("traversal"), "expected traversal error, got: {err}");

        // 2. Forge symlink entry
        {
            let f = File::create(&bad_tgz).unwrap();
            let mut enc = GzEncoder::new(f, Compression::fast());
            let hdr = build_ustar_header("", "escape_link", 0o777, 0, 0, b'2');
            enc.write_all(&hdr).unwrap();
            enc.write_all(&[0u8; BLOCK_SIZE * 2]).unwrap();
            enc.finish().unwrap();
        }
        let sym_err = unpack_tar_gz_safe(&bad_tgz, &out_dir).unwrap_err();
        assert!(sym_err.contains("Symlink"), "expected symlink error, got: {sym_err}");

        let _ = fs::remove_dir_all(&dir);
    }
}
