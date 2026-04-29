#![allow(dead_code)]

use std::path::{Path, PathBuf};

const MASK: u8 = 0x5a;

fn decode(encoded: &[u8]) -> String {
    let bytes: Vec<u8> = encoded.iter().map(|byte| byte ^ MASK).collect();
    String::from_utf8(bytes).expect("dictionary entry must be valid UTF-8")
}

fn decode_bytes(encoded: &[u8]) -> Vec<u8> {
    encoded.iter().map(|byte| byte ^ MASK).collect()
}

pub(crate) fn desktop_app_process() -> String {
    decode(&[13, 63, 25, 50, 59, 46])
}

pub(crate) fn container_id() -> String {
    decode(&[
        57, 53, 55, 116, 46, 63, 52, 57, 63, 52, 46, 116, 34, 51, 52, 13, 63, 25, 50, 59, 46,
    ])
}

pub(crate) fn account_files_dir() -> String {
    decode(&[34, 45, 63, 57, 50, 59, 46, 5, 60, 51, 54, 63, 41])
}

pub(crate) fn account_id_prefix() -> String {
    decode(&[45, 34, 51, 62, 5])
}

pub(crate) fn sticker_magic() -> Vec<u8> {
    decode_bytes(&[45, 34, 61, 60])
}

pub(crate) fn container_data_dir(home: &Path) -> PathBuf {
    home.join("Library/Containers")
        .join(container_id())
        .join("Data")
}

pub(crate) fn documents_account_files_dir(home: &Path) -> PathBuf {
    container_data_dir(home)
        .join("Documents")
        .join(account_files_dir())
}

pub(crate) fn app_support_dir(home: &Path) -> PathBuf {
    container_data_dir(home)
        .join("Library/Application Support")
        .join(container_id())
}

pub(crate) fn kvcomm_dir(home: &Path) -> PathBuf {
    container_data_dir(home).join("Documents/app_data/net/kvcomm")
}
