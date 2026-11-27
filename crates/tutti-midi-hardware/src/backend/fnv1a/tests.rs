use super::fnv1a;

/// **The digest is FNV-1a's**: its published 64-bit vectors for the empty input (the
/// offset basis), "a" and "foobar".
#[test]
fn the_digest_matches_fnv1as_published_vectors() {
    assert_eq!(fnv1a(*b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a(*b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a(*b"foobar"), 0x8594_4171_f739_67e8);
}
