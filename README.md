# batlens

Windows laptop battery monitor, written in Rust.

`status` reads the battery through Win32 ACPI/IOCTL, without WMI, PowerShell or COM.
`report`, `chart` and `info` read history, wear trend and hardware details from the Windows
battery report (`powercfg /batteryreport`).

## Features

- [x] Live charge, AC state, voltage, power, capacity and health
- [x] Time left while discharging
- [x] Battery history report: drain segments, runtime estimate, wear rate
- [x] Hardware, manufacturer, BIOS and EC firmware details
- [x] Terminal charts of health over time and daily drain (Unicode braille or ASCII)
- [x] Multiple battery modules, queried and matched to the report separately
- [x] Battery replacement detection
- [x] Single exe, no runtime dependencies

## Screenshot

```text
> batlens status
Device   : MS-1552 (Summit B15 A11M, LION)
Charge   : 80%  (AC)
Power    : +17.0 W  (negative = discharging)
Voltage  : 12.62 V
Capacity : 44.6 Wh / 52.4 Wh design
Health   : 85.1%
Time left: charging
```

## Install

Windows only. Requires Rust 1.88 or newer.

### Get the source

```cmd
git clone https://github.com/onlyatuna/batlens.git
cd batlens
```

### Build

```cmd
cargo build --release
```

The binary is `target\release\batlens.exe`. `build.bat` runs the same build.

### Run

```cmd
target\release\batlens.exe status
```

### Test

```cmd
cargo test
```

## Usage

```text
batlens status          Live charge, AC state, capacity, health, power, time left
batlens report          History: drain segments, runtime estimate, wear rate, standby vs active
batlens info            Hardware, manufacturer, BIOS, EC firmware cycle count
batlens chart           Battery health over its lifetime (Unicode braille)
batlens chart --drain   Daily average discharge power, last 7 days
batlens chart --ascii   Block-character chart for fonts without braille glyphs
```

| Command | Option | Meaning |
|---|---|---|
| `report` | `--days <N>` | Days of recent usage to inspect (default 14) |
| `report` | `--target <PCT>` | Health % used for the "days to reach" estimate (default 80) |
| `report` | `--all` | Include pre-replacement history in the wear rate |
| `chart` | `--current` | Only the newest battery life-cycle |
| `chart` | `--width <COLS>` | Chart width in columns (default: fit the window) |
| `chart` | `--braille` / `--ascii` | Force the glyph set |

## Behavior

- **History source**: Windows' own battery report, cached for 10 minutes. `status` never runs
  `powercfg`; if no cached report exists, design capacity and health show `N/A` until you run
  `report`, `chart` or `info`.
- **Multiple batteries**: each module is queried separately. A missing design capacity is derived
  from the cached report only when the value is plausible, otherwise it stays `N/A`. If a module
  reports no full-charge capacity, the total stays `N/A`.
- **Battery replacement**: detected from a jump in full-charge capacity. `--all` (report) and
  omitting `--current` (chart) ignore it.
- **Output**: colors are off when stdout is piped or `NO_COLOR=1` is set. Output is written once
  at exit, so `batlens report | head` exits quietly.
- **Console**: the console code page is switched to UTF-8 while running and restored on exit.

## Project layout

- `src/main.rs` - CLI, `status` output, tests
- `src/sys.rs` - Win32 battery IOCTL, console handling
- `src/xml.rs` - `powercfg` execution, report cache, report parsing
- `src/stats.rs` - replacement detection, drain aggregation, wear rate
- `src/render.rs` - charts and terminal width handling
- `src/types.rs` - data types, errors, design-capacity cache logic
- `src/out.rs` - buffered stdout

## Tech stack

- Rust 2024 edition
- clap 4 (CLI)
- chrono (time)
- quick-xml 0.37 (report parsing)
- colored 2 (terminal color)
- unicode-width 0.2 (display width)
- windows-sys 0.61 (Win32 API)
- winres 0.1 (exe metadata, build only)

## Contact

Evan Chen - evanchen940628@gmail.com

## License

[MIT](LICENSE). Copyright (c) 2026 Evan Chen.
