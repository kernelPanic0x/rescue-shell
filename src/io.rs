use base64::prelude::*;
use color_eyre::eyre::{Context, eyre};
use iroh::{PublicKey, SecretKey};
use std::{
    io::{Read, Write, stdin, stdout},
    path::PathBuf,
    str::FromStr,
};

pub fn copy_to_osc52() -> color_eyre::Result<()> {
    let mut buffer = Vec::new();
    stdin().read_to_end(&mut buffer)?;

    let encoded = BASE64_STANDARD.encode(&buffer);

    // \x1b]52;c; -> Start OSC 52 sequence ('c' specifies system clipboard)
    // \x07       -> BEL character to terminate sequence (or \x1b\ for ST)
    let osc52 = format!("\x1b]52;c;{}\x07", encoded);

    let mut stdout = stdout().lock();
    stdout.write_all(osc52.as_bytes())?;
    stdout.flush()?;

    Ok(())
}

pub fn gen_public_key() -> color_eyre::Result<()> {
    let mut input = String::new();
    stdin()
        .read_to_string(&mut input)
        .context("Failed to read from stdin")?;

    let trimmed = input.trim();
    let bytes = hex::decode(trimmed).context("Invalid hex input")?;

    let secret_key = SecretKey::try_from(bytes.as_slice())
        .map_err(|e| eyre!("Invalid secret key bytes: {e}"))?;

    let public_key = secret_key.public();

    println!("{}", public_key);

    Ok(())
}

pub fn gen_secret_key() {
    let secret_key = SecretKey::generate();
    let hex = hex::encode(secret_key.to_bytes());
    println!("{}", hex);
}

pub fn read_public_keys_file(path: &PathBuf) -> color_eyre::Result<Vec<PublicKey>> {
    let contents = std::fs::read_to_string(path)?;

    let pub_keys = contents
        .split_whitespace()
        .map(|s| PublicKey::from_str(s.trim()))
        .collect::<Result<Vec<PublicKey>, _>>()?;

    Ok(pub_keys)
}
