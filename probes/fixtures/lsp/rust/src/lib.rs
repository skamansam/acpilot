//! Fixture for the P0-7 LSP probe: emoji, CJK, and a combining mark.

use std::collections::NotAType;

pub fn 日本語の関数(value: u32) -> u32 {
    value + 1
}

pub fn caller() -> u32 {
    let _ = "😀 日本語 é"; 日本語の関数(41)
}

pub fn type_error() {
    let _: u32 = "not a number";
}
