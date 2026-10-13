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
lextrack --list-devices               # list audio outputs and the saved settings
```

Folders scanned: `~/.clap`, `/usr/lib/clap` and `/usr/local/lib/clap` on Linux; `~/Library/Audio/Plug-Ins/CLAP` and `/Library/Audio/Plug-Ins/CLAP` on macOS; `%COMMONPROGRAMFILES%\CLAP` and `%LOCALAPPDATA%\Programs\Common\CLAP` on Windows. Folders in `$CLAP_PATH` are scanned too.

## Audio output

Press **F2** (or Ctrl+O) in the app to choose the output device, sample rate and buffer size. Use ↑↓ to pick a setting, ←→ to change it, and Enter to apply. lextrack restarts audio with the new settings and saves them, so the next run uses them too. Esc closes the screen without changes. "Default" means the system default for that setting.

Settings are saved in `~/.config/lextrack/settings.conf` on Linux, `~/Library/Application Support/lextrack/settings.conf` on macOS and `%APPDATA%\lextrack\settings.conf` on Windows. Set `$LEXTRACK_CONFIG` to use a different file.

The same settings can be given on the command line. These override the saved ones for that run only:

```bash
lextrack Plugin.clap --device speakers --sample-rate 48000 --buffer 256
lextrack Plugin.clap --buffer default     # use the device's default buffer this time
```

Device names are the ones `lextrack --list-devices` prints. On Linux they are ALSA names: `default` and `pipewire` / `pulse` go through the sound server, and `hw:` / `plughw:` names open a card directly. If the chosen device can't be opened (unplugged, or it rejects the rate or buffer size), lextrack falls back to the system default and says so.

## Keys

| Key | Action |
| --- | --- |
| ↑ / ↓ | Select the previous or next parameter |
| ← / → | Decrease or increase the selected parameter. Continuous parameters move 1% of their range; stepped and choice parameters move one step |
| Shift + ← / → | Fine: 0.1% of the range |
| Ctrl + ← / → | Coarse: 10% of the range |
| Home / End | Minimum / maximum |
| Delete / Backspace | Reset to the default value |
| Enter | Type a value, for example `440` or `-6 dB`. The plugin parses the text |
| / | Filter parameters by name; Esc clears the filter |
| PgUp / PgDn | Move 10 rows |
| Tab | Show or hide the plugin's log messages |
| F2 or Ctrl+O | Audio output settings |
| q / Esc | Quit |

Read-only parameters (meters) are shown greyed out and update live. Hidden parameters are not shown.

### Playing notes

The keyboard works like a piano laid over the home row:

```
 w e   t y u   o p          black keys   C#  D#    F#  G#  A#    C#  D#
a s d f g h j k l ; '       white keys  C  D  E  F  G  A  B  C  D  E  F
```

| Key | Action |
| --- | --- |
| a s d f g h j k l ; ' | White keys, starting from C |
| w e t y u o p | Black keys |
| z / x | One octave down / up. The header shows the current range; it starts at C4–F5 (`a` is middle C) |
| Space | Stop all notes |

The header shows the notes that are sounding. Notes go to the plugin's first note input as CLAP note events, or as MIDI when the plugin only accepts MIDI. For plugins with no note input, the header says so.

**Releasing notes.** Most terminals don't report when a key is let go. lextrack handles this in two ways:

- In terminals that support the kitty keyboard protocol (kitty, foot, WezTerm, Ghostty, Alacritty), and in the Windows console, a note sounds for exactly as long as you hold the key.
- Everywhere else (GNOME Terminal, xterm, tmux and others), the header says *timed notes*. A tap plays for 0.7 s. Holding the key down keeps the note going through the keyboard's auto-repeat, and the note stops about 0.15 s after you let go. A key can't re-strike its own note until that note has stopped.

## How it works

- `src/plugin.rs` loads the `.clap` binary and creates the plugin instance. It also implements the host side of the CLAP API (`clap.params`, `clap.log`, `clap.thread-check`, `clap.state`, `clap.audio-ports`).
- `src/engine.rs` activates the plugin and calls `process` from the sound card callback (via cpal). If no device is available, a silent timer thread calls it instead. Parameter changes go to the plugin as `CLAP_EVENT_PARAM_VALUE` events in the next audio block.
- `src/settings.rs` loads and saves the audio settings.
- `src/notes.rs` maps computer keys to notes and decides when they stop.
- `src/ui.rs` holds the terminal UI (ratatui). Values shown on screen are re-read from the plugin about 30 times a second, so changes the plugin makes itself show up too.
- `src/discovery.rs` finds plugins in the standard CLAP folders.
- `test-plugin/` is a tiny test-tone CLAP plugin used for testing. Build it with `cargo build -p lextrack-test-plugin`, then copy `target/debug/liblextrack_test_plugin.so` to `~/.clap/TestTone.clap`.

On Linux and macOS, anything a plugin prints to the console goes to `lextrack-plugin-output.log` in the temp folder, so it doesn't break the display.

## Not supported yet

- No plugin GUI windows: control is terminal only.
- No audio input or external MIDI input, so effects get silence. Instruments can be played from the computer keyboard.
- Plugin state is not saved or loaded.
