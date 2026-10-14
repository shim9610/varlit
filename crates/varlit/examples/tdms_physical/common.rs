#![allow(dead_code)]

#[path = "common/adapter.rs"]
mod adapter;
#[path = "common/example_data.rs"]
mod example_data;
#[path = "common/verify.rs"]
mod verify;

use std::path::Path;

pub fn write_example(path: &Path) -> varlit::Result<()> {
    adapter::write_example(path)
}

pub fn append_example(path: &Path) -> varlit::Result<()> {
    adapter::append_example(path)
}

pub fn inspect_example(path: &Path) -> varlit::Result<varlit::AdapterCheckReport> {
    adapter::inspect_example(path)
}

pub fn read_and_verify(path: &Path) -> varlit::Result<()> {
    verify::read_and_verify(path)
}

pub fn read_and_verify_bytes(bytes: &[u8]) -> varlit::Result<()> {
    verify::read_and_verify_bytes(bytes)
}

pub fn cleanup(path: &Path) {
    adapter::cleanup(path);
}
