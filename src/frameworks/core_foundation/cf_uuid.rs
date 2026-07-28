/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `CFUUID`.
//!
//! There's no toll-free bridging for this in Apple's implementation (it
//! predates `NSUUID`), so it gets its own tiny host-only class, much like
//! `CFRunLoopTimer`'s `_touchHLE_CFTimerTarget`.

use super::cf_allocator::{kCFAllocatorDefault, CFAllocatorRef};
use super::CFTypeRef;
use crate::abi::GuestArg;
use crate::dyld::{export_c_func, FunctionExports};
use crate::frameworks::foundation::ns_string::{from_rust_string, to_rust_string};
use crate::mem::SafeRead;
use crate::objc::{id, nil, objc_classes, ClassExports, HostObject};
use crate::{impl_GuestRet_for_large_struct, Environment};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub type CFUUIDRef = CFTypeRef;

/// Belongs to `_touchHLE_CFUUID`.
struct CFUUIDHostObject {
    bytes: [u8; 16],
}
impl HostObject for CFUUIDHostObject {}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

// This class is never `alloc`'d or `init`'d by guest code: touchHLE creates
// instances directly via `CFUUIDCreate`. It only needs to exist so that the
// generic `CFRetain`/`CFRelease`/`CFEqual`/etc. machinery (which all operate
// on plain `id`s) works on it like any other object.
@implementation _touchHLE_CFUUID: NSObject
@end

};

/// 16-byte big-endian UUID bytes, matches Apple's `CFUUIDBytes` struct
/// layout, which is returned by value (via the guest ABI's hidden-pointer
/// convention for large structs).
#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct CFUUIDBytes {
    pub byte0: u8,
    pub byte1: u8,
    pub byte2: u8,
    pub byte3: u8,
    pub byte4: u8,
    pub byte5: u8,
    pub byte6: u8,
    pub byte7: u8,
    pub byte8: u8,
    pub byte9: u8,
    pub byte10: u8,
    pub byte11: u8,
    pub byte12: u8,
    pub byte13: u8,
    pub byte14: u8,
    pub byte15: u8,
}
unsafe impl SafeRead for CFUUIDBytes {}
impl_GuestRet_for_large_struct!(CFUUIDBytes);
// 16 bytes = 4 ARM registers, so (unlike a return value) this still fits in
// registers r0-r3 as a parameter, per AAPCS -- no hidden pointer needed here.
impl GuestArg for CFUUIDBytes {
    const REG_COUNT: usize = 4;

    fn from_regs(regs: &[u32]) -> Self {
        let mut bytes = [0u8; 16];
        for (i, word) in regs[0..4].iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        bytes.into()
    }
    fn to_regs(self, regs: &mut [u32]) {
        let bytes: [u8; 16] = self.into();
        for (i, chunk) in bytes.chunks_exact(4).enumerate() {
            regs[i] = u32::from_le_bytes(chunk.try_into().unwrap());
        }
    }
}

impl From<[u8; 16]> for CFUUIDBytes {
    fn from(b: [u8; 16]) -> Self {
        CFUUIDBytes {
            byte0: b[0],
            byte1: b[1],
            byte2: b[2],
            byte3: b[3],
            byte4: b[4],
            byte5: b[5],
            byte6: b[6],
            byte7: b[7],
            byte8: b[8],
            byte9: b[9],
            byte10: b[10],
            byte11: b[11],
            byte12: b[12],
            byte13: b[13],
            byte14: b[14],
            byte15: b[15],
        }
    }
}
impl From<CFUUIDBytes> for [u8; 16] {
    fn from(b: CFUUIDBytes) -> Self {
        [
            b.byte0, b.byte1, b.byte2, b.byte3, b.byte4, b.byte5, b.byte6, b.byte7, b.byte8,
            b.byte9, b.byte10, b.byte11, b.byte12, b.byte13, b.byte14, b.byte15,
        ]
    }
}

/// Generates 16 pseudo-random bytes and stamps them as a version-4,
/// variant-1 UUID (matches the bit-pattern real UUIDs use, though the
/// randomness source here isn't cryptographically secure -- good enough for
/// touchHLE's purposes, e.g. as a per-install "device identifier" that games
/// use to tag network requests).
fn generate_random_uuid_bytes() -> [u8; 16] {
    // Simple process-lifetime counter mixed with wall-clock time, so
    // repeated calls in the same run (and across runs) don't collide.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    // splitmix64-style mixing, run twice to get 128 bits.
    fn splitmix64(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    let seed = nanos ^ counter.wrapping_mul(0x2545F4914F6CDD1D);
    let hi = splitmix64(seed);
    let lo = splitmix64(hi ^ counter);

    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&hi.to_be_bytes());
    bytes[8..16].copy_from_slice(&lo.to_be_bytes());

    // Set version (4) and variant (RFC 4122) bits so this at least *looks*
    // like a standard UUID to anything that checks.
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;

    bytes
}

fn format_uuid_string(bytes: [u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

fn parse_uuid_string(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for i in 0..16 {
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

fn new_cf_uuid(env: &mut Environment, bytes: [u8; 16]) -> CFUUIDRef {
    let class = env.objc.get_known_class("_touchHLE_CFUUID", &mut env.mem);
    let host_object = Box::new(CFUUIDHostObject { bytes });
    env.objc.alloc_object(class, host_object, &mut env.mem)
}

pub fn CFUUIDCreate(env: &mut Environment, allocator: CFAllocatorRef) -> CFUUIDRef {
    assert!(allocator == kCFAllocatorDefault || env.mem.read(allocator).is_system_default()); // unimplemented
    new_cf_uuid(env, generate_random_uuid_bytes())
}

fn CFUUIDCreateFromString(
    env: &mut Environment,
    allocator: CFAllocatorRef,
    uuid_str: id, // CFStringRef
) -> CFUUIDRef {
    assert!(allocator == kCFAllocatorDefault || env.mem.read(allocator).is_system_default()); // unimplemented
    if uuid_str == nil {
        return new_cf_uuid(env, [0u8; 16]);
    }
    let s = to_rust_string(env, uuid_str).to_string();
    let bytes = parse_uuid_string(&s).unwrap_or([0u8; 16]);
    new_cf_uuid(env, bytes)
}

fn CFUUIDCreateString(
    env: &mut Environment,
    allocator: CFAllocatorRef,
    uuid: CFUUIDRef,
) -> id /* CFStringRef */ {
    assert!(allocator == kCFAllocatorDefault || env.mem.read(allocator).is_system_default()); // unimplemented
    let bytes = env.objc.borrow::<CFUUIDHostObject>(uuid).bytes;
    from_rust_string(env, format_uuid_string(bytes))
}

pub fn CFUUIDGetUUIDBytes(env: &mut Environment, uuid: CFUUIDRef) -> CFUUIDBytes {
    let bytes = env.objc.borrow::<CFUUIDHostObject>(uuid).bytes;
    bytes.into()
}

fn CFUUIDCreateFromUUIDBytes(
    env: &mut Environment,
    allocator: CFAllocatorRef,
    bytes: CFUUIDBytes,
) -> CFUUIDRef {
    assert!(allocator == kCFAllocatorDefault || env.mem.read(allocator).is_system_default()); // unimplemented
    new_cf_uuid(env, bytes.into())
}

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(CFUUIDCreate(_)),
    export_c_func!(CFUUIDCreateFromString(_, _)),
    export_c_func!(CFUUIDCreateString(_, _)),
    export_c_func!(CFUUIDGetUUIDBytes(_)),
    export_c_func!(CFUUIDCreateFromUUIDBytes(_, _)),
];
