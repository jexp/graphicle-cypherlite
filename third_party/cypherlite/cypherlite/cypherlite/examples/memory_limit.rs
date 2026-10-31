//! Parse a human-readable memory cap.

use cypherlite::parse_memory_limit;

fn main() {
    assert_eq!(parse_memory_limit("1gib").unwrap(), 1 << 30);
    assert_eq!(parse_memory_limit("512 MiB").unwrap(), 512 << 20);
    assert_eq!(parse_memory_limit("2mb").unwrap(), 2_000_000);
}
