![Banner](banner.png)

[![License: EUPL 1.2](https://img.shields.io/badge/License-EUPL_1.2-blue.svg)](https://opensource.org/licenses/EUPL-1.2)
[![Ko-fi](https://shields.io/badge/ko--fi-Buy_me_a_coffee-ff5f5f?logo=ko-fi&style=for-the-badgeKo-fi)](https://ko-fi.com/kernelpanic0x)

Get a **remote rescue shell** over [magic-wormhole](https://github.com/magic-wormhole/magic-wormhole) and [iroh](https://github.com/n0-computer/iroh) for an end-to-end encrypted interactive PTY session between multiple machines across NATs and firewalls.

![Demo](demo.gif)

## Quick Start

### 1. On the machine needing help:
```sh
curl -sL https://run.any64.de | sh
```

or directly from github:
```sh
curl -sL https://raw.githubusercontent.com/kernelPanic0x/rescue-shell/main/install.sh | sh
```

It will display a one-time code (e.g., `7-guitar-battery`).

### 2. On the machine helping:
```sh
curl -sL https://run.any64.de | sh -s -- connect
```

or directly from github:
```sh
curl -sL https://raw.githubusercontent.com/kernelPanic0x/rescue-shell/main/install.sh | sh -s -- connect
```

Enter the code when prompted. 

> **Exit:** Press `Ctrl+]` to detach from the session cleanly.

Use `SHELL` environment variable to set a shell.

It also sets `RESCUE_SHELL` environment variable to the path of the executable in case you need the OSC52 copy or wormhole functionality.

### CLI Commands

```
rescue-shell serve [OPTIONS]     # Host a session (Victim)
rescue-shell connect [OPTIONS]   # Connect to a session (Helper)
rescue-shell copy                # Copy stdin to OSC52 for remote clipboard
rescue-shell wormhole <args>     # full wormhole-rs cli version 0.8.1 included
```

## Tested platforms

| Platform | Status |
|----------|--------|
| Raspberry Pi 2B+ | ✅ |
| Google Pixel 7a (Termux) | ✅ |
| Arch Linux | ✅ |
| macOS | ⏳ not yet |
| Windows 11 | ✅ |
| FreeBSD 14+ | ⏳ not yet |

## Build from Source

```sh
git clone https://github.com/kernelPanic0x/rescue-shell
cd rescue-shell
cargo build --release
```
