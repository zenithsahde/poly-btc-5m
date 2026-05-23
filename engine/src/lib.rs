// Library 目标：把 main.rs 的模块树以 `pub mod` 暴露出来，供 src/bin/* 的辅助可执行文件
// （如 place_test_order）复用真实的 signer / transaction / config 代码，而不必改动 main.rs。
// 与 main.rs 是各自独立的 crate root，模块源码会被编译两份，但路径解析完全一致。
#![allow(dead_code, unused_variables)]

pub mod cli;
pub mod config;
pub mod execution;
pub mod metrics;
pub mod model;
pub mod position;
pub mod strategy;
pub mod tui;
pub mod web;
pub mod ws;
