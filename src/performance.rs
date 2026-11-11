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
const HISTORY: usize = 36;
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

fn history_graph(values: &VecDeque<f64>, scale: f64) -> String {
    const LEVELS: [char; 9] = ['·', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let mut graph = "·".repeat(HISTORY.saturating_sub(values.len()));
    graph.extend(values.iter().map(|value| {
        let level = (8.0 * value / scale).ceil().clamp(0.0, 8.0) as usize;
        LEVELS[level]
    }));
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

pub struct Performance {
    pub widget: gtk::ScrolledWindow,
    site: gtk::Label,
    cpu: gtk::Label,
    memory: gtk::Label,
    cpu_graph: gtk::Label,
    memory_graph: gtk::Label,
    processes: gtk::Label,
    sampler: RefCell<Sampler>,
    history: RefCell<History>,
    page: Cell<u64>,
    renderer: Cell<Option<u32>>,
    identity: Cell<Option<(u32, u64)>>,
    request: RefCell<Option<(gio::Cancellable, Instant)>>,
}

impl Performance {
    pub fn new() -> Rc<Self> {
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 9);
        panel.add_css_class("performance-panel");
        let heading = label("Performance", "performance-heading");
        let site = label("New Tab", "performance-site");
        site.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        let cpu = label("CPU  —", "performance-cpu");
        let memory = label("MEM  —", "performance-memory");
        let cpu_graph = label(&"·".repeat(HISTORY), "performance-cpu");
        let memory_graph = label(&"·".repeat(HISTORY), "performance-memory");
        let processes = label("", "performance-table");
        let note = label(
            "CPU is relative to one core. RSS includes shared memory. A renderer may host multiple pages. Network and GPU processes are browser-wide.",
            "performance-note",
        );
        note.set_wrap(true);
        note.set_max_width_chars(37);
        for child in [
            &heading,
            &site,
            &cpu,
            &cpu_graph,
            &memory,
            &memory_graph,
            &processes,
            &note,
        ] {
            panel.append(child);
        }
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .width_request(340)
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
            sampler: Default::default(),
            history: Default::default(),
            page: Cell::new(0),
            renderer: Cell::new(None),
            identity: Cell::new(None),
            request: Default::default(),
        })
    }

    pub fn reset(&self) {
        if let Some((request, _)) = self.request.take() {
            request.cancel();
        }
        self.page.set(0);
        self.renderer.set(None);
        self.identity.set(None);
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
        view.send_message_to_page(&message, Some(&request), move |reply| {
            if current_request.is_cancelled() {
                return;
            }
            if let Some(panel) = weak.upgrade() {
                panel.request.replace(None);
                let identity = reply
                    .ok()
                    .filter(|reply| reply.name().as_deref() == Some(MESSAGE))
                    .and_then(|reply| reply.parameters())
                    .and_then(|identity| identity.get::<(u32, u64)>());
                panel.identity.set(identity);
            }
        });
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
            .set_text(&cpu.map_or("CPU  —".into(), |cpu| format!("CPU  {cpu:.1}%")));
        self.memory
            .set_text(&rss.map_or("MEM  —".into(), |rss| format!("MEM  {rss:.1} MiB RSS")));
        let cpu_scale = history.cpu.iter().copied().fold(100.0, f64::max);
        let rss_scale = history.rss.iter().copied().fold(64.0, f64::max);
        self.cpu_graph
            .set_text(&history_graph(&history.cpu, cpu_scale));
        self.memory_graph
            .set_text(&history_graph(&history.rss, rss_scale));
        self.cpu_graph
            .set_tooltip_text(Some(&format!("CPU chart range: 0–{cpu_scale:.0}%")));
        self.memory_graph
            .set_tooltip_text(Some(&format!("RSS chart range: 0–{rss_scale:.0} MiB")));

        let mut table = String::from("PROCESS          PID    CPU%   MiB\n");
        if let Some(row) = current {
            append_row(&mut table, row);
        } else {
            table.push_str("Page renderer unavailable\n");
        }
        table.push_str("\nSHARED\n");
        for row in rows.iter().filter(|row| row.role != Role::Renderer) {
            append_row(&mut table, row);
        }
        self.processes.set_text(table.trim_end());
    }
}

fn append_row(table: &mut String, row: &Usage) {
    let cpu = row.cpu.map_or("—".into(), |cpu| format!("{cpu:.1}"));
    table.push_str(&format!(
        "{:<13} {:>6} {:>7} {:>5.0}\n",
        row.role.title(),
        row.process.pid,
        cpu,
        row.rss
    ));
}
