# lextrack

A terminal tracker modelled on the [Dirtywave M8](https://dirtywave.com), with an M8-style sampler built in and [CLAP](https://cleveraudio.org) plugins in place of the M8's other synths. It opens M8 song files (`.m8s`), shows them on an M8-style screen, and lets you play and edit them with the M8's eight buttons.

```
SONG                             ███████
                                   118.0
   1  2  3  4  5  6  7  8
00 00 02 04 -- -- -- -- --       1 C-3
01 01 03 05 -- -- -- -- --       2 G-4
02 -- -- -- -- -- -- -- --       3 ---
03 -- -- -- -- -- -- -- --       4 ---
...
                                  P GM
                                 LSCPIT
                                  V
                                  E
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
lextrack                                  # a new, empty song
lextrack MYSONG.m8s                       # open a song (a new one if the file doesn't exist)
lextrack MYSONG.m8s --plugin ~/.clap/Surge\ XT.clap   # one plugin for every instrument
lextrack MYSONG.m8s --map mysong.map      # a plugin per instrument (see below)
lextrack MYSONG.m8s --inst 03=~/.clap/Dexed.clap      # override one instrument
lextrack MYSONG.m8s --info                # print the song and the plugin for each instrument
lextrack MYSONG.m8s --render out.wav --seconds 60     # render to a WAV file
lextrack examples/demo.m8s --plugin ~/.clap/Surge\ XT.clap   # a short demo song
lextrack --list                           # list installed plugins
lextrack --list-devices                   # list audio outputs and the saved settings
lextrack MYSONG.m8s --no-audio            # don't open the sound card
```

`lextrack m8 ...` still works as an alias.

Plugins are found in `~/.clap`, `/usr/lib/clap` and `/usr/local/lib/clap` on Linux; `~/Library/Audio/Plug-Ins/CLAP` and `/Library/Audio/Plug-Ins/CLAP` on macOS; `%COMMONPROGRAMFILES%\CLAP` and `%LOCALAPPDATA%\Programs\Common\CLAP` on Windows; and in the folders in `$CLAP_PATH`.

**Saving.** PROJECT > SAVE writes the song back to its `.m8s` file in the M8's own format, so it still opens on an M8. The plugin chosen for each instrument and every plugin's settings go next to it in `MYSONG.lextrack`, which is loaded with the song next time.

## Buttons

The M8 has eight buttons: UP, DOWN, LEFT, RIGHT, SHIFT, PLAY, OPTION and EDIT. Almost everything is one button pressed while others are held. A terminal can't report a modifier key on its own, so each button has two spellings:

| M8 button | Key | Held with an arrow |
| --- | --- | --- |
| UP DOWN LEFT RIGHT | Arrow keys | |
| SHIFT | A | Shift + arrow |
| OPTION | Z or Esc | Alt + arrow |
| EDIT | X or Enter | Ctrl + arrow |
| PLAY | S or Space | |
| EDIT+OPTION | Delete or Backspace | |
| Quit | Ctrl+Q | |

In terminals that report key releases (kitty, foot, WezTerm, Ghostty, Alacritty, and the Windows console), A, Z, X and S can be held like the M8's buttons: hold X and press Right for EDIT+RIGHT, or hold Z and press X for OPTION+EDIT. Elsewhere the modifier keys do the holding, with letters as well as arrows: Shift+Z is SHIFT+OPTION, Shift+X is SHIFT+EDIT, Shift+S is SHIFT+PLAY, Ctrl+S is EDIT+PLAY, Alt+S is OPTION+PLAY, Alt+A is OPTION+SHIFT and Alt+Shift+S is OPTION+SHIFT+PLAY.

A strip under the screen (or beside it, in a 24-row terminal) shows the eight buttons and lights the ones you're pressing, modifiers included, with the name of the last key the terminal sent. It helps to check what a key combination means before blaming the app or the terminal. Where the terminal reports key releases, a button stays lit until you let go of its key; elsewhere it stays lit for a moment after each press, and for as long as a held key repeats.

| Combination | Action |
| --- | --- |
| SHIFT + arrows | Move between views (see the map below) |
| EDIT | On an empty cell, insert the last value used in that column. Press twice quickly on a chain, phrase or instrument number for a new, unused one |
| EDIT + LEFT / RIGHT | Change the value by 1 |
| EDIT + UP / DOWN | Change the value by 16 (an octave for notes) |
| EDIT + OPTION | Delete. On an empty note, insert a note off |
| OPTION + UP / DOWN | Song: jump 16 rows. Chain: the chain on the previous or next song row. Phrase: the previous or next phrase in the chain |
| OPTION + LEFT / RIGHT | Chain and phrase: the same row on the previous or next track. Instrument, table, groove: the previous or next number (UP / DOWN jump by 16) |
| SHIFT + OPTION | Start a selection. Move to extend it, press SHIFT+OPTION again to widen it to whole columns, rows, then everything. OPTION copies, EDIT+OPTION cuts, SHIFT+EDIT clones the chain, phrase or instrument under the cursor |
| SHIFT + EDIT | Paste |
| PLAY | Song: play from the cursor row. Chain: loop the chain. Phrase: loop the phrase. Press again to stop |
| SHIFT + PLAY | From a chain or phrase, play the song from that row |
| EDIT + PLAY | Preview the instrument |
| OPTION + PLAY | Solo the track |
| OPTION + SHIFT | Mute the track |
| OPTION + SHIFT + PLAY | Clear all mutes and solos |

## Views

The mini-map at the bottom right shows where you are. SHIFT+arrows move along it, as on the M8:

```
 P GM        P  project       G  groove        M  instrument mods
LSCPIT       L  live          S  song          C  chain    P  phrase
 V           I  instrument    T  table
 E           V  mixer         E  effects
```

- **SONG**: eight tracks of chain numbers, 256 rows. SHIFT+RIGHT opens the chain under the cursor.
- **CHAIN**: up to 16 phrases (PH) with a transpose each (TSP). SHIFT+RIGHT opens the phrase.
- **PHRASE**: 16 steps of note (N), volume (V), instrument (I) and three effects. SHIFT+RIGHT opens the step's instrument; SHIFT+UP opens the groove.
- **INSTRUMENT**: the M8 type, name, transpose setting and table tick, and what plays it: the built-in sampler for SAMPLER instruments (see below), a CLAP plugin for the rest. Every instrument can use any installed plugin: EDIT on PLUGIN opens the list of installed plugins (the standard CLAP folders, `$CLAP_PATH`, and any plugin named on the command line or in the map), EDIT+LEFT/RIGHT steps through them, and EDIT+OPTION goes back to the shared default. `(DEF)` after a plugin name means the instrument uses the shared default. Below are the plugin's parameters, changed with EDIT+arrows and reset with EDIT+OPTION. SHIFT+RIGHT opens the instrument's table.
- **INSTRUMENT, SAMPLER**: SAMPLE is the WAV file. EDIT on it opens a file browser that plays each file as the cursor reaches it (PLAY plays it again, EDIT picks it, OPTION goes back); EDIT+LEFT/RIGHT steps to the previous or next file in the same folder; EDIT+OPTION clears it. RIGHT moves to REC., where EDIT opens the recording screen (see below). Below it, the waveform with the start (S), loop (L) and end (E) points, or the slices, and the M8 sampler's settings: VOLUME, PITCH, FINE TUNE, PLAY mode, SLICE, START, LOOP ST, LENGTH, DEGRADE, FILTER, CUTOFF, RES, AMP, LIMIT, PAN, DRY, CHORUS, DELAY, REVERB, and two envelopes and two LFOs (destination, amount, attack / hold / decay, or shape / trigger / frequency). EDIT+arrows change them, EDIT+OPTION resets one.
- **TABLE**: 16 steps of transpose, volume and three effects (edited and saved, but not played yet).
- **GROOVE**: the ticks per step, with the total per phrase (96 is straight time).
- **PROJECT**: tempo, transpose, name, SAVE / LOAD / NEW (press EDIT twice to drop unsaved changes), RENDER to a WAV file, and the audio output: device, sample rate and buffer size. APPLY restarts audio with them and saves them for next time.

The right-hand panel shows the output level, the tempo, and the note on each track, with M or S on muted and soloed tracks. Live, mixer, effects and instrument mods are dimmed on the map: they aren't there yet.

Audio settings are saved in `~/.config/lextrack/settings.conf` on Linux, `~/Library/Application Support/lextrack/settings.conf` on macOS and `%APPDATA%\lextrack\settings.conf` on Windows (`$LEXTRACK_CONFIG` overrides this). `--device`, `--sample-rate` and `--buffer` override them for one run. If the chosen device can't be opened, lextrack falls back to the system default and the project view says so.

## Instrument maps

 A map file says which plugin plays each instrument, one rule per line. Rules by instrument number win over rules by kind, and kinds win over `default`. Picking a plugin in the instrument view adds a rule by number. `--plugin` sets `default`; `--inst KEY=PLUGIN` adds a rule; `--map FILE` uses a map file instead of `SONG.lextrack`. Relative paths are relative to the map file. See [`examples/demo.map`](examples/demo.map).

```
00 = ~/.clap/Surge XT.clap              # instrument 00, in hex as the M8 shows it
fmsynth = ~/.clap/Dexed.clap            # every FM synth instrument
0A = ~/.clap/Bundle.clap@com.vendor.x   # a file with several plugins: give the id after @
default = ~/.clap/Surge XT.clap         # everything else
```

Kinds are `wavsynth`, `macrosynth`, `sampler`, `midiout` and `fmsynth`. SAMPLER instruments play their sample on the built-in sampler, and `default` doesn't apply to them; a rule by number or a `sampler = ...` rule hands them to a plugin instead.

## The sampler

SAMPLER instruments play WAV files (8, 16, 24 or 32-bit, or float; mono or stereo) the way the M8's sampler does, one note per track:

- **Sample paths** are stored in the song as the M8 stores them, from the SD card root: `/Samples/drums/kick.wav`. The card root is the folder above the song's folder (`Songs/`), so a copy of an M8 card plays as it is. Samples outside the card root are stored with their full path.
- **Pitch.** C-4 plays the sample at its own pitch; PITCH adds semitones and FINE TUNE (80 is centre) up to a semitone either way.
- **PLAY**: FWD, REV, FWDLOOP, REVLOOP, FWD PP and REV PP (ping-pong between the loop point and the end), and OSC, OSC REV and OSC PP, which play the loop as one cycle of an oscillator at the note's pitch.
- **START, LOOP ST, LENGTH** are fractions of the sample: START where playing begins, LENGTH how much of the rest plays (FF to the end), LOOP ST where the loop begins, between the two.
- **SLICE**: at 00 the whole sample plays. Otherwise each note plays one slice, C-1 the first, at the sample's own pitch: the slices are the WAV file's cue markers when it has any, or that many equal parts.
- **DEGRADE** lowers the sample rate and bit depth. **FILTER** is low, high or band pass or band stop, with CUTOFF and RES. **AMP** drives the sound into the **LIMIT**er: CLIP, SIN, FOLD, WRAP, or POST (clipped after the volume). **VOLUME** is attenuation: 00 is full volume, FF silent. **PAN** 80 is centre.
- **Envelopes** (attack, hold, decay) and **LFOs** (20 shapes; FREE, RETRIG, HOLD or ONCE) modulate VOLUME, PITCH, LOOP ST, LENGTH, DEGRADE, CUTOFF, RES, AMP or PAN by their amount.
- **Recording.** REC. on the SAMPLE row opens the M8's sample recording screen. **SRC** is `L&R` (both channels of the audio input), `INL` or `INR` (one channel, saved as a mono sample) or `MASTER` (lextrack's own output, for resampling). **DEVICE** picks the input device (DEFAULT is the system's). **VOL** is the input gain (80 is unity), shown on the LEVEL meter. **ARM** sets a level: with it on, EDIT on START waits until the sound gets that loud before it starts recording (ARMED), so the take starts on the first sound. **PLAY** plays the song from that song row while recording, and stops it at the end. EDIT on START records, EDIT again (or OPTION) stops; the take's waveform and length are shown, and PLAY listens to it. **NAME** names the file (REC000, REC001... by default) and **SAVE** writes `Samples/Recordings/NAME.wav` under the card root as a 16-bit WAV at the source's sample rate and loads it into the instrument. OPTION goes back (twice, if the take isn't saved). Takes stop at 10 minutes.
- **Sampler effects** in phrases set a setting for the note on that step: `VOL PIT FIN PLY STA LOP LEN DEG FLT CUT RES AMP LIM PAN`, envelope and LFO amounts and times (`EA1 AT1 HO1 DE1 EA2`... `LA1 LF1 LT1`...) and `SLI`. On a step without a note they change the note already playing. EDIT+UP past the last sequencer effect reaches them.
- DRY, CHORUS, DELAY and REVERB are kept in the song but not played yet: there are no send effects. The M8's exact envelope times, LFO rates and filter curves aren't documented, so these are close, not exact.

## How songs are played

- Tracks run independently, like on the M8. Each plays its chain on the current song row, then moves to the next row. An empty row sends the track back to the first row of its block, so blocks loop.
- Timing is 24 ticks per beat. Each phrase step lasts as many ticks as the track's groove says (6 by default). Notes reach the plugin on the exact sample.
- Each track plays one note at a time. A new note releases the previous one.
- M8 note C-4 is MIDI note 60 (C-1 is 24). `--note-offset` changes this.
- The volume column becomes note velocity: `00`-`7F` maps to 0-1, higher values play at full velocity, and an empty volume plays at full velocity.
- Chain transpose applies to instruments whose TRANSP setting is on.
- Effects: `DEL` (delay the note), `GRV` (change groove), `HOP` (jump to a phrase step), `KIL` (cut the note) and `TPO` (tempo), and the sampler's instrument effects (see above). Other effects are ignored for now.

## Not supported yet

- Instrument tables, the remaining effects, the M8 mixer and send effects, live mode and instrument modulators.
- Mapping M8 instrument parameters (filters, envelopes...) to plugin parameters. Each plugin instrument plays its plugin's own settings.
- The sampler's DRY, CHORUS, DELAY and REVERB sends, its waveform's play position, the M8's sample editor (trimming, normalizing, slicing a sample), and recording single tracks or USB audio.
- Plugin GUI windows: plugins are controlled from the instrument view only.
- Songs from M8 firmware 2.x are supported; m8-js documents the file format up to firmware 2.7, so songs saved by firmware 3 or later have not been tested.

The song reader and writer are a Rust port of [m8-js](https://github.com/whitlockjc/m8-js) by Jeremy Whitlock (Apache License 2.0); see [NOTICE](NOTICE).

## How it works

- `src/m8/format.rs` reads and writes `.m8s` files. Saving changes only the bytes that were edited, so unknown parts of the file survive.
- `src/m8/sequencer.rs` turns song rows, chains and phrases into note events tick by tick, and handles chain and phrase looping, mutes and solos.
- `src/m8/rack.rs` runs one plugin per instrument, mixes them, and plays them through the sound card (via cpal) or renders them to a WAV file. Notes reach the plugins on the exact sample.
- `src/m8/instruments.rs` reads and writes instrument maps and plugin states.
- `src/m8/sampler.rs` is the built-in sampler: WAV loading, finding sample files, and the voices.
- `src/m8/recorder.rs` records samples from an audio input (via cpal) or the master output, and saves them as WAV files.
- `src/m8/screen.rs` is the 40x24 character screen in the M8's default colours; `src/m8/input.rs` turns keys into M8 button presses; `src/m8/ui.rs` holds the views; `src/m8/app.rs` is the command line.
- `src/plugin.rs` loads `.clap` files and implements the host side of the CLAP API (`clap.params`, `clap.state`, `clap.log`, `clap.thread-check`, `clap.audio-ports`).
- `src/audio.rs`, `src/settings.rs` and `src/discovery.rs` find sound cards, save audio settings and find plugins.
- `test-plugin/` is a tiny test-tone CLAP plugin used for testing. Build it with `cargo build -p lextrack-test-plugin`, then copy `target/debug/liblextrack_test_plugin.so` to `~/.clap/TestTone.clap`.

On Linux and macOS, anything a plugin prints to the console goes to `lextrack-plugin-output.log` in the temp folder, so it doesn't break the display.
