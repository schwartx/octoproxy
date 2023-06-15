use anyhow::{bail, Context};
use std::env;
use std::io::Read;
use std::{fs::read_to_string, net::SocketAddr, path::PathBuf};
use toml::macros::Deserialize;
use tracing::metadata::LevelFilter;
use tracing::Level;
use tracing_subscriber::EnvFilter;

pub fn parse_socket_address(address: &str) -> anyhow::Result<SocketAddr> {
    // 127.0.0.1:8081 or 0.0.0.0:8081
    if let Ok(listen_address) = address.parse::<SocketAddr>() {
        return Ok(listen_address);
    }

    // only a port
    if let Ok(port) = address.parse::<u16>() {
        let listen_address: SocketAddr = ([127, 0, 0, 1], port).into();
        return Ok(listen_address);
    }

    bail!("cannot parse listen address into socket address")
}

pub trait TomlFileConfig
where
    Self: Sized,
{
    fn load_from_read(r: impl std::io::Read) -> anyhow::Result<Self>
    where
        for<'de> Self: Deserialize<'de>,
    {
        let mut buf = std::io::BufReader::new(r);
        let mut s = String::new();
        buf.read_to_string(&mut s)?;

        let config: Self = match toml::from_str(&s) {
            Ok(config) => config,
            Err(e) => {
                bail!(e);
            }
        };
        Ok(config)
    }

    fn load_from_file(path: PathBuf) -> anyhow::Result<Self>
    where
        for<'de> Self: Deserialize<'de>,
    {
        let data = read_to_string(path).context("Failed to read file")?;
        let config: Self = match toml::from_str(&data) {
            Ok(config) => config,
            Err(e) => {
                // display_toml_error(&data, &e);
                bail!(e);
            }
        };
        Ok(config)
    }
}

/// set `RUST_LOG`
/// environment variable overwrites file config
pub fn parse_log_level_str(log_level: &str) -> LevelFilter {
    let log_level = env::var(EnvFilter::DEFAULT_ENV).unwrap_or(log_level.to_string());
    LevelFilter::from_level(log_level.parse::<Level>().unwrap_or(Level::ERROR))
}

#[test]
fn test_parse_socket_address() {
    let res = parse_socket_address("localhost:8081");
    assert!(res.is_err());

    let res = parse_socket_address("127.0.0.1:8081");
    assert!(res.is_ok());

    let res = parse_socket_address("8081");
    assert!(res.is_ok());
}

#[test]
fn test_parse_log_level_str() {
    let res = parse_log_level_str("INFO");
    assert_eq!(res, Level::INFO);

    let res = parse_log_level_str("DEBUG");
    assert_eq!(res, Level::DEBUG);

    std::env::set_var("RUST_LOG", "INFO");
    let res = parse_log_level_str("");
    assert_ne!(res, Level::DEBUG);
}
