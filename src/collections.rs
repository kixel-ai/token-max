//! Hash collections keyed by transcript data, seeded per process with foldhash, which
//! hashes the short IDs and labels they hold far faster than the standard SipHash.

use foldhash::fast::RandomState;

/// A hash map with the crate's hasher.
pub type HashMap<K, V> = std::collections::HashMap<K, V, RandomState>;
/// A hash set with the crate's hasher.
pub type HashSet<T> = std::collections::HashSet<T, RandomState>;
/// An insertion-ordered map with the crate's hasher.
pub type IndexMap<K, V> = indexmap::IndexMap<K, V, RandomState>;
/// An insertion-ordered set with the crate's hasher.
pub type IndexSet<T> = indexmap::IndexSet<T, RandomState>;
