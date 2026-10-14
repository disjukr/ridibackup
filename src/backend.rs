use crate::{
    http,
    model::{Book, Format},
    package,
    settings::{self, Result, SavedLogin},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::Networking::WinHttp::{
    WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_LOCATION,
};
use zeroize::Zeroizing;
pub enum Command {
    Reconnect,
    SetDirectory(PathBuf),
    Refresh,
    Download(Vec<(usize, usize, String)>),
}
pub enum Event {
    NeedLogin(String),
    Loading(String),
    Directory(PathBuf),
    Partial(Vec<Book>),
    Ready(Vec<Book>),
    Current(usize),
    Progress {
        position: usize,
        bytes: u64,
        total: Option<u64>,
    },
    Status(String),
    Done {
        position: usize,
        book: usize,
    },
    Stopped,
    Complete,
    Error(String),
}
pub struct Backend {
    pub commands: mpsc::Sender<Command>,
    pub events: mpsc::Receiver<Event>,
    pub cancel: Arc<AtomicBool>,
    pub gap: Arc<AtomicU32>,
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
pub struct Session {
    backup: PathBuf,
    account: String,
    catalog: Option<Value>,
    access: Zeroizing<String>,
    refresh: Zeroizing<String>,
    expires: Instant,
    last_request: Option<Instant>,
    retry_at: Option<Instant>,
    pub library: PathBuf,
    device: String,
}
const LOGIN_REQUIRED: &str = "공식 리디 앱에서 다시 로그인한 뒤 다시 확인을 눌러주세요.";
fn rate_limit_delay(server: Duration, attempt: u32) -> Duration {
    server.max(Duration::from_secs(60 << attempt.min(3)))
}
fn failure(tx: &mpsc::Sender<Event>, error: String) {
    if error == LOGIN_REQUIRED {
        send(tx, Event::NeedLogin(error));
    } else {
        send(tx, Event::Error(error));
    }
}
fn send(tx: &mpsc::Sender<Event>, e: Event) {
    let _ = tx.send(e);
}
fn id(v: &Value) -> Result<String> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err("서버 책 ID가 올바르지 않습니다.".into()),
    };
    if s.is_empty() || s == "0" || !s.chars().all(|c| c.is_ascii_digit()) {
        return Err("서버 책 ID가 올바르지 않습니다.".into());
    }
    Ok(s)
}
fn normalized_path(path: &std::path::Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err("백업 폴더는 절대 경로여야 합니다.".into());
    }
    let mut suffix = Vec::new();
    let mut existing = path;
    while !existing.exists() {
        suffix.push(
            existing
                .file_name()
                .ok_or("백업 경로가 올바르지 않습니다.")?
                .to_owned(),
        );
        existing = existing.parent().ok_or("백업 경로가 올바르지 않습니다.")?;
    }
    let mut resolved = fs::canonicalize(existing).map_err(|_| "백업 경로를 확인할 수 없습니다.")?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(PathBuf::from(resolved.to_string_lossy().to_lowercase()))
}
fn validate_backup(target: &std::path::Path, official: &std::path::Path) -> Result<()> {
    let target = normalized_path(target)?;
    for protected in [settings::ridi_dir()?, official.to_owned()] {
        let protected = normalized_path(&protected)?;
        if target.starts_with(&protected) || protected.starts_with(&target) {
            return Err("공식 리디 폴더와 다른 백업 폴더를 선택해주세요.".into());
        }
    }
    Ok(())
}
fn collect_owned(v: &Value, seen: &mut HashSet<String>, out: &mut Vec<String>) -> Result<()> {
    if v["is_ridiselect"] == true || v["service_type"] == "rent" {
        return Ok(());
    }
    if v["service_type"]
        .as_str()
        .is_some_and(|t| !["normal", "buy"].contains(&t))
    {
        return Err("알 수 없는 구매 유형이 있습니다.".into());
    }
    let book = id(&v["b_id"])?;
    if seen.insert(book.clone()) {
        out.push(book);
    }
    Ok(())
}
fn metadata_batch(ids: &[String], items: &[Value]) -> Result<Vec<Value>> {
    let mut metadata = std::collections::HashMap::new();
    for info in items {
        let book = id(&info["id"])?;
        let title = info["title"]["main"]
            .as_str()
            .ok_or("책 제목이 없습니다.")?;
        let format = info["file"]["format"]
            .as_str()
            .or_else(|| info["format"].as_str())
            .ok_or("책 파일 형식이 없습니다.")?;
        if !ids.contains(&book)
            || metadata
                .insert(
                    book.clone(),
                    json!({"id":book,"title":title,"format":format}),
                )
                .is_some()
        {
            return Err("책 정보 응답에 예상하지 못한 항목이 있습니다.".into());
        }
    }
    ids.iter()
        .map(|i| {
            metadata
                .remove(i)
                .ok_or("일부 책의 정보가 누락되었습니다.".into())
        })
        .collect()
}
fn wait(duration: Duration, cancel: &AtomicBool) -> Result<()> {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        if cancel.load(Ordering::Relaxed) {
            return Err("중단됨".into());
        }
        std::thread::sleep(
            Duration::from_millis(100).min(end.saturating_duration_since(Instant::now())),
        );
    }
    Ok(())
}
impl Session {
    fn token(body: &[u8], content: &str, cancel: &AtomicBool) -> Result<Value> {
        let response = http::request(
            "https://account.ridibooks.com/oauth2/token",
            "POST",
            &format!("Content-Type: {content}\r\nAccept: application/json\r\n"),
            body,
            true,
            cancel,
        )?;
        if [400, 401, 403].contains(&response.status) {
            return Err(LOGIN_REQUIRED.into());
        }
        if response.status != 200 {
            return Err(format!("로그인에 실패했습니다. (HTTP {})", response.status));
        }
        response.json(cancel)
    }
    fn from_token(token: Value, device: String, library: Option<PathBuf>) -> Result<Self> {
        let access = token["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("로그인 응답에 토큰이 없습니다.")?
            .to_owned();
        let refresh = token["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("로그인 응답에 갱신 정보가 없습니다.")?
            .to_owned();
        let seconds = token["expires_in"]
            .as_u64()
            .filter(|&s| s > 0 && s < 31_536_000)
            .ok_or("로그인 만료 시간이 올바르지 않습니다.")?;
        let numeric = access
            .split('.')
            .nth(1)
            .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| {
                [
                    "u_idx",
                    "sub",
                    "ridi.com/user_id",
                    "ridi.com/u_id",
                    "user_id",
                    "userId",
                ]
                .iter()
                .find_map(|name| id(&v[*name]).ok())
            });
        let account = numeric
            .clone()
            .or_else(|| {
                library
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(|n| n.trim_start_matches('_').to_owned())
            })
            .ok_or("계정 ID를 확인할 수 없습니다.")?;
        id(&Value::String(account.clone()))?;
        let library = if let Some(user) = numeric {
            let root = settings::ridi_dir()?;
            let userfolder = root.join("datastores/user").join(format!("_{user}"));
            if let Ok(bytes) = fs::read(userfolder.join("UserSettings")) {
                let s = settings::decode_store(&bytes, "UserSettings", Some(&user))?;
                s["data"]["downloadBookDirectory"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| PathBuf::from(s.replace("\\\\", "\\")))
                    .unwrap_or(root.join("library").join(format!("_{user}")))
            } else {
                root.join("library").join(format!("_{user}"))
            }
        } else {
            library.ok_or("계정의 리디 저장 폴더를 확인할 수 없습니다. 리디 클라이언트에서 먼저 로그인해주세요.")?
        };
        let backup = settings::backup_root()?.join(format!("_{account}"));
        validate_backup(&backup, &library)?;
        Ok(Self {
            backup,
            account,
            catalog: None,
            access: Zeroizing::new(access),
            refresh: Zeroizing::new(refresh),
            expires: Instant::now() + Duration::from_secs(seconds),
            last_request: None,
            retry_at: None,
            library,
            device,
        })
    }
    fn saved(s: SavedLogin, cancel: &AtomicBool) -> Result<Self> {
        let body = Zeroizing::new(
            serde_json::to_vec(&json!({"grant_type":"refresh_token","refresh_token":s.refresh}))
                .map_err(|_| "로그인 요청을 만들 수 없습니다.")?,
        );
        Self::from_token(
            Self::token(&body, "application/json", cancel)?,
            s.device,
            s.library,
        )
    }
    fn refresh(&mut self, cancel: &AtomicBool) -> Result<()> {
        let body = Zeroizing::new(
            serde_json::to_vec(
                &json!({"grant_type":"refresh_token","refresh_token":self.refresh.as_str()}),
            )
            .map_err(|_| "로그인 요청을 만들 수 없습니다.")?,
        );
        let token = Self::token(&body, "application/json", cancel)?;
        self.access = Zeroizing::new(
            token["access_token"]
                .as_str()
                .ok_or("로그인 갱신 응답이 올바르지 않습니다.")?
                .to_owned(),
        );
        self.refresh = Zeroizing::new(
            token["refresh_token"]
                .as_str()
                .ok_or("로그인 갱신 응답이 올바르지 않습니다.")?
                .to_owned(),
        );
        self.expires = Instant::now()
            + Duration::from_secs(
                token["expires_in"]
                    .as_u64()
                    .ok_or("로그인 만료 응답이 올바르지 않습니다.")?,
            );
        Ok(())
    }
    fn request(
        &mut self,
        url: &str,
        download: bool,
        cancel: &AtomicBool,
        tx: &mpsc::Sender<Event>,
    ) -> Result<http::Response> {
        let mut refreshed = false;
        for attempt in 0..4 {
            if let Some(end) = self.retry_at {
                while Instant::now() < end {
                    let message = format!(
                        "요청 제한 · {}초 뒤 재시도",
                        end.saturating_duration_since(Instant::now()).as_secs() + 1
                    );
                    send(
                        tx,
                        if download {
                            Event::Status(message)
                        } else {
                            Event::Loading(message)
                        },
                    );
                    wait(Duration::from_millis(500), cancel)?;
                }
                self.retry_at = None;
            }
            if self.expires.saturating_duration_since(Instant::now()) < Duration::from_secs(60) {
                self.refresh(cancel)?;
            }

            if !download {
                if let Some(last) = self.last_request {
                    wait(
                        Duration::from_secs(1).saturating_sub(last.elapsed()),
                        cancel,
                    )?;
                }
            }
            self.last_request = Some(Instant::now());
            let cookie = Zeroizing::new(if download {
                format!(
                    "Cookie: ridi-at={}; ridi-rt={}\r\n",
                    self.access.as_str(),
                    self.refresh.as_str()
                )
            } else {
                format!(
                    "Accept: application/json\r\nCookie: ridi-at={}\r\n",
                    self.access.as_str()
                )
            });
            let response = http::request(url, "GET", &cookie, &[], true, cancel)?;
            match response.status {
                200..=299 => return Ok(response),
                300..=399 if download => return Ok(response),
                401 if !refreshed => {
                    drop(response);
                    refreshed = true;
                    self.refresh(cancel)?;
                }
                401 => return Err(LOGIN_REQUIRED.into()),
                429 => {
                    let time = rate_limit_delay(response.retry_after(), attempt);
                    drop(response);
                    self.retry_at = Some(
                        Instant::now()
                            .checked_add(time)
                            .ok_or("서버가 너무 긴 요청 제한 시간을 반환했습니다.")?,
                    );
                    if attempt == 3 {
                        return Err(
                            "요청 제한이 계속되어 조회를 멈췄습니다. 잠시 후 다시 시도해주세요."
                                .into(),
                        );
                    }
                }
                500..=599 if attempt < 3 => {
                    drop(response);
                    wait(Duration::from_secs(1 << attempt), cancel)?;
                }
                status => return Err(format!("서버 요청에 실패했습니다. (HTTP {status})")),
            }
        }
        Err("서버 요청을 완료할 수 없습니다.".into())
    }
    fn json(&mut self, url: &str, cancel: &AtomicBool, tx: &mpsc::Sender<Event>) -> Result<Value> {
        self.request(url, false, cancel, tx)?.json(cancel)
    }
    fn pages(
        &mut self,
        base: &str,
        cancel: &AtomicBool,
        tx: &mpsc::Sender<Event>,
    ) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut signatures = HashSet::new();
        loop {
            let mut url = url::Url::parse(base).map_err(|_| "목록 주소가 올바르지 않습니다.")?;
            url.query_pairs_mut()
                .append_pair("offset", &out.len().to_string())
                .append_pair("limit", "72");
            let v = self.json(url.as_str(), cancel, tx)?;
            let items = v["items"]
                .as_array()
                .ok_or("책 목록 응답이 올바르지 않습니다.")?;
            if items.is_empty() {
                break;
            }
            let signature = items
                .iter()
                .map(|v| format!("{}:{}", v["unit_id"], v["b_id"]))
                .collect::<Vec<_>>()
                .join(",");
            if !signatures.insert(signature) {
                return Err("서버가 같은 목록을 반복 반환했습니다.".into());
            }
            out.extend(items.iter().cloned());
            if items.len() < 72 {
                break;
            }
        }
        Ok(out)
    }
    fn cache_path(&self) -> Result<PathBuf> {
        let key = settings::sha1(self.library.to_string_lossy().as_bytes())?;
        Ok(settings::app_dir()?.join(format!("inventory-{key}.json")))
    }
    fn inventory(
        &mut self,
        force: bool,
        cancel: &AtomicBool,
        tx: &mpsc::Sender<Event>,
    ) -> Result<Vec<Book>> {
        let path = self.cache_path()?;
        if !force {
            if let Ok(bytes) = fs::read(&path) {
                if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                    let age = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                        .saturating_sub(v["updated"].as_u64().unwrap_or(0));
                    if age < 21600 {
                        if let Ok(books) = self.books_from_cache(&v) {
                            self.catalog = Some(v);
                            return Ok(books);
                        }
                    }
                }
            }
        }
        send(tx, Event::Loading("구입한 책 목록을 불러오는 중…".into()));
        let units = self.pages(
            "https://library-api.ridibooks.com/items/main/?order_type=purchase_date&order_by=desc",
            cancel,
            tx,
        )?;
        let mut pending = Vec::new();
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for (n, unit) in units.iter().enumerate() {
            if unit["unit_type"] == "series" || unit["unit_count"].as_u64().unwrap_or(1) > 1 {
                if !pending.is_empty() {
                    self.inventory_batch(&pending, &mut entries, cancel, tx)?;
                    pending.clear();
                }
                let uid = id(&unit["unit_id"])?;
                let mut offset = 0;
                let mut signatures = HashSet::new();
                loop {
                    let v = self.json(&format!("https://library-api.ridibooks.com/items/main/{uid}?order_type=unit_order&order_by=asc&offset={offset}&limit=72"), cancel, tx)?;
                    let items = v["items"]
                        .as_array()
                        .ok_or("책 목록 응답이 올바르지 않습니다.")?;
                    let signature = items
                        .iter()
                        .map(|v| v["b_id"].to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    if !items.is_empty() && !signatures.insert(signature) {
                        return Err("서버가 같은 목록을 반복 반환했습니다.".into());
                    }
                    for item in items {
                        collect_owned(item, &mut seen, &mut pending)?;
                    }
                    if !pending.is_empty() {
                        self.inventory_batch(&pending, &mut entries, cancel, tx)?;
                        pending.clear();
                    }
                    if items.len() < 72 {
                        break;
                    }
                    offset += items.len();
                }
            } else {
                collect_owned(unit, &mut seen, &mut pending)?;
                if pending.len() >= 72 {
                    self.inventory_batch(&pending, &mut entries, cancel, tx)?;
                    pending.clear();
                }
            }
            send(
                tx,
                Event::Loading(format!(
                    "구입한 책 {}권 · 조회 중 {}/{}묶음",
                    entries.len(),
                    n + 1,
                    units.len()
                )),
            );
        }
        if !pending.is_empty() {
            self.inventory_batch(&pending, &mut entries, cancel, tx)?;
        }
        let value = json!({"updated":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),"books":entries});
        let books = self.books_from_cache(&value)?;
        fs::create_dir_all(path.parent().unwrap())
            .map_err(|_| "책 목록 캐시를 저장할 수 없습니다.")?;
        let temp = path.with_extension("tmp");
        fs::write(
            &temp,
            serde_json::to_vec(&value).map_err(|_| "책 목록 캐시를 만들 수 없습니다.")?,
        )
        .map_err(|_| "책 목록 캐시를 저장할 수 없습니다.")?;
        fs::rename(&temp, &path).map_err(|_| "책 목록 캐시를 저장할 수 없습니다.")?;
        self.catalog = Some(value);
        Ok(books)
    }
    fn inventory_batch(
        &mut self,
        ids: &[String],
        entries: &mut Vec<Value>,
        cancel: &AtomicBool,
        tx: &mpsc::Sender<Event>,
    ) -> Result<()> {
        for chunk in ids.chunks(72) {
            let mut url = url::Url::parse("https://book-api.ridibooks.com/books").unwrap();
            url.query_pairs_mut().append_pair("b_ids", &chunk.join(","));
            let v = self.json(url.as_str(), cancel, tx)?;
            let items = v.as_array().ok_or("책 정보 응답이 올바르지 않습니다.")?;
            let batch = metadata_batch(chunk, items)?;
            let books = self.books_from_cache(&json!({"books":batch}))?;
            entries.extend(batch);
            send(tx, Event::Partial(books));
        }
        Ok(())
    }
    fn books_from_cache(&self, v: &Value) -> Result<Vec<Book>> {
        let mut books = Vec::new();
        let mut seen = HashSet::new();
        for entry in v["books"]
            .as_array()
            .ok_or("책 목록 캐시가 올바르지 않습니다.")?
        {
            let i = id(&entry["id"])?;
            if !seen.insert(i.clone()) {
                return Err("책 목록에 중복된 항목이 있습니다.".into());
            }
            let title = entry["title"]
                .as_str()
                .ok_or("책 제목이 올바르지 않습니다.")?;
            let format = match entry["format"].as_str() {
                Some("bom" | "zip" | "webtoon") => Format::Zip,
                Some("epub") => Format::Epub,
                Some("pdf") => Format::Pdf,
                _ => return Err("지원하지 않는 책 파일 형식이 있습니다.".into()),
            };
            let downloaded = package::complete(&self.backup.join(&i), &i);
            books.push(Book::from_info(i, title.to_owned(), format, downloaded));
        }
        Ok(books)
    }
    fn download(
        &mut self,
        bookid: &str,
        position: usize,
        cancel: &AtomicBool,
        tx: &mpsc::Sender<Event>,
    ) -> Result<()> {
        let bookid = id(&Value::String(bookid.to_owned()))?;
        let dest = self.backup.join(&bookid);
        if package::complete(&dest, &bookid) {
            return Ok(());
        }
        if dest.exists() {
            return Err(
                "저장 폴더에 미완료 파일이 있습니다. 원본 보호를 위해 덮어쓰지 않았습니다.".into(),
            );
        }
        validate_backup(&self.backup, &self.library)?;
        fs::create_dir_all(&self.backup).map_err(|_| "리디 책 저장 폴더를 만들 수 없습니다.")?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let stage = self
            .backup
            .join(format!(".ridibackup-{bookid}-{stamp}.tmp"));
        fs::create_dir(&stage).map_err(|_| "임시 책 폴더를 만들 수 없습니다.")?;
        let result = (|| {
            let mut url =
                url::Url::parse(&format!("https://download.ridibooks.com/books/{bookid}")).unwrap();
            url.query_pairs_mut()
                .append_pair("device_id", &self.device)
                .append_pair("platform", "desktop");
            let mut response = self.request(url.as_str(), true, cancel, tx)?;
            let mut current = url;
            for hop in 0..=5 {
                if !(300..400).contains(&response.status) {
                    break;
                }
                if hop == 5 {
                    return Err("다운로드 주소 변경이 너무 많습니다.".into());
                }
                let location = response
                    .header(WINHTTP_QUERY_LOCATION)
                    .ok_or("다운로드 주소가 없습니다.")?;
                let next = current
                    .join(&location)
                    .map_err(|_| "다운로드 주소가 올바르지 않습니다.")?;
                drop(response);
                response = http::request(next.as_str(), "GET", "", &[], false, cancel)?;
                current = next;
            }
            if response.status != 200 {
                return Err(format!(
                    "다운로드에 실패했습니다. (HTTP {})",
                    response.status
                ));
            }
            let total = response
                .header(WINHTTP_QUERY_CONTENT_LENGTH)
                .and_then(|s| s.parse::<u64>().ok());
            let file = stage.join("transfer.zip");
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file)
                .map_err(|_| "다운로드 파일을 만들 수 없습니다.")?;
            let mut bytes = 0u64;
            let mut buffer = [0u8; 65536];
            let mut update = Instant::now() - Duration::from_secs(1);
            loop {
                let n = response.read(&mut buffer, cancel)?;
                if n == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..n])
                    .map_err(|_| "다운로드 파일을 저장할 수 없습니다.")?;
                bytes += n as u64;
                if total.is_some_and(|total| bytes > total) {
                    return Err("다운로드 파일 크기가 일치하지 않습니다.".into());
                }
                if update.elapsed() > Duration::from_millis(100) {
                    send(
                        tx,
                        Event::Progress {
                            position,
                            bytes,
                            total,
                        },
                    );
                    update = Instant::now();
                }
            }
            output
                .sync_all()
                .map_err(|_| "다운로드 파일을 저장할 수 없습니다.")?;
            drop(output);
            if total.is_some_and(|n| n != bytes) {
                return Err("다운로드가 끝나기 전에 연결이 끊어졌습니다.".into());
            }
            send(
                tx,
                Event::Progress {
                    position,
                    bytes,
                    total: Some(bytes),
                },
            );
            send(tx, Event::Status("파일 확인 중".into()));
            package::extract(&file, &stage, &bookid, cancel)?;
            fs::remove_file(file).map_err(|_| "임시 다운로드 파일을 정리할 수 없습니다.")?;
            if cancel.load(Ordering::Relaxed) {
                return Err("중단됨".into());
            }
            fs::rename(&stage, &dest).map_err(|_| "원본 책 폴더를 저장할 수 없습니다.")?;
            Ok(())
        })();
        if stage.exists() {
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }
}
pub fn spawn() -> Backend {
    let (command_tx, rx) = mpsc::channel();
    let (tx, event_rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let gap = Arc::new(AtomicU32::new(15));
    let c = cancel.clone();
    let g = gap.clone();
    std::thread::spawn(move || {
        let mut session = None;
        match settings::saved_login().and_then(|saved| Session::saved(saved, &c)) {
            Ok(mut s) => match s.inventory(false, &c, &tx) {
                Ok(books) => {
                    send(&tx, Event::Ready(books));
                    session = Some(s);
                }
                Err(error) => {
                    failure(&tx, error);
                    session = Some(s);
                }
            },
            Err(error) => send(&tx, Event::NeedLogin(error)),
        }
        while let Ok(command) = rx.recv() {
            match command {
                Command::Reconnect => {
                    send(&tx, Event::Loading("로그인 확인 중…".into()));
                    match settings::saved_login().and_then(|saved| Session::saved(saved, &c)) {
                        Ok(mut s) => {
                            match s.inventory(false, &c, &tx) {
                                Ok(books) => send(&tx, Event::Ready(books)),
                                Err(error) => failure(&tx, error),
                            }
                            session = Some(s);
                        }
                        Err(error) => {
                            session = None;
                            send(&tx, Event::NeedLogin(error));
                        }
                    }
                }
                Command::SetDirectory(root) => {
                    let Some(s) = &mut session else {
                        send(&tx, Event::Error("먼저 로그인해주세요.".into()));
                        continue;
                    };
                    let result = (|| {
                        let target = root.join(format!("_{}", s.account));
                        validate_backup(&target, &s.library)?;
                        if !root.is_absolute() {
                            return Err("백업 폴더는 절대 경로여야 합니다.".into());
                        }
                        if s.catalog.is_none() {
                            return Err("책 목록을 먼저 조회해주세요.".into());
                        }
                        settings::save_backup_root(&root)?;
                        s.backup = target;
                        send(&tx, Event::Directory(root));
                        let value = s.catalog.as_ref().ok_or("책 목록을 먼저 조회해주세요.")?;
                        s.books_from_cache(value)
                    })();
                    match result {
                        Ok(books) => send(&tx, Event::Ready(books)),
                        Err(error) => failure(&tx, error),
                    }
                }
                Command::Refresh => {
                    if let Some(s) = &mut session {
                        match s.inventory(true, &c, &tx) {
                            Ok(books) => send(&tx, Event::Ready(books)),
                            Err(error) => failure(&tx, error),
                        }
                    }
                }
                Command::Download(items) => {
                    let Some(s) = &mut session else {
                        send(&tx, Event::Error("먼저 로그인해주세요.".into()));
                        continue;
                    };
                    if s.backup
                        .parent()
                        .is_some_and(|root| !root.join("datastores").exists())
                    {
                        send(&tx, Event::Status("datastores 백업 중".into()));
                    }
                    let snapshot = (|| {
                        validate_backup(&s.backup, &s.library)?;
                        crate::snapshots::initialize(
                            &settings::ridi_dir()?.join("datastores"),
                            s.backup.parent().ok_or("백업 경로가 올바르지 않습니다.")?,
                            &c,
                        )
                    })();
                    if let Err(error) = snapshot {
                        if !c.load(Ordering::Relaxed) {
                            failure(&tx, error);
                        }
                        send(&tx, Event::Stopped);
                        continue;
                    }
                    let mut last = None;
                    let mut stopped = false;
                    for (position, book, bookid) in &items {
                        let position = *position;
                        if c.load(Ordering::Relaxed) {
                            stopped = true;
                            break;
                        }
                        send(&tx, Event::Current(position));
                        let exists = package::complete(&s.backup.join(bookid), bookid);
                        if !exists {
                            if let Some(t) = last {
                                loop {
                                    let elapsed: Duration = Instant::now() - t;
                                    let gap = Duration::from_millis(
                                        g.load(Ordering::Relaxed) as u64 * 100,
                                    );
                                    if elapsed >= gap {
                                        break;
                                    }
                                    send(
                                        &tx,
                                        Event::Status(format!(
                                            "다음 요청까지 {:.1}초 대기",
                                            (gap - elapsed).as_secs_f64()
                                        )),
                                    );
                                    if wait(Duration::from_millis(100), &c).is_err() {
                                        stopped = true;
                                        break;
                                    }
                                }
                            }
                            if stopped {
                                break;
                            }
                            send(&tx, Event::Status("다운로드 중".into()));
                            if let Err(error) = s.download(bookid, position, &c, &tx) {
                                if c.load(Ordering::Relaxed) {
                                    stopped = true;
                                } else {
                                    failure(&tx, error);
                                    stopped = true;
                                }
                                break;
                            }
                            last = Some(Instant::now());
                        }
                        // Official store writes are disabled until the complete client schema is verified.
                        send(
                            &tx,
                            Event::Done {
                                position,
                                book: *book,
                            },
                        );
                    }
                    if stopped {
                        send(&tx, Event::Stopped);
                    } else {
                        send(&tx, Event::Complete);
                    }
                }
            }
        }
    });
    Backend {
        commands: command_tx,
        events: event_rx,
        cancel,
        gap,
    }
}
pub fn verify() -> Result<Value> {
    let cancel = AtomicBool::new(false);
    let (tx, _rx) = mpsc::channel();
    let saved = settings::saved_login()?;
    let official = saved.official;
    let mut s = Session::saved(saved, &cancel)?;
    let books = s.inventory(false, &cancel, &tx)?;
    let mut counts = [0; 3];
    let mut complete = [0; 3];
    for book in &books {
        counts[book.format.index()] += 1;
        if book.downloaded {
            complete[book.format.index()] += 1;
        }
    }
    Ok(
        json!({"authenticated":true,"officialSavedLogin":official,"purchased":books.len(),"formats":counts,"downloaded":complete,"library":s.library,"inventoryCached":true}),
    )
}
pub fn verify_download() -> Result<Value> {
    let cancel = AtomicBool::new(false);
    let (tx, _rx) = mpsc::channel();
    let saved = settings::saved_login()?;
    let mut s = Session::saved(saved, &cancel)?;
    let books = s.inventory(false, &cancel, &tx)?;
    let book = books
        .iter()
        .find(|b| b.format == Format::Epub && !b.downloaded && !s.backup.join(&b.id).exists())
        .ok_or("검증할 미다운로드 EPUB이 없습니다.")?;
    s.download(&book.id, 0, &cancel, &tx)?;
    let directory = s.backup.join(&book.id);
    if !package::complete(&directory, &book.id) {
        return Err("저장된 책 파일을 검증할 수 없습니다.".into());
    }
    // A completed native book returns before requesting the server again.
    s.download(&book.id, 0, &cancel, &tx)?;
    let files:Vec<_>=fs::read_dir(&directory).map_err(|_|"저장된 파일을 읽을 수 없습니다.")?.filter_map(std::result::Result::ok).map(|e|json!({"name":e.file_name().to_string_lossy(),"bytes":e.metadata().map(|m|m.len()).unwrap_or(0)})).collect();
    let claims = s
        .access
        .split('.')
        .nth(1)
        .and_then(|v| URL_SAFE_NO_PAD.decode(v).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let names = claims
        .as_ref()
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    Ok(
        json!({"downloaded":true,"duplicateSkipped":true,"bookId":book.id,"format":"EPUB","directory":directory,"files":files,"accountClaimNames":names}),
    )
}

#[cfg(test)]
mod inventory_tests {
    use super::*;
    #[test]
    fn repeated_rate_limits_wait_longer_and_respect_server_delay() {
        assert_eq!(rate_limit_delay(Duration::ZERO, 0), Duration::from_secs(60));
        assert_eq!(
            rate_limit_delay(Duration::from_secs(1), 1),
            Duration::from_secs(120)
        );
        assert_eq!(
            rate_limit_delay(Duration::from_secs(900), 2),
            Duration::from_secs(900)
        );
        assert_eq!(
            rate_limit_delay(Duration::ZERO, 3),
            Duration::from_secs(480)
        );
    }
    #[test]
    fn expired_refresh_requests_official_login() {
        let (tx, rx) = mpsc::channel();
        failure(&tx, LOGIN_REQUIRED.into());
        assert!(matches!(rx.recv().unwrap(), Event::NeedLogin(_)));
        failure(&tx, "network failure".into());
        assert!(matches!(rx.recv().unwrap(), Event::Error(_)));
    }
    #[test]
    fn backup_status_uses_selected_folder_and_protects_official_files() {
        let root =
            std::env::temp_dir().join(format!("ridibackup-path-test-{}", std::process::id()));
        let official = root.join("official");
        let backup = root.join("backup/_123");
        fs::create_dir_all(official.join("1")).unwrap();
        fs::write(official.join("1/1.dat"), b"original-key").unwrap();
        fs::write(official.join("1/1.epub"), b"original-content").unwrap();
        assert!(validate_backup(&official.join("nested"), &official).is_err());
        assert!(validate_backup(&root, &official).is_err());
        assert!(validate_backup(&backup, &official).is_ok());
        let mut session = Session {
            access: Zeroizing::new(String::new()),
            refresh: Zeroizing::new(String::new()),
            expires: Instant::now(),
            last_request: None,
            retry_at: None,
            library: official.clone(),
            backup: backup.clone(),
            account: "123".into(),
            device: String::new(),
            catalog: None,
        };
        let catalog = json!({"books":[{"id":"1","title":"one","format":"epub"}]});
        let cache = session.cache_path().unwrap();
        assert!(!session.books_from_cache(&catalog).unwrap()[0].downloaded);
        fs::create_dir_all(backup.join("1")).unwrap();
        fs::write(backup.join("1/1.dat"), b"backup-key").unwrap();
        fs::write(backup.join("1/1.epub"), b"backup-content").unwrap();
        assert!(session.books_from_cache(&catalog).unwrap()[0].downloaded);
        session.backup = root.join("another/_123");
        assert!(!session.books_from_cache(&catalog).unwrap()[0].downloaded);
        assert_eq!(session.cache_path().unwrap(), cache);
        assert_eq!(
            fs::read(official.join("1/1.epub")).unwrap(),
            b"original-content"
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn incremental_batches_preserve_order_and_reject_missing_books() {
        let ids = vec!["1".into(), "2".into()];
        let items = vec![
            json!({"id":2,"title":{"main":"second"},"file":{"format":"pdf"}}),
            json!({"id":1,"title":{"main":"first"},"file":{"format":"epub"}}),
        ];
        let batch = metadata_batch(&ids, &items).unwrap();
        assert_eq!(batch[0]["id"], "1");
        assert_eq!(batch[1]["id"], "2");
        assert!(metadata_batch(&ids, &items[..1]).is_err());
        assert!(metadata_batch(&ids, &[items[0].clone(), items[0].clone()]).is_err());
        let mut seen = HashSet::new();
        let mut pending = Vec::new();
        for item in [
            json!({"b_id":1,"service_type":"normal"}),
            json!({"b_id":"1","service_type":"buy"}),
            json!({"b_id":2,"service_type":"rent"}),
            json!({"b_id":3,"is_ridiselect":true}),
        ] {
            collect_owned(&item, &mut seen, &mut pending).unwrap();
        }
        assert_eq!(pending, vec!["1"]);
    }
}
