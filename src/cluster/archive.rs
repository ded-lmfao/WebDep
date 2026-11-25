use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use flate2::read::{DeflateDecoder, GzDecoder};
use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::{Compression, Crc};
use sha2::{Digest, Sha256};

const BLOCK_SIZE: usize = 512;
const DEFAULT_SKIP_DIRS: &[&str] = &["node_modules", ".git"];
const MAX_EXTRACT_BYTES: u64 = 16 * 1024 * 1024 * 1024; // 16 GiB safety cap
const MAX_ARCHIVE_ENTRIES: usize = 500_000;

const ZIP_LOCAL_FILE_SIG: u32 = 0x0403_4b50;
const ZIP_CENTRAL_DIR_SIG: u32 = 0x0201_4b50;
const ZIP_EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP_FLAG_UTF8: u16 = 0x0800;

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
            rel_unix
                .rsplit('/')
                .next()
                .unwrap_or("entry")
                .chars()
                .take(80)
                .collect::<String>()
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

/// Validates that a symlink target stored for `entry_rel` is a relative path whose
/// normalized destination stays strictly within the archive root (no escape via `..`).
fn validate_relative_symlink_target(entry_rel: &Path, target: &str) -> Result<String, String> {
    let t = target.trim();
    if t.is_empty() || t.contains('\0') || t.contains(':') {
        return Err(format!("Unsafe symlink target rejected: {target}"));
    }
    if t.starts_with('/') || t.starts_with('\\') {
        return Err(format!("Unsafe absolute symlink target rejected: {target}"));
    }
    let norm = t.replace('\\', "/");
    let mut depth: isize = entry_rel
        .parent()
        .map(|p| p.components().count() as isize)
        .unwrap_or(0);
    for seg in norm.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("Unsafe escaping symlink target rejected: {target}"));
                }
            }
            _ => depth += 1,
        }
    }
    Ok(norm)
}

/// Ensures no existing ancestor of `target` inside `root` is a symlink before creating/writing a file.
fn ensure_no_symlink_ancestors(root: &Path, target: &Path) -> Result<(), String> {
    let Ok(rel) = target.strip_prefix(root) else {
        return Err("Target escapes archive root".to_string());
    };
    let mut cur = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for (idx, comp) in comps.iter().enumerate() {
        cur.push(comp);
        if idx + 1 < comps.len() {
            if let Ok(meta) = fs::symlink_metadata(&cur) {
                if meta.file_type().is_symlink() {
                    return Err(format!(
                        "Refusing to traverse symlink directory during extraction: {}",
                        cur.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

fn append_dir_recursive<W: Write>(
    writer: &mut W,
    disk_dir: &Path,
    archive_prefix: &str,
    skip: &dyn Fn(&str) -> bool,
    files: &mut u64,
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
        if skip(name_str.as_ref()) {
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
            raw_bytes += append_dir_recursive(writer, &path, &rel, skip, files)?;
        } else if meta.is_file() {
            *files += 1;
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
    let mut files = 0u64;
    let skip = |name: &str| skips.contains(&name);
    let existing_subs: Vec<&&str> = subdirs
        .iter()
        .filter(|&&sub| root.join(sub).exists())
        .collect();

    if existing_subs.is_empty() {
        raw_bytes += append_dir_recursive(&mut encoder, root, "", &skip, &mut files)?;
    } else {
        for sub in existing_subs {
            let sub_path = root.join(sub);
            if sub_path.is_dir() {
                write_tar_entry_header(&mut encoder, &format!("{sub}/"), 0o755, 0, 0, b'5')?;
                raw_bytes += append_dir_recursive(&mut encoder, &sub_path, sub, &skip, &mut files)?;
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

/// What `pack_backup_archive` wrote.
pub struct PackStats {
    pub compressed_bytes: u64,
    /// Files from the app's folder (the env file is not counted).
    pub files: u64,
    pub sha256: String,
}

/// Counts and hashes everything written through it.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Backup ignore pattern: an exact name, `*suffix`, or `prefix*`.
fn ignore_name_matches(pattern: &str, name: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*').filter(|s| !s.contains('*')) {
        name.ends_with(suffix)
    } else if let Some(prefix) = pattern.strip_suffix('*').filter(|s| !s.contains('*')) {
        name.starts_with(prefix)
    } else {
        pattern == name
    }
}

fn write_tar_bytes_entry<W: Write>(writer: &mut W, name: &str, bytes: &[u8]) -> Result<(), String> {
    let mtime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    write_tar_entry_header(writer, name, 0o600, bytes.len() as u64, mtime, b'0')?;
    writer.write_all(bytes).map_err(|e| format!("Failed writing {name} to archive: {e}"))?;
    let pad = (BLOCK_SIZE - (bytes.len() % BLOCK_SIZE)) % BLOCK_SIZE;
    if pad > 0 {
        writer
            .write_all(&[0u8; BLOCK_SIZE][..pad])
            .map_err(|e| format!("Failed writing tar block padding: {e}"))?;
    }
    Ok(())
}

/// Packs a whole app folder into a backup archive: `backup.env` (when given) and the folder's
/// contents under `files/`. Everything is included (even `node_modules` and `.git`) except
/// symlinks and names matching `ignore`. The archive is flushed to disk and its SHA-256 is
/// computed while writing. The caller passes a `.part` path and renames it once this succeeds.
pub fn pack_backup_archive(
    source_root: &Path,
    env: Option<&str>,
    dest_tar_gz: &Path,
    ignore: &[String],
) -> Result<PackStats, String> {
    if let Some(parent) = dest_tar_gz.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed creating archive folder {}: {e}", parent.display()))?;
    }
    let out_file = File::create(dest_tar_gz)
        .map_err(|e| format!("Failed creating archive {}: {e}", dest_tar_gz.display()))?;
    let hashing = HashingWriter { inner: BufWriter::new(out_file), hasher: Sha256::new(), bytes: 0 };
    // Fast compression: backups run next to live apps, so favor low CPU use over size.
    let mut encoder = GzEncoder::new(hashing, Compression::fast());

    if let Some(env) = env {
        write_tar_bytes_entry(&mut encoder, "backup.env", env.as_bytes())?;
    }
    write_tar_entry_header(&mut encoder, "files/", 0o755, 0, 0, b'5')?;
    let skip = |name: &str| ignore.iter().any(|p| ignore_name_matches(p, name));
    let mut files = 0u64;
    append_dir_recursive(&mut encoder, source_root, "files", &skip, &mut files)?;
    encoder
        .write_all(&[0u8; BLOCK_SIZE * 2])
        .map_err(|e| format!("Failed writing tar EOF trailer: {e}"))?;

    let HashingWriter { inner, hasher, bytes } = encoder
        .finish()
        .map_err(|e| format!("Failed finalizing gzip stream: {e}"))?;
    let file = inner.into_inner().map_err(|e| format!("Failed flushing archive: {e}"))?;
    file.sync_all().map_err(|e| format!("Failed syncing archive to disk: {e}"))?;
    Ok(PackStats { compressed_bytes: bytes, files, sha256: format!("{:x}", hasher.finalize()) })
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

// ---------------------------------------------------------------------------
// Pure-Rust Streaming ZIP Writer & Safe ZIP Reader (with ZIP64 + Unix modes)
// ---------------------------------------------------------------------------

fn unix_secs_to_dos(mtime: u64) -> (u16, u16) {
    // Convert UNIX seconds into approximate UTC DOS date/time (valid 1980..2107)
    let secs_per_day = 86_400u64;
    let mut days = (mtime / secs_per_day) as i64;
    let tod = mtime % secs_per_day;
    let hour = (tod / 3600) as u16;
    let min = ((tod % 3600) / 60) as u16;
    let sec2 = ((tod % 60) / 2) as u16;
    let dos_time = (hour << 11) | (min << 5) | sec2;

    // Civil-from-days algorithm (Howard Hinnant)
    days += 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u16;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u16;
    let year = (y + if m <= 2 { 1 } else { 0 }).clamp(1980, 2107) as u16;
    let dos_date = ((year - 1980) << 9) | (m << 5) | d;
    (dos_time, dos_date)
}

struct ZipCdEntry {
    name: String,
    method: u16,
    dos_time: u16,
    dos_date: u16,
    crc32: u32,
    comp_size: u64,
    uncomp_size: u64,
    external_attr: u32,
    local_header_offset: u64,
}

struct ZipArchiveWriter {
    file: File,
    entries: Vec<ZipCdEntry>,
    raw_bytes: u64,
}

impl ZipArchiveWriter {
    fn create(dest_zip: &Path) -> Result<Self, String> {
        if let Some(parent) = dest_zip.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed creating zip parent dir {}: {e}", parent.display()))?;
        }
        let file = File::create(dest_zip)
            .map_err(|e| format!("Failed creating zip {}: {e}", dest_zip.display()))?;
        Ok(Self {
            file,
            entries: Vec::new(),
            raw_bytes: 0,
        })
    }

    fn write_dir(&mut self, rel_dir: &str, mode: u32, mtime: u64) -> Result<(), String> {
        let name = if rel_dir.ends_with('/') {
            rel_dir.to_string()
        } else {
            format!("{rel_dir}/")
        };
        let name_bytes = name.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            return Err("ZIP entry path exceeds 64 KiB".to_string());
        }
        let (dos_time, dos_date) = unix_secs_to_dos(mtime);
        let offset = self
            .file
            .stream_position()
            .map_err(|e| format!("ZIP seek failed: {e}"))?;

        let mut hdr = [0u8; 30];
        hdr[0..4].copy_from_slice(&ZIP_LOCAL_FILE_SIG.to_le_bytes());
        hdr[4..6].copy_from_slice(&20u16.to_le_bytes());
        hdr[6..8].copy_from_slice(&ZIP_FLAG_UTF8.to_le_bytes());
        hdr[8..10].copy_from_slice(&0u16.to_le_bytes());
        hdr[10..12].copy_from_slice(&dos_time.to_le_bytes());
        hdr[12..14].copy_from_slice(&dos_date.to_le_bytes());
        hdr[26..28].copy_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        self.file
            .write_all(&hdr)
            .and_then(|_| self.file.write_all(name_bytes))
            .map_err(|e| format!("Failed writing ZIP directory header: {e}"))?;

        let ext_attr = ((0o040000 | (mode & 0o777)) << 16) | 0x10;
        self.entries.push(ZipCdEntry {
            name,
            method: 0,
            dos_time,
            dos_date,
            crc32: 0,
            comp_size: 0,
            uncomp_size: 0,
            external_attr: ext_attr,
            local_header_offset: offset,
        });
        Ok(())
    }

    fn write_symlink(&mut self, rel_path: &str, target: &str, mtime: u64) -> Result<(), String> {
        let name_bytes = rel_path.as_bytes();
        let data = target.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            return Err("ZIP entry path exceeds 64 KiB".to_string());
        }
        let mut crc = Crc::new();
        crc.update(data);
        let crc32 = crc.sum();
        let (dos_time, dos_date) = unix_secs_to_dos(mtime);
        let offset = self
            .file
            .stream_position()
            .map_err(|e| format!("ZIP seek failed: {e}"))?;
        let sz32 = data.len() as u32;

        let mut hdr = [0u8; 30];
        hdr[0..4].copy_from_slice(&ZIP_LOCAL_FILE_SIG.to_le_bytes());
        hdr[4..6].copy_from_slice(&20u16.to_le_bytes());
        hdr[6..8].copy_from_slice(&ZIP_FLAG_UTF8.to_le_bytes());
        hdr[8..10].copy_from_slice(&0u16.to_le_bytes());
        hdr[10..12].copy_from_slice(&dos_time.to_le_bytes());
        hdr[12..14].copy_from_slice(&dos_date.to_le_bytes());
        hdr[14..18].copy_from_slice(&crc32.to_le_bytes());
        hdr[18..22].copy_from_slice(&sz32.to_le_bytes());
        hdr[22..26].copy_from_slice(&sz32.to_le_bytes());
        hdr[26..28].copy_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        self.file
            .write_all(&hdr)
            .and_then(|_| self.file.write_all(name_bytes))
            .and_then(|_| self.file.write_all(data))
            .map_err(|e| format!("Failed writing ZIP symlink entry: {e}"))?;

        let ext_attr = (0o120000 | 0o777) << 16;
        self.entries.push(ZipCdEntry {
            name: rel_path.to_string(),
            method: 0,
            dos_time,
            dos_date,
            crc32,
            comp_size: data.len() as u64,
            uncomp_size: data.len() as u64,
            external_attr: ext_attr,
            local_header_offset: offset,
        });
        Ok(())
    }

    fn write_file(&mut self, rel_path: &str, src_path: &Path, mode: u32, mtime: u64) -> Result<(), String> {
        let name_bytes = rel_path.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            return Err("ZIP entry path exceeds 64 KiB".to_string());
        }
        let mut src = File::open(src_path)
            .map_err(|e| format!("Failed opening {} for zip: {e}", src_path.display()))?;
        let file_len = src.metadata().map(|m| m.len()).unwrap_or(0);
        let use_zip64_local = file_len >= 0xFFFF_0000;
        let method: u16 = if file_len == 0 { 0 } else { 8 };
        let version_needed: u16 = if use_zip64_local { 45 } else { 20 };
        let extra_len: u16 = if use_zip64_local { 20 } else { 0 };
        let (dos_time, dos_date) = unix_secs_to_dos(mtime);
        let offset = self
            .file
            .stream_position()
            .map_err(|e| format!("ZIP seek failed: {e}"))?;

        let mut hdr = [0u8; 30];
        hdr[0..4].copy_from_slice(&ZIP_LOCAL_FILE_SIG.to_le_bytes());
        hdr[4..6].copy_from_slice(&version_needed.to_le_bytes());
        hdr[6..8].copy_from_slice(&ZIP_FLAG_UTF8.to_le_bytes());
        hdr[8..10].copy_from_slice(&method.to_le_bytes());
        hdr[10..12].copy_from_slice(&dos_time.to_le_bytes());
        hdr[12..14].copy_from_slice(&dos_date.to_le_bytes());
        if use_zip64_local {
            hdr[18..22].copy_from_slice(&u32::MAX.to_le_bytes());
            hdr[22..26].copy_from_slice(&u32::MAX.to_le_bytes());
        }
        hdr[26..28].copy_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        hdr[28..30].copy_from_slice(&extra_len.to_le_bytes());

        self.file
            .write_all(&hdr)
            .and_then(|_| self.file.write_all(name_bytes))
            .map_err(|e| format!("Failed writing ZIP local header: {e}"))?;

        if use_zip64_local {
            let mut z64 = [0u8; 20];
            z64[0..2].copy_from_slice(&1u16.to_le_bytes());
            z64[2..4].copy_from_slice(&16u16.to_le_bytes());
            self.file
                .write_all(&z64)
                .map_err(|e| format!("Failed writing ZIP64 local extra: {e}"))?;
        }

        let data_start = self
            .file
            .stream_position()
            .map_err(|e| format!("ZIP position error: {e}"))?;

        let mut crc = Crc::new();
        let mut uncomp_size = 0u64;
        let mut buf = [0u8; 65536];

        if method == 8 {
            let file_clone = self
                .file
                .try_clone()
                .map_err(|e| format!("Failed cloning zip file handle: {e}"))?;
            let mut enc = DeflateEncoder::new(BufWriter::new(file_clone), Compression::fast());
            loop {
                let n = src
                    .read(&mut buf)
                    .map_err(|e| format!("Failed reading {} for zip: {e}", src_path.display()))?;
                if n == 0 {
                    break;
                }
                crc.update(&buf[..n]);
                uncomp_size += n as u64;
                enc.write_all(&buf[..n])
                    .map_err(|e| format!("Failed compressing {} to zip: {e}", src_path.display()))?;
            }
            let bw = enc
                .finish()
                .map_err(|e| format!("Failed finishing deflate stream: {e}"))?;
            bw.into_inner()
                .map_err(|e| format!("Failed flushing deflate writer: {e}"))?;
            self.file
                .seek(SeekFrom::End(0))
                .map_err(|e| format!("ZIP seek end failed: {e}"))?;
        }

        let data_end = self
            .file
            .stream_position()
            .map_err(|e| format!("ZIP position error: {e}"))?;
        let comp_size = data_end.saturating_sub(data_start);
        let crc32 = crc.sum();

        // Patch CRC32 + sizes in Local File Header
        self.file
            .seek(SeekFrom::Start(offset + 14))
            .map_err(|e| format!("ZIP patch seek failed: {e}"))?;
        let mut patch12 = [0u8; 12];
        patch12[0..4].copy_from_slice(&crc32.to_le_bytes());
        if use_zip64_local {
            patch12[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
            patch12[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
            self.file
                .write_all(&patch12)
                .map_err(|e| format!("ZIP header patch failed: {e}"))?;
            self.file
                .seek(SeekFrom::Start(offset + 30 + (name_bytes.len() as u64) + 4))
                .map_err(|e| format!("ZIP64 extra seek failed: {e}"))?;
            let mut z64_sizes = [0u8; 16];
            z64_sizes[0..8].copy_from_slice(&uncomp_size.to_le_bytes());
            z64_sizes[8..16].copy_from_slice(&comp_size.to_le_bytes());
            self.file
                .write_all(&z64_sizes)
                .map_err(|e| format!("ZIP64 extra patch failed: {e}"))?;
        } else {
            patch12[4..8].copy_from_slice(&(comp_size as u32).to_le_bytes());
            patch12[8..12].copy_from_slice(&(uncomp_size as u32).to_le_bytes());
            self.file
                .write_all(&patch12)
                .map_err(|e| format!("ZIP header patch failed: {e}"))?;
        }
        self.file
            .seek(SeekFrom::Start(data_end))
            .map_err(|e| format!("ZIP seek restore failed: {e}"))?;

        self.raw_bytes += uncomp_size;
        let ext_attr = (0o100000 | (mode & 0o777)) << 16;
        self.entries.push(ZipCdEntry {
            name: rel_path.to_string(),
            method,
            dos_time,
            dos_date,
            crc32,
            comp_size,
            uncomp_size,
            external_attr: ext_attr,
            local_header_offset: offset,
        });
        Ok(())
    }

    fn finish(mut self) -> Result<(u64, u64), String> {
        let cd_offset = self
            .file
            .stream_position()
            .map_err(|e| format!("ZIP CD position failed: {e}"))?;

        for e in &self.entries {
            let name_bytes = e.name.as_bytes();
            let need_u = e.uncomp_size >= u32::MAX as u64;
            let need_c = e.comp_size >= u32::MAX as u64;
            let need_o = e.local_header_offset >= u32::MAX as u64;
            let mut z64_payload = Vec::new();
            if need_u {
                z64_payload.extend_from_slice(&e.uncomp_size.to_le_bytes());
            }
            if need_c {
                z64_payload.extend_from_slice(&e.comp_size.to_le_bytes());
            }
            if need_o {
                z64_payload.extend_from_slice(&e.local_header_offset.to_le_bytes());
            }
            let mut extra = Vec::new();
            if !z64_payload.is_empty() {
                extra.extend_from_slice(&1u16.to_le_bytes());
                extra.extend_from_slice(&(z64_payload.len() as u16).to_le_bytes());
                extra.extend_from_slice(&z64_payload);
            }

            let ver_need: u16 = if extra.is_empty() { 20 } else { 45 };
            let mut cd = [0u8; 46];
            cd[0..4].copy_from_slice(&ZIP_CENTRAL_DIR_SIG.to_le_bytes());
            cd[4..6].copy_from_slice(&((3u16 << 8) | 20u16).to_le_bytes()); // Unix
            cd[6..8].copy_from_slice(&ver_need.to_le_bytes());
            cd[8..10].copy_from_slice(&ZIP_FLAG_UTF8.to_le_bytes());
            cd[10..12].copy_from_slice(&e.method.to_le_bytes());
            cd[12..14].copy_from_slice(&e.dos_time.to_le_bytes());
            cd[14..16].copy_from_slice(&e.dos_date.to_le_bytes());
            cd[16..20].copy_from_slice(&e.crc32.to_le_bytes());
            cd[20..24].copy_from_slice(&(if need_c { u32::MAX } else { e.comp_size as u32 }).to_le_bytes());
            cd[24..28].copy_from_slice(&(if need_u { u32::MAX } else { e.uncomp_size as u32 }).to_le_bytes());
            cd[28..30].copy_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            cd[30..32].copy_from_slice(&(extra.len() as u16).to_le_bytes());
            cd[38..42].copy_from_slice(&e.external_attr.to_le_bytes());
            cd[42..46].copy_from_slice(&(if need_o { u32::MAX } else { e.local_header_offset as u32 }).to_le_bytes());

            self.file
                .write_all(&cd)
                .and_then(|_| self.file.write_all(name_bytes))
                .and_then(|_| self.file.write_all(&extra))
                .map_err(|err| format!("Failed writing ZIP central directory: {err}"))?;
        }

        let cd_end = self
            .file
            .stream_position()
            .map_err(|e| format!("ZIP CD end position failed: {e}"))?;
        let cd_size = cd_end.saturating_sub(cd_offset);
        let total_entries = self.entries.len() as u64;

        let need_zip64_eocd = total_entries >= u16::MAX as u64
            || cd_size >= u32::MAX as u64
            || cd_offset >= u32::MAX as u64;

        if need_zip64_eocd {
            let z64_eocd_offset = cd_end;
            let mut z64_eocd = [0u8; 56];
            z64_eocd[0..4].copy_from_slice(&ZIP64_EOCD_SIG.to_le_bytes());
            z64_eocd[4..12].copy_from_slice(&44u64.to_le_bytes());
            z64_eocd[12..14].copy_from_slice(&((3u16 << 8) | 45u16).to_le_bytes());
            z64_eocd[14..16].copy_from_slice(&45u16.to_le_bytes());
            z64_eocd[24..32].copy_from_slice(&total_entries.to_le_bytes());
            z64_eocd[32..40].copy_from_slice(&total_entries.to_le_bytes());
            z64_eocd[40..48].copy_from_slice(&cd_size.to_le_bytes());
            z64_eocd[48..56].copy_from_slice(&cd_offset.to_le_bytes());

            let mut z64_loc = [0u8; 20];
            z64_loc[0..4].copy_from_slice(&ZIP64_LOCATOR_SIG.to_le_bytes());
            z64_loc[8..16].copy_from_slice(&z64_eocd_offset.to_le_bytes());
            z64_loc[16..20].copy_from_slice(&1u32.to_le_bytes());

            self.file
                .write_all(&z64_eocd)
                .and_then(|_| self.file.write_all(&z64_loc))
                .map_err(|e| format!("Failed writing ZIP64 EOCD: {e}"))?;
        }

        let mut eocd = [0u8; 22];
        eocd[0..4].copy_from_slice(&ZIP_EOCD_SIG.to_le_bytes());
        let cnt16 = total_entries.min(u16::MAX as u64) as u16;
        let cd_size32 = cd_size.min(u32::MAX as u64) as u32;
        let cd_off32 = cd_offset.min(u32::MAX as u64) as u32;
        eocd[8..10].copy_from_slice(&cnt16.to_le_bytes());
        eocd[10..12].copy_from_slice(&cnt16.to_le_bytes());
        eocd[12..16].copy_from_slice(&cd_size32.to_le_bytes());
        eocd[16..20].copy_from_slice(&cd_off32.to_le_bytes());
        self.file
            .write_all(&eocd)
            .and_then(|_| self.file.flush())
            .map_err(|e| format!("Failed writing ZIP EOCD: {e}"))?;

        let zip_bytes = self.file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok((zip_bytes, self.raw_bytes))
    }
}

fn append_dir_to_zip(
    writer: &mut ZipArchiveWriter,
    disk_dir: &Path,
    archive_prefix: &str,
    subtree_root: &Path,
    skip_names: &[&str],
) -> Result<(), String> {
    let Ok(entries) = fs::read_dir(disk_dir) else {
        return Ok(());
    };
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|e| e.file_name());

    let canon_subtree = fs::canonicalize(subtree_root).unwrap_or_else(|_| subtree_root.to_path_buf());

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

        if meta.file_type().is_symlink() {
            // Preserve safe relative symlinks that resolve strictly within `subtree_root`
            // (e.g. `node_modules/.bin/*`), while skipping external symlinks like `current/data`
            // (which are packed separately via `__webdep_container__/` and re-linked on target).
            if let Ok(link_target) = fs::read_link(&path) {
                let target_str = link_target.to_string_lossy().replace('\\', "/");
                let rel_path_obj = Path::new(&rel);
                if let Ok(safe_target) = validate_relative_symlink_target(rel_path_obj, &target_str) {
                    if let Ok(resolved) = fs::canonicalize(&path) {
                        if resolved.starts_with(&canon_subtree) {
                            writer.write_symlink(&rel, &safe_target, mtime)?;
                        }
                    }
                }
            }
            continue;
        }

        if meta.is_dir() {
            writer.write_dir(&rel, 0o755, mtime)?;
            append_dir_to_zip(writer, &path, &rel, subtree_root, skip_names)?;
        } else if meta.is_file() {
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o777
            };
            #[cfg(not(unix))]
            let mode = 0o644u32;

            writer.write_file(&rel, &path, mode, mtime)?;
        }
    }
    Ok(())
}

/// Packs all project application files (`current`, `app`, `releases`, `backups`, plus any custom
/// top-level files/dirs in `deploy_dir` except `repo` and `tmp`) AND the project's persistent
/// container storage (`db/`, `storage/`, `home/` under `__webdep_container__/`) into a `.zip` file
/// for lossless P2P node-to-node shifting.
pub fn pack_project_shift_to_zip(
    deploy_dir: &Path,
    container_dir: Option<&Path>,
    dest_zip: &Path,
) -> Result<(u64, u64), String> {
    let mut writer = ZipArchiveWriter::create(dest_zip)?;

    let has_current = deploy_dir.join("current").exists();
    if deploy_dir.exists() {
        if let Ok(entries) = fs::read_dir(deploy_dir) {
            let mut items: Vec<_> = entries.flatten().collect();
            items.sort_by_key(|e| e.file_name());
            for entry in items {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if matches!(name_str.as_ref(), "repo" | "tmp" | ".git") {
                    continue;
                }
                let path = entry.path();
                // Resolve top-level `current` symlink/junction so active release files + node_modules transfer cleanly
                if name_str == "current" && path.is_dir() {
                    let resolved_cur = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    writer.write_dir("current", 0o755, 0)?;
                    append_dir_to_zip(&mut writer, &resolved_cur, "current", &resolved_cur, &[".git"])?;
                    continue;
                }
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if meta.file_type().is_symlink() {
                    continue;
                }
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if meta.is_dir() {
                    writer.write_dir(&name_str, 0o755, mtime)?;
                    // Avoid duplicating node_modules across old releases if `current` already packs it
                    let skips: &[&str] = if name_str == "releases" && has_current {
                        &[".git", "node_modules"]
                    } else {
                        &[".git"]
                    };
                    append_dir_to_zip(&mut writer, &path, &name_str, &path, skips)?;
                } else if meta.is_file() {
                    #[cfg(unix)]
                    let mode = {
                        use std::os::unix::fs::PermissionsExt;
                        meta.permissions().mode() & 0o777
                    };
                    #[cfg(not(unix))]
                    let mode = 0o644u32;
                    writer.write_file(&name_str, &path, mode, mtime)?;
                }
            }
        }
    }

    if let Some(cdir) = container_dir {
        if cdir.is_dir() {
            writer.write_dir("__webdep_container__", 0o755, 0)?;
            append_dir_to_zip(
                &mut writer,
                cdir,
                "__webdep_container__",
                cdir,
                &["tmp", "run", "cache", "container.json", ".git"],
            )?;
        }
    }

    writer.finish()
}

#[derive(Debug)]
struct ParsedZipEntry {
    name: String,
    method: u16,
    crc32: u32,
    comp_size: u64,
    uncomp_size: u64,
    external_attr: u32,
    local_header_offset: u64,
}

fn find_zip_eocd(file: &mut File, file_len: u64) -> Result<(u64, u64, u64), String> {
    if file_len < 22 {
        return Err("Invalid ZIP file (too small)".to_string());
    }
    let search_len = file_len.min(65_557) as usize;
    let start_pos = file_len - search_len as u64;
    file.seek(SeekFrom::Start(start_pos))
        .map_err(|e| format!("Failed seeking ZIP EOCD: {e}"))?;
    let mut tail = vec![0u8; search_len];
    file.read_exact(&mut tail)
        .map_err(|e| format!("Failed reading ZIP EOCD tail: {e}"))?;

    let mut eocd_rel = None;
    for i in (0..=tail.len().saturating_sub(22)).rev() {
        if tail[i..i + 4] == ZIP_EOCD_SIG.to_le_bytes() {
            let comment_len = u16::from_le_bytes([tail[i + 20], tail[i + 21]]) as usize;
            if i + 22 + comment_len == tail.len() {
                eocd_rel = Some(i);
                break;
            }
        }
    }
    let eocd_rel = eocd_rel.ok_or_else(|| "ZIP End of Central Directory not found".to_string())?;
    let eocd_abs = start_pos + eocd_rel as u64;
    let eocd = &tail[eocd_rel..eocd_rel + 22];

    let entries16 = u16::from_le_bytes([eocd[10], eocd[11]]) as u64;
    let cd_size32 = u32::from_le_bytes([eocd[12], eocd[13], eocd[14], eocd[15]]) as u64;
    let cd_off32 = u32::from_le_bytes([eocd[16], eocd[17], eocd[18], eocd[19]]) as u64;

    if eocd_abs >= 20
        && (entries16 == u16::MAX as u64
            || cd_size32 == u32::MAX as u64
            || cd_off32 == u32::MAX as u64)
    {
        file.seek(SeekFrom::Start(eocd_abs - 20))
            .map_err(|e| format!("Failed seeking ZIP64 locator: {e}"))?;
        let mut loc = [0u8; 20];
        file.read_exact(&mut loc)
            .map_err(|e| format!("Failed reading ZIP64 locator: {e}"))?;
        if loc[0..4] == ZIP64_LOCATOR_SIG.to_le_bytes() {
            let z64_off = u64::from_le_bytes(loc[8..16].try_into().unwrap());
            file.seek(SeekFrom::Start(z64_off))
                .map_err(|e| format!("Failed seeking ZIP64 EOCD: {e}"))?;
            let mut z64 = [0u8; 56];
            file.read_exact(&mut z64)
                .map_err(|e| format!("Failed reading ZIP64 EOCD: {e}"))?;
            if z64[0..4] != ZIP64_EOCD_SIG.to_le_bytes() {
                return Err("Invalid ZIP64 EOCD signature".to_string());
            }
            let total_entries = u64::from_le_bytes(z64[32..40].try_into().unwrap());
            let cd_size = u64::from_le_bytes(z64[40..48].try_into().unwrap());
            let cd_offset = u64::from_le_bytes(z64[48..56].try_into().unwrap());
            return Ok((total_entries, cd_size, cd_offset));
        }
    }

    Ok((entries16, cd_size32, cd_off32))
}

fn parse_zip_central_directory(file: &mut File, total_entries: u64, cd_offset: u64) -> Result<Vec<ParsedZipEntry>, String> {
    if total_entries > MAX_ARCHIVE_ENTRIES as u64 {
        return Err("ZIP archive exceeds maximum allowed entry count".to_string());
    }
    file.seek(SeekFrom::Start(cd_offset))
        .map_err(|e| format!("Failed seeking ZIP central directory: {e}"))?;

    let mut out = Vec::with_capacity(total_entries as usize);
    let mut hdr = [0u8; 46];
    for _ in 0..total_entries {
        file.read_exact(&mut hdr)
            .map_err(|e| format!("Failed reading ZIP central directory entry: {e}"))?;
        if hdr[0..4] != ZIP_CENTRAL_DIR_SIG.to_le_bytes() {
            return Err("Corrupt ZIP central directory signature".to_string());
        }
        let method = u16::from_le_bytes([hdr[10], hdr[11]]);
        let crc32 = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
        let mut comp_size = u32::from_le_bytes([hdr[20], hdr[21], hdr[22], hdr[23]]) as u64;
        let mut uncomp_size = u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]) as u64;
        let name_len = u16::from_le_bytes([hdr[28], hdr[29]]) as usize;
        let extra_len = u16::from_le_bytes([hdr[30], hdr[31]]) as usize;
        let comment_len = u16::from_le_bytes([hdr[32], hdr[33]]) as usize;
        let external_attr = u32::from_le_bytes([hdr[38], hdr[39], hdr[40], hdr[41]]);
        let mut local_off = u32::from_le_bytes([hdr[42], hdr[43], hdr[44], hdr[45]]) as u64;

        let mut name_buf = vec![0u8; name_len];
        file.read_exact(&mut name_buf)
            .map_err(|e| format!("Failed reading ZIP entry filename: {e}"))?;
        let name = String::from_utf8_lossy(&name_buf).to_string();

        let mut extra_buf = vec![0u8; extra_len];
        file.read_exact(&mut extra_buf)
            .map_err(|e| format!("Failed reading ZIP extra field: {e}"))?;
        if comment_len > 0 {
            file.seek(SeekFrom::Current(comment_len as i64))
                .map_err(|e| format!("Failed skipping ZIP entry comment: {e}"))?;
        }

        // Parse ZIP64 extra field (0x0001) if 32-bit markers are 0xFFFFFFFF
        let mut pos = 0usize;
        while pos + 4 <= extra_buf.len() {
            let eid = u16::from_le_bytes([extra_buf[pos], extra_buf[pos + 1]]);
            let elen = u16::from_le_bytes([extra_buf[pos + 2], extra_buf[pos + 3]]) as usize;
            pos += 4;
            if pos + elen > extra_buf.len() {
                break;
            }
            if eid == 0x0001 {
                let mut zpos = pos;
                if uncomp_size == u32::MAX as u64 && zpos + 8 <= pos + elen {
                    uncomp_size = u64::from_le_bytes(extra_buf[zpos..zpos + 8].try_into().unwrap());
                    zpos += 8;
                }
                if comp_size == u32::MAX as u64 && zpos + 8 <= pos + elen {
                    comp_size = u64::from_le_bytes(extra_buf[zpos..zpos + 8].try_into().unwrap());
                    zpos += 8;
                }
                if local_off == u32::MAX as u64 && zpos + 8 <= pos + elen {
                    local_off = u64::from_le_bytes(extra_buf[zpos..zpos + 8].try_into().unwrap());
                }
            }
            pos += elen;
        }

        out.push(ParsedZipEntry {
            name,
            method,
            crc32,
            comp_size,
            uncomp_size,
            external_attr,
            local_header_offset: local_off,
        });
    }
    Ok(out)
}

fn read_zip_entry_to_writer<W: Write>(
    file: &mut File,
    entry: &ParsedZipEntry,
    out: &mut W,
) -> Result<u64, String> {
    file.seek(SeekFrom::Start(entry.local_header_offset))
        .map_err(|e| format!("Failed seeking ZIP local header: {e}"))?;
    let mut lhdr = [0u8; 30];
    file.read_exact(&mut lhdr)
        .map_err(|e| format!("Failed reading ZIP local header: {e}"))?;
    if lhdr[0..4] != ZIP_LOCAL_FILE_SIG.to_le_bytes() {
        return Err(format!("Invalid ZIP local header for {}", entry.name));
    }
    let name_len = u16::from_le_bytes([lhdr[26], lhdr[27]]) as i64;
    let extra_len = u16::from_le_bytes([lhdr[28], lhdr[29]]) as i64;
    file.seek(SeekFrom::Current(name_len + extra_len))
        .map_err(|e| format!("Failed seeking ZIP payload: {e}"))?;

    let limited = file.take(entry.comp_size);
    let mut crc = Crc::new();
    let mut written = 0u64;
    let mut buf = [0u8; 65536];

    match entry.method {
        0 => {
            let mut r = limited;
            loop {
                let n = r
                    .read(&mut buf)
                    .map_err(|e| format!("Failed reading stored ZIP entry {}: {e}", entry.name))?;
                if n == 0 {
                    break;
                }
                written += n as u64;
                if written > entry.uncomp_size || written > MAX_EXTRACT_BYTES {
                    return Err(format!("ZIP entry {} exceeded declared size", entry.name));
                }
                crc.update(&buf[..n]);
                out.write_all(&buf[..n])
                    .map_err(|e| format!("Failed writing extracted ZIP file {}: {e}", entry.name))?;
            }
        }
        8 => {
            let mut dec = DeflateDecoder::new(limited);
            loop {
                let n = dec
                    .read(&mut buf)
                    .map_err(|e| format!("Failed decompressing ZIP entry {}: {e}", entry.name))?;
                if n == 0 {
                    break;
                }
                written += n as u64;
                if written > entry.uncomp_size || written > MAX_EXTRACT_BYTES {
                    return Err(format!("ZIP entry {} exceeded declared uncompressed size", entry.name));
                }
                crc.update(&buf[..n]);
                out.write_all(&buf[..n])
                    .map_err(|e| format!("Failed writing extracted ZIP file {}: {e}", entry.name))?;
            }
        }
        m => {
            return Err(format!("Unsupported ZIP compression method {m} for {}", entry.name));
        }
    }

    if written != entry.uncomp_size {
        return Err(format!(
            "ZIP entry {} size mismatch (expected {}, got {})",
            entry.name, entry.uncomp_size, written
        ));
    }
    if crc.sum() != entry.crc32 {
        return Err(format!("ZIP entry {} failed CRC-32 verification", entry.name));
    }
    Ok(written)
}

/// Safely unpacks a `.zip` archive into `dest_dir`, validating paths, CRC-32 checksums,
/// total uncompressed size, and restricting symlinks to validated intra-root relative paths
/// created only after all regular files are written.
pub fn unpack_zip_safe(src_zip: &Path, dest_dir: &Path) -> Result<u64, String> {
    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Failed creating destination {}: {e}", dest_dir.display()))?;
    let mut file = File::open(src_zip)
        .map_err(|e| format!("Failed opening zip {}: {e}", src_zip.display()))?;
    let file_len = file
        .metadata()
        .map_err(|e| format!("Failed stat zip {}: {e}", src_zip.display()))?
        .len();

    let (total_entries, _cd_size, cd_offset) = find_zip_eocd(&mut file, file_len)?;
    let entries = parse_zip_central_directory(&mut file, total_entries, cd_offset)?;

    let mut total_extracted = 0u64;
    let mut deferred_symlinks: Vec<(PathBuf, PathBuf, String)> = Vec::new();

    for entry in &entries {
        if total_extracted.saturating_add(entry.uncomp_size) > MAX_EXTRACT_BYTES {
            return Err("ZIP archive exceeded maximum safe extraction size".to_string());
        }
        let rel_path = validate_relative_entry_path(&entry.name)?;
        if rel_path.as_os_str().is_empty() {
            continue;
        }
        let target_path = dest_dir.join(&rel_path);
        let unix_mode = (entry.external_attr >> 16) & 0o170777;
        let file_type_bits = unix_mode & 0o170000;
        let is_dir = entry.name.ends_with('/') || file_type_bits == 0o040000;
        let is_symlink = file_type_bits == 0o120000;

        if is_symlink {
            if entry.uncomp_size > 4096 {
                return Err(format!("Oversized symlink entry rejected: {}", entry.name));
            }
            let mut raw_target = Vec::new();
            read_zip_entry_to_writer(&mut file, entry, &mut raw_target)?;
            let target_str = String::from_utf8(raw_target)
                .map_err(|_| format!("Non-UTF8 symlink target in {}", entry.name))?;
            let safe_target = validate_relative_symlink_target(&rel_path, &target_str)?;
            deferred_symlinks.push((rel_path, target_path, safe_target));
            continue;
        }

        if is_dir {
            ensure_no_symlink_ancestors(dest_dir, &target_path)?;
            fs::create_dir_all(&target_path)
                .map_err(|e| format!("Failed creating extracted dir {}: {e}", target_path.display()))?;
            continue;
        }

        if let Some(parent) = target_path.parent() {
            ensure_no_symlink_ancestors(dest_dir, &target_path)?;
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed creating parent {}: {e}", parent.display()))?;
        }

        let mut out = File::create(&target_path)
            .map_err(|e| format!("Failed creating extracted file {}: {e}", target_path.display()))?;
        let written = read_zip_entry_to_writer(&mut file, entry, &mut out)?;
        drop(out);
        total_extracted += written;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = unix_mode & 0o777;
            if perm != 0 {
                let _ = fs::set_permissions(&target_path, fs::Permissions::from_mode(perm));
            }
        }
    }

    for (_rel, link_path, link_target) in deferred_symlinks {
        ensure_no_symlink_ancestors(dest_dir, &link_path)?;
        if let Some(parent) = link_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::remove_file(&link_path);
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&link_target, &link_path);
        }
        #[cfg(windows)]
        {
            let resolved_check = link_path.parent().map(|p| p.join(&link_target));
            if resolved_check.as_ref().map(|p| p.is_dir()).unwrap_or(false) {
                let _ = std::os::windows::fs::symlink_dir(&link_target, &link_path);
            } else {
                let _ = std::os::windows::fs::symlink_file(&link_target, &link_path);
            }
        }
    }

    Ok(total_extracted)
}

fn dir_byte_size_and_verify_safe(dir: &Path, root: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(0);
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let meta = fs::symlink_metadata(&p)
            .map_err(|e| format!("Failed checking extracted file {}: {e}", p.display()))?;
        if meta.file_type().is_symlink() {
            let _ = fs::remove_file(&p);
            continue;
        }
        let canon = fs::canonicalize(&p)
            .map_err(|e| format!("Failed canonicalizing {}: {e}", p.display()))?;
        if !canon.starts_with(root) {
            return Err(format!("Extracted path escaped root: {}", p.display()));
        }
        if meta.is_dir() {
            total += dir_byte_size_and_verify_safe(&p, root)?;
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    Ok(total)
}

fn unpack_rar_safe(src_rar: &Path, dest_dir: &Path) -> Result<u64, String> {
    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Failed creating destination {}: {e}", dest_dir.display()))?;
    let canon_dest = fs::canonicalize(dest_dir)
        .map_err(|e| format!("Failed canonicalizing destination {}: {e}", dest_dir.display()))?;

    let src_str = src_rar.to_string_lossy().to_string();
    let dst_str = dest_dir.to_string_lossy().to_string();

    let attempts: [(&str, Vec<String>); 3] = [
        ("unrar", vec!["x".into(), "-o+".into(), "-y".into(), src_str.clone(), dst_str.clone()]),
        ("unar", vec!["-f".into(), "-o".into(), dst_str.clone(), src_str.clone()]),
        ("bsdtar", vec!["-xf".into(), src_str, "-C".into(), dst_str]),
    ];

    let mut extracted = false;
    let mut last_err = String::from("No RAR extractor (unrar, unar, bsdtar) installed on host");
    for (bin, args) in &attempts {
        match Command::new(bin).args(args).output() {
            Ok(out) if out.status.success() => {
                extracted = true;
                break;
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if !err.is_empty() {
                    last_err = format!("{bin}: {err}");
                }
            }
            Err(_) => {}
        }
    }
    if !extracted {
        return Err(last_err);
    }

    dir_byte_size_and_verify_safe(&canon_dest, &canon_dest)
}

/// Auto-detects archive format (`.zip`, `.tar.gz`, or `.rar`) by magic header and unpacks
/// safely into `dest_dir`. Returns total extracted file bytes.
pub fn unpack_archive_auto_safe(src_archive: &Path, dest_dir: &Path) -> Result<u64, String> {
    let mut f = File::open(src_archive)
        .map_err(|e| format!("Failed opening archive {}: {e}", src_archive.display()))?;
    let mut magic = [0u8; 8];
    let n = f
        .read(&mut magic)
        .map_err(|e| format!("Failed reading archive header: {e}"))?;
    drop(f);

    if n >= 4
        && (magic[..4] == [b'P', b'K', 0x03, 0x04] || magic[..4] == [b'P', b'K', 0x05, 0x06])
    {
        unpack_zip_safe(src_archive, dest_dir)
    } else if n >= 2 && magic[..2] == [0x1f, 0x8b] {
        unpack_tar_gz_safe(src_archive, dest_dir)
    } else if n >= 6 && magic[..6] == [b'R', b'a', b'r', b'!', 0x1a, 0x07] {
        unpack_rar_safe(src_archive, dest_dir)
    } else {
        Err("Unrecognized archive format (expected .zip, .tar.gz, or .rar)".to_string())
    }
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
    fn zip_shift_packs_all_app_files_node_modules_and_container_data() {
        let src_deploy = tmp_dir("webdep-zip-deploy");
        let src_container = tmp_dir("webdep-zip-ctr");
        let dst = tmp_dir("webdep-zip-dst");
        let zip_path = dst.join("shift-bundle.zip");

        fs::create_dir_all(src_deploy.join("current/node_modules/pkg")).unwrap();
        fs::create_dir_all(src_deploy.join("current/.git/objects")).unwrap();
        fs::create_dir_all(src_deploy.join("backups/b1")).unwrap();
        fs::write(src_deploy.join("current/server.js"), b"console.log('zip-ok');").unwrap();
        fs::write(src_deploy.join("current/node_modules/pkg/index.js"), b"module.exports=42;").unwrap();
        fs::write(src_deploy.join("current/.git/objects/abc"), b"skip-git").unwrap();
        fs::write(src_deploy.join("backups/b1/note.txt"), b"backup-data").unwrap();

        fs::create_dir_all(src_container.join("db")).unwrap();
        fs::create_dir_all(src_container.join("storage")).unwrap();
        fs::create_dir_all(src_container.join("tmp")).unwrap();
        fs::write(src_container.join("db/app.sqlite"), b"sqlite-binary-bytes").unwrap();
        fs::write(src_container.join("storage/avatar.png"), b"png-bytes").unwrap();
        fs::write(src_container.join("tmp/scratch.tmp"), b"skip-tmp").unwrap();

        let (zip_bytes, raw_bytes) =
            pack_project_shift_to_zip(&src_deploy, Some(&src_container), &zip_path).unwrap();
        assert!(zip_bytes > 0);
        assert!(raw_bytes > 40);

        let unpacked = dst.join("unpacked");
        let extracted = unpack_archive_auto_safe(&zip_path, &unpacked).unwrap();
        assert_eq!(extracted, raw_bytes);
        assert_eq!(
            fs::read_to_string(unpacked.join("current/server.js")).unwrap(),
            "console.log('zip-ok');"
        );
        assert_eq!(
            fs::read_to_string(unpacked.join("current/node_modules/pkg/index.js")).unwrap(),
            "module.exports=42;"
        );
        assert_eq!(
            fs::read_to_string(unpacked.join("backups/b1/note.txt")).unwrap(),
            "backup-data"
        );
        assert_eq!(
            fs::read_to_string(unpacked.join("__webdep_container__/db/app.sqlite")).unwrap(),
            "sqlite-binary-bytes"
        );
        assert_eq!(
            fs::read_to_string(unpacked.join("__webdep_container__/storage/avatar.png")).unwrap(),
            "png-bytes"
        );
        assert!(!unpacked.join("current/.git").exists());
        assert!(!unpacked.join("__webdep_container__/tmp").exists());

        let _ = fs::remove_dir_all(&src_deploy);
        let _ = fs::remove_dir_all(&src_container);
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

        // 3. Forge escaping zip entry
        let bad_zip = dir.join("evil.zip");
        {
            let mut w = ZipArchiveWriter::create(&bad_zip).unwrap();
            let src = dir.join("payload.txt");
            fs::write(&src, b"evil").unwrap();
            w.write_file("../escape.txt", &src, 0o644, 0).unwrap();
            w.finish().unwrap();
        }
        let zip_err = unpack_zip_safe(&bad_zip, &out_dir).unwrap_err();
        assert!(zip_err.contains("traversal"), "expected zip traversal error, got: {zip_err}");

        // 4. Forge escaping symlink in zip
        let bad_sym_zip = dir.join("evil-sym.zip");
        {
            let mut w = ZipArchiveWriter::create(&bad_sym_zip).unwrap();
            w.write_symlink("sub/escape", "../../etc/passwd", 0).unwrap();
            w.finish().unwrap();
        }
        let zip_sym_err = unpack_zip_safe(&bad_sym_zip, &out_dir).unwrap_err();
        assert!(zip_sym_err.contains("escaping symlink"), "expected zip symlink error, got: {zip_sym_err}");

        let _ = fs::remove_dir_all(&dir);
    }
}
