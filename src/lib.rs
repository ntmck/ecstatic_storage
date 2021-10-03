// Dynamically typed thread safe component storage. EcstaticStorage

//TODO:
// 
// All indices may need to be references instead of copy to avoid certain race conditions involving the removal of a component and index which is then read after removal
//  ...or we let the user deal with the issue of missing indices 
//
// Tests for wrapper functions: ecread, ecmodify, ecinsert, etc...
// Tests to confirm thread saftey of wrapper functions.
// Implement memory management function to clear up empty spaces.
// 
// Extract error messages into static strings for wrapper functions.
// ctrl+f TODO and remove when done.
// Document crate using built-in documentation.

#![feature(map_first_last)]

use std::any::{Any, TypeId};
use std::any::type_name;
use std::marker::{Send, Sync};
use std::collections::{HashMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::panic;
use std::collections::hash_map::DefaultHasher;
use std::time::{SystemTime, UNIX_EPOCH};
use std::hash::{Hash, Hasher};

//ignore these warnings as they are relevant for unit tests.
use std::sync::Barrier;
use std::thread;
use std::sync::mpsc::channel;


#[derive(Debug)]
pub enum ErrStorage {
    Empty(String),
    LenFunc(String),
    Read(String),
    GetIndex(String),
    Modify(String),
    Set(String),
    Insert(String),
}

//RwLock Component
pub type LComponent = RwLock<Box<dyn Any + Send + Sync>>;

//RwLock Component store
pub type LComponentStore = RwLock<Vec<LComponent>>;

//Arc-RwLock Component Storage {Arc<RwLock<HashMap<TypeId, Arc<RwLock<Vec<Arc<RwLock<Box<dyn Any + Send + Sync>>>>>>>>>}
pub type ALComponentStorage = Arc<RwLock<HashMap<TypeId, LComponentStore>>>;

pub const PACKED: usize = 0;
pub const FREE: usize = 1;
//Arc-RwLock index storage. Used for both indices indices and freed indices. [0] = indices, [1] = free
pub type ALIndices = Arc<RwLock<[HashMap<TypeId, RwLock<BTreeSet<usize>>>; 2]>>;

//Arc-RwLock length storage for keeping track of the number of non-empty slots in component storage.
pub type ALLengths = Arc<RwLock<HashMap<TypeId, RwLock<usize>>>>;

//Arc-Rwlock ownership tracking. id -> owned component types and its index. 
pub type ALOwnership = Arc<RwLock<HashMap<u64, RwLock<HashMap<TypeId, usize>>>>>;

//Required to modify a downcast_mut element after obtaining the lock.
pub type Modify<T> = fn(&mut T);

type ChLenFunc = fn(usize, usize) -> usize;

//An empty type for emptying component memory.
pub enum Empty { Empty }

pub struct EcstaticStorage {
    components: ALComponentStorage,
    indices: ALIndices,
    lengths: ALLengths,
    ownership: ALOwnership,
}
impl EcstaticStorage {
    const STORAGE_LOCK_ERROR_MSG: &'static str = "Failed to acquire storage lock.";
    const TYPE_NOT_FOUND_ERROR_MSG: &'static str = "Failed to find component of type:";
    const VECTOR_LOCK_ERROR_MSG: &'static str = "Failed to acquire vector lock.";
    const INDEX_OUT_OF_BOUNDS_ERROR_MSG: &'static str = "Index out of bounds for index and type ->";
    const ELEMENT_LOCK_ERROR_MSG: &'static str = "Failed to acquire element lock.";
    const DOWNCAST_ERROR_MSG: &'static str = "Failed to downcast value for type ->";
    const LEN_NOT_FOUND_ERROR_MSG: &'static str = "Failed to find length corresponding to type ->";

    pub fn new() -> EcstaticStorage {
        EcstaticStorage {
            components: Arc::new(RwLock::new(HashMap::new())),
            indices: Arc::new(RwLock::new([HashMap::new(), HashMap::new()])),
            lengths: Arc::new(RwLock::new(HashMap::new())),
            ownership: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn generate_key() -> u64 {
        let mut hasher = DefaultHasher::new();
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().hash(&mut hasher);
        hasher.finish()
    }

    pub fn ecinsert<T>(&self, id: u64, component: T) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        if let Ok(i) = self.insert::<T>(component) {
            self.check_initialized_ownership_for_id::<T>(&id);
            self.ownership
                .read().unwrap()
                .get(&id).unwrap()
                .write().unwrap()
                .insert(TypeId::of::<T>(), i);
            Ok(())
        } else { Err(ErrStorage::GetIndex(format!("Could not read component type: {} from id: {}. Index not found.\n", type_name::<T>(), id))) }
    }

    //inserts a component into storage by type and returns the index it was inserted into.
    fn insert<T>(&self, component: T) -> Result<usize, ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        self.check_initialized_component_vector::<T>();
        self.check_initialized_lengths::<T>();
        let len = self.len::<T>().unwrap();
        let i = self.get_index::<T>(len);
        if let Err(e) = panic::catch_unwind(|| {
            self.components
                .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
                .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::TYPE_NOT_FOUND_ERROR_MSG, type_name::<T>()))
                .write().expect(EcstaticStorage::VECTOR_LOCK_ERROR_MSG)
                .insert(i, RwLock::new(Box::new(component)));

            match self.change_len::<T>(1, |len, amount| -> usize {len + amount}) {
                Ok(_) => (),
                Err(e) => panic!("{:#?}", e),
            }
        }) {
            self.free_index::<T>(i);
            return Err(ErrStorage::Insert(format!("{:#?}", e)))
        }
        self.truncate_storage_to_fit::<T>();
        Ok(i)
    }

    pub fn ecset<T>(&self, id: u64, with: T) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {  
        if let Some(i) = self.get_owned_index_for_component::<T>(id){ self.set::<T>(i, with) } 
        else { Err(ErrStorage::GetIndex(format!("Could not read component type: {} from id: {}. Index not found.\n", type_name::<T>(), id))) }
    }

    //replaces a component at the given index with the given component.
    fn set<T>(&self, i: usize, component: T) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        self.check_initialized_component_vector::<T>();
        if let Err(e) = panic::catch_unwind(|| {
            *self.components
                .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
                .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::TYPE_NOT_FOUND_ERROR_MSG, type_name::<T>()))
                .read().expect(EcstaticStorage::VECTOR_LOCK_ERROR_MSG)
                .get(i).expect(&format!("{} index: {}, type: {}", EcstaticStorage::INDEX_OUT_OF_BOUNDS_ERROR_MSG, i, type_name::<T>()))
                .write().expect(EcstaticStorage::ELEMENT_LOCK_ERROR_MSG) = Box::new(component);
        }) {
            return Err(ErrStorage::Set(format!("{:#?}", e)))
        }
        self.truncate_storage_to_fit::<T>();
        Ok(())
    }

    pub fn ecmodify<T>(&self, id: u64, modify: Modify<T>) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + Copy + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        if let Some(i) = self.get_owned_index_for_component::<T>(id) { self.modify::<T>(i, modify) } 
        else { Err(ErrStorage::GetIndex(format!("Could not read component type: {} from id: {}. Index not found.\n", type_name::<T>(), id))) }
    }

    //modifies the component using the provided function.
    fn modify<T>(&self, i: usize, modify: Modify<T>) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        self.check_initialized_component_vector::<T>();
        if !self.is_empty::<T>(i)? {
            if let Err(e) = panic::catch_unwind(|| {
                match self.components
                    .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
                    .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::TYPE_NOT_FOUND_ERROR_MSG, type_name::<T>()))
                    .read().expect(EcstaticStorage::VECTOR_LOCK_ERROR_MSG)
                    .get(i).expect(&format!("{} index: {}, type: {}", EcstaticStorage::INDEX_OUT_OF_BOUNDS_ERROR_MSG, i, type_name::<T>()))
                    .write() 
                {
                    Ok(mut value) => modify(value.downcast_mut::<T>().expect(&format!("{} type: {}", EcstaticStorage::DOWNCAST_ERROR_MSG, type_name::<T>()))),
                    Err(e) => panic!("{}", EcstaticStorage::ELEMENT_LOCK_ERROR_MSG)
                }
            }) {
                return Err(ErrStorage::Modify(format!("{:#?}", e)))
            }
            self.truncate_storage_to_fit::<T>();
            Ok(())
        } else {
            return Err(ErrStorage::Empty(format!("EcstaticStorage::modify {}", type_name::<T>())))
        }
    }

    pub fn ecread<T>(&self, id: u64) -> Result<T, ErrStorage>
    where T: Any + Send + Sync + Copy + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        if let Some(i) = self.get_owned_index_for_component::<T>(id) { self.read::<T>(i) }
        else { Err(ErrStorage::GetIndex(format!("Could not read component type: {} from id: {}. Index not found.\n", type_name::<T>(), id))) }
    }

    //reads the component at given index.
    fn read<T>(&self, i: usize) -> Result<T, ErrStorage>
    where T: Any + Send + Sync + Copy + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        self.check_initialized_component_vector::<T>();
        if !self.is_empty::<T>(i)? {
            match panic::catch_unwind(|| -> T {
                *self.components
                    .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
                    .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::TYPE_NOT_FOUND_ERROR_MSG, type_name::<T>()))
                    .read().expect(EcstaticStorage::VECTOR_LOCK_ERROR_MSG)
                    .get(i).expect(&format!("{} index: {}, type: {}", EcstaticStorage::INDEX_OUT_OF_BOUNDS_ERROR_MSG, i, type_name::<T>()))
                    .read().expect(EcstaticStorage::ELEMENT_LOCK_ERROR_MSG)
                    .downcast_ref::<T>().expect(&format!("{} type: {}", EcstaticStorage::DOWNCAST_ERROR_MSG, type_name::<T>()))

            }) {
                Ok(v) => { self.truncate_storage_to_fit::<T>(); Ok(v) },
                Err(e) => Err(ErrStorage::Read(format!("{:#?}", e)))
            }
        } else {
            return Err(ErrStorage::Empty(format!("EcstaticStorage::read {}", type_name::<T>())))
        }
    }

    pub fn ecempty<T>(&self, id: u64) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        if let Some(i) = self.get_owned_index_for_component::<T>(id)
        {
            print!("emptying: i: {}\n", i);
            self.ownership
                .read().unwrap()
                .get(&id).unwrap()
                .write().unwrap()
                .remove(&TypeId::of::<T>()).unwrap();
            self.empty::<T>(i)
        }
        else { Err(ErrStorage::GetIndex(format!("Could not read component type: {} from id: {}. Index not found.\n", type_name::<T>(), id))) }
    }

    //Empties, but doesn't deallocate, memory at index for a component type.
    fn empty<T>(&self, i: usize) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        self.check_initialized_component_vector::<T>();
        if let Err(e) = panic::catch_unwind(|| {
            match self.components
                .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
                .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::TYPE_NOT_FOUND_ERROR_MSG, type_name::<T>()))
                .read().expect(EcstaticStorage::VECTOR_LOCK_ERROR_MSG)
                .get(i).expect(&format!("{} index: {}, type: {}, len: {}", EcstaticStorage::INDEX_OUT_OF_BOUNDS_ERROR_MSG, i, type_name::<T>(), self.len::<T>().unwrap())) // index oob??
                .write() {
                    Ok(mut value) => {
                        *value = Box::new(Empty::Empty);

                        match self.change_len::<T>(1, |len, amount| -> usize {if len > 0 { len - amount } else { 0 }}) {
                            Ok(_) => (),
                            Err(e) => panic!("{:#?}", e),
                        }
                    },
                    Err(e) => panic!("{}", EcstaticStorage::ELEMENT_LOCK_ERROR_MSG)
                }
        }) {
            return Err(ErrStorage::Empty(format!("{:#?}", e)))
        }
        self.free_index::<T>(i);
        Ok(())
    }

    pub fn capacity<T>(&self) -> Result<usize, ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        match panic::catch_unwind(|| -> usize {
            self.components
                .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
                .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::TYPE_NOT_FOUND_ERROR_MSG, type_name::<T>()))
                .read().expect(EcstaticStorage::VECTOR_LOCK_ERROR_MSG)
                .len() //capacity = len because emptying a component does not remove the allocation from the vector.

        }) {
            Ok(v) => Ok(v),
            Err(e) => Err(ErrStorage::Read(format!("{:#?}", e)))
        }
    }

    //returns the count of non-empty components of the underlying component vector by type.
    pub fn len<T>(&self) -> Result<usize, ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        match panic::catch_unwind(|| -> usize {
            *self.lengths
                .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
                .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::LEN_NOT_FOUND_ERROR_MSG, type_name::<T>()))
                .read().expect(&format!("{}", EcstaticStorage::ELEMENT_LOCK_ERROR_MSG))
        }) {
            Ok(v) => Ok(v),
            Err(e) => Err(ErrStorage::LenFunc(format!("{:#?}", e)))
        }
    }

    //Note: Do not truncate during emptying. only during insert/set/modify/read operations.
    fn truncate_storage_to_fit<T>(&self)
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        let len = self.len::<T>().expect("truncate_storage failed to get len."); 
        self.components
            .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
            .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::TYPE_NOT_FOUND_ERROR_MSG, type_name::<T>()))
            .write().expect(EcstaticStorage::VECTOR_LOCK_ERROR_MSG)
            .truncate(len);
        self.components
            .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
            .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::TYPE_NOT_FOUND_ERROR_MSG, type_name::<T>()))
            .write().expect(EcstaticStorage::VECTOR_LOCK_ERROR_MSG)
            .shrink_to_fit();
    }

    fn change_len<T>(&self, amount: usize, f: ChLenFunc) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        match panic::catch_unwind(|| {
            match self.lengths
                .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
                .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::LEN_NOT_FOUND_ERROR_MSG, type_name::<T>()))
                .write() {
                    Ok(mut v) => *v = f(*v, amount),
                    Err(e) => panic!("{}", e),
                }
        }) {
            Ok(v) => Ok(()),
            Err(e) => Err(ErrStorage::LenFunc(format!("{:#?}", e)))
        }
    }

    //checks if the value of a type is empty.
    fn is_empty<T>(&self, i: usize) -> Result<bool, ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        self.check_initialized_component_vector::<T>();
        match panic::catch_unwind(|| -> bool {
            match self.components
                .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
                .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::TYPE_NOT_FOUND_ERROR_MSG, type_name::<T>()))
                .read().expect(EcstaticStorage::VECTOR_LOCK_ERROR_MSG)
                .get(i).expect(&format!("{} index: {}, type: {}", EcstaticStorage::INDEX_OUT_OF_BOUNDS_ERROR_MSG, i, type_name::<T>()))
                .read().expect(EcstaticStorage::ELEMENT_LOCK_ERROR_MSG)
                .downcast_ref::<Empty>() {
                    Some(_) => return true,
                    None => return false,
                }
        }) {
            Ok(v) => Ok(v),
            Err(e) => Err(ErrStorage::Empty(format!("Panic on is_empty: {:#?}", e)))
        }
    }

    fn get_owned_index_for_component<T: Any + Send + Sync>(&self, id: u64) -> Option<usize>
    {
        Some(*self.ownership
            .read().unwrap()
            .get(&id).unwrap()
            .read().unwrap()
            .get(&TypeId::of::<T>())
            .expect(&format!("get_owned_index_for_component failed. id: {}, type: {}", id, type_name::<T>()))
        )
    }

    fn check_initialized_lengths<T: Any + Send + Sync>(&self) {
        let mut initialized = false;
        if let Some(_) = self.lengths
            .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
            .get(&TypeId::of::<T>()) {
                initialized = true;
        }

        if !initialized {
            self.lengths
            .write().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
            .insert(TypeId::of::<T>(), RwLock::new(0));
        }
    }

    //attempts to retrieve an index from freed indices. if it finds none, it uses the provided default index. returns the index it used.
    fn get_index<T: Any + Send + Sync>(&self, default_index: usize) -> usize {
        self.check_initialized_index_set::<T>(PACKED);
        self.check_initialized_index_set::<T>(FREE);
        let index: usize;
        match self.indices
            .read().unwrap()
            .get(FREE).unwrap()
            .get(&TypeId::of::<T>()).unwrap()
            .write().unwrap()
            .first() {
                Some(first) => index = *first,
                None => index = default_index,
        }

        self.indices
            .read().unwrap()
            .get(PACKED).unwrap()
            .get(&TypeId::of::<T>()).unwrap()
            .write().unwrap()
            .insert(index);

        index
    }

    //removes index from indices and places into free.
    fn free_index<T: Any + Send + Sync>(&self, i: usize) {
        self.check_initialized_index_set::<T>(PACKED);
        self.check_initialized_index_set::<T>(FREE);
        let removed = self.indices
            .read().unwrap()
            .get(PACKED).unwrap()
            .get(&TypeId::of::<T>()).unwrap()
            .write().unwrap()
            .remove(&i);

        if removed {
            self.indices
                .read().unwrap()
                .get(FREE).unwrap()
                .get(&TypeId::of::<T>()).unwrap()
                .write().unwrap()
                .insert(i);
        }
    }

    fn check_initialized_component_vector<T: Any + Send + Sync>(&self) {
        let is_initialized;
        match self.components
            .read().unwrap()
            .get(&TypeId::of::<T>()) {
                Some(_) => is_initialized = true,
                None => is_initialized = false,
            }
        if !is_initialized {
            self.initialize_storage_vector::<T>();
        }
    }

    fn initialize_storage_vector<T: Any + Send + Sync>(&self) {
        self.components
            .write().unwrap()
            .insert(TypeId::of::<T>(), RwLock::new(vec![]));
    }

    fn check_initialized_index_set<T: Any + Send + Sync>(&self, which: usize) {
        let is_initialized;
        match self.indices
            .read().unwrap()
            .get(which).unwrap()
            .get(&TypeId::of::<T>()) {
                Some(_) => is_initialized = true,
                None => is_initialized = false,
        }
        if !is_initialized {
            self.initialize_index_set::<T>(which);
        }
    }

    fn initialize_index_set<T: Any + Send + Sync>(&self, which: usize) {
        self.indices
            .write().unwrap()
            .get_mut(which).unwrap()
            .insert(TypeId::of::<T>(), RwLock::new(BTreeSet::new()));
    }

    fn check_initialized_ownership_for_id<T: Any + Send + Sync>(&self, id: &u64) {
        let mut is_initialized = true;
        if let None = self.ownership.read().unwrap().get(id) {
            is_initialized = false;
        }
        if !is_initialized {
            self.ownership.write().unwrap().insert(*id, RwLock::new(HashMap::new()));
        }
    }
}

//TODO:
// test thread safety

#[test]
fn test_ecinsert() {
    let ecst = EcstaticStorage::new();
    let key1 = EcstaticStorage::generate_key();
    let key2 = EcstaticStorage::generate_key();
    ecst.ecinsert::<u8>(key1,  1);
    ecst.ecinsert::<u16>(key1, 2);
    ecst.ecinsert::<u32>(key1, 3);
    ecst.ecinsert::<u64>(key1, 4);

    ecst.ecinsert::<u8>(key2, 5);
    ecst.ecinsert::<u16>(key2, 6);
    ecst.ecinsert::<u32>(key2, 7);
    ecst.ecinsert::<u64>(key2, 8);

    struct Test {
        t1: String,
        t2: u64,
        t3: Vec<bool>,
    }
    ecst.ecinsert::<Test>(key1, Test{
        t1: String::from("test1"),
        t2: 22,
        t3: Vec::new(),
    });
}

#[test]
fn test_ecempty() {
    let ecst = EcstaticStorage::new();
    let key1 = EcstaticStorage::generate_key();
    ecst.ecinsert::<u8>(key1, 3);
    ecst.ecempty::<u8>(key1);

    ecst.ecinsert::<u8>(key1, 2);
    let read = ecst.ecread::<u8>(key1).unwrap();
    assert!(read == 2);
}

#[test]
fn test_reusing_indices_and_capacity() {
    let ecst = EcstaticStorage::new();
    let key1 = EcstaticStorage::generate_key();
    for i in 0..10 {
        ecst.ecinsert::<u8>(key1, 1);
        ecst.ecempty::<u8>(key1);
    }
    //indices are reused if capacity remains 1.
    ecst.ecinsert::<u8>(key1, 1);
    let cap = ecst.capacity::<u8>().unwrap();
    assert!(cap == 1, "ecst.capacity t1 :: actual: {}, expected: {}\n", cap, 1);
}

#[test]
fn test_len() {
    let ecst = EcstaticStorage::new();
    let key1 = EcstaticStorage::generate_key();
    let key2 = EcstaticStorage::generate_key();

    ecst.ecinsert::<u8>(key1, 3);
    let len = ecst.len::<u8>().unwrap();
    assert!(len == 1, "ecst.len t1 :: actual: {}, expected: {}\n", len, 1);

    ecst.ecinsert::<u8>(key2, 5);
    let len = ecst.len::<u8>().unwrap();
    assert!(len == 2, "ecst.len t2 :: actual: {}, expected: {}\n", len, 2);

    ecst.ecempty::<u8>(key1);
    let len = ecst.len::<u8>().unwrap();
    assert!(len == 1, "ecst.len t3 :: actual: {}, expected: {}\n", len, 1);

    ecst.ecempty::<u8>(key2); //index oob?
    let len = ecst.len::<u8>().unwrap();
    assert!(len == 0, "ecst.len t4 :: actual: {}, expected: {}\n", len, 0);
}

#[test]
fn test_ecread() {
    let ecst = EcstaticStorage::new();
    let key1 = EcstaticStorage::generate_key();
    let key2 = EcstaticStorage::generate_key();
    let key3 = EcstaticStorage::generate_key();

    ecst.ecinsert::<u8>(key1, 1);
    ecst.ecinsert::<u8>(key2, 2);
    ecst.ecinsert::<u8>(key3, 3);
    let read1 = ecst.ecread::<u8>(key1).unwrap();
    let read2 = ecst.ecread::<u8>(key2).unwrap();
    let read3 = ecst.ecread::<u8>(key3).unwrap();
    assert!(read1 == 1, "ecst.read t1 :: actual: {}, expected: {}\n", read1, 1);
    assert!(read2 == 2, "ecst.read t2 :: actual: {}, expected: {}\n", read2, 2);
    assert!(read3 == 3, "ecst.read t3 :: actual: {}, expected: {}\n", read3, 3);

//----------------------------------------------------------------------------

    let ecst = EcstaticStorage::new();

    ecst.ecinsert::<u8>(key1,  1);
    ecst.ecinsert::<u16>(key1, 2);
    ecst.ecinsert::<u32>(key1, 3);
    ecst.ecinsert::<u64>(key1, 4);

    ecst.ecinsert::<u8>(key2, 5);
    ecst.ecinsert::<u16>(key2, 6);
    ecst.ecinsert::<u32>(key2, 7);
    ecst.ecinsert::<u64>(key2, 8);

    #[derive(Debug, Copy, Clone)]
    struct Test<'a> {
        t1: &'a str,
        t2: u64,
    }
    ecst.ecinsert::<Test>(key1, Test{
        t1: "derp",
        t2: 22,
    });

    let read1 = ecst.ecread::<u8>(key1).unwrap();
    assert!(read1 == 1, "ecst.read t4 :: actual: {}, expected: {}\n", read1, 1);

    let read1 = ecst.ecread::<u16>(key1).unwrap();
    assert!(read1 == 2, "ecst.read t5 :: actual: {}, expected: {}\n", read1, 2);

    let read1 = ecst.ecread::<u32>(key1).unwrap();
    assert!(read1 == 3, "ecst.read t6 :: actual: {}, expected: {}\n", read1, 3);

    let read1 = ecst.ecread::<u64>(key1).unwrap();
    assert!(read1 == 4, "ecst.read t7 :: actual: {}, expected: {}\n", read1, 4);

    let read1 = ecst.ecread::<u8>(key2).unwrap();
    assert!(read1 == 5, "ecst.read t8 :: actual: {}, expected: {}\n", read1, 5);

    let read1 = ecst.ecread::<u16>(key2).unwrap();
    assert!(read1 == 6, "ecst.read t9 :: actual: {}, expected: {}\n", read1, 6);

    let read1 = ecst.ecread::<u32>(key2).unwrap();
    assert!(read1 == 7, "ecst.read t10 :: actual: {}, expected: {}\n", read1, 7);

    let read1 = ecst.ecread::<u64>(key2).unwrap();
    assert!(read1 == 8, "ecst.read t11 :: actual: {}, expected: {}\n", read1, 8);

    let read1 = ecst.ecread::<Test>(key1).unwrap();
    assert!(read1.t1 == "derp", "ecst.read t12 :: actual: {:#?}\n", read1);
    assert!(read1.t2 == 22, "ecst.read t13 :: actual: {:#?}\n", read1);
}

#[test]
fn test_ecset() {
    let ecst = EcstaticStorage::new();
    let key1 = EcstaticStorage::generate_key();
    ecst.ecinsert::<u8>(key1, 1);
    ecst.ecset::<u8>(key1, 2);
    let read1 = ecst.ecread::<u8>(key1).unwrap();
    assert!(read1 == 2, "ecst.set t1 :: actual: {}, expected: {}\n", read1, 2);
}

#[test]
fn test_ecmodify() {
    let ecst = EcstaticStorage::new();
    let key1 = EcstaticStorage::generate_key();
    ecst.ecinsert::<u8>(key1, 1);
    ecst.ecmodify::<u8>(key1, |v| { *v += 200; }); //Modify<T> = fn(&mut T);
    let read1 = ecst.ecread::<u8>(key1).unwrap();
    assert!(read1 == 201, "ecst.modify t1 :: actual: {}, expected: {}\n", read1, 201);
}