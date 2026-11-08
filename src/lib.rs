//! Speedcraft: cross-exchange crypto arbitrage with hard risk limits.
//!
//! Read `docs/PLAN.md` first. The one-sentence version: this bot only sends a
//! trade when both legs are simultaneously profitable after real fees, uses
//! immediate-or-cancel orders so nothing rests on a book, unwinds any
//! one-legged fill immediately, and halts on a daily loss limit or a STOP file.
//! It cannot promise profit. It can promise that it never knowingly sends a
//! losing trade and that its worst case is bounded by the limits you set.

pub mod book;
pub mod carry;
pub mod coins;
pub mod config;
pub mod engine;
pub mod exchanges;
pub mod executor;
pub mod ledger;
pub mod maker;
pub mod replay;
pub mod risk;
pub mod runtime;
pub mod scan;
pub mod server;
pub mod strategy;
pub mod types;
