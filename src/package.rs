use crate::settings::Result;
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path},
    sync::atomic::{AtomicBool, Ordering},
};
fn u16at(b: &[u8], n: usize) -> Result<u16> {
    b.get(n..n + 2)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
        .ok_or("다운로드 ZIP 헤더가 잘렸습니다.".into())
}
fn u32at(b: &[u8], n: usize) -> Result<u32> {
    b.get(n..n + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or("다운로드 ZIP 헤더가 잘렸습니다.".into())
}
fn u64at(b: &[u8], n: usize) -> Result<u64> {
    b.get(n..n + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or("ZIP64 헤더가 잘렸습니다.".into())
}
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['\\', ':', '\0'])
        && !name.starts_with('/')
        && name
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..")
        && Path::new(name)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
pub fn complete(directory: &Path, id: &str) -> bool {
    directory
        .join(format!("{id}.dat"))
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() > 0)
        && ["zip", "epub", "pdf"].iter().any(|ext| {
            directory
                .join(format!("{id}.{ext}"))
                .metadata()
                .is_ok_and(|m| m.is_file() && m.len() > 0)
        })
        || directory.join(format!("{id}.dat")).is_file() && directory.join("content").is_dir()
}
pub fn extract(zip: &Path, to: &Path, id: &str, cancel: &AtomicBool) -> Result<()> {
    let mut file = File::open(zip).map_err(|_| "다운로드 파일을 열 수 없습니다.")?;
    let size = file
        .metadata()
        .map_err(|_| "다운로드 파일을 확인할 수 없습니다.")?
        .len();
    if size < 22 {
        return Err("다운로드 ZIP 파일이 잘렸습니다.".into());
    }
    let tail = size.min(65557) as usize;
    file.seek(SeekFrom::End(-(tail as i64)))
        .map_err(|_| "ZIP 헤더를 찾을 수 없습니다.")?;
    let mut end = vec![0; tail];
    file.read_exact(&mut end)
        .map_err(|_| "ZIP 헤더를 읽을 수 없습니다.")?;
    let at = (0..=tail - 22)
        .rev()
        .find(|&i| {
            end[i..i + 4] == [0x50, 0x4b, 0x05, 0x06]
                && i + 22 + u16at(&end, i + 20).unwrap_or(0) as usize == tail
        })
        .ok_or("ZIP 종료 헤더가 없습니다.")?;
    if u16at(&end, at + 4)? != 0
        || u16at(&end, at + 6)? != 0
        || u16at(&end, at + 8)? != u16at(&end, at + 10)?
    {
        return Err("분할 ZIP 파일은 지원하지 않습니다.".into());
    }
    let mut count = u16at(&end, at + 10)? as u64;
    let mut central_size = u32at(&end, at + 12)? as u64;
    let mut central_offset = u32at(&end, at + 16)? as u64;
    if count == u16::MAX as u64
        || central_size == u32::MAX as u64
        || central_offset == u32::MAX as u64
    {
        let absolute = size - tail as u64 + at as u64;
        if absolute < 20 {
            return Err("ZIP64 위치 정보가 없습니다.".into());
        }
        file.seek(SeekFrom::Start(absolute - 20))
            .map_err(|_| "ZIP64 위치를 찾을 수 없습니다.")?;
        let mut locator = [0u8; 20];
        file.read_exact(&mut locator)
            .map_err(|_| "ZIP64 위치 정보가 잘렸습니다.")?;
        if u32at(&locator, 0)? != 0x07064b50
            || u32at(&locator, 4)? != 0
            || u32at(&locator, 16)? != 1
        {
            return Err("ZIP64 위치 정보가 올바르지 않습니다.".into());
        }
        let offset = u64at(&locator, 8)?;
        if offset.checked_add(56).is_none_or(|n| n > absolute - 20) {
            return Err("ZIP64 헤더 위치가 올바르지 않습니다.".into());
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| "ZIP64 헤더를 찾을 수 없습니다.")?;
        let mut record = [0u8; 56];
        file.read_exact(&mut record)
            .map_err(|_| "ZIP64 헤더가 잘렸습니다.")?;
        if u32at(&record, 0)? != 0x06064b50
            || u64at(&record, 4)? < 44
            || u32at(&record, 16)? != 0
            || u32at(&record, 20)? != 0
            || u64at(&record, 24)? != u64at(&record, 32)?
        {
            return Err("ZIP64 헤더가 올바르지 않습니다.".into());
        }
        count = u64at(&record, 32)?;
        central_size = u64at(&record, 40)?;
        central_offset = u64at(&record, 48)?;
    }
    if central_offset
        .checked_add(central_size)
        .is_none_or(|n| n > size)
        || central_size > 16 * 1024 * 1024
        || count > 100000
    {
        return Err("지원하지 않는 ZIP 파일입니다.".into());
    }
    let central_len = central_size as usize;
    file.seek(SeekFrom::Start(central_offset))
        .map_err(|_| "ZIP 목록을 찾을 수 없습니다.")?;
    let mut central = vec![0; central_len];
    file.read_exact(&mut central)
        .map_err(|_| "ZIP 목록이 잘렸습니다.")?;
    let mut pos = 0;
    let mut names = HashSet::new();
    for _ in 0..count {
        if cancel.load(Ordering::Relaxed) {
            return Err("중단됨".into());
        }
        if u32at(&central, pos)? != 0x02014b50 {
            return Err("ZIP 목록 형식이 올바르지 않습니다.".into());
        }
        let flags = u16at(&central, pos + 8)?;
        let method = u16at(&central, pos + 10)?;
        let crc = u32at(&central, pos + 16)?;
        let mut packed = u32at(&central, pos + 20)? as u64;
        let mut plain = u32at(&central, pos + 24)? as u64;
        let name_len = u16at(&central, pos + 28)? as usize;
        let extra = u16at(&central, pos + 30)? as usize;
        let comment = u16at(&central, pos + 32)? as usize;
        let mut offset = u32at(&central, pos + 42)? as u64;
        let n = central
            .get(pos + 46..pos + 46 + name_len)
            .ok_or("ZIP 파일 이름이 잘렸습니다.")?;
        let name = std::str::from_utf8(n).map_err(|_| "지원하지 않는 ZIP 파일 이름입니다.")?;
        let fields = central
            .get(pos + 46 + name_len..pos + 46 + name_len + extra)
            .ok_or("ZIP 확장 정보가 잘렸습니다.")?;
        let mut field_at = 0;
        let mut zip64 = false;
        while field_at < fields.len() {
            let kind = u16at(fields, field_at)?;
            let length = u16at(fields, field_at + 2)? as usize;
            let data = fields
                .get(field_at + 4..field_at + 4 + length)
                .ok_or("ZIP 확장 정보가 잘렸습니다.")?;
            if kind == 1 {
                if zip64 {
                    return Err("ZIP64 확장 정보가 중복되었습니다.".into());
                }
                zip64 = true;
                let mut p = 0;
                if plain == u32::MAX as u64 {
                    plain = u64at(data, p)?;
                    p += 8;
                }
                if packed == u32::MAX as u64 {
                    packed = u64at(data, p)?;
                    p += 8;
                }
                if offset == u32::MAX as u64 {
                    offset = u64at(data, p)?;
                }
            }
            field_at += 4 + length;
        }
        if (plain == u32::MAX as u64 || packed == u32::MAX as u64 || offset == u32::MAX as u64)
            && !zip64
        {
            return Err("ZIP64 확장 정보가 없습니다.".into());
        }
        pos = pos
            .checked_add(46 + name_len + extra + comment)
            .ok_or("ZIP 목록이 올바르지 않습니다.")?;
        if name.ends_with('/') {
            continue;
        }
        if !safe_name(name) {
            return Err("다운로드 ZIP에 안전하지 않은 경로가 있습니다.".into());
        }
        let base = name.rsplit('/').next().unwrap();
        if base != "zzzzzzzzzz"
            && !(["dat", "zip", "epub", "pdf"]
                .iter()
                .any(|ext| base == format!("{id}.{ext}")))
        {
            continue;
        }
        if !names.insert(base.to_owned())
            || flags & 1 != 0
            || ![0, 8].contains(&method)
            || plain > 16 * 1024 * 1024 * 1024
            || offset
                .checked_add(30)
                .and_then(|n| n.checked_add(packed))
                .is_none_or(|n| n > central_offset)
        {
            return Err("다운로드 ZIP 항목이 올바르지 않습니다.".into());
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| "ZIP 항목을 찾을 수 없습니다.")?;
        let mut header = [0u8; 30];
        file.read_exact(&mut header)
            .map_err(|_| "ZIP 항목이 잘렸습니다.")?;
        if u32at(&header, 0)? != 0x04034b50 || u16at(&header, 8)? != method {
            return Err("ZIP 항목 헤더가 일치하지 않습니다.".into());
        }
        let local_name_len = u16at(&header, 26)? as usize;
        let local_extra = u16at(&header, 28)? as u64;
        let mut local_name = vec![0; local_name_len];
        file.read_exact(&mut local_name)
            .map_err(|_| "ZIP 항목 이름이 잘렸습니다.")?;
        if local_name != n {
            return Err("ZIP 항목 이름이 일치하지 않습니다.".into());
        }
        file.seek(SeekFrom::Current(local_extra as i64))
            .map_err(|_| "ZIP 항목 데이터를 찾을 수 없습니다.")?;
        let input = std::io::Read::by_ref(&mut file).take(packed);
        let mut reader: Box<dyn Read + '_> = if method == 8 {
            Box::new(flate2::read::DeflateDecoder::new(input))
        } else {
            Box::new(input)
        };
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to.join(base))
            .map_err(|_| "원본 파일을 만들 수 없습니다.")?;
        let mut bytes = 0u64;
        let mut hash = crc32fast::Hasher::new();
        let mut buf = [0u8; 65536];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("중단됨".into());
            }
            let n = reader
                .read(&mut buf)
                .map_err(|_| "ZIP 압축을 해제할 수 없습니다.")?;
            if n == 0 {
                break;
            }
            bytes += n as u64;
            if bytes > plain {
                return Err("ZIP 파일 크기가 일치하지 않습니다.".into());
            }
            hash.update(&buf[..n]);
            output
                .write_all(&buf[..n])
                .map_err(|_| "원본 파일을 저장할 수 없습니다.")?;
        }
        output
            .sync_all()
            .map_err(|_| "원본 파일을 저장할 수 없습니다.")?;
        if bytes != plain || hash.finalize() != crc {
            return Err("다운로드 파일 체크섬이 일치하지 않습니다.".into());
        }
    }
    if !complete(to, id) {
        return Err("다운로드에 원본 책 파일이 없습니다.".into());
    }
    let marker = to.join("zzzzzzzzzz");
    if marker.exists() {
        if fs::read(&marker).map_err(|_| "완료 표식을 읽을 수 없습니다.")? != b"END OF ZIP"
        {
            return Err("완료 표식이 올바르지 않습니다.".into());
        }
    } else {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .map_err(|_| "완료 표식을 만들 수 없습니다.")?;
        file.write_all(b"END OF ZIP")
            .and_then(|_| file.sync_all())
            .map_err(|_| "완료 표식을 저장할 수 없습니다.")?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traversal() {
        for n in ["../x", "a/../x", "/x", "C:/x", "a\\x", "a//x", "./x"] {
            assert!(!safe_name(n));
        }
        assert!(safe_name("original/123.dat"));
    }
    fn fixture(corrupt: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, bytes, method) in [
            ("123.dat", &b"key material"[..], 0u16),
            ("123.zip", &b"original encrypted payload"[..], 8u16),
        ] {
            let payload = if method == 8 {
                let mut encoder =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(bytes).unwrap();
                encoder.finish().unwrap()
            } else {
                bytes.to_vec()
            };
            let offset = out.len() as u32;
            let crc = crc32fast::hash(bytes) ^ u32::from(corrupt);
            let packed = payload.len() as u32;
            let plain = bytes.len() as u32;
            out.extend(0x04034b50u32.to_le_bytes());
            for n in [20u16, 0, method, 0, 0] {
                out.extend(n.to_le_bytes());
            }
            for n in [crc, packed, plain] {
                out.extend(n.to_le_bytes());
            }
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(payload);
            central.extend(0x02014b50u32.to_le_bytes());
            for n in [20u16, 20, 0, method, 0, 0] {
                central.extend(n.to_le_bytes());
            }
            for n in [crc, packed, plain] {
                central.extend(n.to_le_bytes());
            }
            for n in [name.len() as u16, 0, 0, 0, 0] {
                central.extend(n.to_le_bytes());
            }
            central.extend(0u32.to_le_bytes());
            central.extend(offset.to_le_bytes());
            central.extend(name.as_bytes());
        }
        let offset = out.len() as u32;
        let size = central.len() as u32;
        out.extend(central);
        out.extend(0x06054b50u32.to_le_bytes());
        for n in [0u16, 0, 2, 2] {
            out.extend(n.to_le_bytes());
        }
        out.extend(size.to_le_bytes());
        out.extend(offset.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }
    #[test]
    fn raw_archive_and_crc() {
        let root = std::env::temp_dir().join(format!(
            "ridibackup-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let cancel = AtomicBool::new(false);
        let good = root.join("good");
        let bad = root.join("bad");
        fs::create_dir(&good).unwrap();
        fs::create_dir(&bad).unwrap();
        let archive = root.join("download.zip");
        fs::write(&archive, fixture(false)).unwrap();
        extract(&archive, &good, "123", &cancel).unwrap();
        assert_eq!(
            fs::read(good.join("123.zip")).unwrap(),
            b"original encrypted payload"
        );
        assert!(complete(&good, "123"));
        fs::write(&archive, fixture(true)).unwrap();
        assert!(extract(&archive, &bad, "123", &cancel).is_err());
        cancel.store(true, Ordering::Relaxed);
        assert!(extract(&archive, &bad, "123", &cancel).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn zip64_footer() {
        let mut zip = fixture(false);
        let old = zip.split_off(zip.len() - 22);
        let offset = u32at(&old, 16).unwrap() as u64;
        let size = u32at(&old, 12).unwrap() as u64;
        let record_at = zip.len() as u64;
        zip.extend(0x06064b50u32.to_le_bytes());
        zip.extend(44u64.to_le_bytes());
        zip.extend(45u16.to_le_bytes());
        zip.extend(45u16.to_le_bytes());
        zip.extend(0u32.to_le_bytes());
        zip.extend(0u32.to_le_bytes());
        zip.extend(2u64.to_le_bytes());
        zip.extend(2u64.to_le_bytes());
        zip.extend(size.to_le_bytes());
        zip.extend(offset.to_le_bytes());
        zip.extend(0x07064b50u32.to_le_bytes());
        zip.extend(0u32.to_le_bytes());
        zip.extend(record_at.to_le_bytes());
        zip.extend(1u32.to_le_bytes());
        let mut footer = old;
        footer[8..12].copy_from_slice(&[255; 4]);
        footer[12..20].copy_from_slice(&[255; 8]);
        zip.extend(footer);
        let root = std::env::temp_dir().join(format!(
            "ridibackup-zip64-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let archive = root.join("input.zip");
        fs::write(&archive, zip).unwrap();
        let target = root.join("book");
        fs::create_dir(&target).unwrap();
        extract(&archive, &target, "123", &AtomicBool::new(false)).unwrap();
        assert!(complete(&target, "123"));
        fs::remove_dir_all(root).unwrap();
    }
}
