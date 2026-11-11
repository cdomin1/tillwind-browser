use crate::nvml;

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Tab,
    Shared,
}

impl Scope {
    fn title(self) -> &'static str {
        match self {
            Self::Tab => "Tab renderer",
            Self::Shared => "Shared browser",
        }
    }
}

#[derive(Clone, Copy)]
enum Counter {
    Time(u64),
    Cycles { busy: u64, total: u64 },
}

impl Counter {
    fn utilization(self, previous: Self, elapsed: Duration, capacity: u64) -> Option<f64> {
        let (busy, total) = match (self, previous) {
            (Self::Time(now), Self::Time(before)) => {
                (now.checked_sub(before)? as f64, elapsed.as_nanos() as f64)
            }
            (
                Self::Cycles { busy, total },
                Self::Cycles {
                    busy: before_busy,
                    total: before_total,
                },
            ) => (
                busy.checked_sub(before_busy)? as f64,
                total.checked_sub(before_total)? as f64,
            ),
            _ => return None,
        };
        (total > 0.0).then(|| 100.0 * busy / total / capacity as f64)
    }
}

#[derive(PartialEq, Eq, Hash)]
struct ClientId {
    device: String,
    id: u64,
}

struct Engine {
    counter: Counter,
    capacity: u64,
    sampled_at: Option<Instant>,
}

struct Client {
    scope: Scope,
    engines: HashMap<String, Engine>,
}

fn fdinfo(text: &str, scope: Scope) -> Option<(ClientId, Client)> {
    if !text.contains("drm-client-id:") {
        return None;
    }
    let fields: HashMap<_, _> = text
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key, value.trim()))
        .collect();
    let driver = fields.get("drm-driver")?;
    let id = fields.get("drm-client-id")?.parse().ok()?;
    let device = fields.get("drm-pdev").unwrap_or(driver).to_string();
    let capacity = |name: &str| {
        fields
            .get(format!("drm-engine-capacity-{name}").as_str())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(1)
            .max(1)
    };
    let mut engines = HashMap::new();
    for (key, value) in &fields {
        if let Some(name) = key.strip_prefix("drm-engine-")
            && let Some(value) = value.strip_suffix(" ns")
            && let Ok(value) = value.parse()
        {
            engines.insert(
                name.to_owned(),
                Engine {
                    counter: Counter::Time(value),
                    capacity: capacity(name),
                    sampled_at: None,
                },
            );
        }
    }
    for (key, value) in &fields {
        if let Some(name) = key.strip_prefix("drm-cycles-")
            && !engines.contains_key(name)
            && let (Ok(busy), Some(total)) = (
                value.parse(),
                fields
                    .get(format!("drm-total-cycles-{name}").as_str())
                    .and_then(|value| value.parse().ok()),
            )
        {
            engines.insert(
                name.to_owned(),
                Engine {
                    counter: Counter::Cycles { busy, total },
                    capacity: capacity(name),
                    sampled_at: None,
                },
            );
        }
    }
    (!engines.is_empty()).then_some((ClientId { device, id }, Client { scope, engines }))
}

#[derive(Default)]
pub struct Activity {
    engines: BTreeMap<(Scope, String, String), f64>,
    has_counters: bool,
    note: Option<&'static str>,
}

impl Activity {
    pub fn summary(&self) -> String {
        if !self.has_counters {
            return "Engine counters unavailable".into();
        }
        if self.engines.is_empty() {
            return self.note.unwrap_or("Reading engine counters…").into();
        }
        let peak = |scope| {
            self.engines
                .iter()
                .filter(|((source, _, _), _)| *source == scope)
                .map(|(_, value)| *value)
                .reduce(f64::max)
                .map_or("—".into(), |value| format!("{value:.1}%"))
        };
        format!(
            "Tab {}  ·  Shared {}",
            peak(Scope::Tab),
            peak(Scope::Shared)
        )
    }

    fn add_nvidia(&mut self, samples: Vec<nvml::Sample>, processes: &HashMap<u32, Scope>) {
        let mut latest = HashMap::<(u32, String), nvml::Sample>::new();
        for sample in samples {
            if processes.contains_key(&sample.pid) {
                latest
                    .entry((sample.pid, sample.device.clone()))
                    .and_modify(|current| {
                        if sample.timestamp > current.timestamp {
                            *current = sample.clone();
                        }
                    })
                    .or_insert(sample);
            }
        }
        self.has_counters |= !latest.is_empty();
        for ((pid, _), sample) in latest {
            let scope = processes[&pid];
            for (engine, value) in [
                ("compute/3D", sample.compute),
                ("encode", sample.encode),
                ("decode", sample.decode),
            ] {
                if value <= 100 {
                    *self
                        .engines
                        .entry((scope, sample.device.clone(), engine.into()))
                        .or_default() += f64::from(value);
                }
            }
        }
        for value in self.engines.values_mut() {
            *value = value.min(100.0);
        }
    }

    pub fn detail(&self) -> String {
        let mut detail = String::from(
            "Peak engine activity by scope. Shared browser activity can include other tabs. A dash means counters are unavailable for that scope.",
        );
        for ((scope, device, engine), value) in &self.engines {
            detail.push_str(&format!(
                "\n{} · {device} · {engine}: {value:.1}%",
                scope.title()
            ));
        }
        detail
    }
}

#[derive(Default)]
pub struct Sampler {
    previous: HashMap<ClientId, Client>,
    nvml: Option<nvml::Nvml>,
    nvml_checked: bool,
}

impl Sampler {
    pub fn sample(&mut self, processes: impl Iterator<Item = (u32, Scope)>) -> Activity {
        let processes: HashMap<_, _> = processes.collect();
        let mut clients = HashMap::<ClientId, Client>::new();
        for (&pid, &scope) in &processes {
            for entry in fs::read_dir(format!("/proc/{pid}/fdinfo"))
                .into_iter()
                .flatten()
                .flatten()
            {
                let Ok(text) = fs::read_to_string(entry.path()) else {
                    continue;
                };
                if let Some((id, client)) = fdinfo(&text, scope) {
                    // Duplicated descriptors describe the same kernel client. If a client
                    // is shared between the renderer and browser, charge it to shared.
                    clients
                        .entry(id)
                        .and_modify(|current| {
                            if scope == Scope::Shared {
                                current.scope = Scope::Shared;
                            }
                        })
                        .or_insert(client);
                }
            }
        }
        let mut activity = self.update(clients, Instant::now());
        if !self.nvml_checked {
            self.nvml = nvml::Nvml::load();
            self.nvml_checked = true;
        }
        if let Some(nvml) = self.nvml.as_mut() {
            let readings = nvml.read();
            if readings.available {
                activity.has_counters = true;
                activity.note = Some("No current NVIDIA process samples");
            }
            activity.add_nvidia(readings.samples, &processes);
        }
        activity
    }

    fn update(&mut self, mut clients: HashMap<ClientId, Client>, now: Instant) -> Activity {
        let mut activity = Activity {
            has_counters: !clients.is_empty(),
            ..Default::default()
        };
        for (id, client) in &mut clients {
            for (name, engine) in &mut client.engines {
                engine.sampled_at = Some(now);
                let Some(previous) = self
                    .previous
                    .get(id)
                    .and_then(|client| client.engines.get(name))
                else {
                    continue;
                };
                let elapsed = now - previous.sampled_at.unwrap_or(now);
                if let Some(value) =
                    engine
                        .counter
                        .utilization(previous.counter, elapsed, engine.capacity)
                {
                    let key = (client.scope, id.device.clone(), name.clone());
                    *activity.engines.entry(key).or_default() += value;
                } else if std::mem::discriminant(&engine.counter)
                    == std::mem::discriminant(&previous.counter)
                {
                    // DRM may briefly report a lower counter before catching up.
                    engine.counter = previous.counter;
                    engine.sampled_at = previous.sampled_at;
                }
            }
        }
        for value in activity.engines.values_mut() {
            *value = value.min(100.0);
        }
        self.previous = clients;
        activity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i915_engine_time_reports_activity_and_idle() {
        let record = |time| {
            let text = format!(
                "drm-driver: i915\ndrm-pdev: 0000:00:02.0\ndrm-client-id: 7\n\
                 drm-engine-render: {time} ns\ndrm-engine-capacity-render: 2\n"
            );
            [fdinfo(&text, Scope::Tab).unwrap()].into()
        };
        let mut sampler = Sampler::default();
        let now = Instant::now();
        assert_eq!(
            sampler.update(record(0), now).summary(),
            "Reading engine counters…"
        );
        let now = now + Duration::from_secs(1);
        assert_eq!(
            sampler.update(record(500_000_000), now).summary(),
            "Tab 25.0%  ·  Shared —"
        );
        assert_eq!(
            sampler
                .update(record(500_000_000), now + Duration::from_secs(1))
                .summary(),
            "Tab 0.0%  ·  Shared —"
        );
    }

    #[test]
    fn amd_video_engine_preserves_the_sampling_window_when_counters_regress() {
        let record = |time| {
            let text = format!(
                "drm-driver: amdgpu\ndrm-pdev: 0000:04:00.0\ndrm-client-id: 12\ndrm-engine-dec: {time} ns\n"
            );
            [fdinfo(&text, Scope::Tab).unwrap()].into()
        };
        let now = Instant::now();
        let mut sampler = Sampler::default();
        sampler.update(record(100_000_000), now);
        assert!(
            sampler
                .update(record(80_000_000), now + Duration::from_secs(1))
                .engines
                .is_empty()
        );
        let reading = sampler.update(record(500_000_000), now + Duration::from_secs(2));
        assert_eq!(reading.summary(), "Tab 20.0%  ·  Shared —");
    }

    #[test]
    fn hybrid_gpu_keeps_nvidia_samples_separate_and_filters_unrelated_processes() {
        let mut activity = Activity {
            has_counters: true,
            engines: [((Scope::Tab, "Intel".into(), "render".into()), 11.0)].into(),
            ..Default::default()
        };
        let sample = |pid, timestamp, compute| nvml::Sample {
            pid,
            timestamp,
            compute,
            device: "NVIDIA · NVML 0".into(),
            encode: 0,
            decode: 0,
        };
        activity.add_nvidia(
            vec![
                sample(1, 5, 20),
                sample(1, 4, 90),
                sample(2, 5, 40),
                sample(9, 6, 99),
            ],
            &[(1, Scope::Tab), (2, Scope::Shared)].into(),
        );
        assert_eq!(activity.summary(), "Tab 20.0%  ·  Shared 40.0%");
        assert!(activity.detail().contains("Intel · render: 11.0%"));
    }

    #[test]
    fn cycle_counters_require_a_matching_total() {
        let start = "drm-driver: xe\ndrm-client-id: 3\ndrm-cycles-rcs: 200\n";
        assert!(fdinfo(start, Scope::Shared).is_none());
        let record = format!("{start}drm-total-cycles-rcs: 1000\n");
        let (_, client) = fdinfo(&record, Scope::Shared).unwrap();
        let before = Counter::Cycles {
            busy: 100,
            total: 500,
        };
        assert_eq!(
            client.engines["rcs"]
                .counter
                .utilization(before, Duration::from_secs(1), 1),
            Some(20.0)
        );
    }
}
