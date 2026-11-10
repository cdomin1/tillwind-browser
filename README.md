# Tillwind

Tillwind is an early prototype of a lightweight Linux browser with a Safari-inspired layout. The browser shell uses Rust and GTK 4; WebKitGTK handles web content.

## Run on Linux

Install a current stable Rust toolchain, a C toolchain, `pkg-config`, and the GTK 4 (4.10 or newer) and WebKitGTK 6.0 development libraries. On Arch Linux, the native dependencies are provided by `base-devel`, `gtk4`, and `webkitgtk-6.0`. On Debian-based distributions that provide WebKitGTK 6.0, look for `build-essential`, `pkg-config`, `libgtk-4-dev`, and `libwebkitgtk-6.0-dev`.

```sh
cargo run --release --locked
```

Use Ctrl+L for the address bar, Ctrl+T for a new tab, and Ctrl+W to close a tab. Text that isn't recognized as a web address is sent to DuckDuckGo when submitted. Open tab URLs and the selected tab are saved on normal window close under the user's local data directory. On restart, only the selected tab loads; other tabs load when selected. This does not restore unsaved form state or navigation history.

## Performance smoke test

The GitHub Actions Linux build checks formatting and links against real GTK and WebKitGTK libraries. It does not exercise playback or the desktop UI.

- In an empty scratch directory, run `python3 -m http.server 8000 --bind 127.0.0.1`. Open three tabs at `http://127.0.0.1:8000/?tab=1`, `?tab=2`, and `?tab=3` (using the same full URL for each). Select tab 2, close the window, and restart. Check the server log: tab 2 should load first; requests for the other tabs should appear only when selected.
- Compare playback against GNOME Web on the same Linux machine using the same video codec, resolution, and frame rate. Check the actual decoder, CPU across the browser processes, and dropped frames. Tillwind leaves rendering and decoder selection to the installed WebKitGTK stack; hardware decoding is not guaranteed.
- Repeat a fixed tab open/close sequence and check whether total proportional memory keeps climbing after a settling period. Compare idle CPU before and after playback. Avoid treating a warm cache or a short page-load spike as a leak.

This is a development prototype, not a daily-driver release. GPU diagnostics, pressure-based tab unloading, and performance claims are deferred until the Linux baseline has been measured.
