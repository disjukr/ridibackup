use crate::settings::Result;
use std::{
    ffi::c_void,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use url::Url;
use windows_sys::Win32::{
    Foundation::*, Networking::WinHttp::*, System::Time::SystemTimeToFileTime,
};
struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            WinHttpCloseHandle(self.0);
        }
    }
}
fn handle(p: *mut c_void) -> Result<Handle> {
    if p.is_null() {
        Err("네트워크 연결을 만들 수 없습니다.".into())
    } else {
        Ok(Handle(p))
    }
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub struct Response {
    request: Handle,
    _connection: Handle,
    _session: Handle,
    pub status: u32,
}
impl Response {
    pub fn header(&self, key: u32) -> Option<String> {
        unsafe {
            let mut n = 0;
            WinHttpQueryHeaders(self.request.0, key, null(), null_mut(), &mut n, null_mut());
            if n == 0 || n > 32768 {
                return None;
            }
            let mut b = vec![0u16; n as usize / 2 + 1];
            if WinHttpQueryHeaders(
                self.request.0,
                key,
                null(),
                b.as_mut_ptr() as _,
                &mut n,
                null_mut(),
            ) == 0
            {
                return None;
            }
            let len = b.iter().position(|&x| x == 0).unwrap_or(b.len());
            Some(String::from_utf16_lossy(&b[..len]))
        }
    }
    pub fn read(&self, buf: &mut [u8], cancel: &AtomicBool) -> Result<usize> {
        if cancel.load(Ordering::Relaxed) {
            return Err("중단됨".into());
        }
        let mut n = 0;
        let ok = unsafe {
            WinHttpReadData(
                self.request.0,
                buf.as_mut_ptr() as _,
                buf.len() as u32,
                &mut n,
            )
        };
        if cancel.load(Ordering::Relaxed) {
            return Err("중단됨".into());
        }
        if ok == 0 {
            return Err("다운로드 연결이 끊어졌습니다.".into());
        }
        Ok(n as usize)
    }
    pub fn json(&self, cancel: &AtomicBool) -> Result<serde_json::Value> {
        let mut out = Vec::new();
        let mut buf = [0u8; 32768];
        loop {
            let n = self.read(&mut buf, cancel)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > 32 * 1024 * 1024 {
                return Err("서버 응답 크기가 너무 큽니다.".into());
            }
        }
        serde_json::from_slice(&out).map_err(|_| "서버 응답 형식이 올바르지 않습니다.".into())
    }
    pub fn retry_after(&self) -> Duration {
        let Some(value) = self.header(WINHTTP_QUERY_RETRY_AFTER) else {
            return Duration::from_secs(60);
        };
        if let Ok(n) = value.parse::<u64>() {
            return Duration::from_secs(n.max(60));
        }
        unsafe {
            let mut st: SYSTEMTIME = zeroed();
            let mut ft: FILETIME = zeroed();
            if WinHttpTimeToSystemTime(wide(&value).as_ptr(), &mut st) != 0
                && SystemTimeToFileTime(&st, &mut ft) != 0
            {
                let seconds =
                    ((ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64) / 10_000_000;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    + 11_644_473_600;
                return Duration::from_secs(seconds.saturating_sub(now).max(60));
            }
        }
        Duration::from_secs(60)
    }
}
pub fn trusted(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "account.ridibooks.com"
                    | "library-api.ridibooks.com"
                    | "book-api.ridibooks.com"
                    | "download.ridibooks.com"
            )
        )
}
pub fn request(
    url: &str,
    method: &str,
    headers: &str,
    body: &[u8],
    authenticated: bool,
    cancel: &AtomicBool,
) -> Result<Response> {
    let url = Url::parse(url).map_err(|_| "서버 주소가 올바르지 않습니다.")?;
    if url.scheme() != "https"
        || url.username() != ""
        || url.password().is_some()
        || url.host_str().is_none()
        || authenticated && !trusted(&url)
    {
        return Err("허용되지 않는 서버 주소입니다.".into());
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("중단됨".into());
    }
    unsafe {
        let session = handle(WinHttpOpen(
            wide("Ridibooks/0.11.7").as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        ))?;
        WinHttpSetTimeouts(session.0, 10000, 10000, 10000, 5000);
        let connection = handle(WinHttpConnect(
            session.0,
            wide(url.host_str().unwrap()).as_ptr(),
            url.port_or_known_default().unwrap_or(443),
            0,
        ))?;
        let path = format!(
            "{}{}",
            url.path(),
            url.query().map(|q| format!("?{q}")).unwrap_or_default()
        );
        let request = handle(WinHttpOpenRequest(
            connection.0,
            wide(method).as_ptr(),
            wide(&path).as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        let disable = WINHTTP_DISABLE_REDIRECTS | WINHTTP_DISABLE_COOKIES;
        if WinHttpSetOption(
            request.0,
            WINHTTP_OPTION_DISABLE_FEATURE,
            &disable as *const _ as _,
            size_of::<u32>() as u32,
        ) == 0
        {
            return Err("네트워크 설정을 적용할 수 없습니다.".into());
        }
        let h = wide(headers);
        if WinHttpSendRequest(
            request.0,
            h.as_ptr(),
            (h.len() - 1) as u32,
            if body.is_empty() {
                null()
            } else {
                body.as_ptr() as _
            },
            body.len() as u32,
            body.len() as u32,
            0,
        ) == 0
            || WinHttpReceiveResponse(request.0, null_mut()) == 0
        {
            if cancel.load(Ordering::Relaxed) {
                return Err("중단됨".into());
            }
            return Err("서버에 연결할 수 없습니다.".into());
        }
        let mut status = 0u32;
        let mut size = size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            &mut status as *mut _ as _,
            &mut size,
            null_mut(),
        ) == 0
        {
            return Err("서버 상태를 확인할 수 없습니다.".into());
        }
        Ok(Response {
            request,
            _connection: connection,
            _session: session,
            status,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_hosts() {
        assert!(trusted(
            &Url::parse("https://download.ridibooks.com/books/1").unwrap()
        ));
        for s in [
            "https://evil.example/",
            "http://account.ridibooks.com/",
            "https://user@account.ridibooks.com/",
            "https://account.ridibooks.com:444/",
        ] {
            assert!(!trusted(&Url::parse(s).unwrap()));
        }
    }
}
