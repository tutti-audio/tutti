//! A name no other test in the machine picks.

use tutti_types::Mint;

/// `<prefix>_<pid>_<n>`: unique across the processes a test run starts (the
/// process id) and across the calls of one process (a counter).
///
/// For a name the OS shares, such as a shared-memory segment, a pipe, a MIDI
/// port or a temp file, where a name another test run left behind or a parallel
/// test holds would make two tests meet. The name adds only two underscores and
/// the two numbers to `prefix`, so a short `prefix` stays inside a platform's
/// short shared-memory name limit.
pub fn unique_name(prefix: &str) -> String {
    static NEXT: Mint = Mint::new();
    format!("{prefix}_{}_{}", std::process::id(), NEXT.mint())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two calls with one prefix differ, and the name spells prefix, process, count.
    #[test]
    fn calls_differ_and_the_name_has_the_documented_shape() {
        let (a, b) = (unique_name("seg"), unique_name("seg"));
        assert_ne!(a, b);
        let pid = std::process::id();
        for name in [&a, &b] {
            let rest = name
                .strip_prefix(&format!("seg_{pid}_"))
                .unwrap_or_else(|| panic!("{name} is not seg_<pid>_<n>"));
            assert!(rest.parse::<u64>().is_ok(), "{name}");
        }
    }

    /// Threads racing for names never get one twice.
    #[test]
    fn concurrent_calls_never_repeat() {
        let names: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| (0..100).map(|_| unique_name("r")).collect::<Vec<_>>()))
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        let distinct: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(distinct.len(), names.len());
    }
}
