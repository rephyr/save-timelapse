//! Interning for prototype names.
//!
//! A base has tens of distinct prototype names against hundreds of thousands
//! of entities, so storing the name on each one is a heap allocation per
//! entity for one of a few dozen repeated strings. Everything downstream
//! (replayed world state, the viewer's render frames) refers to names by a
//! `u16` instead and looks the string up only when it has to.

use std::collections::HashMap;

pub type NameId = u16;

#[derive(Debug, Clone, Default)]
pub struct NameTable {
    names: Vec<String>,
    ids: HashMap<String, NameId>,
}

impl NameTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, name: &str) -> NameId {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = NameId::try_from(self.names.len()).expect("more than u16::MAX distinct names");
        self.names.push(name.to_string());
        self.ids.insert(name.to_string(), id);
        id
    }

    pub fn get(&self, name: &str) -> Option<NameId> {
        self.ids.get(name).copied()
    }

    pub fn name(&self, id: NameId) -> &str {
        &self.names[id as usize]
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_names_intern_to_one_id() {
        let mut table = NameTable::new();
        let belt = table.intern("transport-belt");
        assert_eq!(table.intern("transport-belt"), belt);
        assert_ne!(table.intern("pipe"), belt);
        assert_eq!(table.len(), 2);
        assert_eq!(table.name(belt), "transport-belt");
    }

    #[test]
    fn ids_are_handed_out_densely_from_zero() {
        let mut table = NameTable::new();
        assert_eq!(table.intern("a"), 0);
        assert_eq!(table.intern("b"), 1);
        assert_eq!(table.intern("a"), 0);
        assert_eq!(table.intern("c"), 2);
    }

    #[test]
    fn get_does_not_intern() {
        let mut table = NameTable::new();
        table.intern("pipe");
        assert_eq!(table.get("pipe"), Some(0));
        assert_eq!(table.get("absent"), None);
        assert_eq!(table.len(), 1);
    }
}
