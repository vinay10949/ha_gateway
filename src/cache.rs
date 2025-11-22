//used parking_lot::RwLock over std::sync::RwLock as  prior one is designed for speed, fairness, no poisioning and has lower memory usage

use lru_time_cache::LruCache;
use parking_lot::RwLock;
use std::time::Duration;

const CACHE_TTL: Duration = Duration::from_secs(2);
const CACHE_CAPACITY: usize = 1000;

/// LruCache with time-based expiration
pub struct Cache {
    store: RwLock<LruCache<String, serde_json::Value>>,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            store: RwLock::new(LruCache::with_expiry_duration_and_capacity(
                CACHE_TTL,
                CACHE_CAPACITY,
            )),
        }
    }

    pub fn get(&self, key: &str) -> Option<serde_json::Value> {
        let mut store = self.store.write();
        if let Some(value) = store.get(key) {
            Some(value.clone())
        } else {
            None
        }
    }

    pub fn put(&self, key: String, value: serde_json::Value) {
        let mut store = self.store.write();
        store.insert(key.clone(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_put_and_get() {
        let cache = Cache::new();
        let key = "test_key".to_string();
        let value = serde_json::json!({"result": "0x1234"});

        cache.put(key.clone(), value.clone());
        let cached = cache.get(&key);

        assert!(cached.is_some());
        assert_eq!(cached.unwrap(), value);
    }

    #[test]
    fn test_cache_miss() {
        let cache = Cache::new();
        let result = cache.get("invalid_key");
        assert!(result.is_none());
    }

    #[test]
    fn test_cache_expiry() {
        let cache = Cache::new();
        let key = "expired_key".to_string();
        let value = serde_json::json!({"result": "0x1234"});

        cache.put(key.clone(), value);
        assert!(cache.get(&key).is_some());

        std::thread::sleep(Duration::from_secs(3));
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn test_cache_lru_eviction() {
        // Create a cache with small capacity for testing
        let cache = Cache {
            store: RwLock::new(LruCache::with_expiry_duration_and_capacity(
                Duration::from_secs(60),
                2,
            )),
        };

        cache.put("key1".to_string(), serde_json::json!("value1"));
        cache.put("key2".to_string(), serde_json::json!("value2"));
        cache.put("key3".to_string(), serde_json::json!("value3"));

        assert!(cache.get("key1").is_none()); //Evicted
        assert!(cache.get("key2").is_some());
        assert!(cache.get("key3").is_some());
    }
}
