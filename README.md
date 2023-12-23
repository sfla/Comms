# Comms

[😎 Fetch the most recently built Comms for Windows here! 😎](https://gitlab.com/comms-app/comms/-/jobs/artifacts/main/raw/comms.exe?job=build_release_windows)

![screenshot of the connect pane](doc/screenshot-connect.png)
![screenshot of the user-list pane](doc/screenshot-user-list.png)

## Features

* Lightweight app, consumes < 15MB of memory
* Optimized audio pipelines to keep latency low
* Uses peer-to-peer connections to reduce latency further

## Latency benchmarks

Round-trip audio latency between two Windows devices on the same local network:

```
[From ~Stockholm SE]
+-----------------------------+-------+
| Comms (Local Network)       | 29ms  |
+-----------------------------+-------+
| Zoom ("studio mode")        | 70ms  |
+-----------------------------+-------+
| TeamSpeak 3 (Local Network) | 109ms |
+-----------------------------+-------+
| Google Meet (Chrome)        | 159ms |
+-----------------------------+-------+
| Discord (Spånga, 1ms away)  | 176ms |
+-----------------------------+-------+
| Google Meet (Firefox)       | 232ms |
+-----------------------------+-------+
```

## Linux Build Instructions

1. Install `rust`, GTK4 and opus libs
2. Install Libadwaita 1.3 (likely by building it from source)
3. From `comms`, run `cargo run`

## Windows Build Instructions

1. Install [`rust`](https://rustup.rs/). Follow the directions and install the Visual Studio tools Rust recommends.
2. Ensure Rust is using the `msvc` toolchain, if it isn't already: `rustup default stable-msvc`
3. Install GTK4 and libadwaita using [`gvsbuild`](https://github.com/wingtk/gvsbuild)
   1. Follow [the "Development Environment" instructions (install git, MSYS2, Python, etc)](https://github.com/wingtk/gvsbuild#development-environment)
   2. In [the "Build GTK" step](https://github.com/wingtk/gvsbuild#build-gtk), just do `gvsbuild build gtk4 adwaita-icon-theme libadwaita`
   3The final step you need to follow is ["Add GTK to Your Environmental Variables"](https://github.com/wingtk/gvsbuild#add-gtk-to-your-environmental-variables)
4. From the `comms` repository, run `cargo run --bin comms`

### Windows _Faster_ Build Instructions

Incremental builds of Comms can be a little slow - one of the main delays here is linking.
You can speed this up by using the LLVM linker instead of the default MSVC one:

1. Download and run the most recent `LLVM-$version-win64.exe` [from the LLVM releases page](https://github.com/llvm/llvm-project/releases)
2. Add `C:\Program Files\LLVM\bin` to your `PATH`
3. Edit `$env:USERPROFILE\.cargo\config.toml` and add the following contents:
    ```
    [target.x86_64-pc-windows-msvc]
    linker = "lld-link.exe"
    ```
4. Run `cargo clean`, then `cargo build --bin comms`

## Wireshark Decoder
See `extras/comms-dissector.lua` for a script that can be used in Wireshark to parse the internet traffic.

Put the `comms-dissector.lua` script in `<Path to Wireshark folder>\Wireshark\plugins`

On Windows it is typically `C:\Program Files\Wireshark\plugins`.

Use (if tshark is in your paths) `tshark -G plugins` to see currently installed plugins and their respective paths. If the Comms lua decoder is properly installed it should show up in the list as e.g:
```
comms-dissector.lua             lua script      C:\Program Files\Wireshark\plugins\comms-dissector.lua
```
This plugin should also show up in Wireshark: Help -> About Wireshark -> Plugins.

To decode a packet, right click on packet (with e.g. port 18847) -> Decode As -> Current -> COMMS -> Save
