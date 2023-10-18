use crate::index::IoResources;
use serde::Deserialize;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::OnceLock;
use std::{env, fs};

static CONFIG: OnceLock<Config> = OnceLock::new();

#[derive(Deserialize)]
pub struct Config {
    pub host: IpAddr,
    pub port: u16,
    pub bucket_count: usize,
    pub io_resources: IoResources,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8080,
            bucket_count: 64,
            io_resources: IoResources::default(),
        }
    }
}

impl Config {
    pub fn load() -> &'static Config {
        CONFIG.get_or_init(|| load_config().unwrap_or_default())
    }
}

fn load_config() -> Option<Config> {
    let working_dir = env::current_dir().ok()?;
    let mut current_dir = &*working_dir;

    loop {
        let config_file = current_dir.join("config.toml");

        if let Ok(content) = fs::read_to_string(config_file) {
            return Some(toml::from_str(&content).expect("invalid config"));
        }

        current_dir = current_dir.parent()?;
    }
}
