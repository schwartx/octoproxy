pub mod config;
pub mod metric;
pub mod proxy;
pub mod proxy_client;
pub mod tls;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
