//! The envelope-keys pin (ADR-0430 §6), shared by the first-party children's
//! tests and the wire kit's own: a child's serialized request envelope and
//! the committed manifest it serves must name the same keys.

use serde_json::Value;

/// Assert the envelope-keys contract over a serialized envelope example
/// and the committed manifest it must agree with (ADR-0430 §6): the
/// example's keys, sorted, are exactly the manifest's flag names plus
/// its positional's name, sorted. A disagreement panics naming both
/// sides, so a manifest edit that breaks the child's envelope fails in
/// the child's crate, not mid-import.
///
/// Test support, not product code: each child's test serializes its own
/// envelope example and hands it to this one assertion.
pub fn assert_envelope_keys(example: &Value, manifest: &Value) {
    let mut keys: Vec<String> = example
        .as_object()
        .expect("a serialized envelope is an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    let mut declared: Vec<String> = manifest["flags"]
        .as_array()
        .expect("the flag table is an array")
        .iter()
        .map(|flag| {
            flag["name"]
                .as_str()
                .expect("a flag entry names itself")
                .to_string()
        })
        .collect();
    declared.push(
        manifest["positional"]["name"]
            .as_str()
            .expect("the positional's name")
            .to_string(),
    );
    declared.sort();
    assert_eq!(
        keys, declared,
        "the manifest's declared names and the envelope's keys must agree"
    );
}
