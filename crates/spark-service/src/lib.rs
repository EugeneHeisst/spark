//! Spark's loopback gateway and self-hosted LAN service.
mod discovery;
mod error;
mod gateway;
mod host;
mod store;
mod validation;

pub use gateway::{Service, ServiceOptions};
