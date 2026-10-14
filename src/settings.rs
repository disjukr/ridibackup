use crate::store_aes::Aes32NDec;
use aes::cipher::{BlockDecrypt, KeyInit};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};
use windows_sys::Win32::Security::{Credentials::*, Cryptography::*};
use zeroize::Zeroize;
pub type Result<T> = std::result::Result<T, String>;
pub struct SavedLogin {
    pub refresh: String,
    pub device: String,
    pub library: Option<PathBuf>,
    pub official: bool,
}
pub fn ridi_dir() -> Result<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|p| PathBuf::from(p).join("Ridibooks"))
        .ok_or("리디 설정 경로를 찾을 수 없습니다.".into())
}
pub fn app_dir() -> Result<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|p| PathBuf::from(p).join("ridibackup"))
        .ok_or("설정 경로를 찾을 수 없습니다.".into())
}
pub fn backup_root() -> Result<PathBuf> {
    if let Ok(bytes) = fs::read(app_dir()?.join("preferences.json")) {
        let v: Value =
            serde_json::from_slice(&bytes).map_err(|_| "백업 폴더 설정을 읽을 수 없습니다.")?;
        if let Some(path) = v["backupDirectory"].as_str() {
            return Ok(PathBuf::from(path));
        }
    }
    desktop_dir().map(|p| p.join("ridibackup"))
}
fn desktop_dir() -> Result<PathBuf> {
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_Desktop, SHGetKnownFolderPath},
    };
    unsafe {
        let mut path = null_mut();
        if SHGetKnownFolderPath(&FOLDERID_Desktop, 0, null_mut(), &mut path) < 0 {
            return Err("Windows 바탕화면 폴더를 찾을 수 없습니다.".into());
        }
        let mut length = 0;
        while *path.add(length) != 0 {
            length += 1;
        }
        let result = PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(
            path, length,
        )));
        CoTaskMemFree(path as _);
        Ok(result)
    }
}
pub fn save_backup_root(path: &Path) -> Result<()> {
    let folder = app_dir()?;
    fs::create_dir_all(&folder).map_err(|_| "백업 폴더 설정을 저장할 수 없습니다.")?;
    let file = folder.join("preferences.json");
    let temp = folder.join(format!("preferences-{}.tmp", std::process::id()));
    fs::write(
        &temp,
        serde_json::to_vec(&json!({"backupDirectory":path}))
            .map_err(|_| "백업 폴더 설정이 올바르지 않습니다.")?,
    )
    .map_err(|_| "백업 폴더 설정을 저장할 수 없습니다.")?;
    fs::rename(&temp, &file).map_err(|_| "백업 폴더 설정을 저장할 수 없습니다.".into())
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub fn read_credential(name: &str) -> Result<Vec<u8>> {
    unsafe {
        let mut p = null_mut();
        if CredReadW(wide(name).as_ptr(), CRED_TYPE_GENERIC, 0, &mut p) == 0 {
            return Err("저장된 로그인 정보가 없습니다.".into());
        }
        let c = &*p;
        let data =
            std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec();
        CredFree(p as _);
        Ok(data)
    }
}
pub fn sha1(bytes: &[u8]) -> Result<String> {
    unsafe {
        let mut provider = 0;
        let mut hash = 0;
        if CryptAcquireContextW(
            &mut provider,
            null(),
            null(),
            PROV_RSA_AES,
            CRYPT_VERIFYCONTEXT,
        ) == 0
        {
            return Err("설정 체크섬을 확인할 수 없습니다.".into());
        }
        let result = (|| {
            if CryptCreateHash(provider, CALG_SHA1, 0, 0, &mut hash) == 0
                || CryptHashData(hash, bytes.as_ptr(), bytes.len() as u32, 0) == 0
            {
                return Err("설정 체크섬을 확인할 수 없습니다.".into());
            }
            let mut out = [0u8; 20];
            let mut length = 20;
            if CryptGetHashParam(hash, HP_HASHVAL, out.as_mut_ptr(), &mut length, 0) == 0 {
                return Err("설정 체크섬을 확인할 수 없습니다.".into());
            }
            Ok(out.iter().map(|x| format!("{x:02x}")).collect())
        })();
        if hash != 0 {
            CryptDestroyHash(hash);
        }
        CryptReleaseContext(provider, 0);
        result
    }
}
pub fn decode_store(bytes: &[u8], name: &str, user: Option<&str>) -> Result<Value> {
    if bytes.len() < 272 || &bytes[..4] != b"data" {
        return Err("리디 설정 파일 형식이 올바르지 않습니다.".into());
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
    if ![3, 4].contains(&version) {
        return Err("지원하지 않는 리디 설정 버전입니다.".into());
    }
    let n = u16::from_be_bytes(bytes[72..74].try_into().unwrap()) as usize;
    let scope = u16::from_be_bytes(bytes[106..108].try_into().unwrap()) as usize;
    let expected = if user.is_some() { "user" } else { "global" };
    if n > 64
        || scope > 32
        || &bytes[8..8 + n] != name.as_bytes()
        || &bytes[74..74 + scope] != expected.as_bytes()
        || bytes[8 + n..72].iter().any(|&x| x != 0)
        || bytes[74 + scope..106].iter().any(|&x| x != 0)
        || bytes[148..256].iter().any(|&x| x != 0)
    {
        return Err("리디 설정 헤더가 올바르지 않습니다.".into());
    }
    let encrypted = &bytes[256..];
    if !encrypted.len().is_multiple_of(16) || sha1(encrypted)?.as_bytes() != &bytes[108..148] {
        return Err("리디 설정 체크섬이 일치하지 않습니다.".into());
    }
    let mut key = if let Some(user) = user {
        sha1(format!("{name}-{user}").as_bytes())?.as_bytes()[2..18].to_vec()
    } else if version == 3 {
        b"dkdlfeksdkanzlsksjgdjdiwl\x07\x07\x07\x07\x07\x07\x07".to_vec()
    } else {
        let mut credential = read_credential("com.ridi.books/global")?;
        let parsed =
            std::str::from_utf8(&credential).map_err(|_| "리디 설정 키가 올바르지 않습니다.")?;
        let mut k = STANDARD
            .decode(parsed)
            .map_err(|_| "리디 설정 키가 올바르지 않습니다.")?;
        credential.zeroize();
        let pad = (16 - k.len() % 16) % 16;
        k.extend(std::iter::repeat_n(pad as u8, pad));
        k
    };
    let cipher =
        Aes32NDec::new_from_slice(&key).map_err(|_| "리디 설정 키 길이가 올바르지 않습니다.")?;
    key.zeroize();
    let mut plain = encrypted.to_vec();
    for block in plain.chunks_exact_mut(16) {
        cipher.decrypt_block(block.into());
    }
    let padding = *plain.last().unwrap() as usize;
    let result = if padding == 0
        || padding > 16
        || plain[plain.len() - padding..]
            .iter()
            .any(|&x| x as usize != padding)
    {
        Err("리디 설정을 해석할 수 없습니다.".into())
    } else {
        serde_json::from_slice(&plain[..plain.len() - padding])
            .map_err(|_| "리디 설정 JSON이 올바르지 않습니다.".into())
    };
    plain.zeroize();
    result
}
pub fn official_library() -> Result<Option<PathBuf>> {
    let root = ridi_dir()?;
    let user = root.join("datastores/user");
    let mut folders = Vec::new();
    if let Ok(entries) = fs::read_dir(user) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir()
                && name.starts_with('_')
                && name[1..].chars().all(|c| c.is_ascii_digit())
            {
                folders.push(entry.path());
            }
        }
    }
    if folders.len() != 1 {
        return Ok(None);
    }
    let folder = &folders[0];
    let name = folder.file_name().unwrap().to_string_lossy();
    let store = folder.join("UserSettings");
    if let Ok(bytes) = fs::read(store) {
        let s = decode_store(&bytes, "UserSettings", Some(name.trim_start_matches('_')))?;
        if let Some(p) = s["data"]["downloadBookDirectory"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            return Ok(Some(PathBuf::from(p.replace("\\\\", "\\"))));
        }
    }
    Ok(Some(root.join("library").join(name.as_ref())))
}
pub fn saved_login() -> Result<SavedLogin> {
    let root = ridi_dir()?;
    let bytes = fs::read(root.join("datastores/global/Settings"))
        .map_err(|_| "저장된 리디 로그인이 없습니다.")?;
    let s = decode_store(&bytes, "Settings", None)?;
    if s["data"]["autoLogin"]["enable"] == false {
        return Err("공식 리디 앱에서 로그인 정보를 저장한 뒤 다시 확인해주세요.".into());
    }
    let refresh = s["data"]["autoLogin"]["refreshToken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("저장된 리디 로그인이 없습니다.")?
        .to_owned();
    let device = s["data"]["device"]["deviceId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("리디 기기 정보가 없습니다.")?
        .to_owned();
    Ok(SavedLogin {
        refresh,
        device,
        library: official_library()?,
        official: true,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checksums() {
        assert_eq!(
            sha1(b"abc").unwrap(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }
    #[test]
    fn rejects_short_store() {
        assert!(decode_store(b"data", "Settings", None).is_err());
    }
    #[test]
    fn user_store_roundtrip_and_tamper() {
        use aes::cipher::BlockEncrypt;
        let name = "UserSettings";
        let key = sha1(b"UserSettings-123").unwrap();
        let cipher = aes::Aes128::new_from_slice(&key.as_bytes()[2..18]).unwrap();
        let mut plain = br#"{"data":{"downloadBookDirectory":"example"}}"#.to_vec();
        let padding = 16 - plain.len() % 16;
        plain.extend(std::iter::repeat_n(padding as u8, padding));
        for block in plain.chunks_exact_mut(16) {
            cipher.encrypt_block(block.into());
        }
        let mut store = vec![0u8; 256];
        store[..4].copy_from_slice(b"data");
        store[4..8].copy_from_slice(&4u32.to_be_bytes());
        store[8..8 + name.len()].copy_from_slice(name.as_bytes());
        store[72..74].copy_from_slice(&(name.len() as u16).to_be_bytes());
        store[74..78].copy_from_slice(b"user");
        store[106..108].copy_from_slice(&4u16.to_be_bytes());
        store[108..148].copy_from_slice(sha1(&plain).unwrap().as_bytes());
        store.extend(plain);
        assert_eq!(
            decode_store(&store, name, Some("123")).unwrap()["data"]["downloadBookDirectory"],
            "example"
        );
        store[256] ^= 1;
        assert!(decode_store(&store, name, Some("123")).is_err());
    }
}
