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
}

impl Activity {
    pub fn summary(&self) -> String {
        if !self.has_counters {
            return "Engine counters unavailable".into();
        }
        if self.engines.is_empty() {
            return "Reading engine counters…".into();
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
    time: Option<Instant>,
}

impl Sampler {
    pub fn sample(&mut self, processes: impl Iterator<Item = (u32, Scope)>) -> Activity {
        let mut clients = HashMap::<ClientId, Client>::new();
        for (pid, scope) in processes {
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
        self.update(clients, Instant::now())
    }

    fn update(&mut self, mut clients: HashMap<ClientId, Client>, now: Instant) -> Activity {
        let elapsed = self.time.replace(now).map(|before| now - before);
        let mut activity = Activity {
            has_counters: !clients.is_empty(),
            ..Default::default()
        };
        if let Some(elapsed) = elapsed {
            for (id, client) in &mut clients {
                let Some(previous) = self.previous.get(id) else {
                    continue;
                };
                for (name, engine) in &mut client.engines {
                    let Some(previous) = previous.engines.get(name) else {
                        continue;
                    };
                    if let Some(value) =
                        engine
                            .counter
                            .utilization(previous.counter, elapsed, engine.capacity)
                    {
                        let key = (client.scope, id.device.clone(), name.clone());
                        *activity.engines.entry(key).or_default() += value;
                    } else {
                        // DRM may briefly report a lower counter before catching up.
                        engine.counter = previous.counter;
                    }
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
