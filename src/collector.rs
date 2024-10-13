// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::collections::hash_map::DefaultHasher;
use std::convert::TryInto;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};

use crate::frames::UnresolvedFrames;

pub const BUCKETS: usize = 1 << 12;
pub const BUCKETS_ASSOCIATIVITY: usize = 4;
pub const CAPACITY: usize =
    (1 * 1024 * 1024 * 1024) / std::mem::size_of::<Entry<UnresolvedFrames>>();

#[derive(Debug)]
pub struct Entry<T> {
    pub item: T,
    pub count: isize,
}

impl<T: Default> Default for Entry<T> {
    fn default() -> Self {
        Entry {
            item: Default::default(),
            count: 0,
        }
    }
}

#[derive(Debug)]
pub struct Bucket<T: 'static> {
    pub length: usize,
    entries: Box<[Entry<T>; BUCKETS_ASSOCIATIVITY]>,
}

impl<T: Eq + Default> Default for Bucket<T> {
    fn default() -> Bucket<T> {
        let entries = Box::default();

        Self { length: 0, entries }
    }
}

impl<T: Eq> Bucket<T> {
    pub fn add(&mut self, key: T, count: isize) -> Option<Entry<T>> {
        let mut done = false;
        self.entries[0..self.length].iter_mut().for_each(|ele| {
            if ele.item == key {
                ele.count += count;
                done = true;
            }
        });

        if done {
            None
        } else if self.length < BUCKETS_ASSOCIATIVITY {
            let ele = &mut self.entries[self.length];
            ele.item = key;
            ele.count = count;

            self.length += 1;
            None
        } else {
            let mut min_index = 0;
            let mut min_count = self.entries[0].count;
            for index in 0..self.length {
                let count = self.entries[index].count;
                if count < min_count {
                    min_index = index;
                    min_count = count;
                }
            }

            let mut new_entry = Entry { item: key, count };
            std::mem::swap(&mut self.entries[min_index], &mut new_entry);
            Some(new_entry)
        }
    }

    pub fn iter(&self) -> BucketIterator<T> {
        BucketIterator::<T> {
            related_bucket: self,
            index: 0,
        }
    }
}

pub struct BucketIterator<'a, T: 'static> {
    related_bucket: &'a Bucket<T>,
    index: usize,
}

impl<'a, T> Iterator for BucketIterator<'a, T> {
    type Item = &'a Entry<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.related_bucket.length {
            self.index += 1;
            Some(&self.related_bucket.entries[self.index - 1])
        } else {
            None
        }
    }
}

pub struct HashCounter<T: Hash + Eq + 'static> {
    buckets: Box<[Bucket<T>; BUCKETS]>,
}

impl<T: Hash + Eq + Default + Debug> Default for HashCounter<T> {
    fn default() -> Self {
        let mut v: Vec<Bucket<T>> = Vec::with_capacity(BUCKETS);
        v.resize_with(BUCKETS, Default::default);
        let buckets = v.into_boxed_slice().try_into().unwrap();

        Self { buckets }
    }
}

impl<T: Hash + Eq> HashCounter<T> {
    fn hash(key: &T) -> u64 {
        let mut s = DefaultHasher::new();
        key.hash(&mut s);
        s.finish()
    }

    pub fn add(&mut self, key: T, count: isize) -> Option<Entry<T>> {
        let hash_value = Self::hash(&key);
        let bucket = &mut self.buckets[(hash_value % BUCKETS as u64) as usize];

        bucket.add(key, count)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry<T>> {
        let mut iter: Box<dyn Iterator<Item = &Entry<T>>> =
            Box::new(self.buckets[0].iter().chain(std::iter::empty()));
        for bucket in self.buckets[1..].iter() {
            iter = Box::new(iter.chain(bucket.iter()));
        }

        iter
    }
}

pub struct Collector<T: Hash + Eq + 'static> {
    map: HashCounter<T>,
    temp_array: Vec<Entry<T>>,
}

impl<T: Hash + Eq + 'static> Collector<T> {
    pub fn iter(&self) -> impl Iterator<Item = &Entry<T>> {
        self.map.iter().chain(self.temp_array.iter())
    }
}

impl<T: Hash + Eq + Default + Debug + 'static> Collector<T> {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            map: HashCounter::<T>::default(),
            temp_array: Vec::with_capacity(CAPACITY),
        })
    }
}

impl<T: Hash + Eq + 'static> Collector<T> {
    pub fn add(&mut self, key: T, count: isize) -> std::io::Result<()> {
        if let Some(evict) = self.map.add(key, count) {
            if self.temp_array.len() == CAPACITY {
                // REASON: We have to avoid allocating inside of a signal
                // handler.
                panic!("Exceeded collector capacity");
            }
            self.temp_array.push(evict);
        }

        Ok(())
    }
}

#[cfg(test)]
mod test_utils {
    use super::*;
    use std::collections::BTreeMap;

    pub fn add_map(hashmap: &mut BTreeMap<usize, isize>, entry: &Entry<usize>) {
        match hashmap.get_mut(&entry.item) {
            None => {
                hashmap.insert(entry.item, entry.count);
            }
            Some(count) => *count += entry.count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn stack_hash_counter() {
        let mut stack_hash_counter = HashCounter::<usize>::default();
        stack_hash_counter.add(0, 1);
        stack_hash_counter.add(1, 1);
        stack_hash_counter.add(1, 1);

        stack_hash_counter.iter().for_each(|item| {
            if item.item == 0 {
                assert_eq!(item.count, 1);
            } else if item.item == 1 {
                assert_eq!(item.count, 2);
            } else {
                unreachable!();
            }
        });
    }

    #[test]
    fn evict_test() {
        let mut stack_hash_counter = HashCounter::<usize>::default();
        let mut real_map = BTreeMap::new();

        for item in 0..(1 << 10) * 4 {
            for _ in 0..(item % 4) {
                match stack_hash_counter.add(item, 1) {
                    None => {}
                    Some(evict) => {
                        test_utils::add_map(&mut real_map, &evict);
                    }
                }
            }
        }

        stack_hash_counter.iter().for_each(|entry| {
            test_utils::add_map(&mut real_map, entry);
        });

        for item in 0..(1 << 10) * 4 {
            let count = (item % 4) as isize;
            match real_map.get(&item) {
                Some(item) => {
                    assert_eq!(*item, count);
                }
                None => {
                    assert_eq!(count, 0);
                }
            }
        }
    }

    #[test]
    fn collector_test() {
        let mut collector = Collector::new().unwrap();
        let mut real_map = BTreeMap::new();

        for item in 0..(1 << 12) * 4 {
            for _ in 0..(item % 4) {
                collector.add(item, 1).unwrap();
            }
        }

        collector.iter().for_each(|entry| {
            test_utils::add_map(&mut real_map, entry);
        });

        for item in 0..(1 << 12) * 4 {
            let count = (item % 4) as isize;
            match real_map.get(&item) {
                Some(value) => {
                    assert_eq!(count, *value);
                }
                None => {
                    assert_eq!(count, 0);
                }
            }
        }
    }

    #[test]
    fn collector_alignment_test() {
        let a: u16 = 1; // aligned to 2 bytes
        let mut c = Collector::new().unwrap();
        let _ = c.add(a, 4isize);
        let _ = c.add(3, 4isize);
        c.iter().for_each(|entry| {
            println!("{:?}", entry);
        });
    }
}
