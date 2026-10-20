//! Direkt: a self-hostable direct download manager.

#![forbid(unsafe_code)]

pub mod auth;
pub mod config;
pub mod domain;
pub mod engine;
pub mod error;
pub mod fsutil;
pub mod http_client;
pub mod net;
pub mod providers;
pub mod service;
pub mod settings;
pub mod store;
pub mod web;
