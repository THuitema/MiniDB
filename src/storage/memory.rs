/*
Version 1: In-memory hashmap
 */

use std::collections::HashMap;

pub struct InMemoryDB {
    data: HashMap<String, Vec<u8>>
}

impl InMemoryDB {
    pub fn new() -> Self {
        Self {
            data: HashMap::new()
        }
    }

    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.data.get(key).map(|v| v.as_slice())
    }

    pub fn put(&mut self, key: String, value: Vec<u8>) {
        self.data.insert(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::InMemoryDB;

    #[test]
    fn missing_key_returns_none() {
        let db = InMemoryDB::new();
        assert_eq!(db.get("missing"), None);
    }

    #[test]
    fn overwrite_key_replaces_value() {
        let mut db = InMemoryDB::new();
        db.put("name".to_string(), b"Bob".to_vec());        
        assert_eq!(db.get("name"), Some(&b"Bob"[..]));
        db.put("name".to_string(), b"Tom".to_vec());        
        assert_eq!(db.get("name"), Some(&b"Tom"[..]));
    }
}