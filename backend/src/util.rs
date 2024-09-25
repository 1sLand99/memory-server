use byteorder::{ByteOrder, LittleEndian};
use hex;
use lazy_static::lazy_static;
use libc::{self, c_char, c_int, c_void};
use lz4;
use lz4::block::compress;
use memchr::memmem;
use percent_encoding::percent_decode_str;
use rayon::prelude::*;
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;

use std::env;
use std::ffi::CStr;
use std::ffi::CString;

use crate::native_bridge;
use capstone::arch::arm64::ArchMode;
use capstone::prelude::*;
use capstone::Syntax;
use log::{debug, error, info, trace, warn};
use std::io::Error;
use std::io::{BufRead, BufReader};
use std::panic;
use std::process;
use std::slice;
use std::str;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;
use std::sync::{Arc, Mutex};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileItem {
    item_type: String,
    name: String,
    size: Option<i64>,
    last_opened: Option<i64>,
    children: Option<Vec<FileItem>>,
}

pub fn read_memory_64(pid: i32, address: u64) -> Result<u64, String> {
    let mut buffer = [0u8; 8];
    native_bridge::read_process_memory(pid, address as *mut libc::c_void, 8, &mut buffer).map_err(
        |e| {
            format!(
                "Failed to read 64-bit memory at address {:#x}: {}",
                address, e
            )
        },
    )?;
    Ok(u64::from_le_bytes(buffer))
}

pub fn _read_memory_32(pid: i32, address: u32) -> Result<u32, String> {
    let mut buffer = [0u8; 4];
    native_bridge::read_process_memory(pid, address as *mut libc::c_void, 4, &mut buffer).map_err(
        |e| {
            format!(
                "Failed to read 32-bit memory at address {:#x}: {}",
                address, e
            )
        },
    )?;
    Ok(u32::from_le_bytes(buffer))
}

pub fn _evaluate_expression(expr: &str) -> Result<isize, String> {
    let re = regex::Regex::new(r"(\d+)\s*([+\-*/])\s*(\d+)").unwrap();
    if let Some(caps) = re.captures(expr) {
        let a: isize = caps[1]
            .parse()
            .map_err(|_| "Invalid number in expression".to_string())?;
        let b: isize = caps[3]
            .parse()
            .map_err(|_| "Invalid number in expression".to_string())?;
        match &caps[2] {
            "+" => Ok(a + b),
            "-" => Ok(a - b),
            "*" => Ok(a * b),
            "/" => Ok(a / b),
            _ => Err("Unsupported operation".to_string()),
        }
    } else {
        expr.parse().map_err(|_| "Invalid expression".to_string())
    }
}

pub fn resolve_nested_address(
    pid: i32,
    nested_addr: &str,
    modules: &Vec<serde_json::Value>,
) -> Result<u64, String> {
    let re =
        regex::Regex::new(r"(\[)|(\])|([^\[\]]+)").map_err(|e| format!("Regex error: {}", e))?;
    let mut stack = Vec::new();
    let mut current_expr = String::new();

    for cap in re.captures_iter(nested_addr) {
        if let Some(_) = cap.get(1) {
            if !current_expr.is_empty() {
                stack.push(current_expr);
                current_expr = String::new();
            }
            current_expr.push('[');
        } else if let Some(_) = cap.get(2) {
            if !current_expr.is_empty() {
                let inner_value = resolve_single_level_address(&current_expr, modules)?;
                let memory_value = read_memory_64(pid, inner_value)?;
                if let Some(mut prev_expr) = stack.pop() {
                    prev_expr.push_str(&format!("0x{:X}", memory_value));
                    current_expr = prev_expr;
                } else {
                    current_expr = format!("0x{:X}", memory_value);
                }
            }
            current_expr.push(']');
        } else if let Some(m) = cap.get(3) {
            current_expr.push_str(m.as_str());
        }
    }

    resolve_single_level_address(&current_expr, modules)
}

pub fn resolve_single_level_address(
    addr: &str,
    modules: &Vec<serde_json::Value>,
) -> Result<u64, String> {
    let re = regex::Regex::new(r"([-+])?(?:\s*)((?:\w|-)+(?:\.\w+)*|\d+|0x[\da-fA-F]+)")
        .map_err(|e| format!("Regex error: {}", e))?;
    let mut current_address: u64 = 0;
    let mut first_item = true;

    for cap in re.captures_iter(addr) {
        let op = cap.get(1).map_or("+", |m| m.as_str());
        let part = cap.get(2).unwrap().as_str();

        let value = if let Some(module_info) = modules.iter().find(|m| {
            m["modulename"]
                .as_str()
                .map_or(false, |name| part.eq_ignore_ascii_case(name))
        }) {
            let base = module_info["base"].as_u64().ok_or("Invalid base address")?;
            base
        } else {
            u64::from_str_radix(part.trim_start_matches("0x"), 16)
                .map_err(|_| format!("Invalid number: {}", part))?
        };

        if first_item {
            current_address = value;
            first_item = false;
        } else {
            match op {
                "+" => current_address = current_address.wrapping_add(value),
                "-" => current_address = current_address.wrapping_sub(value),
                _ => return Err(format!("Invalid operation: {}", op)),
            }
        }
    }

    Ok(current_address)
}

pub fn resolve_symbolic_address(
    pid: i32,
    symbolic_addr: &str,
    modules: &Vec<serde_json::Value>,
) -> Result<usize, String> {
    let resolved = resolve_nested_address(pid, symbolic_addr, modules)?;
    Ok(resolved as usize)
}

pub fn parse_directory_structure(raw_data: &str) -> Vec<FileItem> {
    let mut root_items = Vec::new();
    let mut stack: Vec<*mut FileItem> = Vec::new();

    for line in raw_data.lines() {
        let indent = line.chars().take_while(|&c| c == ' ').count() / 2;
        let content = line.trim_start();

        if let Some((item_type, rest)) = content.split_once(':') {
            let new_item = match item_type {
                "dir" => FileItem {
                    item_type: "directory".to_string(),
                    name: rest.to_string(),
                    size: None,
                    last_opened: None,
                    children: None,
                },
                "file" => {
                    let parts: Vec<&str> = rest.split(',').collect();
                    if parts.len() == 3 {
                        FileItem {
                            item_type: "file".to_string(),
                            name: parts[0].to_string(),
                            size: parts[1].parse().ok(),
                            last_opened: parts[2].parse().ok(),
                            children: None,
                        }
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };

            while stack.len() > indent {
                stack.pop();
            }

            if stack.is_empty() {
                root_items.push(new_item);

                if item_type == "dir" {
                    let last_ptr = root_items.last_mut().unwrap() as *mut FileItem;
                    stack.push(last_ptr);
                }
            } else {
                unsafe {
                    let parent = stack.last_mut().unwrap();
                    if let Some(children) = &mut (**parent).children {
                        children.push(new_item);
                    } else {
                        (**parent).children = Some(vec![new_item]);
                    }

                    if item_type == "dir" {
                        let last_ptr = (**parent).children.as_mut().unwrap().last_mut().unwrap()
                            as *mut FileItem;
                        stack.push(last_ptr);
                    }
                }
            }
        }
    }

    root_items
}

pub fn disassemble(bytecode: *const u8, length: usize, address: u64) -> String {
    let bytes = unsafe { slice::from_raw_parts(bytecode, length) };
    let cs = Capstone::new()
        .arm64()
        .mode(arch::arm64::ArchMode::Arm)
        .detail(true)
        .build()
        .expect("Failed to create Capstone object");

    let instructions = cs
        .disasm_all(bytes, address)
        .expect("Failed to disassemble");
    let mut result = String::new();

    for i in instructions.iter() {
        let mnemonic = i.mnemonic().unwrap_or("");
        let op_str = i.op_str().unwrap_or("");
        result.push_str(&format!("{:#x}: {} {}\n", i.address(), mnemonic, op_str));
    }

    result
}
