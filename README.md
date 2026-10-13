# lextrack

A terminal host for [CLAP](https://cleveraudio.org) audio plugins. It loads a plugin, plays its output through your sound card, and lists every plugin parameter in the terminal. You change parameters from the keyboard.

```
┌ Test Tone lextrack 0.1.0 ─────────────────────────────────────────────────────┐
│ Default Audio Device · 48000 Hz   out ██████████████░░░░░░  -16.2 dB          │
└───────────────────────────────────────────────────────────────────────────────┘
┌ 7 parameters ─────────────────────────────────────────────────────────────────┐
│  Output / Level         -16.2 dB         ███████████████████████████▋········ │
│  Output / Pan           Center           ██████████████████··················· │
│  Output / Mute          Off              ●──────────────────────────────────── │
│  Oscillator / Frequency 259.6 Hz         ████▍································ │
│▶ Oscillator / Waveform  Saw              ─────────────────●─────────────────── │
│  Oscillator / Detune    +0 st            ─────────────────●─────────────────── │
│  Meter / Peak [ro]      -16.2 dB         █████▉······························· │
└───────────────────────────────────────────────────────────────────────────────┘
 ↑↓ select  ←→ change  Shift fine  Ctrl coarse  Home/End min/max  d default ...
```

## Build

You need Rust 1.75 or newer. Ubuntu 24.04's packaged `cargo` is new enough, as is anything from https://rustup.rs. On Linux you also need the ALSA headers:

```bash
sudo apt install build-essential pkg-config libasound2-dev cargo   # Debian/Ubuntu
sudo dnf install gcc pkgconf alsa-lib-devel                  # Fedora
sudo pacman -S base-devel alsa-lib                           # Arch

cargo build --release
```

The binary is at `target/release/lextrack`. On PipeWire or PulseAudio systems, audio goes through their ALSA compatibility layer, which is the default on current distros.

## Run

```bash
lextrack                              # scan the standard CLAP folders, pick a plugin
lextrack ~/.clap/Surge\ XT.clap       # load one file directly
lextrack Plugin.clap --index 1        # a file with several plugins: pick by position...
lextrack Plugin.clap --id com.x.y     # ...or by id
lextrack --list                       # list installed plugins
lextrack Plugin.clap --dump           # print parameters without the UI
lextrack Plugin.clap --no-audio       # don't open the sound card
```

Folders scanned: `~/.clap`, `/usr/lib/clap` and `/usr/local/lib/clap` on Linux; `~/Library/Audio/Plug-Ins/CLAP` and `/Library/Audio/Plug-Ins/CLAP` on macOS; `%COMMONPROGRAMFILES%\CLAP` and `%LOCALAPPDATA%\Programs\Common\CLAP` on Windows. Folders in `$CLAP_PATH` are scanned too.

## Keys

| Key | Action |
| --- | --- |
| ↑ / ↓ | Select the previous or next parameter |
| ← / → | Decrease or increase the selected parameter. Continuous parameters move 1% of their range; stepped and choice parameters move one step |
| Shift + ← / → | Fine: 0.1% of the range |
| Ctrl + ← / → | Coarse: 10% of the range |
| Home / End | Minimum / maximum |
| d | Reset to the default value |
| Enter | Type a value, for example `440` or `-6 dB`. The plugin parses the text |
| / | Filter parameters by name; Esc clears the filter |
| PgUp / PgDn | Move 10 rows |
| L | Show or hide the plugin's log messages |
| q / Esc | Quit |

Read-only parameters (meters) are shown greyed out and update live. Hidden parameters are not shown.

## How it works

- `src/plugin.rs` loads the `.clap` binary and creates the plugin instance. It also implements the host side of the CLAP API (`clap.params`, `clap.log`, `clap.thread-check`, `clap.state`, `clap.audio-ports`).
- `src/engine.rs` activates the plugin and calls `process` from the sound card callback (via cpal). If no device is available, a silent timer thread calls it instead. Parameter changes go to the plugin as `CLAP_EVENT_PARAM_VALUE` events in the next audio block.
- `src/ui.rs` holds the terminal UI (ratatui). Values shown on screen are re-read from the plugin about 30 times a second, so changes the plugin makes itself show up too.
- `src/discovery.rs` finds plugins in the standard CLAP folders.
- `test-plugin/` is a tiny test-tone CLAP plugin used for testing. Build it with `cargo build -p lextrack-test-plugin`, then copy `target/debug/liblextrack_test_plugin.so` to `~/.clap/TestTone.clap`.

On Linux and macOS, anything a plugin prints to the console goes to `lextrack-plugin-output.log` in the temp folder, so it doesn't break the display.

## Not supported yet

- No plugin GUI windows: control is terminal only.
- No audio or MIDI input, so effects get silence and instruments get no notes.
- Plugin state is not saved or loaded.
