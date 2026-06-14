//! llmux — multi-account, multi-provider LLM proxy for Claude Code with
//! quota-maximizing scheduling. See `.prd/01-spec.md` and `.prd/02-architecture.md`
//! for the contract this crate implements.

pub mod auth;
pub mod build_info;
pub mod cli;
pub mod config;
pub mod dashboard;
pub mod demo;
pub mod logging;
pub mod provider;
pub mod proxy;
pub mod routing;
pub mod scheduler;
pub mod tui;
