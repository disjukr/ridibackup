use std::collections::HashSet;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    Epub,
    Pdf,
}
impl Format {
    pub fn index(self) -> usize {
        match self {
            Self::Zip => 0,
            Self::Epub => 1,
            Self::Pdf => 2,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Zip => "ZIP",
            Self::Epub => "EPUB",
            Self::Pdf => "PDF",
        }
    }
}
pub struct Book {
    pub id: String,
    pub title: String,
    pub search: String,
    pub initials: String,
    pub format: Format,
    pub downloaded: bool,
}
impl Book {
    pub fn from_info(id: String, title: String, format: Format, downloaded: bool) -> Self {
        let search = normalize(&title);
        let initials = initials(&search);
        Self {
            id,
            title,
            search,
            initials,
            format,
            downloaded,
        }
    }
}
pub fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}
fn initials(s: &str) -> String {
    let heads: Vec<_> = "ㄱㄲㄴㄷㄸㄹㅁㅂㅃㅅㅆㅇㅈㅉㅊㅋㅌㅍㅎ".chars().collect();
    s.chars()
        .map(|c| {
            let n = c as u32;
            if (0xac00..=0xd7a3).contains(&n) {
                heads[((n - 0xac00) / 588) as usize]
            } else {
                c
            }
        })
        .collect()
}
pub fn fuzzy(q: &str, text: &str) -> bool {
    let mut it = q.chars();
    let mut next = it.next();
    for c in text.chars() {
        if next == Some(c) {
            next = it.next();
        }
        if next.is_none() {
            return true;
        }
    }
    next.is_none()
}
pub fn demo_books() -> Vec<Book> {
    let titles = [
        "달구름 우체국의 로봇",
        "별사탕 탐정단의 종이 행성",
        "유리숲을 달리는 고래",
        "시계꽃 기사와 잠든 등대",
        "구름 서랍을 정리하는 방법",
        "일곱 번째 화요일의 지도",
        "월간 종이별 관측소",
    ];
    (0..250_000)
        .map(|i| {
            let (format, n) = if i < 228_600 {
                (Format::Zip, i)
            } else if i < 249_150 {
                (Format::Epub, i - 228_600)
            } else {
                (Format::Pdf, i - 249_150)
            };
            let title = match format {
                Format::Zip => format!("{} {}권", titles[n % 4], n / 4 + 1),
                Format::Epub => format!("{} · {}", titles[4 + n % 2], n / 2 + 1),
                Format::Pdf => format!("{} {}호", titles[6], n + 1),
            };
            let search = normalize(&title);
            let heads = initials(&search);
            Book {
                id: (7000000000u64 + i as u64).to_string(),
                title,
                search,
                initials: heads,
                format,
                downloaded: n < [3600, 900, 100][format.index()],
            }
        })
        .collect()
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Idle,
    Running,
    Stopped,
    Complete,
}
pub struct Downloads {
    pub ids: Vec<usize>,
    pub index: usize,
    pub percent: u32,
    pub phase: Phase,
    pub gap_tenths: u32,
    pub waiting_since: Option<std::time::Instant>,
}
impl Default for Downloads {
    fn default() -> Self {
        Self {
            ids: vec![],
            index: 0,
            percent: 0,
            phase: Phase::Idle,
            gap_tenths: 15,
            waiting_since: None,
        }
    }
}
impl Downloads {
    pub fn start(&mut self, selected: &HashSet<usize>) {
        self.ids = selected.iter().copied().collect();
        self.ids.sort_unstable();
        self.index = 0;
        self.percent = 0;
        self.waiting_since = None;
        self.phase = if self.ids.is_empty() {
            Phase::Idle
        } else {
            Phase::Running
        };
    }
    pub fn stop(&mut self) {
        if self.phase == Phase::Running {
            self.phase = Phase::Stopped;
            self.percent = 0;
            self.waiting_since = None;
        }
    }
    pub fn restart(&mut self) {
        if self.phase == Phase::Stopped {
            self.phase = Phase::Running;
        }
    }
    pub fn can_return(&self) -> bool {
        matches!(self.phase, Phase::Stopped | Phase::Complete)
    }
    pub fn count(&self, books: &[Book]) -> usize {
        self.ids.iter().filter(|&&i| books[i].downloaded).count()
    }
    pub fn tick(&mut self, books: &mut [Book], now: std::time::Instant) {
        if self.phase != Phase::Running {
            return;
        }
        while self.index < self.ids.len() && books[self.ids[self.index]].downloaded {
            self.index += 1;
        }
        if self.index == self.ids.len() {
            self.phase = Phase::Complete;
            self.percent = 0;
            return;
        }
        if self
            .waiting_since
            .is_some_and(|t| now.duration_since(t).as_millis() < (self.gap_tenths as u128) * 100)
        {
            return;
        }
        self.waiting_since = None;
        self.percent = (self.percent + 8).min(100);
        if self.percent == 100 {
            let i = self.ids[self.index];
            books[i].downloaded = true;
            self.index += 1;
            self.percent = 0;
            self.waiting_since = Some(now);
            if self.index == self.ids.len() {
                self.phase = Phase::Complete;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fuzzy_korean() {
        assert!(fuzzy("ㄷㄱㄹ", &initials("달구름 우체국의 로봇")));
        assert!(fuzzy("달로봇", "달구름 우체국의 로봇"));
        assert!(!fuzzy("로봇달", "달구름 우체국의 로봇"));
    }
    #[test]
    fn stop_restart_and_skip() {
        let mut b = demo_books();
        let selected = HashSet::from([0, 3600, 3601]);
        let mut d = Downloads::default();
        let now = std::time::Instant::now();
        d.start(&selected);
        d.tick(&mut b, now);
        assert_eq!(d.index, 1);
        assert_eq!(d.count(&b), 1);
        assert_eq!(d.percent, 8);
        assert!(!d.can_return());
        d.stop();
        assert!(d.can_return());
        assert_eq!(d.percent, 0);
        d.restart();
        for _ in 0..13 {
            d.tick(&mut b, now);
        }
        assert_eq!(d.count(&b), 2);
        d.tick(&mut b, now);
        assert_eq!(d.percent, 0);
        d.gap_tenths = 0;
        d.tick(&mut b, now);
        assert_eq!(d.percent, 8);
    }
}
