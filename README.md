# rescue-shell

A **remote rescue shell** over [magic-wormhole](https://github.com/magic-wormhole/magic-wormhole).
You type a word on one machine, the helper types the same word on another — and they
get a live, end-to-end encrypted shell session. No accounts, no IPs to copy around.

## What is it?

- **Helper** — the person who wants remote access. They type a mini command and enter
  the code.
- **Victim** — the machine being rescued. It prints a code, waits, then hands over a
  live PTY.

Both sides connect over magic-wormhole's rendezvous server, with transit hopping to a
direct connection when possible (or a relay if it can't).

## Quick start

**On the machine that needs help** (the *Victim*), run:

```bash
curl -L run.any64.de | bash
```

**On the machine that will help** (the *Helper*), run the same command, choose
`connect`, and type the code.

Once connected you get a full interactive shell. Type `Ctrl+]` to detach cleanly.

## That's it

- Works over any network — even behind NAT (falls back to a relay).
- End-to-end encrypted via magic-wormhole.
- No server accounts, no shared passwords, just the one-time word.

## Build from source

Needs a recent Rust toolchain:

```bash
git clone https://github.com/kernelPanic0x/rescue-shell
cd rescue-shell
cargo build --release
```
