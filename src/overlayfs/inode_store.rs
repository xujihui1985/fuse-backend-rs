// Copyright (C) 2023 Ant Group. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io::{Error, Result};
use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
};

use super::{Inode, OverlayInode, VFS_MAX_INO};

use radix_trie::Trie;

pub(crate) struct RemovedInode {
    pub(crate) node: Arc<OverlayInode>,
    pub(crate) was_active: bool,
}

pub struct InodeStore {
    // Active inodes.
    inodes: HashMap<Inode, Arc<OverlayInode>>,
    // Deleted inodes which were unlinked but have non zero lookup count.
    deleted: HashMap<Inode, Arc<OverlayInode>>,
    // Last generation assigned to each inode number.
    generations: HashMap<Inode, u64>,
    // Path to inode mapping, used to reserve inode number for same path.
    path_mapping: Trie<String, Inode>,
    next_inode: u64,
}

impl InodeStore {
    pub(crate) fn new() -> Self {
        Self {
            inodes: HashMap::new(),
            deleted: HashMap::new(),
            generations: HashMap::new(),
            path_mapping: Trie::new(),
            next_inode: 1,
        }
    }

    pub(crate) fn alloc_unique_inode(&mut self) -> Result<Inode> {
        // Iter VFS_MAX_INO times to find a free inode number.
        let mut ino = self.next_inode;
        for _ in 0..VFS_MAX_INO {
            if ino > VFS_MAX_INO {
                ino = 1;
            }
            if !self.inodes.contains_key(&ino) && !self.deleted.contains_key(&ino) {
                self.next_inode = ino + 1;
                return Ok(ino);
            }
            ino += 1;
        }
        error!("reached maximum inode number: {}", VFS_MAX_INO);
        Err(Error::other(format!(
            "maximum inode number {} reached",
            VFS_MAX_INO
        )))
    }

    pub(crate) fn alloc_inode(&mut self, path: &String) -> Result<Inode> {
        if let Some(inode) = self.path_mapping.get(path) {
            // Reuse the path's inode only while it is still active. If the old inode is in
            // `deleted`, the kernel may still hold dentries for the old nodeid/generation pair.
            if self.inodes.contains_key(inode) {
                return Ok(*inode);
            }
        }

        self.alloc_unique_inode()
    }

    pub(crate) fn insert_inode(&mut self, inode: Inode, node: Arc<OverlayInode>) {
        let generation = node.generation.load(Ordering::Relaxed);
        if generation == 0 {
            let generation = *self.generations.entry(inode).or_insert(1);
            node.generation.store(generation, Ordering::Relaxed);
        } else {
            self.generations
                .entry(inode)
                .and_modify(|current| *current = (*current).max(generation))
                .or_insert(generation);
        }
        self.path_mapping.insert(node.path.clone(), inode);
        self.inodes.insert(inode, node);
    }

    pub(crate) fn insert_path(&mut self, inode: Inode, path: String) {
        self.path_mapping.insert(path, inode);
    }

    pub(crate) fn get_inode(&self, inode: Inode) -> Option<Arc<OverlayInode>> {
        self.inodes.get(&inode).cloned()
    }

    pub(crate) fn get_deleted_inode(&self, inode: Inode) -> Option<Arc<OverlayInode>> {
        self.deleted.get(&inode).cloned()
    }

    pub(crate) fn inc_active_lookup(&self, inode: Inode, node: &Arc<OverlayInode>) -> Option<u64> {
        match self.inodes.get(&inode) {
            Some(active) if Arc::ptr_eq(active, node) => {
                if node.lookups.load(Ordering::Acquire) == 0
                    && node.link_paths.lock().unwrap().is_empty()
                {
                    return None;
                }

                Some(node.inc_lookup())
            }
            _ => None,
        }
    }

    pub(crate) fn forget_inode(&mut self, inode: Inode, count: u64) -> Option<RemovedInode> {
        if count == 0 {
            return None;
        }

        let node = match self.inodes.get(&inode) {
            Some(v) => v.clone(),
            None => match self.deleted.get(&inode) {
                Some(v) => v.clone(),
                None => return None,
            },
        };

        if node.dec_lookup(count) != 0 {
            return None;
        }

        if self.inodes.contains_key(&inode) && !node.link_paths.lock().unwrap().is_empty() {
            return None;
        }

        self.remove_inode(inode, None)
    }

    // Return the inode only if it's permanently deleted from both self.inodes and self.deleted_inodes.
    pub(crate) fn remove_inode(
        &mut self,
        inode: Inode,
        path_removed: Option<String>,
    ) -> Option<RemovedInode> {
        if let Some(path) = path_removed.as_ref() {
            self.path_mapping.remove(path);
        }

        let removed = match self.inodes.remove(&inode) {
            Some(v) => {
                // Refcount is not 0, we have to delay the removal.
                if v.lookups.load(Ordering::Acquire) > 0 {
                    self.deleted.insert(inode, v.clone());
                    return None;
                }
                self.retire_inode_generation(inode);
                Some(RemovedInode {
                    node: v,
                    was_active: true,
                })
            }
            None => {
                // If the inode is not in hash, it must be in deleted_inodes.
                match self.deleted.get(&inode) {
                    Some(v) => {
                        // Refcount is 0, the inode can be removed now.
                        if v.lookups.load(Ordering::Acquire) == 0 {
                            let removed = self.deleted.remove(&inode);
                            if removed.is_some() {
                                self.retire_inode_generation(inode);
                            }
                            removed.map(|node| RemovedInode {
                                node,
                                was_active: false,
                            })
                        } else {
                            // Refcount is not 0, the inode will be removed later.
                            None
                        }
                    }
                    None => None,
                }
            }
        };

        removed
    }

    pub(crate) fn remove_path(&mut self, path: &String) {
        self.path_mapping.remove(path);
    }

    fn retire_inode_generation(&mut self, inode: Inode) {
        let generation = self.generations.entry(inode).or_insert(1);
        *generation = generation.saturating_add(1).max(1);
    }

    // As a debug function, print all inode numbers in hash table.
    // This function consumes quite lots of memory, so it's disabled by default.
    #[allow(dead_code)]
    pub(crate) fn debug_print_all_inodes(&self) {
        // Convert the HashMap to Vector<(inode, pathname)>
        let mut all_inodes = self
            .inodes
            .iter()
            .map(|(inode, ovi)| (inode, ovi.path.clone(), ovi.lookups.load(Ordering::Relaxed)))
            .collect::<Vec<_>>();
        all_inodes.sort_by(|a, b| a.0.cmp(b.0));
        trace!("all active inodes: {:?}", all_inodes);

        let mut to_delete = self
            .deleted
            .iter()
            .map(|(inode, ovi)| (inode, ovi.path.clone(), ovi.lookups.load(Ordering::Relaxed)))
            .collect::<Vec<_>>();
        to_delete.sort_by(|a, b| a.0.cmp(b.0));
        trace!("all deleted inodes: {:?}", to_delete);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_alloc_unique() {
        let mut store = InodeStore::new();
        let empty_node = Arc::new(OverlayInode::new());
        store.insert_inode(1, empty_node.clone());
        store.insert_inode(2, empty_node.clone());
        store.insert_inode(VFS_MAX_INO - 1, empty_node.clone());

        let inode = store.alloc_unique_inode().unwrap();
        assert_eq!(inode, 3);
        assert_eq!(store.next_inode, 4);

        store.next_inode = VFS_MAX_INO - 1;
        let inode = store.alloc_unique_inode().unwrap();
        assert_eq!(inode, VFS_MAX_INO);

        let inode = store.alloc_unique_inode().unwrap();
        assert_eq!(inode, 3);
    }

    #[test]
    fn test_alloc_existing_path() {
        let mut store = InodeStore::new();
        let mut node_a = OverlayInode::new();
        node_a.path = "/a".to_string();
        store.insert_inode(1, Arc::new(node_a));
        let mut node_b = OverlayInode::new();
        node_b.path = "/b".to_string();
        let node_b = Arc::new(node_b);
        store.insert_inode(2, node_b.clone());
        assert_eq!(node_b.generation.load(Ordering::Relaxed), 1);
        let mut node_c = OverlayInode::new();
        node_c.path = "/c".to_string();
        store.insert_inode(VFS_MAX_INO - 1, Arc::new(node_c));

        let inode = store.alloc_inode(&"/a".to_string()).unwrap();
        assert_eq!(inode, 1);

        let inode = store.alloc_inode(&"/b".to_string()).unwrap();
        assert_eq!(inode, 2);

        let inode = store.alloc_inode(&"/c".to_string()).unwrap();
        assert_eq!(inode, VFS_MAX_INO - 1);

        let inode = store.alloc_inode(&"/notexist".to_string()).unwrap();
        assert_eq!(inode, 3);
    }

    #[test]
    fn test_remove_inode() {
        let mut store = InodeStore::new();
        let mut node_a = OverlayInode::new();
        node_a.lookups.fetch_add(1, Ordering::Relaxed);
        node_a.path = "/a".to_string();
        store.insert_inode(1, Arc::new(node_a));

        let mut node_b = OverlayInode::new();
        node_b.path = "/b".to_string();
        store.insert_inode(2, Arc::new(node_b));

        let mut node_c = OverlayInode::new();
        node_c.lookups.fetch_add(1, Ordering::Relaxed);
        node_c.path = "/c".to_string();
        store.insert_inode(VFS_MAX_INO - 1, Arc::new(node_c));

        let inode = store.alloc_inode(&"/new".to_string()).unwrap();
        assert_eq!(inode, 3);

        // Not existing.
        let inode = store.remove_inode(4, None);
        assert!(inode.is_none());

        // Existing but with non-zero refcount.
        let inode = store.remove_inode(1, None);
        assert!(inode.is_none());
        assert!(store.get_deleted_inode(1).is_some());
        assert!(store.path_mapping.get(&"/a".to_string()).is_some());

        // Remove again with file path.
        let inode = store.remove_inode(1, Some("/a".to_string()));
        assert!(inode.is_none());
        assert!(store.get_deleted_inode(1).is_some());
        assert!(store.path_mapping.get(&"/a".to_string()).is_none());

        // Node b has refcount 0, removing will be permanent.
        let inode = store.remove_inode(2, Some("/b".to_string()));
        assert!(inode.is_some());
        assert!(store.get_deleted_inode(2).is_none());
        assert!(store.path_mapping.get(&"/b".to_string()).is_none());

        // Allocate new inode, it should reuse inode 2 since inode 1 is still in deleted list.
        store.next_inode = 1;
        let inode = store.alloc_inode(&"/b".to_string()).unwrap();
        assert_eq!(inode, 2);
        let mut node_b2 = OverlayInode::new();
        node_b2.path = "/b".to_string();
        let node_b2 = Arc::new(node_b2);
        store.insert_inode(inode, node_b2.clone());
        assert_eq!(node_b2.generation.load(Ordering::Relaxed), 2);

        // The old inode for "/c" is still in the deleted table, so a new nodeid must be used.
        let inode = store.alloc_inode(&"/c".to_string()).unwrap();
        assert_eq!(inode, 3);
    }
}
