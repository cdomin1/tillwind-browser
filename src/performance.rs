use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    fs, io,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use gtk::{gio, glib, prelude::*};
use webkit6::{WebView, prelude::*};

const MESSAGE: &str = "tillwind-process";
const HISTORY: usize = 60;
const CPU_GRAPH_HEIGHT: i32 = 68;
const MEMORY_GRAPH_HEIGHT: i32 = 38;
const MIB: f64 = 1024.0 * 1024.0;

pub struct WebExtension(PathBuf);

impl WebExtension {
    pub fn install() -> io::Result<Self> {
        let directory = glib::mkdtemp(std::env::temp_dir().join("tillwind-web-XXXXXX"))
            .ok_or_else(|| io::Error::other("could not create extension directory"))?;
        let extension = Self(directory);
        fs::write(
            extension.0.join("tillwind-web-process.so"),
            include_bytes!(concat!(env!("OUT_DIR"), "/tillwind-web-process.so")),
        )?;
        Ok(extension)
    }

    pub fn configure(&self, context: &webkit6::WebContext) {
        if let Some(directory) = self.0.to_str() {
            context.add_path_to_sandbox(&self.0, true);
            context.set_web_process_extensions_directory(directory);
        }
    }
}

impl Drop for WebExtension {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0.join("tillwind-web-process.so"));
        let _ = fs::remove_dir(&self.0);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Renderer,
    Browser,
    Network,
    Graphics,
}

impl Role {
    fn title(self) -> &'static str {
        match self {
            Self::Renderer => "Page renderer",
            Self::Browser => "Tillwind",
            Self::Network => "Network",
            Self::Graphics => "GPU process",
        }
    }
}

struct Process {
    pid: u32,
    parent: u32,
    name: String,
    start: u64,
    ticks: u64,
    rss: u64,
    threads: u64,
}

impl Process {
    fn read(pid: u32) -> Option<Self> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, stat) = stat.split_once('(')?;
        let (name, stat) = stat.rsplit_once(") ")?;
        let fields: Vec<_> = stat.split_whitespace().collect();
        let field = |index: usize| fields.get(index)?.parse::<u64>().ok();
        Some(Self {
            pid,
            parent: u32::try_from(field(1)?).ok()?,
            name: name.to_owned(),
            ticks: field(11)?.saturating_add(field(12)?),
            start: field(19)?,
            rss: field(21)?,
            threads: field(17)?,
        })
    }

    fn namespace_pid(&self) -> Option<u32> {
        let status = fs::read_to_string(format!("/proc/{}/status", self.pid)).ok()?;
        status.lines().find_map(|line| {
            line.strip_prefix("NSpid:")?
                .split_whitespace()
                .last()?
                .parse()
                .ok()
        })
    }

    fn role(&self, browser: u32) -> Option<Role> {
        if self.pid == browser {
            return Some(Role::Browser);
        }
        match self.name.as_str() {
            "WebKitWebProces" | "WebKitWebProcess" => Some(Role::Renderer),
            "WebKitNetworkPr" | "WebKitNetworkProcess" => Some(Role::Network),
            "WebKitGPUProces" | "WebKitGPUProcess" => Some(Role::Graphics),
            _ => None,
        }
    }
}

struct Usage {
    process: Process,
    role: Role,
    cpu: Option<f64>,
    rss: f64,
}

#[derive(Default)]
struct Sampler {
    previous: HashMap<(u32, u64), u64>,
    time: Option<Instant>,
}

impl Sampler {
    fn sample(&mut self) -> Vec<Usage> {
        let browser = std::process::id();
        let mut processes = HashMap::new();
        let mut children = HashMap::<u32, Vec<u32>>::new();
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            if let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
                && let Some(process) = Process::read(pid)
            {
                children.entry(process.parent).or_default().push(pid);
                processes.insert(pid, process);
            }
        }

        // sysconf only queries process-wide constants and does not write through pointers.
        let (ticks_per_second, page_size) = unsafe {
            (
                libc::sysconf(libc::_SC_CLK_TCK),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        let now = Instant::now();
        let elapsed = self
            .time
            .replace(now)
            .map(|time| (now - time).as_secs_f64());
        let mut next = HashMap::new();
        let mut usage = Vec::new();
        let mut pending = vec![browser];
        while let Some(pid) = pending.pop() {
            if let Some(children) = children.remove(&pid) {
                pending.extend(children);
            }
            if let Some(process) = processes.remove(&pid)
                && let Some(role) = process.role(browser)
            {
                let key = (pid, process.start);
                let cpu = self
                    .previous
                    .get(&key)
                    .zip(elapsed)
                    .and_then(|(&before, elapsed)| {
                        let seconds = elapsed * ticks_per_second as f64;
                        (seconds > 0.0)
                            .then(|| 100.0 * process.ticks.saturating_sub(before) as f64 / seconds)
                    });
                next.insert(key, process.ticks);
                let rss = process.rss as f64 * page_size.max(0) as f64 / MIB;
                usage.push(Usage {
                    process,
                    role,
                    cpu,
                    rss,
                });
            }
        }
        self.previous = next;
        usage.sort_by_key(|row| row.process.pid);
        usage
    }
}

fn renderer_pid(rows: &[Usage], identity: (u32, u64)) -> Option<u32> {
    let (local_pid, start) = identity;
    if start == 0 {
        return None;
    }
    let mut matches = rows.iter().filter(|row| {
        row.role == Role::Renderer
            && row.process.start == start
            && row.process.namespace_pid() == Some(local_pid)
    });
    let pid = matches.next()?.process.pid;
    matches.next().is_none().then_some(pid)
}

fn label(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class(class);
    label
}

fn panel_frame(title: &str, class: &str, children: &[&impl IsA<gtk::Widget>]) -> gtk::Frame {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 5);
    body.add_css_class("performance-frame-body");
    for child in children {
        body.append(*child);
    }
    let frame = gtk::Frame::new(Some(title));
    frame.add_css_class("performance-frame");
    frame.add_css_class(class);
    frame.set_child(Some(&body));
    frame
}

fn chart_scale(values: &VecDeque<f64>, minimum: f64) -> f64 {
    values.iter().copied().fold(minimum, f64::max)
}

fn history_graph(
    history: &Rc<RefCell<History>>,
    samples: fn(&History) -> &VecDeque<f64>,
    minimum: f64,
    color: (f64, f64, f64),
    height: i32,
) -> gtk::DrawingArea {
    let graph = gtk::DrawingArea::builder().content_height(height).build();
    let history = history.clone();
    graph.set_draw_func(move |_, context, width, height| {
        let width = f64::from(width);
        let height = f64::from(height);
        let history = history.borrow();
        let values = samples(&history);
        let scale = chart_scale(values, minimum);
        let column = width / HISTORY as f64;
        let levels = (height / 4.0) as usize;
        if levels == 0 || column <= 0.0 {
            return;
        }

        // Keep the grid visible even before the first sample arrives.
        context.set_source_rgba(color.0, color.1, color.2, 0.23);
        for guide in [0.0, 0.5, 1.0] {
            let y = 1.0 + (height - 3.0) * guide;
            for index in 0..HISTORY {
                context.rectangle((index as f64 + 0.5) * column, y, 1.0, 1.0);
            }
        }
        let _ = context.fill();

        context.set_source_rgb(color.0, color.1, color.2);
        let first = HISTORY - values.len();
        for (index, value) in values.iter().enumerate() {
            let dots = (value / scale * levels as f64).ceil() as usize;
            let x = ((first + index) as f64 + 0.5) * column;
            for dot in 0..dots {
                context.rectangle(x, height - 3.0 - 4.0 * dot as f64, 1.5, 1.5);
            }
        }
        let _ = context.fill();
    });
    graph
}

#[derive(Default)]
struct History {
    cpu: VecDeque<f64>,
    rss: VecDeque<f64>,
}

impl History {
    fn record(&mut self, usage: &Usage) {
        for (history, value) in [(&mut self.cpu, usage.cpu), (&mut self.rss, Some(usage.rss))] {
            if let Some(value) = value {
                if history.len() == HISTORY {
                    history.pop_front();
                }
                history.push_back(value);
            }
        }
    }
}

const VIDEO_STATS: &str = r#"
(() => {
    const videos = Array.from(document.querySelectorAll('video'));
    const video = videos.find(video => !video.paused && video.videoWidth) ??
                  videos.find(video => video.videoWidth);
    if (!video) return null;

    const quality = video.getVideoPlaybackQuality();
    let buffer = 0;
    for (let index = 0; index < video.buffered.length; index++) {
        if (video.buffered.start(index) <= video.currentTime &&
            video.buffered.end(index) >= video.currentTime) {
            buffer = video.buffered.end(index) - video.currentTime;
            break;
        }
    }
    return JSON.stringify({
        width: video.videoWidth,
        height: video.videoHeight,
        dropped: quality.droppedVideoFrames,
        frames: quality.totalVideoFrames,
        buffer
    });
})()
"#;

#[derive(serde::Deserialize)]
struct VideoStats {
    width: u32,
    height: u32,
    dropped: u64,
    frames: u64,
    buffer: f64,
}

pub struct Performance {
    pub widget: gtk::ScrolledWindow,
    site: gtk::Label,
    cpu: gtk::Label,
    memory: gtk::Label,
    cpu_graph: gtk::DrawingArea,
    memory_graph: gtk::DrawingArea,
    processes: gtk::Label,
    video: gtk::Frame,
    video_text: gtk::Label,
    sampler: RefCell<Sampler>,
    history: Rc<RefCell<History>>,
    page: Cell<u64>,
    renderer: Cell<Option<u32>>,
    identity: Cell<Option<(u32, u64)>>,
    request: RefCell<Option<(gio::Cancellable, Instant)>>,
}

impl Performance {
    pub fn new() -> Rc<Self> {
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 6);
        panel.add_css_class("performance-panel");
        let heading = label("Performance", "performance-heading");
        let site = label("New Tab", "performance-site");
        site.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let cpu = label("—", "performance-value");
        let memory = label("—", "performance-value");
        let history = Rc::new(RefCell::new(History::default()));
        let cpu_graph = history_graph(
            &history,
            |history| &history.cpu,
            100.0,
            (0.45, 0.81, 0.72),
            CPU_GRAPH_HEIGHT,
        );
        let memory_graph = history_graph(
            &history,
            |history| &history.rss,
            64.0,
            (0.76, 0.83, 0.55),
            MEMORY_GRAPH_HEIGHT,
        );
        let cpu_contents = gtk::Box::new(gtk::Orientation::Vertical, 5);
        cpu_contents.append(&cpu);
        cpu_contents.append(&cpu_graph);
        let memory_contents = gtk::Box::new(gtk::Orientation::Vertical, 5);
        memory_contents.append(&memory);
        memory_contents.append(&memory_graph);
        let processes = label("", "performance-table");
        let note = label(
            "CPU · 100% = one core. RSS includes shared memory. WebKit can share a renderer between pages.",
            "performance-note",
        );
        note.set_wrap(true);
        note.set_max_width_chars(35);
        panel.append(&heading);
        panel.append(&site);
        panel.append(&panel_frame("cpu", "performance-cpu", &[&cpu_contents]));
        panel.append(&panel_frame(
            "mem",
            "performance-memory",
            &[&memory_contents],
        ));
        let video_text = label("", "performance-value");
        let video = panel_frame("video", "performance-video", &[&video_text]);
        video.set_visible(false);
        panel.append(&video);
        panel.append(&panel_frame(
            "proc",
            "performance-processes",
            &[&processes, &note],
        ));
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .width_request(340)
            .halign(gtk::Align::End)
            .valign(gtk::Align::Fill)
            .css_classes(["performance-overlay"])
            .child(&panel)
            .visible(false)
            .build();
        Rc::new(Self {
            widget,
            site,
            cpu,
            memory,
            cpu_graph,
            memory_graph,
            processes,
            video,
            video_text,
            sampler: Default::default(),
            history,
            page: Cell::new(0),
            renderer: Cell::new(None),
            identity: Cell::new(None),
            request: Default::default(),
        })
    }

    pub fn attach(&self, page: &gtk::Overlay) {
        if let Some(parent) = self.widget.parent().and_downcast::<gtk::Overlay>() {
            if parent == *page {
                return;
            }
            parent.remove_overlay(&self.widget);
        }
        page.add_overlay(&self.widget);
        page.set_measure_overlay(&self.widget, false);
    }

    pub fn reset(&self) {
        if let Some((request, _)) = self.request.take() {
            request.cancel();
        }
        self.page.set(0);
        self.renderer.set(None);
        self.identity.set(None);
        self.video.set_visible(false);
        self.resize_graphs(false);
        self.sampler.replace(Sampler::default());
        self.history.replace(History::default());
        self.render(&[], None);
    }

    pub fn refresh(self: &Rc<Self>, view: &WebView, site: &str) {
        let page = view.page_id();
        if self.page.get() != page || self.site.text() != site {
            self.reset();
            self.page.set(page);
            self.site.set_text(site);
        }
        let rows = self.sampler.borrow_mut().sample();
        let renderer = self
            .identity
            .get()
            .and_then(|identity| renderer_pid(&rows, identity));
        self.render(&rows, renderer);
        if let Some((request, started)) = self.request.take() {
            if started.elapsed() < Duration::from_secs(3) {
                self.request.replace(Some((request, started)));
                return;
            }
            request.cancel();
        }
        let request = gio::Cancellable::new();
        self.request
            .replace(Some((request.clone(), Instant::now())));
        let current_request = request.clone();
        let weak = Rc::downgrade(self);
        let message = webkit6::UserMessage::new(MESSAGE, None);
        let active_view = view.downgrade();
        view.send_message_to_page(&message, Some(&request), move |reply| {
            if current_request.is_cancelled() {
                return;
            }
            if let Some(panel) = weak.upgrade() {
                let identity = reply
                    .ok()
                    .filter(|reply| reply.name().as_deref() == Some(MESSAGE))
                    .and_then(|reply| reply.parameters())
                    .and_then(|identity| identity.get::<(u32, u64)>());
                panel.identity.set(identity);
                if let Some(view) = active_view.upgrade() {
                    panel.refresh_video(&view, &current_request);
                } else {
                    panel.request.replace(None);
                }
            }
        });
    }

    fn refresh_video(self: &Rc<Self>, view: &WebView, request: &gio::Cancellable) {
        let weak = Rc::downgrade(self);
        let current_request = request.clone();
        view.evaluate_javascript(
            VIDEO_STATS,
            Some("tillwind-performance"),
            None,
            Some(request),
            move |result| {
                if current_request.is_cancelled() {
                    return;
                }
                if let Some(panel) = weak.upgrade() {
                    panel.request.replace(None);
                    let stats = result
                        .ok()
                        .and_then(|value| serde_json::from_str::<VideoStats>(&value.to_str()).ok());
                    panel.video.set_visible(stats.is_some());
                    panel.resize_graphs(stats.is_some());
                    if let Some(stats) = stats {
                        panel.video_text.set_text(&format!(
                            "{} × {}    buffer {:.1}s\nFrames {} · dropped {}",
                            stats.width, stats.height, stats.buffer, stats.frames, stats.dropped
                        ));
                    }
                }
            },
        );
    }

    fn resize_graphs(&self, video_visible: bool) {
        let (cpu, memory) = if video_visible && self.widget.height() < 490 {
            (46, 24)
        } else {
            (CPU_GRAPH_HEIGHT, MEMORY_GRAPH_HEIGHT)
        };
        self.cpu_graph.set_content_height(cpu);
        self.memory_graph.set_content_height(memory);
    }

    fn render(&self, rows: &[Usage], renderer: Option<u32>) {
        if self.renderer.replace(renderer) != renderer {
            self.history.replace(History::default());
        }
        let current = rows.iter().find(|row| Some(row.process.pid) == renderer);
        let mut history = self.history.borrow_mut();
        if let Some(usage) = current {
            history.record(usage);
        }
        let cpu = current.and_then(|row| row.cpu);
        let rss = current.map(|row| row.rss);
        self.cpu
            .set_text(&cpu.map_or("—".into(), |cpu| format!("{cpu:.1}%   renderer")));
        self.memory
            .set_text(&rss.map_or("—".into(), |rss| format!("{rss:.1} MiB   resident")));
        let cpu_scale = chart_scale(&history.cpu, 100.0);
        let rss_scale = chart_scale(&history.rss, 64.0);
        self.cpu_graph.queue_draw();
        self.memory_graph.queue_draw();
        self.cpu_graph
            .set_tooltip_text(Some(&format!("CPU history · 0–{cpu_scale:.0}%")));
        self.memory_graph
            .set_tooltip_text(Some(&format!("RSS history · 0–{rss_scale:.0} MiB")));

        let mut table = String::from("PROCESS        PID   CPU%   MiB  THR\n");
        if let Some(row) = current {
            append_row(&mut table, row);
        } else {
            table.push_str("Page renderer unavailable\n");
        }
        table.push_str("SHARED\n");
        for row in rows.iter().filter(|row| row.role != Role::Renderer) {
            append_row(&mut table, row);
        }
        self.processes.set_text(table.trim_end());
    }
}

fn append_row(table: &mut String, row: &Usage) {
    let cpu = row.cpu.map_or("—".into(), |cpu| format!("{cpu:.1}"));
    table.push_str(&format!(
        "{:<13} {:>6} {:>6} {:>5.0} {:>4}\n",
        row.role.title(),
        row.process.pid,
        cpu,
        row.rss,
        row.process.threads
    ));
}
