use crate::settings::Result;
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

fn files(root: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries =
        fs::read_dir(root.join(relative)).map_err(|_| "datastores 폴더를 읽을 수 없습니다.")?;
    for entry in entries {
        let entry = entry.map_err(|_| "datastores 목록을 읽을 수 없습니다.")?;
        let path = relative.join(entry.file_name());
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| "datastores 파일을 확인할 수 없습니다.")?;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("datastores의 링크 경로는 백업할 수 없습니다.".into());
        }
        if metadata.is_dir() {
            files(root, &path, out)?;
        } else if metadata.is_file() {
            out.push(path);
        } else {
            return Err("지원하지 않는 datastores 항목입니다.".into());
        }
    }
    Ok(())
}
fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err("중단됨".into())
    } else {
        Ok(())
    }
}
pub fn initialize(source: &Path, root: &Path, cancel: &AtomicBool) -> Result<bool> {
    cancelled(cancel)?;
    let destination = root.join("datastores");
    if destination.exists() {
        let metadata = fs::symlink_metadata(&destination)
            .map_err(|_| "datastores 백업 경로를 확인할 수 없습니다.")?;
        return if metadata.is_dir() && metadata.file_attributes() & 0x400 == 0 {
            Ok(false)
        } else {
            Err("datastores 백업 경로가 일반 폴더가 아닙니다.".into())
        };
    }
    fs::create_dir_all(root).map_err(|_| "백업 폴더를 만들 수 없습니다.")?;
    backup(source, root, cancel)
}
fn remove_temporary(directory: &Path, root: &Path) -> Result<()> {
    let directory =
        fs::canonicalize(directory).map_err(|_| "임시 백업 경로를 확인할 수 없습니다.")?;
    let root = fs::canonicalize(root).map_err(|_| "백업 경로를 확인할 수 없습니다.")?;
    if directory.parent() != Some(root.as_path()) {
        return Err("임시 백업 경로가 올바르지 않습니다.".into());
    }
    fs::remove_dir_all(directory).map_err(|_| "임시 백업을 정리할 수 없습니다.".into())
}
fn backup(source: &Path, root: &Path, cancel: &AtomicBool) -> Result<bool> {
    cancelled(cancel)?;
    match fs::symlink_metadata(source) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Ok(m) if m.is_dir() && m.file_attributes() & 0x400 == 0 => {}
        _ => return Err("원본 datastores 경로를 확인할 수 없습니다.".into()),
    }
    let mut paths = Vec::new();
    files(source, Path::new(""), &mut paths)?;
    paths.sort();
    let mut opened: Vec<(PathBuf, File)> = Vec::new();
    for path in &paths {
        cancelled(cancel)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(source.join(path))
            .map_err(|_| {
                "datastores 파일을 읽을 수 없습니다. 공식 리디 앱을 종료한 뒤 재개해주세요."
            })?;
        opened.push((path.clone(), file));
    }
    fs::create_dir_all(root).map_err(|_| "백업 폴더를 만들 수 없습니다.")?;
    let stamp = format!(
        "{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id()
    );
    let stage = root.join(format!(".datastores-{stamp}.tmp"));
    fs::create_dir(&stage).map_err(|_| "datastores 임시 백업을 만들 수 없습니다.")?;
    let result = (|| {
        for (path, input) in &mut opened {
            cancelled(cancel)?;
            let destination = stage.join(path);
            fs::create_dir_all(destination.parent().unwrap())
                .map_err(|_| "datastores 백업 폴더를 만들 수 없습니다.")?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)
                .map_err(|_| "datastores 백업 파일을 만들 수 없습니다.")?;
            let mut buffer = [0u8; 65536];
            loop {
                cancelled(cancel)?;
                let n = input
                    .read(&mut buffer)
                    .map_err(|_| "datastores 파일을 읽을 수 없습니다.")?;
                if n == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..n])
                    .map_err(|_| "datastores 파일을 백업할 수 없습니다.")?;
            }
            output
                .sync_all()
                .map_err(|_| "datastores 백업을 저장할 수 없습니다.")?;
        }
        let mut now = Vec::new();
        files(source, Path::new(""), &mut now)?;
        now.sort();
        if now != paths {
            return Err("백업 중 datastores 목록이 바뀌었습니다. 다시 재개해주세요.".into());
        }
        cancelled(cancel)?;
        let current = root.join("datastores");
        let previous = root.join(format!(".datastores-old-{stamp}.tmp"));
        if current.exists() {
            let metadata = fs::symlink_metadata(&current)
                .map_err(|_| "기존 datastores 백업을 확인할 수 없습니다.")?;
            if !metadata.is_dir() || metadata.file_attributes() & 0x400 != 0 {
                return Err("기존 datastores 백업 경로가 올바르지 않습니다.".into());
            }
            fs::rename(&current, &previous)
                .map_err(|_| "기존 datastores 백업을 교체할 수 없습니다.")?;
        }
        if fs::rename(&stage, &current).is_err() {
            if previous.exists() {
                fs::rename(&previous, &current).map_err(|_| {
                    "이전 datastores 백업 복원이 필요합니다. .datastores-old-*.tmp 폴더를 확인해주세요."
                })?;
            }
            return Err("datastores 백업을 완료할 수 없습니다.".into());
        }
        if previous.exists() {
            remove_temporary(&previous, root)?;
        }
        Ok(true)
    })();
    if stage.exists() {
        let _ = remove_temporary(&stage, root);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_bytes_overwrite_and_cancel() {
        let root =
            std::env::temp_dir().join(format!("ridibackup-datastores-test-{}", std::process::id()));
        let source = root.join("source");
        let target = root.join("backup");
        fs::create_dir_all(source.join("user/_123")).unwrap();
        fs::write(
            source.join("user/_123/DownloadBookAll"),
            b"original encrypted data",
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        assert!(backup(&source, &target, &cancel).unwrap());
        assert_eq!(
            fs::read(target.join("datastores/user/_123/DownloadBookAll")).unwrap(),
            b"original encrypted data"
        );
        fs::write(
            source.join("user/_123/DownloadBookAll"),
            b"new encrypted data",
        )
        .unwrap();
        cancel.store(true, Ordering::Relaxed);
        assert!(backup(&source, &target, &cancel).is_err());
        assert_eq!(
            fs::read(target.join("datastores/user/_123/DownloadBookAll")).unwrap(),
            b"original encrypted data"
        );
        cancel.store(false, Ordering::Relaxed);
        assert!(backup(&source, &target, &cancel).unwrap());
        assert_eq!(
            fs::read(target.join("datastores/user/_123/DownloadBookAll")).unwrap(),
            b"new encrypted data"
        );
        assert!(!target.join("datastores-history").exists());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
        let directory = root.join("first-backup");
        let existing = root.join("existing-empty");
        fs::create_dir(&existing).unwrap();
        assert!(initialize(&source, &existing, &cancel).unwrap());
        assert!(existing.join("datastores").is_dir());
        cancel.store(true, Ordering::Relaxed);
        assert!(initialize(&source, &directory, &cancel).is_err());
        assert!(!directory.exists());
        cancel.store(false, Ordering::Relaxed);
        assert!(initialize(&source, &directory, &cancel).unwrap());
        fs::write(source.join("user/_123/DownloadBookAll"), b"changed again").unwrap();
        assert!(!initialize(&source, &directory, &cancel).unwrap());
        assert_eq!(
            fs::read(directory.join("datastores/user/_123/DownloadBookAll")).unwrap(),
            b"new encrypted data"
        );
        assert_eq!(
            fs::read(source.join("user/_123/DownloadBookAll")).unwrap(),
            b"changed again"
        );
        let without_client = root.join("without-client/_456");
        assert!(!initialize(&root.join("missing-source"), &without_client, &cancel).unwrap());
        assert!(without_client.is_dir());
        fs::remove_dir_all(root).unwrap();
    }
}
