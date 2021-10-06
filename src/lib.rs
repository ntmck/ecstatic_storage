//! Dynamically typed thread safe component storage.

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
use std::thread;


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

/// Locked Component Type.
pub type LComponent = RwLock<Box<dyn Any + Send + Sync>>;

/// Locked Component Vector
pub type LComponentStore = RwLock<Vec<LComponent>>;

/// Arc Locked Storage Map
pub type ALComponentStorage = Arc<RwLock<HashMap<TypeId, LComponentStore>>>;

pub const PACKED: usize = 0;
pub const FREE: usize = 1;
//Arc-RwLock index storage. Used for both packed indices and freed indices. [0] = indices, [1] = free

/// Arc Locked index map array. Contains indices currently in use accessed by PACKED or 0 and indices ready for reuse accessed by FREE or 1.
pub type ALIndices = Arc<RwLock<[HashMap<TypeId, RwLock<BTreeSet<usize>>>; 2]>>;

/// Arc Locked length storage for keeping track of the number of non-empty slots in component storage.
pub type ALLengths = Arc<RwLock<HashMap<TypeId, RwLock<usize>>>>;

/// Arc Locked ownership tracking. u64 id -> owned component types and their respective indices. 
pub type ALOwnership = Arc<RwLock<HashMap<u64, RwLock<HashMap<TypeId, usize>>>>>;

/// Type required by EcstaticStorage::ecmodify() to modify a component in storage without explicitly setting it.
pub type Modify<T> = fn(&mut T);

type ChLenFunc = fn(usize, usize) -> usize;

/// An empty type for marking free component memory.
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
    const INDEX_NOT_FOUND_ERROR_MSG: &'static str = "Failed locate component index for the given id. ->";
    const ECINSERT_FAILED_ERROR_MSG: &'static str = "Failed to insert component. ->";
    const COMPONENT_EMPTY_ERROR_MSG: &'static str = "Expected non-empty component, but found empty. ->";

    pub fn new() -> Arc<EcstaticStorage> {
        Arc::new(EcstaticStorage {
            components: Arc::new(RwLock::new(HashMap::with_capacity(1))),
            indices: Arc::new(RwLock::new([HashMap::with_capacity(1), HashMap::with_capacity(1)])),
            lengths: Arc::new(RwLock::new(HashMap::with_capacity(1))),
            ownership: Arc::new(RwLock::new(HashMap::with_capacity(1))),
        })
    }

    fn debug_dump_components<T>(&self)
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe + std::fmt::Debug
    {
        let len = self.components
            .read().unwrap()
            .get(&TypeId::of::<T>()).unwrap()
            .read().unwrap()
            .len();
        let cap = self.components
            .read().unwrap()
            .get(&TypeId::of::<T>()).unwrap()
            .read().unwrap()
            .capacity();

        print!("Type: {}, Capacity: {}, Length: {}\n", type_name::<T>(), cap, len);
        for c in self.components
                .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
                .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::TYPE_NOT_FOUND_ERROR_MSG, type_name::<T>()))
                .read().expect(EcstaticStorage::VECTOR_LOCK_ERROR_MSG)
                .iter() 
        {
            print!("{:?}\n", c.read().unwrap().downcast_ref::<T>());
        }
    }

    fn debug_dump_indices<T>(&self)
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe + std::fmt::Debug
    {
        print!("______________\n");
        print!("Packed:\n");
        for p in self.indices
            .read().unwrap()
            .get(PACKED).unwrap()
            .get(&TypeId::of::<T>()).unwrap()
            .read().unwrap()
            .iter()
        {
            print!("\t{}\n", p);
        }
        print!("Free:\n");
        for f in self.indices
            .read().unwrap()
            .get(FREE).unwrap()
            .get(&TypeId::of::<T>()).unwrap()
            .read().unwrap()
            .iter()
        {
            print!("\t{}\n", f);
        }
        print!("______________\n");
    }

    /// Default function to generate a unique u64 key for use as an id for EcstaticStorage functions.
    pub fn generate_key() -> u64 {
        let mut hasher = DefaultHasher::new();
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().hash(&mut hasher);
        hasher.finish()
    }

    /// Inserts a component into storage and maps the component to the id.
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
        } else { Err(ErrStorage::Insert(format!("{} type: {}, id: {}", EcstaticStorage::ECINSERT_FAILED_ERROR_MSG, type_name::<T>(), id))) }
    }

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
                .push(RwLock::new(Box::new(component)));
            let empty = self.is_empty::<T>(i).unwrap_or(true);
            if empty {
                self.components
                    .read().expect(EcstaticStorage::STORAGE_LOCK_ERROR_MSG)
                    .get(&TypeId::of::<T>()).expect(&format!("{} {}", EcstaticStorage::TYPE_NOT_FOUND_ERROR_MSG, type_name::<T>()))
                    .write().expect(EcstaticStorage::VECTOR_LOCK_ERROR_MSG)
                    .swap_remove(i);
            }

            match self.change_len::<T>(1, |len, amount| -> usize {len + amount}) {
                Ok(_) => (),
                Err(e) => panic!("{:#?}", e),
            }
        }) {
            self.free_index::<T>(i);
            return Err(ErrStorage::Insert(format!("{:#?}", e)))
        }
        Ok(i)
    }

    /// Replaces the component in storage owned by the given id with the given component.
    pub fn ecset<T>(&self, id: u64, with: T) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {  
        if let Some(i) = self.get_owned_index_for_component::<T>(id){ self.set::<T>(i, with) } 
        else { Err(ErrStorage::GetIndex(format!("{} type: {} id: {}", EcstaticStorage::INDEX_NOT_FOUND_ERROR_MSG, type_name::<T>(), id))) }
    }

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
        Ok(())
    }

    /// Modifies the value of a component owned by the given id through the provided callback without replacing the value entirely.
    pub fn ecmodify<T>(&self, id: u64, modify: Modify<T>) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + Copy + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        if let Some(i) = self.get_owned_index_for_component::<T>(id) { self.modify::<T>(i, modify) } 
        else { Err(ErrStorage::GetIndex(format!("{} type: {} id: {}", EcstaticStorage::INDEX_NOT_FOUND_ERROR_MSG, type_name::<T>(), id))) }
        
    }

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
                    Err(e) => panic!("{} {}", EcstaticStorage::ELEMENT_LOCK_ERROR_MSG, e)
                }
            }) {
                return Err(ErrStorage::Modify(format!("{:#?}", e)))
            }
            Ok(())
        } else {
            return Err(ErrStorage::Empty(format!("{} type: {}", EcstaticStorage::COMPONENT_EMPTY_ERROR_MSG, type_name::<T>())))
        }
    }

    /// Returns a copy of the component owned by the given id.
    pub fn ecread<T>(&self, id: u64) -> Result<T, ErrStorage>
    where T: Any + Send + Sync + Copy + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        if let Some(i) = self.get_owned_index_for_component::<T>(id) { self.read::<T>(i) }
        else { Err(ErrStorage::GetIndex(format!("{} type: {} id: {}", EcstaticStorage::INDEX_NOT_FOUND_ERROR_MSG, type_name::<T>(), id))) }
    }

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
                Ok(v) => Ok(v),
                Err(e) => Err(ErrStorage::Read(format!("{:#?}", e)))
            }
        } else {
            return Err(ErrStorage::Empty(format!("{} type: {}", EcstaticStorage::COMPONENT_EMPTY_ERROR_MSG, type_name::<T>())))
        }
    }

    /// Empties the component for the given id. WARNING: This does NOT free up the memory associated with the component. It only marks it as empty for either reuse in subsequent inserts or for compress_memory to free the memory.
    pub fn ecempty<T>(&self, id: u64) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        if let Some(i) = self.get_owned_index_for_component::<T>(id)
        {
            self.ownership
                .read().unwrap()
                .get(&id).unwrap()
                .write().unwrap()
                .remove(&TypeId::of::<T>()).unwrap();
            self.empty::<T>(i)
        }
        else { Err(ErrStorage::GetIndex(format!("{} type: {} id: {}", EcstaticStorage::INDEX_NOT_FOUND_ERROR_MSG, type_name::<T>(), id))) }
    }

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
                    Err(e) => panic!("{} {}", EcstaticStorage::ELEMENT_LOCK_ERROR_MSG, e)
                }
        }) {
            return Err(ErrStorage::Empty(format!("{:#?}", e)))
        }
        self.free_index::<T>(i);
        Ok(())
    }

    /// Returns the length of all components including the empty ones. Note: this may not reflect the underlying vector capacity until compress_memory is called at least once.
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

    /// Returns the count of non-empty components. Empty components are NOT counted.
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
            Ok(_) => Ok(()),
            Err(e) => Err(ErrStorage::LenFunc(format!("{:#?}", e)))
        }
    }

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


    /// Compresses memory for a given type. Use this after emptying a large number of components to free up memory.
    pub fn compress_memory<T>(&self) -> Result<(), ErrStorage>
    where T: Any + Send + Sync + Copy + std::panic::UnwindSafe + std::panic::RefUnwindSafe
    {
        self.check_initialized_index_set::<T>(PACKED);
        self.check_initialized_index_set::<T>(FREE);
 
        let packed_len = self.indices
            .read().unwrap()
            .get(PACKED).unwrap()
            .get(&TypeId::of::<T>()).unwrap()
            .read().unwrap()
            .len();

        //foreach active component, relocate it to the result of getindex and update the ownership.
        for _ in 0..packed_len {
            let i_packed = self.indices
                .read().unwrap()
                .get(PACKED).unwrap()
                .get(&TypeId::of::<T>()).unwrap()
                .write().unwrap()
                .pop_first().unwrap();

            //swap old component with new component.
            let old = self.read::<T>(i_packed)?; //read clones the data.
            self.empty::<T>(i_packed)?; //empty index and free it.
            self.insert::<T>(old).unwrap(); //inserts into next free space.
        }
        //all empty spaces should now be right
        //truncate all empty spaces to the right.
        self.truncate_storage_to_fit::<T>();
        
        self.indices
            .read().unwrap()
            .get(PACKED).unwrap()
            .get(&TypeId::of::<T>()).unwrap()
            .write().unwrap()
            .retain(|&i| { i >= self.len::<T>().unwrap() }); //remove from freed indices such that any i >= type_len is removed.
        Ok(())
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
            .pop_first() {
                Some(first) => index = first,
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
            .insert(TypeId::of::<T>(), RwLock::new(Vec::with_capacity(1)));
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
            self.ownership.write().unwrap().insert(*id, RwLock::new(HashMap::with_capacity(1)));
        }
    }
}

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
fn test_ecread_ecempty_many_and_capacity() {
    let ecst = EcstaticStorage::new();
    let key1 = EcstaticStorage::generate_key();
    let key2 = EcstaticStorage::generate_key();
    let key3 = EcstaticStorage::generate_key();

    ecst.ecinsert::<u8>(key1, 1);
    ecst.ecinsert::<u8>(key2, 2);
    ecst.ecinsert::<u8>(key3, 3);

    ecst.ecempty::<u8>(key1);
    ecst.ecempty::<u8>(key2);

    let read = ecst.ecread::<u8>(key3).unwrap();
    assert!(read == 3);

    let read = ecst.ecread::<u8>(key3).unwrap();
    assert!(read == 3);

    ecst.debug_dump_indices::<u8>();
    ecst.ecinsert::<u8>(key1, 4);

    let read = ecst.ecread::<u8>(key1).unwrap();
    assert!(read == 4);

    ecst.ecinsert::<u8>(key2, 5);

    let read = ecst.ecread::<u8>(key2).unwrap();
    assert!(read == 5, "actual: {}, expected: {}\n", read, 5);
}

#[test]
fn test_reusing_indices_and_single_compression_capacity() {
    let ecst = EcstaticStorage::new();
    let key1 = EcstaticStorage::generate_key();
    for i in 0..10 {
        ecst.ecinsert::<u8>(key1, 1);
        ecst.ecempty::<u8>(key1);
    }
    
    ecst.ecinsert::<u8>(key1, 1);

    print!("\n\nbefore_compress\n");
    ecst.debug_dump_components::<u8>();
    print!("\n\nafter_compress\n");
    ecst.compress_memory::<u8>();
    ecst.debug_dump_components::<u8>();

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

#[test]
fn test_multi_thread() {
    let ecst = EcstaticStorage::new(); //returns an Arc<Storage>
    let key1 = EcstaticStorage::generate_key();
    let ecst_clone = ecst.clone();

    let handle = thread::spawn(move ||{
        ecst_clone.ecinsert::<u8>(key1, 1);
    });
    handle.join();

    let read = ecst.ecread::<u8>(key1).unwrap();
    assert!(read == 1);
}