//! A map with the property order of a plain JavaScript object.
//!
//! The Reference Gateway keeps request headers, query parameters and middleware headers in
//! plain objects, whose keys iterate in a fixed order: keys that are array indices (canonical
//! decimal integers from 0 to 2^32 - 2) first, in ascending numeric order, then every other key
//! in insertion order. Assigning to an existing key keeps its position.

use std::collections::{BTreeMap, HashMap};

/// Whether `key` is an array index, which JavaScript orders before other keys.
pub(crate) fn is_array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > 10 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes.len() > 1 && bytes[0] == b'0' {
        return None;
    }
    let value: u64 = key.parse().ok()?;
    if value <= u64::from(u32::MAX - 1) {
        u32::try_from(value).ok()
    } else {
        None
    }
}

/// An ordered map with JavaScript object key order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JsObject<V> {
    indices: BTreeMap<u32, V>,
    names: Vec<(String, V)>,
    positions: HashMap<String, usize>,
}

impl<V> Default for JsObject<V> {
    fn default() -> Self {
        Self {
            indices: BTreeMap::new(),
            names: Vec::new(),
            positions: HashMap::new(),
        }
    }
}

impl<V> JsObject<V> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        match is_array_index(key) {
            Some(index) => self.indices.get(&index),
            None => self.positions.get(key).map(|&i| &self.names[i].1),
        }
    }

    pub(crate) fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        match is_array_index(key) {
            Some(index) => self.indices.get_mut(&index),
            None => match self.positions.get(key) {
                Some(&i) => Some(&mut self.names[i].1),
                None => None,
            },
        }
    }

    /// `object[key] = value`.
    pub(crate) fn insert(&mut self, key: String, value: V) {
        if let Some(index) = is_array_index(&key) {
            self.indices.insert(index, value);
        } else if let Some(&i) = self.positions.get(&key) {
            self.names[i].1 = value;
        } else {
            self.positions.insert(key.clone(), self.names.len());
            self.names.push((key, value));
        }
    }

    /// `delete object[key]`.
    pub(crate) fn remove(&mut self, key: &str) -> Option<V> {
        if let Some(index) = is_array_index(key) {
            return self.indices.remove(&index);
        }
        let i = self.positions.remove(key)?;
        let (_, value) = self.names.remove(i);
        for position in self.positions.values_mut() {
            if *position > i {
                *position -= 1;
            }
        }
        Some(value)
    }

    pub(crate) fn len(&self) -> usize {
        self.indices.len() + self.names.len()
    }

    /// Entries in JavaScript iteration order.
    pub(crate) fn into_entries(self) -> Vec<(String, V)> {
        let mut out: Vec<(String, V)> = self
            .indices
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        out.extend(self.names);
        out
    }

    /// Entries in JavaScript iteration order, borrowed.
    pub(crate) fn entries(&self) -> Vec<(String, &V)> {
        let mut out: Vec<(String, &V)> = self
            .indices
            .iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        out.extend(self.names.iter().map(|(k, v)| (k.clone(), v)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_indices_are_canonical_integers_below_2_pow_32_minus_1() {
        assert_eq!(is_array_index("0"), Some(0));
        assert_eq!(is_array_index("42"), Some(42));
        assert_eq!(is_array_index("4294967294"), Some(4_294_967_294));
        for key in [
            "",
            "01",
            "-1",
            "1.5",
            "4294967295",
            "99999999999",
            "a1",
            " 1",
        ] {
            assert_eq!(is_array_index(key), None, "{key:?}");
        }
    }

    #[test]
    fn iterates_indices_first_then_insertion_order() {
        let mut object = JsObject::new();
        for key in ["b", "42", "a", "7", "01", "b"] {
            object.insert(key.to_string(), key.len());
        }
        let keys: Vec<String> = object.into_entries().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["7", "42", "b", "a", "01"]);
    }

    #[test]
    fn assignment_keeps_position_and_delete_removes() {
        let mut object = JsObject::new();
        object.insert("a".to_string(), 1);
        object.insert("b".to_string(), 2);
        object.insert("a".to_string(), 3);
        assert_eq!(object.remove("a"), Some(3));
        object.insert("a".to_string(), 4);
        assert_eq!(object.get("b"), Some(&2));
        let entries = object.into_entries();
        assert_eq!(entries, [("b".to_string(), 2), ("a".to_string(), 4)]);
    }
}
