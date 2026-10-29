//! Interned storage for tree entry names.
//!
//! Every version of a directory repeats the names of its entries, so a
//! large history has hundreds of millions of entries but only about a
//! hundred thousand distinct names. Interning keeps one copy of each
//! distinct name and gives each entry a 4-byte `NameId` instead of its own
//! `Vec<u8>`, which is smaller to store and cheap to compare.

use intaglio::bytes::SymbolTable;
use intaglio::Symbol;

use crate::error::ParseError;

/// A handle to an entry name in a `NameStore`: the position the name was
/// interned at. Four bytes, so it is cheap to keep in every tree entry.
///
/// A `NameId` is only meaningful with the store that issued it. The
/// interner's own symbol type stays inside this module, so the interner
/// can be swapped out without touching the rest of the crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NameId(u32);

impl NameId {
    fn from_symbol(symbol: Symbol) -> Self {
        Self(symbol.id())
    }

    fn symbol(self) -> Symbol {
        Symbol::new(self.0)
    }
}

/// Interns entry names as bytes (git does not require them to be UTF-8)
/// and de-duplicates them, so that each distinct name is stored once.
#[derive(Debug, Default)]
pub struct NameStore {
    table: SymbolTable<ahash::RandomState>,
    /// Sum of the lengths of all interned names
    total_bytes: usize,
}

impl NameStore {
    /// An empty store.
    pub fn new() -> Self {
        Self {
            table: SymbolTable::with_hasher(ahash::RandomState::default()),
            total_bytes: 0,
        }
    }

    /// The id of `bytes`: the existing one if an equal name has been
    /// interned, otherwise a new one after storing a copy of `bytes`.
    pub fn intern(&mut self, bytes: &[u8]) -> Result<NameId, ParseError> {
        // Looked up first so the common case, a name seen before, does not
        // copy the bytes: the interner only accepts owned input.
        if let Some(symbol) = self.table.check_interned(bytes) {
            return Ok(NameId::from_symbol(symbol));
        }
        let symbol = self
            .table
            .intern(bytes.to_vec())
            .map_err(|e| ParseError::TooManyNames(e.max_capacity()))?;
        self.total_bytes += bytes.len();
        Ok(NameId::from_symbol(symbol))
    }

    /// The name an id was issued for.
    ///
    /// Panics if the id did not come from this store.
    pub fn get(&self, id: NameId) -> &[u8] {
        self.table
            .get(id.symbol())
            .expect("NameId was issued by this store")
    }

    /// Number of distinct names stored
    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// Total length of all distinct names stored, in bytes
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

#[cfg(feature = "bench")]
mod benches {
    use divan::counter::ItemsCount;
    use divan::{black_box, Bencher};

    use super::NameStore;

    /// `count` distinct names like real file names, 8 to 24 bytes
    fn names(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|i| format!("file_{i:06}{}.rs", "x".repeat(i % 12)).into_bytes())
            .collect()
    }

    /// Look up every name in a store of `store_size` names, in a scattered
    /// order so that little of the table is in cache from one lookup to
    /// the next. Separates the cost of a lookup in a large table from the
    /// cost of hashing: hashing is the same at every size.
    #[divan::bench(args = [1_000, 10_000, 100_000, 1_000_000])]
    fn intern_hit(bencher: Bencher, store_size: usize) {
        let all = names(store_size);
        let mut store = NameStore::new();
        for name in &all {
            store.intern(name).unwrap();
        }
        // 7919 is prime, so this visits every index once
        let probes: Vec<&[u8]> = (0..store_size)
            .map(|i| all[(i * 7919) % store_size].as_slice())
            .collect();
        bencher
            .counter(ItemsCount::new(probes.len()))
            .bench_local(|| {
                for name in &probes {
                    black_box(store.intern(black_box(name)).unwrap());
                }
            });
    }

    /// Get every name in a store of `store_size` names by id, in the same
    /// scattered order as `intern_hit`
    #[divan::bench(args = [1_000, 100_000, 1_000_000])]
    fn get(bencher: Bencher, store_size: usize) {
        let mut store = NameStore::new();
        let ids: Vec<_> = names(store_size)
            .iter()
            .map(|name| store.intern(name).unwrap())
            .collect();
        let probes: Vec<_> = (0..store_size)
            .map(|i| ids[(i * 7919) % store_size])
            .collect();
        bencher
            .counter(ItemsCount::new(probes.len()))
            .bench_local(|| {
                for &id in &probes {
                    black_box(store.get(black_box(id)));
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_store_is_empty() {
        let store = NameStore::new();
        assert_eq!(store.len(), 0);
        assert!(store.is_empty());
        assert_eq!(store.total_bytes(), 0);
    }

    #[test]
    fn intern_returns_the_same_id_for_equal_bytes() {
        let mut store = NameStore::new();
        let a = store.intern(b"src/main.rs").unwrap();
        let b = store.intern(b"src/lib.rs").unwrap();
        let again = store.intern(b"src/main.rs").unwrap();

        assert_eq!(a, again);
        assert_ne!(a, b);
        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
    }

    #[test]
    fn get_returns_the_interned_bytes() {
        let mut store = NameStore::new();
        let empty = store.intern(b"").unwrap();
        let non_utf8 = store.intern(b"caf\xe9").unwrap();
        let plain = store.intern(b"README").unwrap();

        assert_eq!(store.get(empty), b"");
        assert_eq!(store.get(non_utf8), b"caf\xe9");
        assert_eq!(store.get(plain), b"README");
    }

    #[test]
    fn total_bytes_counts_each_distinct_name_once() {
        let mut store = NameStore::new();
        store.intern(b"abc").unwrap();
        store.intern(b"abc").unwrap();
        store.intern(b"").unwrap();
        store.intern(b"defgh").unwrap();

        assert_eq!(store.len(), 3);
        assert_eq!(store.total_bytes(), 8);
    }

    #[test]
    #[should_panic(expected = "issued by this store")]
    fn get_panics_on_an_id_from_another_store() {
        let mut other = NameStore::new();
        let id = other.intern(b"x").unwrap();
        let store = NameStore::new();
        store.get(id);
    }

    #[test]
    fn name_id_is_four_bytes() {
        // Large repositories keep a NameId for every tree entry.
        assert_eq!(std::mem::size_of::<NameId>(), 4);
        assert_eq!(std::mem::size_of::<Option<NameId>>(), 8);
    }
}
