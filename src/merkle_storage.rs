//! # MerkleStorage
//!
//! Storage for key/values with git-like semantics and history.
//!
//! # Data Structure
//! A storage with just one key `a/b/c` and its corresponding value `8` is represented like this:
//!
//! ``
//! [commit] ---> [tree1] --a--> [tree2] --b--> [tree3] --c--> [blob_8]
//! ``
//!
//! The db then contains the following:
//! ```no_compile
//! <hash_of_blob; blob8>
//! <hash_of_tree3, tree3>, where tree3 is a map {c: hash_blob8}
//! <hash_of_tree2, tree2>, where tree2 is a map {b: hash_of_tree3}
//! <hash_of_tree2, tree2>, where tree1 is a map {a: hash_of_tree2}
//! <hash_of_commit>; commit>, where commit points to the root tree (tree1)
//! ```
//!
//! Then, when looking for a path a/b/c in a spcific commit, we first get the hash of the root tree
//! from the commit, then get the tree from the database, get the hash of "a", look it up in the db,
//! get the hash of "b" from that tree, load from db, then get the hash of "c" and retrieve the
//! final value.
//!
//!
//! Now, let's assume we want to add a path `X` also referencing the value `8`. That creates a new
//! tree that reuses the previous subtree for `a/b/c` and branches away from root for `X`:
//!
//! ```no_compile
//! [tree1] --a--> [tree2] --b--> [tree3] --c--> [blob_8]
//!                   ^                             ^
//!                   |                             |
//! [tree_X]----a-----                              |
//!     |                                           |
//!      ----------------------X--------------------
//! ```
//!
//! The following is added to the database:
//! ``
//! <hash_of_tree_X; tree_X>, where tree_X is a map {a: hash_of_tree2, X: hash_of_blob8}
//! ``
//!
//! Reference: https://git-scm.com/book/en/v2/Git-Internals-Git-Objects
use std::path::Path;
use rocksdb::{DB, Options, IteratorMode};
use std::hash::Hash;
use im_rc::OrdMap as OrdMap;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use crypto::digest::Digest;
use crypto::blake2b::Blake2b;
use failure::Fail;

pub type ContextKey = Vec<String>;
pub type ContextValue = Vec<u8>;
pub type EntryHash = Vec<u8>;

#[derive(Clone, Debug, Serialize, Deserialize)]
enum NodeKind {
    NonLeaf,
    Leaf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Node {
    node_kind: NodeKind,
    entry_hash: EntryHash,
}

type Tree = OrdMap<String, Node>;

#[derive(Debug, Hash, Clone, Serialize, Deserialize)]
struct Commit {
    parent_commit_hash: EntryHash,
    root_hash: EntryHash,
    time: u64,
    author: String,
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Entry {
    Tree(Tree),
    Blob(ContextValue),
    Commit(Commit),
}

pub struct MerkleStorage {
    current_stage_tree: Option<Tree>,
    db: DB,
    staged: HashMap<EntryHash, Entry>,
    last_commit: Option<Commit>,
}

const HASH_LEN: usize = 32;
static HASH_LEN_BYTES: [u8; 8] = (HASH_LEN as u64).to_be_bytes();

#[derive(Debug, Fail)]
pub enum StorageError {
    /// External libs errors
    #[fail(display = "RocksDB error: {:?}", error)]
    DBError { error: rocksdb::Error },
    #[fail(display = "Serialization error: {:?}", error)]
    SerializationError { error: bincode::Error },
    #[fail(display = "No root found for this commit!")]

    /// Internal unrecoverable bugs that should never occur
    CommitRootNotFound,
    #[fail(display = "Cannot commit without a predecessor!")]
    MissingAncestorCommit,
    #[fail(display = "There is a commit or three under key {:?}, but not a value!", key)]
    ValueIsNotABlob { key: String },
    #[fail(display = "Found wrong structure. Was looking for {}, but found {}", sought, found)]
    FoundUnexpectedStructure { sought: String, found: String },
    #[fail(display = "Entry not found!")]
    EntryNotFound,

    /// Wrong user input errors
    #[fail(display = "No value under key {:?}.", key)]
    ValueNotFound { key: String },
    #[fail(display = "Cannot search for an empty key.")]
    KeyEmpty,
}

impl From<rocksdb::Error> for StorageError {
    fn from(error: rocksdb::Error) -> Self { StorageError::DBError { error } }
}

impl From<bincode::Error> for StorageError {
    fn from(error: bincode::Error) -> Self { StorageError::SerializationError { error } }
}

impl MerkleStorage {
    pub fn new(db: DB) -> Self {
        MerkleStorage {
            db,
            staged: HashMap::new(),
            current_stage_tree: None,
            last_commit: None,
        }
    }

    /// Get value. Staging area is checked first, then last (checked out) commit.
    pub fn get(&mut self, key: &ContextKey) -> Result<ContextValue, StorageError> {
        let root = &self.get_staged_root()?;
        let root_hash = self.hash_tree(&root);

        self.get_from_tree(&root_hash, key)
    }

    /// Get value from historical context identified by commit hash.
    pub fn get_history(&self, commit_hash: &EntryHash, key: &ContextKey) -> Result<ContextValue, StorageError> {
        let commit = self.get_commit(commit_hash)?;

        self.get_from_tree(&commit.root_hash, key)
    }

    fn get_from_tree(&self, root_hash: &EntryHash, key: &ContextKey) -> Result<ContextValue, StorageError> {
        let mut full_path = key.clone();
        let file = full_path.pop().ok_or(StorageError::KeyEmpty)?;
        let path = full_path;
        let root = self.get_tree(root_hash)?;
        let node = self.find_tree(&root, &path)?;

        let node = match node.get(&file) {
            None => return Err(StorageError::ValueNotFound { key: self.key_to_string(key) }),
            Some(entry) => entry,
        };
        match self.get_entry(&node.entry_hash)? {
            Entry::Blob(blob) => Ok(blob),
            _ => Err(StorageError::ValueIsNotABlob { key: self.key_to_string(key) })
        }
    }

    /// Flush the staging area and and move to work on a certain commit from history.
    pub fn checkout(&mut self, context_hash: EntryHash) -> Result<(), StorageError> {
        let commit = self.get_commit(&context_hash)?;
        self.current_stage_tree = Some(self.get_tree(&commit.root_hash)?);
        self.last_commit = Some(commit);
        self.staged = HashMap::new();
        Ok(())
    }

    /// Take the current changes in the staging area, create a commit and persist all changes
    /// to database under the new commit. Return last commit if there are no changes, that is
    /// empty commits are not allowed.
    pub fn commit(&mut self,
                  time: u64,
                  author: String,
                  message: String
    ) -> Result<EntryHash, StorageError> {
        let staged_root = self.get_staged_root()?;
        let staged_root_hash = self.hash_tree(&staged_root);

//        let last_commit = match &self.last_commit {
////            None => vec![], //return Err(StorageError::MissingAncestorCommit),
//            Some(last_commit) => {
//                // if nothing changed, just return last commit's hash
////                if staged_root_hash == last_commit.root_hash {
////                    return Ok(self.hash_commit(last_commit));
////                }
//                last_commit
//            }
//        };

        let parent_commit_hash= self.last_commit.as_ref()
            .map_or(vec![], |c| self.hash_commit(&c));
        let new_commit = Commit {
            root_hash: staged_root_hash, parent_commit_hash, time, author, message,
        };
        let entry = Entry::Commit(new_commit.clone());
        self.put_to_staging_area(&self.hash_commit(&new_commit), entry.clone());
        self.persist_staged_entry_recursively(&entry)?;
        self.staged = HashMap::new();
        self.last_commit = Some(new_commit.clone());
        Ok(self.hash_commit(&new_commit))
    }

    /// Set key/val to the staging area.
    pub fn set(&mut self, key: ContextKey, value: ContextValue) -> Result<(), StorageError> {
        let root = self.get_staged_root()?;
        let new_root_hash = &self._set(&root, key, value)?;
        self.current_stage_tree = Some(self.get_tree(new_root_hash)?);
        Ok(())
    }

    fn _set(&mut self, root: &Tree, key: ContextKey, value: ContextValue) -> Result<EntryHash, StorageError> {
        let blob_hash = self.hash_blob(&value);
        self.put_to_staging_area(&blob_hash, Entry::Blob(value));
        let new_node = Node { entry_hash: blob_hash, node_kind: NodeKind::Leaf };
        self.compute_new_root_with_change(root, &key, Some(new_node))
    }

    /// Delete an item from the staging area.
    pub fn delete(&mut self, key: ContextKey) -> Result<(), StorageError> {
        let root = self.get_staged_root()?;
        let new_root_hash = &self._delete(&root, key)?;
        self.current_stage_tree = Some(self.get_tree(new_root_hash)?);
        Ok(())
    }

    fn _delete(&mut self, root: &Tree, key: ContextKey) -> Result<EntryHash, StorageError> {
        if key.is_empty() { return Ok(self.hash_tree(root)); }

        self.compute_new_root_with_change(root, &key, None)
    }

    /// Copy subtree under a new path.
    /// TODO Consider copying values!
    pub fn copy(&mut self, from_key: ContextKey, to_key: ContextKey) -> Result<(), StorageError> {
        let root = self.get_staged_root()?;
        let new_root_hash = &self._copy(&root, from_key, to_key)?;
        self.current_stage_tree = Some(self.get_tree(new_root_hash)?);
        Ok(())
    }

    fn _copy(&mut self, root: &Tree, from_key: ContextKey, to_key: ContextKey) -> Result<EntryHash, StorageError> {
        let source_tree = self.find_tree(root, &from_key)?;
        let source_tree_hash = self.hash_tree(&source_tree);
        Ok(self.compute_new_root_with_change(
            &root, &to_key, Some(self.get_non_leaf(source_tree_hash)))?)
    }

    /// Get a new tree with `new_entry_hash` put under given `key`.
    ///
    /// # Arguments
    ///
    /// * `root` - Tree to modify
    /// * `key` - path under which the changes takes place
    /// * `new_entry_hash` - None for deletion, Some for inserting a hash under the key.
    fn compute_new_root_with_change(&mut self,
                                    root: &Tree,
                                    key: &ContextKey,
                                    new_node: Option<Node>,
    ) -> Result<EntryHash, StorageError> {
        if key.is_empty() {
            return Ok(new_node.unwrap_or_else(
                || self.get_non_leaf(self.hash_tree(root))).entry_hash);
        }

        let last = key.last().unwrap();
        let path = key.clone().drain(0..key.len() - 1).collect();
        let mut tree = self.find_tree(root, &path)?;

        match new_node {
            None => tree.remove(last),
            Some(new_node) => {
                tree.insert(last.clone(), new_node)
            }
        };

        let new_tree_hash = self.hash_tree(&tree);
        self.put_to_staging_area(&new_tree_hash, Entry::Tree(tree));

        self.compute_new_root_with_change(
            root, &path, Some(self.get_non_leaf(new_tree_hash)))
    }

    /// Find tree by path. Return an empty tree if no tree under this path exists or if a blob
    /// (= value) is encountered along the way.
    ///
    /// # Arguments
    ///
    /// * `root` - reference to a tree in which we search
    /// * `key` - sought path
    fn find_tree(&self, root: &Tree, key: &ContextKey) -> Result<Tree, StorageError> {
        if key.is_empty() { return Ok(root.clone()); }

        let child_node = match root.get(key.first().unwrap()) {
            Some(hash) => hash,
            None => return Ok(Tree::new()),
        };

        match self.get_entry(&child_node.entry_hash)? {
            Entry::Tree(tree) => {
                self.find_tree(&tree, &key.clone().drain(1..).collect())
            }
            Entry::Blob(_) => return Ok(Tree::new()),
            Entry::Commit { .. } => Err(StorageError::FoundUnexpectedStructure {
                sought: "tree".to_string(),
                found: "commit".to_string(),
            })
        }
    }

    /// Get latest staged tree. If it's empty, init genesis  and return genesis root.
    fn get_staged_root(&mut self) -> Result<Tree, StorageError> {
        match &self.current_stage_tree {
            None => {
                let tree = Tree::new();
                self.put_to_staging_area(&self.hash_tree(&tree), Entry::Tree(tree.clone()));
                Ok(tree)
            }
            Some(tree) => Ok(tree.clone()),
        }
    }

    fn put_to_staging_area(&mut self, key: &Vec<u8>, value: Entry) {
        self.staged.insert(key.clone(), value);
    }

    /// Persists an entry and its descendants from staged area to disk.
    fn persist_staged_entry_recursively(&self, entry: &Entry) -> Result<(), StorageError> {
        self.db.put(
            self.hash_entry(entry),
            bincode::serialize(entry)?)?;

        match entry {
            Entry::Blob(_) => Ok(()),
            Entry::Tree(tree) => {
                // Go through all descendants and gather errors. Remap error if there is a failure
                // anywhere in the recursion paths. TODO: is revert possible?
                tree.iter().map(|(_, child_node)| {
                    match self.staged.get(&child_node.entry_hash) {
                        None => Ok(()),
                        Some(entry) => self.persist_staged_entry_recursively(entry),
                    }
                }).find_map(|res| {
                    match res {
                        Ok(_) => None,
                        Err(err) => Some(Err(err)),
                    }
                }).unwrap_or(Ok(()))
            }
            Entry::Commit(commit) => {
                match self.staged.get(&commit.root_hash) {
                    None => Err(StorageError::CommitRootNotFound),
                    Some(entry) => self.persist_staged_entry_recursively(entry),
                }
            }
        }
    }

    fn hash_entry(&self, entry: &Entry) -> EntryHash {
        match entry {
            Entry::Commit(commit) => self.hash_commit(&commit),
            Entry::Tree(tree) => self.hash_tree(&tree),
            Entry::Blob(blob) => self.hash_blob(blob),
        }
    }

    fn hash_commit(&self, commit: &Commit) -> EntryHash {
        let mut hasher = Blake2b::new(HASH_LEN);
        let mut out = vec![0; HASH_LEN];
        hasher.input(&HASH_LEN_BYTES);
        hasher.input(&commit.root_hash);
        if commit.parent_commit_hash == vec![] {
            hasher.input(&(0 as u64).to_be_bytes());
        } else {
            hasher.input(&(1 as u64).to_be_bytes());
            hasher.input(&(commit.parent_commit_hash.len() as u64).to_be_bytes());
            hasher.input(&commit.parent_commit_hash);
        }
        hasher.input(&(commit.time as u64).to_be_bytes());
        hasher.input(&(commit.author.len() as u64).to_be_bytes());
        hasher.input(&commit.author.clone().into_bytes());
        hasher.input(&(commit.message.len() as u64).to_be_bytes());
        hasher.input(&commit.message.clone().into_bytes());
        hasher.result(&mut out);
        out
    }

    fn hash_tree(&self, tree: &Tree) -> EntryHash {
        let mut hasher = Blake2b::new(HASH_LEN);
        let mut out = vec![0; HASH_LEN];

//        println!("HASING A TREE...");
        hasher.input(&(tree.len() as u64).to_be_bytes());
//        println!("len of tree: {:x?}", (tree.len() as u64).to_be_bytes());
        // TODO use parallel iterator?
//        for (k, v) in tree {
        tree.iter().for_each(|(k, v)| {
            hasher.input(&self.encode_irmin_node_kind(&v.node_kind));
//            println!("child node kind encoded {:x?}", &self.encode_irmin_node_kind(&v.node_kind));
            hasher.input(&[k.len() as u8]);
//            println!("key len {:x?}", [k.len() as u8]);
            hasher.input_str(k);
//            println!("key bytes {:x?}", k.as_bytes());
            hasher.input(&HASH_LEN_BYTES);
//            println!("len of hash: {:x?}", (v.entry_hash.len() as u64).to_be_bytes());
            hasher.input(&v.entry_hash);
//            println!("entry hash bytes {:x?}", v.entry_hash);
        });
        hasher.result(&mut out);
//        println!("HASHED TREE {:x?}", out);
        out
    }

    fn hash_blob(&self, blob: &ContextValue) -> EntryHash {
        let mut hasher = Blake2b::new(HASH_LEN);
        let mut out = vec![0; HASH_LEN];
        hasher.input(&(blob.len() as u64).to_be_bytes());
//        println!("blob len: {:x?}", (blob.len() as u64).to_be_bytes());
        hasher.input(blob);
//        println!("blob: {:x?}", blob);
        hasher.result(&mut out);
//        println!("BLOB HASH: {:x?}", out);
        out
    }

    fn encode_irmin_node_kind(&self, kind: &NodeKind) -> Vec<u8> {
        match kind {
            NodeKind::NonLeaf => vec![0, 0, 0, 0, 0, 0, 0, 0],
            NodeKind::Leaf => vec![255, 0, 0, 0, 0, 0, 0, 0],
        }
    }

    #[allow(dead_code)]
    pub fn print_db(&self) { // Dev/debug method
        self.db.iterator(IteratorMode::Start).take(1_000).for_each(|(k, v)| {
            let val: Entry = bincode::deserialize(&*v).unwrap();
            println!("{:x?} --- {:x?}", k, val);
        });
    }

    fn get_tree(&self, hash: &EntryHash) -> Result<Tree, StorageError> {
        match self.get_entry(hash)? {
            Entry::Tree(tree) => Ok(tree),
            Entry::Blob(_) => Err(StorageError::FoundUnexpectedStructure {
                sought: "tree".to_string(),
                found: "blob".to_string(),
            }),
            Entry::Commit { .. } => Err(StorageError::FoundUnexpectedStructure {
                sought: "tree".to_string(),
                found: "commit".to_string(),
            }),
        }
    }

    fn get_commit(&self, hash: &EntryHash) -> Result<Commit, StorageError> {
        match self.get_entry(hash)? {
            Entry::Commit(commit) => Ok(commit),
            Entry::Tree(_) => Err(StorageError::FoundUnexpectedStructure {
                sought: "commit".to_string(),
                found: "tree".to_string(),
            }),
            Entry::Blob(_) => Err(StorageError::FoundUnexpectedStructure {
                sought: "commit".to_string(),
                found: "blob".to_string(),
            }),
        }
    }

    fn get_entry(&self, hash: &EntryHash) -> Result<Entry, StorageError> {
        match self.staged.get(hash) {
            None => {
                let entry_bytes = self.db.get(hash)?;
                match entry_bytes {
                    None => Err(StorageError::EntryNotFound),
                    Some(entry_bytes) => Ok(bincode::deserialize(&entry_bytes)?),
                }
            }
            Some(entry) => Ok(entry.clone()),
        }
    }

    fn get_non_leaf(&self, hash: EntryHash) -> Node {
        Node { node_kind: NodeKind::NonLeaf, entry_hash: hash }
    }

    pub fn get_db<P: AsRef<Path>>(path: P) -> DB {
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);

        DB::open(&db_opts, path).unwrap()
    }

    fn key_to_string(&self, key: &ContextKey) -> String {
        key.clone().join("/")
    }
}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /*
    * Tests need to run sequentially, otherwise they will try to open RocksDB at the same time.
    */
    lazy_static! {
        static ref SYNC: Mutex<()> = Mutex::new(());
    }

    fn get_db_name() -> &'static str { "_merkle_db_test" }
    fn get_db() -> DB { MerkleStorage::get_db(get_db_name()) }
    fn get_storage() -> MerkleStorage { MerkleStorage::new(get_db()) }
    fn clean_db() {
        let _ = DB::destroy(&Options::default(), get_db_name());
        let _ = fs::remove_dir_all(get_db_name());
    }

    #[test]
    fn test_tree_hash() {
        let _db_sync = SYNC.lock().unwrap();
        let mut storage = get_storage();
        storage.set(vec!["a".to_string(), "foo".to_string()], vec![97, 98, 99]); // abc
        storage.set(vec!["b".to_string(), "boo".to_string()], vec![97, 98]);
        storage.set(vec!["a".to_string(), "aaa".to_string()], vec![97, 98, 99, 100]);
        storage.set(vec!["x".to_string()], vec![97]);
        storage.set(vec!["one".to_string(), "two".to_string(), "three".to_string()], vec![97]);
        let tree = storage.current_stage_tree.clone().unwrap().clone();

        let hash = storage.hash_tree(&tree);

        assert_eq!([0xDB, 0xAE, 0xD7, 0xB6], hash[0..4]);
    }

    #[test]
    fn test_commit_hash() {
        let _db_sync = SYNC.lock().unwrap();
        let mut storage = get_storage();
        storage.set(vec!["a".to_string()], vec![97, 98, 99]);

        let commit = storage.commit(
            0, "Tezos".to_string(), "Genesis".to_string());

        assert_eq!([0xCF, 0x95, 0x18, 0x33], commit.unwrap()[0..4]);

        storage.set(vec!["data".to_string(), "x".to_string()], vec![97]);
        let commit = storage.commit(
            0, "Tezos".to_string(), "".to_string());

        assert_eq!([0xCA, 0x7B, 0xC7, 0x02], commit.unwrap()[0..4]);
        // full irmin hash: ca7bc7022ffbd35acc97f7defb00c486bb7f4d19a2d62790d5949775eb74f3c8
    }

    #[test]
    fn get_test() {
        let _db_sync = SYNC.lock().unwrap();
        clean_db();

        let commit1;
        let commit2;
        let key_abc: ContextKey = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let key_abx: ContextKey = vec!["a".to_string(), "b".to_string(), "x".to_string()];
        let key_eab: ContextKey = vec!["e".to_string(), "a".to_string(), "b".to_string()];
        let key_az: ContextKey = vec!["a".to_string(), "z".to_string()];
        let key_d: ContextKey = vec!["d".to_string()];

        {
            let mut storage = get_storage();
            storage.set(key_abc.clone(), vec![1u8, 2u8]);
            storage.set(key_abx.clone(), vec![3u8]);
            assert_eq!(storage.get(&key_abc).unwrap(), vec![1u8, 2u8]);
            assert_eq!(storage.get(&key_abx).unwrap(), vec![3u8]);
            commit1 = storage.commit(0, "".to_string(), "".to_string()).unwrap();

            storage.set(key_az.clone(), vec![4u8]);
            storage.set(key_abx.clone(), vec![5u8]);
            storage.set(key_d.clone(), vec![6u8]);
            storage.set(key_eab.clone(), vec![7u8]);
            assert_eq!(storage.get(&key_abx).unwrap(), vec![5u8]);
            commit2 = storage.commit(0, "".to_string(), "".to_string()).unwrap();
        }

        let storage = get_storage();
        assert_eq!(storage.get_history(&commit1, &key_abc).unwrap(), vec![1u8, 2u8]);
        assert_eq!(storage.get_history(&commit1, &key_abx).unwrap(), vec![3u8]);
        assert_eq!(storage.get_history(&commit2, &key_abx).unwrap(), vec![5u8]);
        assert_eq!(storage.get_history(&commit2, &key_az).unwrap(), vec![4u8]);
        assert_eq!(storage.get_history(&commit2, &key_d).unwrap(), vec![6u8]);
        assert_eq!(storage.get_history(&commit2, &key_eab).unwrap(), vec![7u8]);
    }

    #[test]
    fn test_copy() {
        let _db_sync = SYNC.lock().unwrap();
        clean_db();

        let mut storage = get_storage();
        let key_abc: ContextKey = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        storage.set(key_abc.clone(), vec![1 as u8]);
        storage.copy(vec!["a".to_string()], vec!["z".to_string()]);

        assert_eq!(
            vec![1 as u8],
            storage.get(&vec!["z".to_string(), "b".to_string(), "c".to_string()]).unwrap());
        // TODO test copy over commits
    }

    #[test]
    fn test_delete() {
        let _db_sync = SYNC.lock().unwrap();
        clean_db();

        let mut storage = get_storage();
        let key_abc: ContextKey = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let key_abx: ContextKey = vec!["a".to_string(), "b".to_string(), "x".to_string()];
        storage.set(key_abc.clone(), vec![2 as u8]);
        storage.set(key_abx.clone(), vec![3 as u8]);
        storage.delete(key_abx.clone());
        let commit1 = storage.commit(0, "".to_string(), "".to_string()).unwrap();

        assert!(storage.get_history(&commit1, &key_abx).is_err());
    }

    #[test]
    fn test_deleted_entry_available() {
        let _db_sync = SYNC.lock().unwrap();
        clean_db();

        let mut storage = get_storage();
        let key_abc: ContextKey = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        storage.set(key_abc.clone(), vec![2 as u8]);
        let commit1 = storage.commit(0, "".to_string(), "".to_string()).unwrap();
        storage.delete(key_abc.clone());
        let _commit2 = storage.commit(0, "".to_string(), "".to_string()).unwrap();

        assert_eq!(vec![2 as u8], storage.get_history(&commit1, &key_abc).unwrap());
    }

    #[test]
    fn test_delete_in_separate_commit() {
        let _db_sync = SYNC.lock().unwrap();
        clean_db();

        let mut storage = get_storage();
        let key_abc: ContextKey = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let key_abx: ContextKey = vec!["a".to_string(), "b".to_string(), "x".to_string()];
        storage.set(key_abc.clone(), vec![2 as u8]).unwrap();
        storage.set(key_abx.clone(), vec![3 as u8]).unwrap();
        storage.commit(0, "".to_string(), "".to_string()).unwrap();

        storage.delete(key_abx.clone());
        let commit2 = storage.commit(
            0, "".to_string(), "".to_string()).unwrap();

        assert!(storage.get_history(&commit2, &key_abx).is_err());
    }

    #[test]
    fn test_checkout() {
        let _db_sync = SYNC.lock().unwrap();
        clean_db();

        let commit1;
        let commit2;
        let key_abc: ContextKey = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let key_abx: ContextKey = vec!["a".to_string(), "b".to_string(), "x".to_string()];

        {
            let mut storage = get_storage();
            storage.set(key_abc.clone(), vec![1u8]).unwrap();
            storage.set(key_abx.clone(), vec![2u8]).unwrap();
            commit1 = storage.commit(0, "".to_string(), "".to_string()).unwrap();

            storage.set(key_abc.clone(), vec![3u8]).unwrap();
            storage.set(key_abx.clone(), vec![4u8]).unwrap();
            commit2 = storage.commit(0, "".to_string(), "".to_string()).unwrap();
        }

        let mut storage = get_storage();
        storage.checkout(commit1);
        assert_eq!(storage.get(&key_abc).unwrap(), vec![1u8]);
        assert_eq!(storage.get(&key_abx).unwrap(), vec![2u8]);
        // this set be wiped by checkout
        storage.set(key_abc.clone(), vec![8u8]).unwrap();

        storage.checkout(commit2);
        assert_eq!(storage.get(&key_abc).unwrap(), vec![3u8]);
        assert_eq!(storage.get(&key_abx).unwrap(), vec![4u8]);
    }

    // TODO talk to Brano about this
//    #[test]
//    fn test_idempotence() {
//        let _db_sync = SYNC.lock().unwrap();
//        clean_db();
//
//        let mut storage = get_storage();
//
//        let key1: ContextKey = vec!["a".to_string(), "b".to_string()];
//        storage.set(key1.clone(), vec![2 as u8]).unwrap();
//        let commit1 = storage.commit(
//            0, "".to_string(), "".to_string()).unwrap();
//
//        storage.set(key1, vec![2 as u8]).unwrap();
//        let commit2 = storage.commit(
//            0, "".to_string(), "".to_string()).unwrap();
//
//        assert_eq!(commit1, commit2);
//    }

    #[test]
    fn test_persistence_over_reopens() {
        let _db_sync = SYNC.lock().unwrap();
        { clean_db(); }

        let key_abc: ContextKey = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let commit1;
        {
            let mut storage = get_storage();
            let key_abx: ContextKey = vec!["a".to_string(), "b".to_string(), "x".to_string()];
            storage.set(key_abc.clone(), vec![2 as u8]).unwrap();
            storage.set(key_abx.clone(), vec![3 as u8]).unwrap();
            commit1 = storage.commit(0, "".to_string(), "".to_string()).unwrap();
        }

        let storage = get_storage();
        assert_eq!(vec![2 as u8], storage.get_history(&commit1, &key_abc).unwrap());
    }

    #[test]
    fn test_get_errors() {
        let _db_sync = SYNC.lock().unwrap();
        { clean_db(); }

        let mut storage = get_storage();

        let res = storage.get(&vec![]);
        assert!(if let StorageError::KeyEmpty = res.err().unwrap() { true } else { false });

        let res = storage.get(&vec!["a".to_string()]);
        println!("wtf {:?}", res);
        assert!(if let StorageError::ValueNotFound { .. } = res.err().unwrap() { true } else { false });
    }


    #[test]
    fn test_db_error() { // Test a DB error by writing into a read-only database.
        let _db_sync = SYNC.lock().unwrap();
        {
            clean_db();
            get_storage();
        }

        let db = DB::open_for_read_only(
            &Options::default(), get_db_name(), true).unwrap();
        let mut storage = MerkleStorage::new(db);
        storage.set(vec!["a".to_string()], vec![1u8]);
        let res = storage.commit(
            0, "".to_string(), "".to_string());

        assert!(if let StorageError::DBError { .. } = res.err().unwrap() { true } else { false });
    }
}