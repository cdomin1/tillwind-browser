# JavaScript and GPU performance

## Baseline on Arch Linux

Measured on November 10, 2026 with an Intel Core i5-1135G7, Iris Xe graphics, GTK 4.24.1, and WebKitGTK 2.54.1. The tests used fresh profiles on an isolated Xvfb display. These are exploratory runs with uncontrolled desktop load. The Speedometer runs used different Tillwind builds.

| Benchmark | Tillwind | WebKitGTK MiniBrowser |
| --- | ---: | ---: |
| [JetStream 2.2](https://browserbench.org/JetStream2.2/) | 149.143 | — |
| [Speedometer 3.1](https://browserbench.org/Speedometer3.1/) | 5.83 ± 0.34 initially, 14.4 ± 0.79 on the final build | 7.06 ± 1.8 |
| [JetStream 3.0](https://browserbench.org/JetStream/) | Incomplete at Kotlin Compose Wasm | Kotlin workload also failed in isolation |
| [MotionMark 1.3.2](https://browserbench.org/MotionMark1.3.2/) | Pending a native graphics run | — |

The JetStream 3.0 Kotlin workload also failed in WebKitGTK MiniBrowser. The runner encountered an error while formatting the workload failure, so the underlying cause remains unconfirmed.

The Speedometer results do not isolate the effect of the diagnostics changes.

JavaScriptCore's baseline JIT, DFG, and FTL optimizers were enabled in the installed engine. This WebKitGTK build also enabled WebGL, accelerated canvas drawing, and threaded scrolling by default. It listed WebGPU as unstable and disabled. A separate probe on the same X11 display did not expose the WebGPU API when the feature flag was enabled. Tillwind retains the engine default. GPU compute is available to web applications through APIs such as WebGPU when the engine, graphics backend, and driver support it.

Tillwind opens restored pages on demand. When the performance overlay is open, it samples Linux process and GPU counters on a background I/O worker. Closing the overlay removes its timer. WebKit handles JavaScript compilation and graphics acceleration.

To compare builds, use the same benchmark version, browser viewport, display backend, and power conditions. Start with one tab and the overlay closed. Alternate at least three runs of each build and record the median and range. Test MotionMark on a native GPU-accelerated display.

## GPU vendor paths

| GPU and driver | Process activity | Video decoding |
| --- | --- | --- |
| Intel i915 and Xe | DRM engine time or cycles, depending on driver | VA-API via the Intel media driver and GStreamer `va` |
| AMD with amdgpu | DRM graphics, compute, and media engines exposed by the kernel | VA-API via Mesa's Radeon driver and GStreamer `va` |
| NVIDIA proprietary driver | NVML samples for 3D/compute, encoder, and decoder activity where supported | NVDEC via GStreamer `nvcodec` when the build and driver expose the relevant decoder |

Tillwind loads NVML only when diagnostics are in use and the NVIDIA driver library is present. The overlay shows the busiest measured engine in each scope. It keeps browser-wide work separate from the active renderer and shows missing samples as unavailable. On hybrid systems, engine details retain their device identifiers. The overlay does not add device totals together. Intel was hardware-tested. AMD counters and the NVIDIA interface were exercised with test fixtures.

## Verified Intel hardware decode

Tillwind played the same local 1280×720, 30 fps VP8 clip with the software `vp8dec` and Intel hardware `vavp8dec` decoders. GStreamer logs confirmed the decoder selected in each case.

| Decoder | Browser-process CPU during playback | Dropped frames |
| --- | ---: | ---: |
| `vp8dec` software | 37.4% of one logical core | 0 / 301 |
| `vavp8dec` Intel VA-API | 33.6% of one logical core | 0 / 301 |

Both clips played for ten seconds. This run used the Arch `gst-plugin-va` package in a temporary test profile with the system `intel-media-driver`. GStreamer selected the hardware decoder using its default priorities. Codec availability depends on the GPU, driver, and installed plugins.

Check available decoders on a target system with `gst-inspect-1.0 va` or `gst-inspect-1.0 nvcodec`. To verify which decoder WebKit actually selected, launch a release build with `GST_DEBUG=GST_ELEMENT_FACTORY:4` and play a local clip while inspecting the log. Hover over the GPU reading for its engine breakdown.

### References

- [Linux DRM client usage statistics](https://docs.kernel.org/gpu/drm-usage-stats.html)
- [Intel Xe DRM client usage statistics](https://docs.kernel.org/gpu/xe/xe-drm-usage-stats.html)
- [NVIDIA NVML device queries](https://docs.nvidia.com/deploy/nvml-api/latest/api/group__nvmlDeviceQueries.html)
- [GStreamer hardware video decoding](https://gstreamer.freedesktop.org/documentation/tutorials/playback/hardware-accelerated-video-decoding.html)
