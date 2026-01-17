#![windows_subsystem = "windows"]

use fst::{Set, SetBuilder, Streamer};
use iced::widget::{
    Column, button, column, container, progress_bar, row, scrollable, text, text_input,
};
use iced::window::icon;
use iced::{Alignment, Application, Command, Element, Length, Settings, Theme, executor, Size};
use jwalk::WalkDir;
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

// YOUR APP VERSION
const CURRENT_VERSION: &str = "1.0.0";
// REPLACE THIS WITH YOUR GITHUB USERNAME LATER
const UPDATE_URL: &str =
    "https://raw.githubusercontent.com/kcvabeysinghe/flash_find/main/version.json";

pub fn main() -> iced::Result {
    let icon = load_icon().ok();
    let settings = Settings {
        window: iced::window::Settings {
            icon,
            min_size: Some(Size::new(800.0, 600.0)),
            ..iced::window::Settings::default()
        },
        ..Settings::default()
    };
    FlashFind::run(settings)
}

fn load_icon() -> Result<icon::Icon, Box<dyn std::error::Error>> {
    let bytes = include_bytes!("icon.png");
    let img = image::load_from_memory(bytes)?.to_rgba8();
    let (width, height) = img.dimensions();
    let rgba = img.into_raw();
    Ok(icon::from_rgba(rgba, width, height)?)
}

#[derive(Debug, Clone)]
enum Message {
    SearchChanged(String),
    FileClicked(String),
    StartReindex,
    IndexProgress(f32, String),
    SearchFinished(Vec<String>, u128, usize),
    CacheLoaded(Arc<Vec<String>>),
    UpdateFound(String),
    OpenDownloadPage,
    Tick,
}

#[derive(serde::Deserialize)]
struct UpdateInfo {
    version: String,
    url: String,
}

#[derive(Debug, Eq, PartialEq)]
struct ScoredResult {
    path: String,
    score: i32,
    len: usize,
}

impl Ord for ScoredResult {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .cmp(&self.score)
            .then_with(|| other.len.cmp(&self.len))
    }
}

impl PartialOrd for ScoredResult {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

type IndexReceiver = Arc<Mutex<mpsc::Receiver<(f32, String)>>>;
type SearchReceiver = Arc<Mutex<mpsc::Receiver<(Vec<String>, u128, usize)>>>;
type CacheReceiver = Arc<Mutex<mpsc::Receiver<Arc<Vec<String>>>>>;

struct FlashFind {
    query: String,
    results: Vec<String>,
    search_cache: Option<Arc<Vec<String>>>,
    is_indexing: bool,
    progress: f32,
    status_msg: String,
    search_time_msg: String,
    new_version_url: Option<String>,

    index_receiver: Option<IndexReceiver>,
    search_sender: mpsc::Sender<(Vec<String>, u128, usize)>,
    search_receiver: SearchReceiver,
    cache_loader_sender: mpsc::Sender<Arc<Vec<String>>>,
    cache_loader_receiver: CacheReceiver,
    update_receiver: Arc<Mutex<mpsc::Receiver<String>>>,
    current_search_id: usize,
}

impl Application for FlashFind {
    type Message = Message;
    type Theme = Theme;
    type Executor = executor::Default;
    type Flags = ();

    fn new(_flags: ()) -> (Self, Command<Message>) {
        let (tx_search, rx_search) = mpsc::channel();
        let (tx_cache, rx_cache) = mpsc::channel();
        let (tx_update, rx_update) = mpsc::channel();

        let tx_cache_clone = tx_cache.clone();
        thread::spawn(move || {
            if let Ok(bytes) = fs::read("flash_index.fst")
                && let Ok(set) = Set::new(bytes)
            {
                let mut stream = set.stream();
                let mut cache = Vec::new();
                while let Some(key) = stream.next() {
                    if let Ok(s) = std::str::from_utf8(key) {
                        cache.push(s.to_string());
                    }
                }
                let _ = tx_cache_clone.send(Arc::new(cache));
            }
        });

        // BACKGROUND THREAD: Check for Updates
        thread::spawn(move || {
            if let Ok(resp) = ureq::get(UPDATE_URL).call() {
                // FIXED: Collapsed if block as suggested by Clippy
                if let Ok(info) = serde_json::from_reader::<_, UpdateInfo>(resp.into_reader())
                    && info.version != CURRENT_VERSION 
                {
                    let _ = tx_update.send(info.url);
                }
            }
        });

        (
            Self {
                query: String::new(),
                results: Vec::new(),
                search_cache: None,
                is_indexing: false,
                progress: 0.0,
                status_msg: "Loading Cache...".to_string(),
                search_time_msg: String::new(),
                new_version_url: None,

                index_receiver: None,
                search_sender: tx_search,
                search_receiver: Arc::new(Mutex::new(rx_search)),
                cache_loader_sender: tx_cache,
                cache_loader_receiver: Arc::new(Mutex::new(rx_cache)),
                update_receiver: Arc::new(Mutex::new(rx_update)),
                current_search_id: 0,
            },
            Command::none(),
        )
    }

    fn title(&self) -> String {
        String::from("Flash Find")
    }

    fn update(&mut self, message: Message) -> Command<Message> {
        match message {
            Message::UpdateFound(url) => {
                self.new_version_url = Some(url);
                Command::none()
            }
            Message::OpenDownloadPage => {
                if let Some(url) = &self.new_version_url {
                    let _ = open::that(url);
                }
                Command::none()
            }
            Message::CacheLoaded(cache) => {
                let count = cache.len();
                self.search_cache = Some(cache);
                self.status_msg = format!("Ready. {} files in RAM.", count);
                Command::none()
            }

            Message::SearchChanged(new_query) => {
                self.query = new_query;
                self.current_search_id += 1;
                let my_id = self.current_search_id;

                if self.query.is_empty() {
                    self.results.clear();
                    self.search_time_msg.clear();
                    return Command::none();
                }

                if let Some(cache_arc) = &self.search_cache {
                    let cache = cache_arc.clone();
                    let query = self.query.clone();
                    let tx = self.search_sender.clone();

                    thread::spawn(move || {
                        let start = Instant::now();
                        let query_lower = query.to_lowercase();

                        let top_docs: Vec<String> = cache
                            .par_iter()
                            .fold(
                                || BinaryHeap::with_capacity(51),
                                |mut heap: BinaryHeap<ScoredResult>, path| {
                                    let filename = match path.rfind('\\') {
                                        Some(idx) => &path[idx + 1..],
                                        None => path,
                                    };

                                    let mut score = 0;
                                    if filename == query_lower {
                                        score = 10000;
                                    } else if filename.starts_with(&query_lower) {
                                        score = 5000;
                                    } else if filename.contains(&query_lower) {
                                        score = 100;
                                    } else if path.contains(&query_lower) {
                                        score = 10;
                                    }

                                    if score > 0 {
                                        heap.push(ScoredResult {
                                            path: path.clone(),
                                            score,
                                            len: path.len(),
                                        });
                                        if heap.len() > 50 {
                                            heap.pop();
                                        }
                                    }
                                    heap
                                },
                            )
                            .reduce(BinaryHeap::new, |mut a, b| {
                                for item in b {
                                    a.push(item);
                                    if a.len() > 50 {
                                        a.pop();
                                    }
                                }
                                a
                            })
                            .into_sorted_vec()
                            .into_iter()
                            .rev()
                            .map(|r| r.path)
                            .collect();

                        let _ = tx.send((top_docs, start.elapsed().as_micros(), my_id));
                    });
                }
                Command::none()
            }

            Message::SearchFinished(results, micros, id) => {
                if id == self.current_search_id {
                    self.results = results;
                    if micros < 1000 {
                        self.search_time_msg = format!("Found in {}µs", micros);
                    } else {
                        self.search_time_msg = format!("Found in {:.2}ms", micros as f64 / 1000.0);
                    }
                }
                Command::none()
            }

            Message::FileClicked(path) => {
                let _ = open::that(path);
                Command::none()
            }

            Message::StartReindex => {
                self.is_indexing = true;
                self.progress = 0.0;
                self.status_msg = "Starting scan...".to_string();

                let (tx, rx) = mpsc::channel();
                self.index_receiver = Some(Arc::new(Mutex::new(rx)));

                thread::spawn(move || {
                    let mut all_paths: Vec<String> = Vec::new();
                    let _ = tx.send((0.0, "Phase 1: Discovering files...".to_string()));

                    for letter in b'C'..=b'Z' {
                        let drive_letter = char::from(letter);
                        let root_str = format!("{}:\\", drive_letter);
                        let root_path = PathBuf::from(&root_str);
                        if root_path.exists() {
                            for dir_entry in WalkDir::new(&root_path)
                                .skip_hidden(true)
                                .into_iter()
                                .flatten()
                            {
                                if let Some(path_str) = dir_entry.path().to_str() {
                                    all_paths.push(path_str.to_lowercase());
                                }
                            }
                        }
                    }

                    let total_files = all_paths.len();
                    let _ = tx.send((0.5, format!("Phase 2: Sorting {} files...", total_files)));
                    all_paths.par_sort();

                    let _ = tx.send((0.8, "Saving Index...".to_string()));
                    let mut build = SetBuilder::memory();
                    for path in &all_paths {
                        let _ = build.insert(path);
                    }
                    let bytes = build.into_inner().unwrap_or_default();
                    let _ = fs::write("flash_index.tmp", &bytes);
                    let _ = fs::rename("flash_index.tmp", "flash_index.fst");

                    let _ = tx.send((1.0, format!("Done. Indexed {} files.", total_files)));
                });
                Command::none()
            }

            Message::IndexProgress(prog, msg) => {
                self.progress = prog;
                self.status_msg = msg;
                if prog >= 1.0 {
                    self.is_indexing = false;
                    let tx_cache = self.cache_loader_sender.clone();
                    thread::spawn(move || {
                        if let Ok(bytes) = fs::read("flash_index.fst")
                            && let Ok(set) = Set::new(bytes)
                        {
                            let mut stream = set.stream();
                            let mut cache = Vec::new();
                            while let Some(key) = stream.next() {
                                if let Ok(s) = std::str::from_utf8(key) {
                                    cache.push(s.to_string());
                                }
                            }
                            let _ = tx_cache.send(Arc::new(cache));
                        }
                    });
                }
                Command::none()
            }

            Message::Tick => Command::none(),
        }
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        let index_sub = if self.is_indexing {
            if let Some(rx_arc) = &self.index_receiver {
                let rx_clone = rx_arc.clone();
                iced::time::every(Duration::from_millis(100)).map(move |_| {
                    if let Ok(rx) = rx_clone.try_lock()
                        && let Ok((prog, msg)) = rx.try_recv()
                    {
                        return Message::IndexProgress(prog, msg);
                    }
                    Message::Tick
                })
            } else {
                iced::Subscription::none()
            }
        } else {
            iced::Subscription::none()
        };

        let rx_clone = self.search_receiver.clone();
        let search_sub = iced::time::every(Duration::from_millis(5)).map(move |_| {
            if let Ok(rx) = rx_clone.try_lock()
                && let Ok((res, time, id)) = rx.try_recv()
            {
                return Message::SearchFinished(res, time, id);
            }
            Message::Tick
        });

        let rx_cache = self.cache_loader_receiver.clone();
        let cache_sub = iced::time::every(Duration::from_millis(200)).map(move |_| {
            if let Ok(rx) = rx_cache.try_lock()
                && let Ok(cache) = rx.try_recv()
            {
                return Message::CacheLoaded(cache);
            }
            Message::Tick
        });

        let rx_update = self.update_receiver.clone();
        let update_sub = iced::time::every(Duration::from_secs(10)).map(move |_| {
            if let Ok(rx) = rx_update.try_lock()
                && let Ok(url) = rx.try_recv()
            {
                return Message::UpdateFound(url);
            }
            Message::Tick
        });

        iced::Subscription::batch(vec![index_sub, search_sub, cache_sub, update_sub])
    }

    fn view(&self) -> Element<'_, Message> {
        let status_text = text(&self.status_msg).size(14);
        let progress_section = if self.is_indexing {
            column![
                status_text,
                progress_bar(0.0..=1.0, self.progress).height(Length::Fixed(10.0))
            ]
            .spacing(5)
        } else {
            column![status_text]
        };
        let time_display = text(&self.search_time_msg)
            .size(12)
            .style(iced::theme::Text::Color(iced::Color::from_rgb(
                0.5, 0.5, 0.5,
            )));

        let update_btn = button("Update Index")
            .on_press(Message::StartReindex)
            .padding(10);

        // --- NEW UPDATE BUTTON (Only shows if update is available) ---
        let mut header_row = row![text("Flash Find").size(30), update_btn]
            .spacing(20)
            .align_items(Alignment::Center);

        if self.new_version_url.is_some() {
            let new_ver_btn = button(text("✨ New Update!").size(14))
                .style(iced::theme::Button::Positive) // Green button
                .on_press(Message::OpenDownloadPage)
                .padding(10);
            header_row = header_row.push(new_ver_btn);
        }
        // -------------------------------------------------------------

        let search_input = text_input("Type to search...", &self.query)
            .on_input(Message::SearchChanged)
            .padding(15)
            .size(20);

        let mut results_col = Column::new().spacing(5);
        for path in &self.results {
            let btn = button(text(path).size(14))
                .on_press(Message::FileClicked(path.clone()))
                .style(iced::theme::Button::Secondary)
                .padding(8)
                .width(Length::Fill);
            results_col = results_col.push(btn);
        }

        let content = column![
            header_row,
            progress_section,
            time_display,
            search_input,
            scrollable(results_col)
        ]
        .spacing(15)
        .padding(20)
        .max_width(800);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x()
            .into()
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }
}