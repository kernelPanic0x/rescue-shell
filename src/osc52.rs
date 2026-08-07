use base64::prelude::*;
use std::io::{Read, Write, stdin, stdout};

pub fn copy_to_osc52() -> anyhow::Result<()> {
    // 1. Read all raw bytes from stdin
    let mut buffer = Vec::new();
    stdin().read_to_end(&mut buffer)?;

    // 2. Encode bytes to Base64
    let encoded = BASE64_STANDARD.encode(&buffer);

    // 3. Construct the OSC 52 sequence
    // \x1b]52;c; -> Start OSC 52 sequence ('c' specifies system clipboard)
    // \x07       -> BEL character to terminate sequence (or \x1b\ for ST)
    let osc52 = format!("\x1b]52;c;{}\x07", encoded);

    // 4. Write to stdout and flush immediately
    let mut stdout = stdout().lock();
    stdout.write_all(osc52.as_bytes())?;
    stdout.flush()?;

    Ok(())
}
