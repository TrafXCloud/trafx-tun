use std::time::Duration;

pub mod cc;
pub mod mux;
pub mod pacer;
pub mod utils;

pub const UDP_TTL: Duration = Duration::from_secs(180);
