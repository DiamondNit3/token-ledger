#![forbid(unsafe_code)]
//! Local accounting, pricing, reporting, and privacy primitives for Token Ledger.

pub mod adapters;
pub mod billing;
pub mod config;
pub mod cost;
pub mod db;
pub mod demo;
pub mod html;
pub mod model;
pub mod pricing;
pub mod reconcile;
pub mod report;
pub mod scanner;
pub mod terminal;

pub const APP_NAME: &str = "Token Ledger";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
