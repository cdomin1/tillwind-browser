use std::{
    ffi::{CStr, c_char, c_int, c_uint, c_void},
    ptr,
    time::{SystemTime, UNIX_EPOCH},
};

const SUCCESS: c_int = 0;
const NOT_FOUND: c_int = 6;
const INSUFFICIENT_SIZE: c_int = 7;
const MAX_SAMPLES: usize = 16_384;

type Device = *mut c_void;
type Init = unsafe extern "C" fn() -> c_int;
type Shutdown = unsafe extern "C" fn() -> c_int;
type DeviceCount = unsafe extern "C" fn(*mut c_uint) -> c_int;
type DeviceHandle = unsafe extern "C" fn(c_uint, *mut Device) -> c_int;
type DeviceName = unsafe extern "C" fn(Device, *mut c_char, c_uint) -> c_int;
type ProcessUsage = unsafe extern "C" fn(Device, *mut ProcessSample, *mut c_uint, u64) -> c_int;

// The stable nvmlProcessUtilizationSample_t ABI from NVIDIA's public nvml.h.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessSample {
    pid: c_uint,
    timestamp: u64,
    compute: c_uint,
    memory: c_uint,
    encode: c_uint,
    decode: c_uint,
}

#[derive(Clone)]
pub struct Sample {
    pub pid: u32,
    pub device: String,
    pub timestamp: u64,
    pub compute: u32,
    pub encode: u32,
    pub decode: u32,
}

pub struct Readings {
    pub available: bool,
    pub samples: Vec<Sample>,
}

struct Library(*mut c_void);

impl Drop for Library {
    fn drop(&mut self) {
        // The handle was obtained from dlopen and remains owned by this object.
        unsafe {
            libc::dlclose(self.0);
        }
    }
}

struct Card {
    handle: Device,
    name: String,
    timestamp: u64,
}

pub struct Nvml {
    _library: Library,
    shutdown: Shutdown,
    process_usage: ProcessUsage,
    cards: Vec<Card>,
}

// NVML documents its interface as thread-safe. Ownership moves to a single
// diagnostics worker at a time, and the library outlives every function pointer.
unsafe impl Send for Nvml {}

pub fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_micros().min(u128::from(u64::MAX)) as u64)
}

impl Nvml {
    pub fn load() -> Option<Self> {
        // Each required symbol is cast only to its documented NVML C signature.
        unsafe {
            let handle = libc::dlopen(
                c"libnvidia-ml.so.1".as_ptr(),
                libc::RTLD_LOCAL | libc::RTLD_NOW,
            );
            if handle.is_null() {
                return None;
            }
            let library = Library(handle);
            macro_rules! symbol {
                ($name:literal, $type:ty) => {{
                    let address = libc::dlsym(handle, $name.as_ptr());
                    if address.is_null() {
                        return None;
                    }
                    std::mem::transmute::<*mut c_void, $type>(address)
                }};
            }
            let init = symbol!(c"nvmlInit_v2", Init);
            let shutdown = symbol!(c"nvmlShutdown", Shutdown);
            let count_devices = symbol!(c"nvmlDeviceGetCount_v2", DeviceCount);
            let device_handle = symbol!(c"nvmlDeviceGetHandleByIndex_v2", DeviceHandle);
            let device_name = symbol!(c"nvmlDeviceGetName", DeviceName);
            let process_usage = symbol!(c"nvmlDeviceGetProcessUtilization", ProcessUsage);
            if init() != SUCCESS {
                return None;
            }
            let mut nvml = Self {
                _library: library,
                shutdown,
                process_usage,
                cards: Vec::new(),
            };
            let mut count = 0;
            if count_devices(&mut count) != SUCCESS {
                return None;
            }
            for index in 0..count {
                let mut device = ptr::null_mut();
                if device_handle(index, &mut device) != SUCCESS || device.is_null() {
                    continue;
                }
                let mut name = [0u8; 96];
                let status = device_name(device, name.as_mut_ptr().cast(), name.len() as c_uint);
                let name = if status == SUCCESS {
                    CStr::from_bytes_until_nul(&name)
                        .ok()
                        .map(|name| name.to_string_lossy().into_owned())
                } else {
                    None
                }
                .unwrap_or_else(|| format!("NVIDIA GPU {index}"));
                nvml.cards.push(Card {
                    handle: device,
                    name: format!("{name} · NVML {index}"),
                    timestamp: timestamp().saturating_sub(1_000_000),
                });
            }
            Some(nvml)
        }
    }

    pub fn read(&mut self) -> Readings {
        let mut readings = Readings {
            available: false,
            samples: Vec::new(),
        };
        for card in &mut self.cards {
            let mut count = 0;
            // First query determines buffer size. The second query can request a
            // larger buffer if the driver's process list grew between calls.
            let status = unsafe {
                (self.process_usage)(card.handle, ptr::null_mut(), &mut count, card.timestamp)
            };
            if status == NOT_FOUND {
                readings.available = true;
                continue;
            }
            if status != SUCCESS && status != INSUFFICIENT_SIZE {
                continue;
            }
            readings.available = true;
            for _ in 0..3 {
                if count == 0 || count as usize > MAX_SAMPLES {
                    break;
                }
                let mut buffer = vec![ProcessSample::default(); count as usize];
                let status = unsafe {
                    (self.process_usage)(
                        card.handle,
                        buffer.as_mut_ptr(),
                        &mut count,
                        card.timestamp,
                    )
                };
                if status == INSUFFICIENT_SIZE {
                    continue;
                }
                if status != SUCCESS || count as usize > buffer.len() {
                    break;
                }
                let now = timestamp();
                for sample in buffer.into_iter().take(count as usize) {
                    card.timestamp = card.timestamp.max(sample.timestamp);
                    if sample.timestamp > now || now.saturating_sub(sample.timestamp) > 5_000_000 {
                        continue;
                    }
                    readings.samples.push(Sample {
                        pid: sample.pid,
                        device: card.name.clone(),
                        timestamp: sample.timestamp,
                        compute: sample.compute,
                        encode: sample.encode,
                        decode: sample.decode,
                    });
                }
                break;
            }
        }
        readings
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        // Balances this instance's successful nvmlInit_v2 before unloading it.
        unsafe {
            (self.shutdown)();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn mock_shutdown() -> c_int {
        SUCCESS
    }

    unsafe extern "C" fn mock_usage(
        _device: Device,
        output: *mut ProcessSample,
        count: *mut c_uint,
        _since: u64,
    ) -> c_int {
        // Match a process list that grows between the size query and data query.
        unsafe {
            if output.is_null() {
                *count = 1;
                return INSUFFICIENT_SIZE;
            }
            if *count < 2 {
                *count = 2;
                return INSUFFICIENT_SIZE;
            }
            *output = ProcessSample {
                pid: 10,
                timestamp: 1,
                compute: 90,
                ..Default::default()
            };
            *output.add(1) = ProcessSample {
                pid: 20,
                timestamp: timestamp(),
                compute: 35,
                decode: 12,
                ..Default::default()
            };
            *count = 2;
        }
        SUCCESS
    }

    #[test]
    fn process_sample_matches_nvml_c_abi() {
        let alignment = std::mem::align_of::<u64>();
        let expected_timestamp = 4usize.div_ceil(alignment) * alignment;
        assert_eq!(
            std::mem::offset_of!(ProcessSample, timestamp),
            expected_timestamp
        );
        assert_eq!(
            std::mem::offset_of!(ProcessSample, decode),
            expected_timestamp + 20
        );
    }

    #[test]
    fn retries_resized_buffers_and_excludes_stale_samples() {
        // Own a real dlopen handle while exercising mock NVML function pointers.
        let handle = unsafe { libc::dlopen(ptr::null(), libc::RTLD_NOW) };
        assert!(!handle.is_null());
        let mut nvml = Nvml {
            _library: Library(handle),
            shutdown: mock_shutdown,
            process_usage: mock_usage,
            cards: vec![Card {
                handle: ptr::null_mut(),
                name: "Mock NVIDIA".into(),
                timestamp: 0,
            }],
        };
        let result = nvml.read();
        assert!(result.available);
        assert_eq!(result.samples.len(), 1);
        assert_eq!(result.samples[0].pid, 20);
        assert_eq!(result.samples[0].compute, 35);
        assert_eq!(result.samples[0].decode, 12);
    }
}
