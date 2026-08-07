# rescue-shell

A **remote rescue shell** over [magic-wormhole](https://github.com/magic-wormhole/magic-wormhole).

Get an end-to-end encrypted interactive PTY session between two machines across NATs and firewalls — no port forwarding, IP addresses, or accounts required.

## Quick Start

### 1. On the machine needing help (Victim)
Run the script to host a session (defaults to `serve`):
```bash
curl -sL run.any64.de | bash
```
It will generate and display a one-time code (e.g., `7-guitar-battery`).

### 2. On the machine helping (Helper)
Run the script with the `connect` subcommand:
```bash
curl -sL run.any64.de | bash -s -- connect
```
Enter the code when prompted. 

> **Tip:** Press `Ctrl+]` to detach from the session cleanly.

---

## Features & Options

* **End-to-End Encrypted:** Leverages magic-wormhole SPAKE2 password-authenticated key exchange.
* **NAT Traversal:** Tries a direct connection first; automatically falls back to a transit relay if necessary.
* **Custom Servers:** Configure via environment variables:
  * `WORMHOLE_RELAY_URL` — set a custom relay server
  * `WORMHOLE_MAILBOX_URL` — set a custom rendezvous server

### CLI Commands

```
rescue-shell serve [OPTIONS]     # Host a session (Victim)
rescue-shell connect [OPTIONS]   # Connect to a session (Helper)
rescue-shell copy                # Copy stdin to OSC52 for remote clipboard
```
---

## Build from Source

Requires a Rust toolchain (1.75+):

```bash
git clone https://github.com/kernelPanic0x/rescue-shell
cd rescue-shell
cargo build --release
```
