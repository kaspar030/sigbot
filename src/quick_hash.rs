use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub trait QuickHash {
    fn quick_hash(&self) -> u64;
}

impl<T: Hash> QuickHash for T {
    fn quick_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}
