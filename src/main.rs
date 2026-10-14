#![windows_subsystem = "windows"]
mod backend;
mod http;
mod model;
mod package;
mod settings;
mod snapshots;
mod store_aes;
use model::*;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::Instant,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::LibraryLoader::*,
    UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

const TITLE: u16 = 100;
const CHART: u16 = 101;
const SEARCH: u16 = 102;
const ZIP: u16 = 103;
const EPUB: u16 = 104;
const PDF: u16 = 105;
const PENDING: u16 = 106;
const ALL: u16 = 107;
const SELECTION: u16 = 109;
const LIBRARY: u16 = 110;

const START: u16 = 112;
const DIRECTORY_LABEL: u16 = 113;
const DIRECTORY_PATH: u16 = 114;
const DIRECTORY_BROWSE: u16 = 115;
const COUNT: u16 = 201;
const OVERALL: u16 = 202;
const CURRENT_LABEL: u16 = 203;
const CURRENT: u16 = 204;
const STATUS: u16 = 205;
const FILE_PROGRESS: u16 = 206;
const BYTES: u16 = 207;
const RESTART: u16 = 208;
const STOP: u16 = 209;
const RETURN: u16 = 210;
const GAP_LABEL: u16 = 211;
const GAP: u16 = 212;
const GAP_SPIN: u16 = 213;
const GAP_UNIT: u16 = 214;
const QUEUE: u16 = 215;
const GAP_ERROR: u16 = 216;
const SHOW_COMPLETED: u16 = 217;
const LOGIN_TITLE: u16 = 300;
const LOGIN: u16 = 306;
const LOGIN_ERROR: u16 = 307;

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Library,
    Download,
    Login,
}
#[derive(Clone)]
struct SearchRow {
    search: String,
    initials: String,
    format: usize,
}
struct FilterRequest {
    rows: Arc<Vec<SearchRow>>,
    generation: u64,
    query: String,
    formats: [bool; 3],
    pending: bool,
    downloaded: Vec<bool>,
}
struct FilterResult {
    generation: u64,
    ids: Vec<usize>,
}
struct Data {
    books: Vec<Book>,
    filtered: Vec<usize>,
    queue_ids: Vec<usize>,
    selected: HashSet<usize>,
    downloads: Downloads,
    available: usize,
    searching: bool,
    totals: [usize; 3],
    downloaded: [usize; 3],
}
struct App {
    hwnd: HWND,
    controls: RefCell<HashMap<u16, HWND>>,
    font: Cell<HFONT>,
    screen: Cell<Screen>,
    dpi: Cell<u32>,
    data: RefCell<Data>,
    generation: Arc<AtomicU64>,
    request: mpsc::Sender<FilterRequest>,
    results: RefCell<mpsc::Receiver<FilterResult>>,
    smoke: bool,
    smoke_step: Cell<u32>,
    queue_current: Cell<usize>,
    show_completed: Cell<bool>,
    queue_dirty: Cell<bool>,
    search_rows: RefCell<Arc<Vec<SearchRow>>>,
    live: Option<backend::Backend>,
    connecting: Cell<bool>,
    live_bytes: Cell<u64>,
    live_total: Cell<Option<u64>>,
    live_status: RefCell<String>,
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
unsafe fn choose_directory(owner: HWND) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::{System::Com::*, UI::Shell::*};
    let initialized = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
    let title = wide("백업 폴더 선택");
    let mut info: BROWSEINFOW = zeroed();
    info.hwndOwner = owner;
    info.lpszTitle = title.as_ptr();
    info.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
    let item = SHBrowseForFolderW(&info);
    let result = if item.is_null() {
        None
    } else {
        let mut path = vec![0u16; 32768];
        let result = if SHGetPathFromIDListEx(item, path.as_mut_ptr(), path.len() as u32, 0) != 0 {
            let n = path.iter().position(|&c| c == 0).unwrap_or(path.len());
            Some(std::path::PathBuf::from(String::from_utf16_lossy(
                &path[..n],
            )))
        } else {
            None
        };
        CoTaskMemFree(item as _);
        result
    };
    if initialized >= 0 {
        CoUninitialize();
    }
    result
}
fn num(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
unsafe fn text(hwnd: HWND, s: &str) {
    SetWindowTextW(hwnd, wide(s).as_ptr());
}
unsafe fn read(hwnd: HWND) -> String {
    let n = GetWindowTextLengthW(hwnd);
    let mut b = vec![0u16; n as usize + 1];
    GetWindowTextW(hwnd, b.as_mut_ptr(), n + 1);
    String::from_utf16_lossy(&b[..n as usize])
}
fn worker(
    generation: Arc<AtomicU64>,
    rx: mpsc::Receiver<FilterRequest>,
    tx: mpsc::Sender<FilterResult>,
) {
    while let Ok(mut request) = rx.recv() {
        while let Ok(next) = rx.try_recv() {
            request = next;
        }
        let mut ids = Vec::new();
        for (i, row) in request.rows.iter().enumerate() {
            if i % 4096 == 0 && generation.load(Ordering::Relaxed) != request.generation {
                break;
            }
            if request.formats[row.format]
                && (!request.pending || !request.downloaded[i])
                && (request.query.is_empty()
                    || fuzzy(&request.query, &row.search)
                    || fuzzy(&request.query, &row.initials))
            {
                ids.push(i);
            }
        }
        if generation.load(Ordering::Relaxed) == request.generation
            && tx
                .send(FilterResult {
                    generation: request.generation,
                    ids,
                })
                .is_err()
        {
            break;
        }
    }
}
impl App {
    fn new(hwnd: HWND, smoke: bool) -> Self {
        let demo = smoke || std::env::args().any(|s| s == "--demo");
        let books = if demo { demo_books() } else { Vec::new() };
        let rows: Arc<Vec<SearchRow>> = Arc::new(
            books
                .iter()
                .map(|b| SearchRow {
                    search: b.search.clone(),
                    initials: b.initials.clone(),
                    format: b.format.index(),
                })
                .collect(),
        );
        let generation = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let g = generation.clone();
        let search_rows = rows.clone();
        std::thread::spawn(move || worker(g, rx, result_tx));
        Self {
            hwnd,
            controls: RefCell::new(HashMap::new()),
            font: Cell::new(null_mut()),
            screen: Cell::new(Screen::Library),
            dpi: Cell::new(96),
            data: RefCell::new(Data {
                filtered: (0..books.len()).filter(|&i| !books[i].downloaded).collect(),
                queue_ids: Vec::new(),
                books,
                selected: HashSet::new(),
                downloads: Downloads::default(),
                available: 0,
                searching: false,
                totals: if demo { [228600, 20550, 850] } else { [0; 3] },
                downloaded: if demo { [3600, 900, 100] } else { [0; 3] },
            }),
            generation,
            request: tx,
            results: RefCell::new(result_rx),
            smoke,
            smoke_step: Cell::new(0),
            queue_current: Cell::new(usize::MAX),
            show_completed: Cell::new(false),
            queue_dirty: Cell::new(true),
            search_rows: RefCell::new(search_rows),
            live: if demo { None } else { Some(backend::spawn()) },
            connecting: Cell::new(!demo),
            live_bytes: Cell::new(0),
            live_total: Cell::new(None),
            live_status: RefCell::new(if demo {
                String::new()
            } else {
                "책 목록을 불러오는 중…".to_owned()
            }),
        }
    }
    fn control(&self, id: u16) -> HWND {
        self.controls
            .borrow()
            .get(&id)
            .copied()
            .unwrap_or(null_mut())
    }
    unsafe fn add(&self, id: u16, class: &str, label: &str, style: u32) -> HWND {
        let h = CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(label).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            0,
            0,
            self.hwnd,
            id as usize as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        assert!(!h.is_null(), "create control {id}");
        self.controls.borrow_mut().insert(id, h);
        h
    }
    unsafe fn label(&self, id: u16, label: &str) {
        self.add(id, "STATIC", label, 0);
    }
    unsafe fn button(&self, id: u16, label: &str) {
        self.add(id, "BUTTON", label, WS_TABSTOP | BS_PUSHBUTTON as u32);
    }
    unsafe fn checkbox(&self, id: u16, label: &str, checked: bool) {
        let h = self.add(id, "BUTTON", label, WS_TABSTOP | BS_AUTOCHECKBOX as u32);
        SendMessageW(
            h,
            BM_SETCHECK,
            if checked { BST_CHECKED } else { BST_UNCHECKED } as usize,
            0,
        );
    }
    unsafe fn list(&self, id: u16, check: bool) {
        let h = self.add(
            id,
            "SysListView32",
            "",
            WS_TABSTOP | WS_BORDER | LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS | LVS_SINGLESEL,
        );
        SendMessageW(
            h,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            0,
            (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | if check { LVS_EX_CHECKBOXES } else { 0 })
                as isize,
        );
        if check {
            SendMessageW(h, LVM_SETCALLBACKMASK, LVIS_STATEIMAGEMASK as usize, 0);
        }
        SetWindowTheme(h, wide("Explorer").as_ptr(), null());
        let labels: &[&str] = if check {
            &["", "제목", "형식", "상태"]
        } else {
            &["제목", "형식", "상태"]
        };
        for (i, label) in labels.iter().enumerate() {
            let mut t = wide(label);
            let mut col: LVCOLUMNW = zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM;
            col.pszText = t.as_mut_ptr();
            col.cx = if check {
                [32, 500, 80, 140][i]
            } else {
                [500, 80, 140][i]
            };
            col.iSubItem = i as i32;
            SendMessageW(h, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
        }
    }
    unsafe fn init(&self) {
        self.label(TITLE, "구입한 책 250,000권");
        self.add(CHART, "RidibackupChart", "", 0);
        let search = self.add(
            SEARCH,
            "EDIT",
            "",
            WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
        );
        SendMessageW(
            search,
            EM_SETCUEBANNER,
            1,
            wide("책 제목 검색").as_ptr() as isize,
        );
        for (id, label) in [(ZIP, "ZIP"), (EPUB, "EPUB"), (PDF, "PDF")] {
            self.checkbox(id, label, true);
        }
        self.checkbox(PENDING, "다운로드 필요", true);
        self.label(SELECTION, "선택 0권");
        self.list(LIBRARY, true);
        self.add(ALL, "BUTTON", "", WS_TABSTOP | BS_3STATE as u32);
        let header = SendMessageW(self.control(LIBRARY), LVM_GETHEADER, 0, 0) as HWND;
        SetParent(self.control(ALL), header);
        SetWindowSubclass(header, Some(header_proc), 1, self.hwnd as usize);
        for h in [self.control(LIBRARY), header] {
            SetWindowLongPtrW(
                h,
                GWL_EXSTYLE,
                GetWindowLongPtrW(h, GWL_EXSTYLE) | WS_EX_CONTROLPARENT as isize,
            );
        }
        self.add(
            START,
            "BUTTON",
            "다운로드 시작",
            WS_TABSTOP | BS_OWNERDRAW as u32,
        );
        self.label(DIRECTORY_LABEL, "백업 폴더");
        self.add(
            DIRECTORY_PATH,
            "STATIC",
            &settings::backup_root()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            0x8000,
        ); // SS_PATHELLIPSIS
        self.button(DIRECTORY_BROWSE, "변경…");
        self.label(COUNT, "");
        self.add(OVERALL, "msctls_progress32", "", PBS_SMOOTH);
        self.label(CURRENT_LABEL, "현재 책:");
        self.label(CURRENT, "");
        self.label(STATUS, "");
        self.add(FILE_PROGRESS, "msctls_progress32", "", PBS_SMOOTH);
        self.add(BYTES, "STATIC", "", 2); // SS_RIGHT: keep the numeric field anchored.
        self.button(RESTART, "재개");
        self.button(STOP, "중단");
        self.button(RETURN, "책 목록으로 돌아가기");
        self.label(GAP_LABEL, "다운로드 사이 간격");
        self.add(
            GAP,
            "EDIT",
            "1.5",
            WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
        );
        let spin = self.add(GAP_SPIN, "msctls_updown32", "", UDS_ARROWKEYS);
        SendMessageW(spin, UDM_SETRANGE32, 0, 864000);
        self.label(GAP_UNIT, "초");
        self.label(GAP_ERROR, "");
        self.checkbox(SHOW_COMPLETED, "완료된 책 표시하기", false);
        self.list(QUEUE, false);
        self.label(LOGIN_TITLE, "공식 리디 앱에서 로그인해주세요.");
        self.button(LOGIN, "다시 확인");
        self.label(LOGIN_ERROR, "");
        self.set_font(GetDpiForWindow(self.hwnd));
        self.show(self.screen.get());
        self.refresh_library();
        SetTimer(self.hwnd, 1, 100, None);
    }
    unsafe fn set_font(&self, dpi: u32) {
        self.dpi.set(dpi.max(96));
        let new = CreateFontW(
            -((15 * self.dpi.get() / 96) as i32),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            wide("Segoe UI").as_ptr(),
        );
        for h in self.controls.borrow().values() {
            SendMessageW(*h, WM_SETFONT, new as usize, 1);
        }
        let old = self.font.replace(new);
        if !old.is_null() {
            DeleteObject(old);
        }
        self.layout();
    }
    unsafe fn show(&self, screen: Screen) {
        self.screen.set(screen);
        for (&id, &h) in self.controls.borrow().iter() {
            let belongs = if id < 200 {
                Screen::Library
            } else if id < 300 {
                Screen::Download
            } else {
                Screen::Login
            };
            ShowWindow(h, if belongs == screen { SW_SHOW } else { SW_HIDE });
        }
        self.layout();
    }
    unsafe fn place(&self, id: u16, x: i32, y: i32, w: i32, h: i32) {
        let scale = |v: i32| v * self.dpi.get() as i32 / 96;
        MoveWindow(
            self.control(id),
            scale(x),
            scale(y),
            scale(w.max(0)),
            scale(h.max(0)),
            1,
        );
    }
    unsafe fn layout(&self) {
        let mut rc: RECT = zeroed();
        GetClientRect(self.hwnd, &mut rc);
        let w = rc.right * 96 / self.dpi.get() as i32;
        let h = rc.bottom * 96 / self.dpi.get() as i32;
        let width = w - 40;
        self.place(TITLE, 20, 18, width - 170, 24);
        self.place(CHART, 20, 52, width, 190);
        self.place(SEARCH, 20, 254, (width - 390).max(160), 30);
        let fx = (w - 390).max(195);
        self.place(ZIP, fx, 257, 60, 26);
        self.place(EPUB, fx + 64, 257, 70, 26);
        self.place(PDF, fx + 142, 257, 60, 26);
        self.place(PENDING, fx + 210, 257, 140, 26);
        self.place(ALL, 3, 2, 20, 18);
        self.place(SELECTION, 20, 304, width, 24);
        self.place(LIBRARY, 20, 342, width, h - 402);
        self.place(DIRECTORY_LABEL, 20, h - 45, 80, 24);
        self.place(DIRECTORY_PATH, 102, h - 45, width - 170, 24);
        self.place(DIRECTORY_BROWSE, w - 90, h - 51, 70, 30);
        self.place(START, w - 170, 12, 150, 32);
        self.place(COUNT, 20, 18, width, 24);
        self.place(OVERALL, 20, 51, width, 16);
        self.place(CURRENT_LABEL, 20, 87, 68, 24);
        self.place(CURRENT, 92, 87, width - 72, 24);
        self.place(STATUS, 20, 156, width - 250, 24);
        self.place(FILE_PROGRESS, 20, 126, width, 14);
        self.place(BYTES, w - 250, 156, 230, 24);
        self.place(RESTART, 20, 198, 110, 32);
        self.place(STOP, 140, 198, 90, 32);
        self.place(RETURN, 240, 198, 190, 32);
        self.place(GAP_LABEL, 20, 245, 160, 24);
        self.place(GAP, 184, 239, 75, 30);
        self.place(GAP_SPIN, 260, 239, 20, 30);
        self.place(GAP_UNIT, 290, 245, 25, 24);
        self.place(GAP_ERROR, 325, 245, w - 555, 24);
        self.place(SHOW_COMPLETED, w - 210, 241, 190, 26);
        self.place(QUEUE, 20, 289, width, h - 309);
        let left = (w - 330) / 2;
        self.place(LOGIN_TITLE, left, 204, 330, 30);
        self.place(LOGIN_ERROR, left, 246, 330, 100);
        self.place(LOGIN, left, 366, 330, 34);
        for id in [LIBRARY, QUEUE] {
            let list = self.control(id);
            let scale = self.dpi.get() as i32;
            let offset = if id == LIBRARY { 1 } else { 0 };
            if offset == 1 {
                SendMessageW(list, LVM_SETCOLUMNWIDTH, 0, (32 * scale / 96) as isize);
            }
            SendMessageW(
                list,
                LVM_SETCOLUMNWIDTH,
                offset,
                ((width - 275 - offset as i32 * 32).max(180) * scale / 96) as isize,
            );
            SendMessageW(
                list,
                LVM_SETCOLUMNWIDTH,
                offset + 1,
                (75 * scale / 96) as isize,
            );
            SendMessageW(
                list,
                LVM_SETCOLUMNWIDTH,
                offset + 2,
                (165 * scale / 96) as isize,
            );
        }
        InvalidateRect(self.control(CHART), null(), 1);
    }
    unsafe fn refresh_library(&self) {
        let (n, selected, matching, searching) = {
            let d = self.data.borrow();
            (
                d.filtered.len(),
                d.selected.len(),
                d.filtered.iter().filter(|i| d.selected.contains(i)).count(),
                d.searching,
            )
        };
        text(
            self.control(SELECTION),
            &format!("선택 {}권", num(selected)),
        );
        let count_text = if self.connecting.get() {
            self.live_status.borrow().clone()
        } else {
            format!("구입한 책 {}권", num(self.data.borrow().books.len()))
        };
        text(self.control(TITLE), &count_text);
        let check = if matching == 0 {
            BST_UNCHECKED
        } else if matching == n {
            BST_CHECKED
        } else {
            BST_INDETERMINATE
        };
        SendMessageW(self.control(ALL), BM_SETCHECK, check as usize, 0);
        EnableWindow(
            self.control(START),
            (selected > 0 && !searching && !self.connecting.get()) as i32,
        );
        EnableWindow(self.control(ALL), (n > 0 && !searching) as i32);
        EnableWindow(
            self.control(DIRECTORY_BROWSE),
            (!self.connecting.get()) as i32,
        );
        SendMessageW(
            self.control(LIBRARY),
            LVM_SETITEMCOUNT,
            n,
            LVSICF_NOINVALIDATEALL as isize,
        );
        InvalidateRect(self.control(LIBRARY), null(), 0);
    }
    unsafe fn filter(&self) {
        let q = normalize(&read(self.control(SEARCH)));
        let formats = [ZIP, EPUB, PDF]
            .map(|id| SendMessageW(self.control(id), BM_GETCHECK, 0, 0) == BST_CHECKED as isize);
        let pending =
            SendMessageW(self.control(PENDING), BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let downloaded = {
            let mut d = self.data.borrow_mut();
            d.searching = true;
            d.books.iter().map(|b| b.downloaded).collect()
        };
        let _ = self.request.send(FilterRequest {
            rows: self.search_rows.borrow().clone(),
            generation,
            query: q,
            formats,
            pending,
            downloaded,
        });
        self.refresh_library();
    }
    unsafe fn poll(&self) {
        self.poll_live();
        let mut latest = None;
        while let Ok(result) = self.results.borrow().try_recv() {
            if result.generation == self.generation.load(Ordering::Relaxed) {
                latest = Some(result);
            }
        }
        if let Some(result) = latest {
            {
                let mut d = self.data.borrow_mut();
                d.filtered = result.ids;
                d.searching = false;
            }
            SendMessageW(self.control(LIBRARY), LVM_ENSUREVISIBLE, 0, 0);
            self.refresh_library();
        }
    }
    unsafe fn toggle(&self, row: usize) {
        if self.screen.get() != Screen::Library {
            return;
        }
        {
            let mut d = self.data.borrow_mut();
            if let Some(&id) = d.filtered.get(row) {
                if !d.selected.remove(&id) {
                    d.selected.insert(id);
                }
            }
        }
        self.refresh_library();
    }
    unsafe fn command(&self, id: u16, notification: u16) {
        match id {
            DIRECTORY_BROWSE => {
                if self.connecting.get() {
                    return;
                }
                if let Some(path) = choose_directory(self.hwnd) {
                    if let Some(live) = &self.live {
                        self.connecting.set(true);
                        *self.live_status.borrow_mut() = "백업 폴더 확인 중…".into();
                        let _ = live.commands.send(backend::Command::SetDirectory(path));
                        self.refresh_library();
                    } else {
                        text(self.control(DIRECTORY_PATH), &path.to_string_lossy());
                    }
                }
            }
            SEARCH if notification == EN_CHANGE as u16 => {
                KillTimer(self.hwnd, 2);
                SetTimer(self.hwnd, 2, 180, None);
            }
            ZIP | EPUB | PDF | PENDING => self.filter(),
            ALL => {
                {
                    let mut d = self.data.borrow_mut();
                    let remove = d.filtered.iter().all(|i| d.selected.contains(i));
                    let ids = d.filtered.clone();
                    for i in ids {
                        if remove {
                            d.selected.remove(&i);
                        } else {
                            d.selected.insert(i);
                        }
                    }
                }
                self.refresh_library();
            }
            SHOW_COMPLETED => {
                self.show_completed.set(
                    SendMessageW(self.control(SHOW_COMPLETED), BM_GETCHECK, 0, 0)
                        == BST_CHECKED as isize,
                );
                self.queue_dirty.set(true);
                self.refresh_download();
            }
            START => {
                if self.connecting.get()
                    || self.data.borrow().selected.is_empty()
                    || self.data.borrow().searching
                {
                    return;
                }
                {
                    let mut d = self.data.borrow_mut();
                    let selected = d.selected.clone();
                    d.downloads.start(&selected);
                    d.available = d.downloads.count(&d.books);
                }
                self.queue_current.set(usize::MAX);
                self.show(Screen::Download);
                self.start_live();
                self.tick();
                SetFocus(self.control(STOP));
            }
            RESTART => {
                self.data.borrow_mut().downloads.restart();
                self.start_live();
                self.tick();
                SetFocus(self.control(STOP));
            }
            STOP => {
                if let Some(live) = &self.live {
                    live.cancel.store(true, Ordering::Relaxed);
                    *self.live_status.borrow_mut() = "중단 중…".into();
                } else {
                    self.data.borrow_mut().downloads.stop();
                }
                self.refresh_download();
                SetFocus(self.control(RESTART));
            }
            RETURN => {
                if !self.data.borrow().downloads.can_return() {
                    return;
                }
                let gap = self.data.borrow().downloads.gap_tenths;
                self.data.borrow_mut().downloads = Downloads {
                    gap_tenths: gap,
                    ..Downloads::default()
                };
                self.show(Screen::Library);
                self.filter();
                SetFocus(self.control(SEARCH));
            }
            GAP if notification == EN_CHANGE as u16 => self.apply_gap(),
            LOGIN => {
                if let Some(live) = &self.live {
                    EnableWindow(self.control(LOGIN), 0);
                    self.connecting.set(true);
                    live.cancel.store(false, Ordering::Relaxed);
                    let _ = live.commands.send(backend::Command::Reconnect);
                }
            }
            _ => {}
        }
    }
    unsafe fn apply_gap(&self) {
        let s = read(self.control(GAP));
        if s.trim().is_empty() {
            return;
        }
        match s.parse::<f64>() {
            Ok(v) if v.is_finite() && (0.0..=86400.0).contains(&v) => {
                self.data.borrow_mut().downloads.gap_tenths = (v * 10.0).round() as u32;
                if let Some(live) = &self.live {
                    live.gap.store((v * 10.0).round() as u32, Ordering::Relaxed);
                }
                text(self.control(GAP_ERROR), "");
                self.refresh_download();
            }
            _ => text(self.control(GAP_ERROR), "0–86400초를 입력하세요."),
        }
    }
    unsafe fn tick(&self) {
        if self.live.is_some() {
            if self.screen.get() == Screen::Download {
                self.refresh_download();
            }
            return;
        }
        if self.screen.get() != Screen::Download {
            return;
        }
        {
            let mut d = self.data.borrow_mut();
            let old = d.downloads.index;
            let active = d
                .downloads
                .ids
                .get(old)
                .copied()
                .filter(|&i| !d.books[i].downloaded);
            let Data {
                books, downloads, ..
            } = &mut *d;
            downloads.tick(books, Instant::now());
            let new = d.downloads.index;
            if new != old {
                d.available = d.downloads.count(&d.books);
                if let Some(i) = active {
                    if d.books[i].downloaded {
                        let format = d.books[i].format.index();
                        d.downloaded[format] += 1;
                    }
                }
            }
        }
        self.refresh_download();
    }
    unsafe fn refresh_download(&self) {
        let changed = self.queue_current.get() != self.data.borrow().downloads.index
            || self.queue_dirty.replace(false);
        if changed {
            let mut d = self.data.borrow_mut();
            d.queue_ids = d
                .downloads
                .ids
                .iter()
                .copied()
                .filter(|&i| self.show_completed.get() || !d.books[i].downloaded)
                .collect();
        }
        let (count, overall, current, status, bytes, percent, run, can_return, len, index) = {
            let d = self.data.borrow();
            let dl = &d.downloads;
            let now = Instant::now();
            let status = match dl.phase {
                Phase::Stopped => "중단됨".to_owned(),
                Phase::Complete => "다운로드 완료".to_owned(),
                Phase::Running => {
                    if let Some(t) = dl.waiting_since {
                        let remaining = (dl.gap_tenths as f64 / 10.0
                            - now.duration_since(t).as_secs_f64())
                        .max(0.0);
                        if remaining > 0.0 {
                            format!("다음 요청까지 {remaining:.1}초 대기")
                        } else {
                            "다운로드 중".to_owned()
                        }
                    } else {
                        "다운로드 중".to_owned()
                    }
                }
                Phase::Idle => String::new(),
            };
            let current = dl
                .ids
                .get(dl.index)
                .map(|&i| d.books[i].title.clone())
                .unwrap_or_default();
            let p = dl.percent;
            (
                format!("다운로드됨 {} / {}권", num(d.available), num(dl.ids.len())),
                if dl.ids.is_empty() {
                    0
                } else {
                    100 * d.available / dl.ids.len()
                },
                current,
                status,
                format!("{:.1} / 129.5 MB · {}%", 129.5 * p as f64 / 100.0, p),
                p,
                dl.phase == Phase::Running,
                dl.can_return(),
                dl.ids.len(),
                dl.index,
            )
        };
        text(self.control(COUNT), &count);
        text(self.control(CURRENT), &current);
        let status = if self.live.is_some() {
            self.live_status.borrow().clone()
        } else {
            status
        };
        let bytes = if self.live.is_some() {
            let mb = self.live_bytes.get() as f64 / 1_048_576.0;
            match self.live_total.get() {
                Some(total) => format!(
                    "{mb:.1} / {:.1} MB · {percent}%",
                    total as f64 / 1_048_576.0
                ),
                None => format!("{mb:.1} MB"),
            }
        } else {
            bytes
        };
        text(self.control(STATUS), &status);
        text(self.control(BYTES), if index >= len { "" } else { &bytes });
        SendMessageW(self.control(OVERALL), PBM_SETPOS, overall, 0);
        SendMessageW(self.control(FILE_PROGRESS), PBM_SETPOS, percent as usize, 0);
        EnableWindow(
            self.control(STOP),
            (run && self
                .live
                .as_ref()
                .is_none_or(|live| !live.cancel.load(Ordering::Relaxed))) as i32,
        );
        EnableWindow(
            self.control(RESTART),
            (!run && can_return && index < len) as i32,
        );
        EnableWindow(self.control(RETURN), can_return as i32);
        let (queue_len, current_row) = {
            let d = self.data.borrow();
            (
                d.queue_ids.len(),
                d.downloads
                    .ids
                    .get(index)
                    .and_then(|id| d.queue_ids.iter().position(|i| i == id)),
            )
        };
        SendMessageW(
            self.control(QUEUE),
            LVM_SETITEMCOUNT,
            queue_len,
            LVSICF_NOSCROLL as isize,
        );
        if self.queue_current.replace(index) != index || changed {
            if let Some(row) = current_row {
                SendMessageW(self.control(QUEUE), LVM_ENSUREVISIBLE, row, 0);
            }
        }
        InvalidateRect(self.control(QUEUE), null(), 0);
    }
    unsafe fn notify(&self, lp: LPARAM) -> LRESULT {
        let hdr = &*(lp as *const NMHDR);
        match hdr.code {
            LVN_GETDISPINFOW => {
                let info = &mut *(lp as *mut NMLVDISPINFOW);
                let d = self.data.borrow();
                let row = info.item.iItem as usize;
                let id = if hdr.idFrom == LIBRARY as usize {
                    d.filtered.get(row)
                } else {
                    d.queue_ids.get(row)
                };
                if let Some(&id) = id {
                    let book = &d.books[id];
                    if info.item.mask & LVIF_TEXT != 0 {
                        let column =
                            info.item.iSubItem - if hdr.idFrom == LIBRARY as usize { 1 } else { 0 };
                        let value = match column {
                            -1 => String::new(),
                            0 => book.title.clone(),
                            1 => book.format.label().to_owned(),
                            _ => {
                                if book.downloaded {
                                    "다운로드 완료".to_owned()
                                } else if hdr.idFrom == QUEUE as usize
                                    && d.downloads.ids.get(d.downloads.index) == Some(&id)
                                {
                                    match d.downloads.phase {
                                        Phase::Running => "다운로드 중",
                                        Phase::Stopped => "중단됨",
                                        _ => "대기",
                                    }
                                    .to_owned()
                                } else if hdr.idFrom == QUEUE as usize {
                                    "대기".to_owned()
                                } else {
                                    "다운로드 필요".to_owned()
                                }
                            }
                        };
                        let w = wide(&value);
                        if !info.item.pszText.is_null() && info.item.cchTextMax > 0 {
                            let n = (w.len() - 1).min(info.item.cchTextMax as usize - 1);
                            std::ptr::copy_nonoverlapping(w.as_ptr(), info.item.pszText, n);
                            *info.item.pszText.add(n) = 0;
                        }
                    }
                    if info.item.mask & LVIF_STATE != 0 {
                        info.item.stateMask = LVIS_STATEIMAGEMASK;
                        info.item.state = if d.selected.contains(&id) {
                            2 << 12
                        } else {
                            1 << 12
                        };
                    }
                }
                0
            }
            NM_CLICK if hdr.idFrom == LIBRARY as usize => {
                let click = &*(lp as *const NMITEMACTIVATE);
                let mut hit: LVHITTESTINFO = zeroed();
                hit.pt = click.ptAction;
                SendMessageW(
                    self.control(LIBRARY),
                    LVM_HITTEST,
                    0,
                    &mut hit as *mut _ as isize,
                );
                if hit.iItem >= 0 && hit.flags & LVHT_ONITEMSTATEICON != 0 {
                    self.toggle(hit.iItem as usize);
                }
                0
            }
            LVN_KEYDOWN if hdr.idFrom == LIBRARY as usize => {
                let key = &*(lp as *const NMLVKEYDOWN);
                if key.wVKey == 0x20 {
                    let row = SendMessageW(
                        self.control(LIBRARY),
                        LVM_GETNEXTITEM,
                        usize::MAX,
                        LVNI_FOCUSED as isize,
                    );
                    if row >= 0 {
                        self.toggle(row as usize);
                    }
                }
                0
            }
            UDN_DELTAPOS if hdr.idFrom == GAP_SPIN as usize => {
                let spin = &*(lp as *const NMUPDOWN);
                let n =
                    (self.data.borrow().downloads.gap_tenths as i32 - spin.iDelta).clamp(0, 864000);
                text(self.control(GAP), &format!("{:.1}", n as f64 / 10.0));
                1
            }
            _ => 0,
        }
    }
    unsafe fn start_live(&self) {
        if let Some(live) = &self.live {
            live.cancel.store(false, Ordering::Relaxed);
            let items = {
                let d = self.data.borrow();
                d.downloads
                    .ids
                    .iter()
                    .enumerate()
                    .skip(d.downloads.index)
                    .map(|(position, &book)| (position, book, d.books[book].id.clone()))
                    .collect()
            };
            self.live_bytes.set(0);
            self.live_total.set(None);
            *self.live_status.borrow_mut() = "다운로드 준비 중".into();
            let _ = live.commands.send(backend::Command::Download(items));
        }
    }
    unsafe fn poll_live(&self) {
        let Some(live) = &self.live else {
            return;
        };
        while let Ok(event) = live.events.try_recv() {
            match event {
                backend::Event::Directory(path) => {
                    text(self.control(DIRECTORY_PATH), &path.to_string_lossy());
                }
                backend::Event::Loading(message) => {
                    self.connecting.set(true);
                    *self.live_status.borrow_mut() = message.clone();
                    if self.screen.get() == Screen::Login {
                        text(self.control(LOGIN_ERROR), &message);
                    } else {
                        text(self.control(TITLE), &message);
                    }
                }
                backend::Event::NeedLogin(error) => {
                    self.connecting.set(false);
                    self.show(Screen::Login);
                    text(self.control(LOGIN_ERROR), &error);
                    EnableWindow(self.control(LOGIN), 1);
                    SetFocus(self.control(LOGIN));
                }
                backend::Event::Partial(books) => {
                    self.connecting.set(true);
                    {
                        let mut d = self.data.borrow_mut();
                        let existing: HashSet<_> = d.books.iter().map(|b| b.id.clone()).collect();
                        let mut rows = self.search_rows.borrow_mut();
                        let rows = Arc::make_mut(&mut rows);
                        for book in books {
                            if existing.contains(&book.id) {
                                continue;
                            }
                            rows.push(SearchRow {
                                search: book.search.clone(),
                                initials: book.initials.clone(),
                                format: book.format.index(),
                            });
                            d.totals[book.format.index()] += 1;
                            if book.downloaded {
                                d.downloaded[book.format.index()] += 1;
                            }
                            d.books.push(book);
                        }
                    }
                    *self.live_status.borrow_mut() = format!(
                        "구입한 책 {}권 · 조회 중",
                        num(self.data.borrow().books.len())
                    );
                    if self.screen.get() != Screen::Library {
                        self.show(Screen::Library);
                    }
                    self.filter();
                    InvalidateRect(self.control(CHART), null(), 1);
                }
                backend::Event::Ready(books) => {
                    self.connecting.set(false);
                    let rows: Arc<Vec<SearchRow>> = Arc::new(
                        books
                            .iter()
                            .map(|b| SearchRow {
                                search: b.search.clone(),
                                initials: b.initials.clone(),
                                format: b.format.index(),
                            })
                            .collect(),
                    );
                    *self.search_rows.borrow_mut() = rows;
                    {
                        let mut d = self.data.borrow_mut();
                        let selected: HashSet<_> =
                            d.selected.iter().map(|&i| d.books[i].id.clone()).collect();
                        d.selected = books
                            .iter()
                            .enumerate()
                            .filter(|(_, b)| selected.contains(&b.id))
                            .map(|(i, _)| i)
                            .collect();
                        d.totals = [0; 3];
                        d.downloaded = [0; 3];
                        for b in &books {
                            d.totals[b.format.index()] += 1;
                            if b.downloaded {
                                d.downloaded[b.format.index()] += 1;
                            }
                        }
                        d.books = books;
                    }
                    self.show(Screen::Library);
                    self.filter();
                    InvalidateRect(self.control(CHART), null(), 1);
                    EnableWindow(self.control(LOGIN), 1);
                }
                backend::Event::Current(position) => {
                    self.data.borrow_mut().downloads.index = position;
                    self.data.borrow_mut().downloads.percent = 0;
                    self.live_bytes.set(0);
                    self.live_total.set(None);
                }
                backend::Event::Progress {
                    position,
                    bytes,
                    total,
                } => {
                    let mut d = self.data.borrow_mut();
                    d.downloads.index = position;
                    d.downloads.percent = total
                        .filter(|&n| n > 0)
                        .map(|n| ((bytes as u128 * 100 / n as u128).min(100)) as u32)
                        .unwrap_or(0);
                    self.live_bytes.set(bytes);
                    self.live_total.set(total);
                }
                backend::Event::Status(status) => {
                    *self.live_status.borrow_mut() = status;
                }
                backend::Event::Done { position, book } => {
                    let mut d = self.data.borrow_mut();
                    if !d.books[book].downloaded {
                        let format = d.books[book].format.index();
                        d.downloaded[format] += 1;
                        d.available += 1;
                        d.books[book].downloaded = true;
                    }
                    d.downloads.index = position + 1;
                    d.downloads.percent = 0;
                    self.queue_dirty.set(true);
                }
                backend::Event::Stopped => {
                    self.data.borrow_mut().downloads.stop();
                    self.live_bytes.set(0);
                    self.live_total.set(None);
                    *self.live_status.borrow_mut() = "중단됨".into();
                }
                backend::Event::Complete => {
                    self.data.borrow_mut().downloads.phase = Phase::Complete;
                    *self.live_status.borrow_mut() = "다운로드 완료".into();
                }
                backend::Event::Error(error) => {
                    self.connecting.set(false);
                    MessageBoxW(
                        self.hwnd,
                        wide(&error).as_ptr(),
                        wide("ridibackup").as_ptr(),
                        MB_OK | MB_ICONERROR,
                    );
                    EnableWindow(self.control(LOGIN), 1);
                    self.refresh_library();
                }
            }
        }
        if self.screen.get() == Screen::Download {
            self.refresh_download();
        }
    }
    unsafe fn draw_primary(&self, item: &DRAWITEMSTRUCT) {
        let dc = item.hDC;
        let disabled = item.itemState & ODS_DISABLED != 0;
        let pressed = item.itemState & ODS_SELECTED != 0;
        let mut color = GetSysColor(if disabled {
            COLOR_BTNFACE
        } else {
            COLOR_HIGHLIGHT
        });
        if pressed && !disabled {
            color = ((color & 255) * 85 / 100)
                | ((((color >> 8) & 255) * 85 / 100) << 8)
                | ((((color >> 16) & 255) * 85 / 100) << 16);
        }
        let brush = CreateSolidBrush(color);
        FillRect(dc, &item.rcItem, brush);
        DeleteObject(brush);
        let old = SelectObject(dc, self.font.get());
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(
            dc,
            GetSysColor(if disabled {
                COLOR_GRAYTEXT
            } else {
                COLOR_HIGHLIGHTTEXT
            }),
        );
        let title = wide("다운로드 시작");
        let mut bounds = item.rcItem;
        DrawTextW(
            dc,
            title.as_ptr(),
            title.len() as i32 - 1,
            &mut bounds,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        if item.itemState & ODS_FOCUS != 0 {
            let mut focus = item.rcItem;
            InflateRect(&mut focus, -3, -3);
            DrawFocusRect(dc, &focus);
        }
        SelectObject(dc, old);
    }
    unsafe fn paint_chart(&self, h: HWND) {
        let mut ps: PAINTSTRUCT = zeroed();
        let dc = BeginPaint(h, &mut ps);
        let mut rc: RECT = zeroed();
        GetClientRect(h, &mut rc);
        FillRect(dc, &rc, GetSysColorBrush(COLOR_WINDOW));
        SelectObject(dc, self.font.get());
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, GetSysColor(COLOR_WINDOWTEXT));
        let scale = |v: i32| v * self.dpi.get() as i32 / 96;
        let (total, downloaded) = {
            let d = self.data.borrow();
            (d.totals, d.downloaded)
        };
        let sum: usize = total.iter().sum();
        let colors = [0xB56C17, 0x6D8339, 0x266495];
        let column = rc.right / 4;
        let radius = scale(44);
        let center_y = scale(77);
        let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
        let centered = |label: &str, col: i32, y: i32| {
            let t = wide(label);
            let mut bounds = RECT {
                left: col * column + scale(5),
                top: scale(y),
                right: (col + 1) * column - scale(5),
                bottom: scale(y + 22),
            };
            DrawTextW(
                dc,
                t.as_ptr(),
                t.len() as i32 - 1,
                &mut bounds,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        };
        for col in 0..4 {
            let cx = col * column + column / 2;
            centered(
                if col == 0 {
                    "파일 형식"
                } else {
                    ["ZIP 다운로드", "EPUB 다운로드", "PDF 다운로드"][col as usize - 1]
                },
                col,
                7,
            );
            if col == 0 {
                if sum == 0 {
                    draw_sector(
                        dc,
                        cx,
                        center_y,
                        radius,
                        0.0,
                        std::f64::consts::TAU,
                        GetSysColor(COLOR_BTNFACE),
                    );
                }
                let mut angle = -std::f64::consts::FRAC_PI_2;
                for i in 0..3 {
                    let sweep = std::f64::consts::TAU * total[i] as f64 / sum.max(1) as f64;
                    draw_sector(dc, cx, center_y, radius, angle, sweep, colors[i]);
                    angle += sweep;
                    let y = scale(130 + i as i32 * 18);
                    let brush = CreateSolidBrush(colors[i]);
                    FillRect(
                        dc,
                        &RECT {
                            left: cx - scale(53),
                            top: y + scale(6),
                            right: cx - scale(45),
                            bottom: y + scale(14),
                        },
                        brush,
                    );
                    DeleteObject(brush);
                    let t = wide(&format!(
                        "{}  {:.1}%",
                        ["ZIP", "EPUB", "PDF"][i],
                        total[i] as f64 * 100.0 / sum.max(1) as f64
                    ));
                    TextOutW(dc, cx - scale(36), y, t.as_ptr(), t.len() as i32 - 1);
                }
            } else {
                let i = col as usize - 1;
                let rate = if total[i] == 0 {
                    0.0
                } else {
                    downloaded[i] as f64 / total[i] as f64
                };
                draw_sector(
                    dc,
                    cx,
                    center_y,
                    radius,
                    0.0,
                    std::f64::consts::TAU,
                    GetSysColor(COLOR_BTNFACE),
                );
                draw_sector(
                    dc,
                    cx,
                    center_y,
                    radius,
                    -std::f64::consts::FRAC_PI_2,
                    std::f64::consts::TAU * rate,
                    colors[i],
                );
                centered(
                    &format!("{} / {}권", num(downloaded[i]), num(total[i])),
                    col,
                    131,
                );
            }
            draw_sector(
                dc,
                cx,
                center_y,
                scale(31),
                0.0,
                std::f64::consts::TAU,
                GetSysColor(COLOR_WINDOW),
            );
            if col == 0 {
                centered(&num(sum), col, 59);
                centered("권", col, 79);
            } else {
                let i = col as usize - 1;
                centered(
                    &format!(
                        "{:.1}%",
                        if total[i] == 0 {
                            0.0
                        } else {
                            downloaded[i] as f64 * 100.0 / total[i] as f64
                        }
                    ),
                    col,
                    66,
                );
            }
        }
        SelectObject(dc, old_pen);
        FrameRect(dc, &rc, GetSysColorBrush(COLOR_3DSHADOW));
        EndPaint(h, &ps);
    }
    unsafe fn smoke(&self) {
        match self.smoke_step.get() {
            0 => {
                self.show(Screen::Login);
                assert_eq!(read(self.control(LOGIN)), "다시 확인");
                assert!(self.control(302).is_null() && self.control(304).is_null());
                self.show(Screen::Library);
                assert_eq!(
                    SendMessageW(self.control(LIBRARY), LVM_GETITEMCOUNT, 0, 0),
                    245400
                );
                assert_eq!(
                    SendMessageW(self.control(PENDING), BM_GETCHECK, 0, 0),
                    BST_CHECKED as isize
                );
                self.data.borrow_mut().selected.insert(0);
                self.toggle(0);
                assert_eq!(self.data.borrow().selected.len(), 2);
                assert_eq!(
                    SendMessageW(self.control(ALL), BM_GETCHECK, 0, 0),
                    BST_INDETERMINATE as isize
                );
                assert!(IsWindowEnabled(self.control(START)) != 0);
                self.command(START, 0);
                assert_eq!(self.data.borrow().downloads.index, 1);
                SendMessageW(
                    self.control(SHOW_COMPLETED),
                    BM_SETCHECK,
                    BST_UNCHECKED as usize,
                    0,
                );
                self.command(SHOW_COMPLETED, 0);
                assert_eq!(SendMessageW(self.control(QUEUE), LVM_GETITEMCOUNT, 0, 0), 1);
                assert!(IsWindowEnabled(self.control(RETURN)) == 0);
                self.smoke_step.set(1);
            }
            1 => {
                if self.data.borrow().downloads.percent < 24 {
                    return;
                }
                assert!(read(self.control(BYTES)).contains('%'));
                self.command(STOP, 0);
                assert!(IsWindowEnabled(self.control(RETURN)) != 0);
                assert_eq!(self.data.borrow().downloads.percent, 0);
                text(self.control(GAP), "0.1");
                assert_eq!(self.data.borrow().downloads.gap_tenths, 1);
                self.command(RESTART, 0);
                assert!(IsWindowEnabled(self.control(RETURN)) == 0);
                self.smoke_step.set(2);
            }
            2 => {
                if self.data.borrow().downloads.phase != Phase::Complete {
                    return;
                }
                assert_eq!(self.data.borrow().available, 2);
                assert_eq!(SendMessageW(self.control(QUEUE), LVM_GETITEMCOUNT, 0, 0), 0);
                SendMessageW(
                    self.control(SHOW_COMPLETED),
                    BM_SETCHECK,
                    BST_CHECKED as usize,
                    0,
                );
                self.command(SHOW_COMPLETED, 0);
                assert_eq!(SendMessageW(self.control(QUEUE), LVM_GETITEMCOUNT, 0, 0), 2);
                assert!(IsWindowEnabled(self.control(RETURN)) != 0);
                self.command(RETURN, 0);
                assert_eq!(self.data.borrow().selected, HashSet::from([0, 3600]));
                text(self.control(SEARCH), "ㄷㄱㄹ");
                self.filter();
                self.smoke_step.set(3);
            }
            3 => {
                if self.data.borrow().searching {
                    return;
                }
                assert!(!self.data.borrow().filtered.is_empty());
                assert!(self
                    .data
                    .borrow()
                    .filtered
                    .iter()
                    .all(|&i| self.data.borrow().books[i].title.starts_with("달구름 우체국의 로봇")));
                self.command(ALL, 0);
                assert_eq!(
                    SendMessageW(self.control(ALL), BM_GETCHECK, 0, 0),
                    BST_CHECKED as isize
                );
                assert_eq!(
                    self.data.borrow().selected.len(),
                    self.data.borrow().filtered.len() + 2
                );
                self.command(ALL, 0);
                assert_eq!(self.data.borrow().selected, HashSet::from([0, 3600]));
                assert_eq!(
                    SendMessageW(self.control(ALL), BM_GETCHECK, 0, 0),
                    BST_UNCHECKED as isize
                );
                let path = std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("smoke.txt");
                std::fs::write(path,"PASS: native virtual lists (250,000 books), selection, Korean fuzzy filter, download progress, stop/restart, live interval, return lock, completion\n").unwrap();
                PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
            }
            _ => {}
        }
    }
}
unsafe fn draw_sector(
    dc: HDC,
    cx: i32,
    cy: i32,
    radius: i32,
    start: f64,
    sweep: f64,
    color: COLORREF,
) {
    if sweep <= 0.0 {
        return;
    }
    let steps = ((sweep / std::f64::consts::TAU * 180.0).ceil() as usize).max(1);
    let mut points = Vec::with_capacity(steps + 3);
    points.push(POINT { x: cx, y: cy });
    for step in 0..=steps {
        let angle = start + sweep * step as f64 / steps as f64;
        points.push(POINT {
            x: cx + (radius as f64 * angle.cos()).round() as i32,
            y: cy + (radius as f64 * angle.sin()).round() as i32,
        });
    }
    let brush = CreateSolidBrush(color);
    let old = SelectObject(dc, brush);
    Polygon(dc, points.as_ptr(), points.len() as i32);
    SelectObject(dc, old);
    DeleteObject(brush);
}
unsafe extern "system" fn header_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    parent: usize,
) -> LRESULT {
    if msg == WM_COMMAND && wp & 0xffff == ALL as usize {
        SendMessageW(parent as HWND, msg, wp, lp);
        return 0;
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}
unsafe extern "system" fn chart_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_PAINT {
        let parent = GetParent(hwnd);
        let ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *const App;
        if !ptr.is_null() {
            (*ptr).paint_chart(hwnd);
            return 0;
        }
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_CREATE {
        let args: Vec<_> = std::env::args().collect();
        let app = Box::new(App::new(hwnd, args.iter().any(|s| s == "--smoke")));
        let ptr = Box::into_raw(app);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
        (*ptr).init();
        return 0;
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let app = &*ptr;
    match msg {
        WM_SIZE => {
            app.layout();
            0
        }
        WM_COMMAND => {
            app.command((wp & 0xffff) as u16, ((wp >> 16) & 0xffff) as u16);
            0
        }
        WM_NOTIFY => app.notify(lp),
        WM_DRAWITEM => {
            let item = &*(lp as *const DRAWITEMSTRUCT);
            if item.CtlID == START as u32 {
                app.draw_primary(item);
                return 1;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_TIMER => {
            if wp == 2 {
                KillTimer(hwnd, 2);
                app.filter();
            } else {
                app.poll();
                app.tick();
                if app.smoke {
                    app.smoke();
                }
            }
            0
        }
        WM_DPICHANGED => {
            let rc = &*(lp as *const RECT);
            app.set_font((wp & 0xffff) as u32);
            SetWindowPos(
                hwnd,
                null_mut(),
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            0
        }
        WM_GETMINMAXINFO => {
            let mm = &mut *(lp as *mut MINMAXINFO);
            mm.ptMinTrackSize = POINT {
                x: (660 * app.dpi.get() / 96) as i32,
                y: (620 * app.dpi.get() / 96) as i32,
            };
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            KillTimer(hwnd, 1);
            KillTimer(hwnd, 2);
            PostQuitMessage(0);
            0
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let app = Box::from_raw(ptr as *mut App);
            DeleteObject(app.font.get());
            drop(app);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
fn main() {
    if std::env::args().any(|s| s == "--verify-download") {
        let value = match backend::verify_download() {
            Ok(value) => value,
            Err(error) => serde_json::json!({"error": error}),
        };
        let path = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join("download-check.json");
        let _ = std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap());
        return;
    }
    if std::env::args().any(|s| s == "--verify-api") {
        let value = match backend::verify() {
            Ok(value) => value,
            Err(error) => serde_json::json!({"error": error}),
        };
        let path = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join("api-check.json");
        let _ = std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap());
        return;
    }
    unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_PROGRESS_CLASS | ICC_UPDOWN_CLASS,
        });
        let instance = GetModuleHandleW(null());
        let class = wide("RidibackupWindow");
        let chart = wide("RidibackupChart");
        let mut wc: WNDCLASSW = zeroed();
        wc.hInstance = instance;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        wc.hbrBackground = GetSysColorBrush(COLOR_BTNFACE);
        wc.lpszClassName = class.as_ptr();
        wc.lpfnWndProc = Some(window_proc);
        RegisterClassW(&wc);
        wc.lpszClassName = chart.as_ptr();
        wc.lpfnWndProc = Some(chart_proc);
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class.as_ptr(),
            wide(
                if std::env::args().any(|s| s == "--demo" || s == "--smoke") {
                    "ridibackup — UI 미리보기"
                } else {
                    "ridibackup"
                },
            )
            .as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            680,
            700,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if msg.message == WM_KEYDOWN && msg.wParam == 0x74 {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App;
                if !ptr.is_null() {
                    let app = &*ptr;
                    if app.screen.get() == Screen::Library && !app.connecting.get() {
                        if let Some(live) = &app.live {
                            app.connecting.set(true);
                            live.cancel.store(false, Ordering::Relaxed);
                            let _ = live.commands.send(backend::Command::Refresh);
                        }
                    }
                }
                continue;
            }
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
