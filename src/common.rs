/// TIOCGWINSZ on /dev/tty — for size negotiation when mirroring.
pub fn console_size() -> Option<(u16, u16)> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open("/dev/tty").ok()?;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row))
}
