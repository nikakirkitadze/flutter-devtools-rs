//! Append-mostly storage in fixed-size chunks with cheap, immutable snapshots.
//! Sealed chunks are shared by `Arc`; mutating one copies only that chunk.

use std::sync::Arc;

const DEFAULT_CHUNK: usize = 4096;

pub struct Chunked<T: Clone> {
    chunk_size: usize,
    sealed: Vec<Arc<Vec<T>>>,
    tail: Vec<T>,
    /// Cached hand-out for snapshots; rebuilt only after a chunk seals or a
    /// sealed chunk is touched.
    cache: Option<Arc<[Arc<Vec<T>>]>>,
}

impl<T: Clone> Default for Chunked<T> {
    fn default() -> Self {
        Self::with_chunk(DEFAULT_CHUNK)
    }
}

impl<T: Clone> Chunked<T> {
    pub fn with_chunk(n: usize) -> Self {
        let chunk_size = n.max(1);
        Self {
            chunk_size,
            sealed: Vec::new(),
            tail: Vec::with_capacity(chunk_size.min(DEFAULT_CHUNK)),
            cache: None,
        }
    }

    pub fn len(&self) -> usize {
        self.sealed.len() * self.chunk_size + self.tail.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends an item and returns its index.
    pub fn push(&mut self, item: T) -> usize {
        let index = self.len();
        self.tail.push(item);
        if self.tail.len() == self.chunk_size {
            let full = std::mem::replace(&mut self.tail, Vec::with_capacity(self.chunk_size));
            self.sealed.push(Arc::new(full));
            self.cache = None;
        }
        index
    }

    pub fn get(&self, i: usize) -> Option<&T> {
        let (c, o) = (i / self.chunk_size, i % self.chunk_size);
        if c < self.sealed.len() {
            self.sealed[c].get(o)
        } else if c == self.sealed.len() {
            self.tail.get(o)
        } else {
            None
        }
    }

    /// Copy-on-write: clones the chunk only when a snapshot still shares it.
    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        let (c, o) = (i / self.chunk_size, i % self.chunk_size);
        if c < self.sealed.len() {
            // Drop the cached hand-out first so only real snapshots count as sharers.
            self.cache = None;
            Arc::make_mut(&mut self.sealed[c]).get_mut(o)
        } else if c == self.sealed.len() {
            self.tail.get_mut(o)
        } else {
            None
        }
    }

    pub fn snapshot(&mut self) -> ChunkedSnapshot<T> {
        let sealed = match &self.cache {
            Some(c) => c.clone(),
            None => {
                let c: Arc<[Arc<Vec<T>>]> = self.sealed.clone().into();
                self.cache = Some(c.clone());
                c
            }
        };
        ChunkedSnapshot {
            chunk_size: self.chunk_size,
            sealed,
            tail: self.tail.clone().into(),
        }
    }
}

#[derive(Clone)]
pub struct ChunkedSnapshot<T: Clone> {
    chunk_size: usize,
    sealed: Arc<[Arc<Vec<T>>]>,
    tail: Arc<[T]>,
}

impl<T: Clone> Default for ChunkedSnapshot<T> {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK,
            sealed: Arc::from(Vec::new()),
            tail: Arc::from(Vec::new()),
        }
    }
}

impl<T: Clone> ChunkedSnapshot<T> {
    pub fn len(&self) -> usize {
        self.sealed.len() * self.chunk_size + self.tail.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn get(&self, i: usize) -> Option<&T> {
        let (c, o) = (i / self.chunk_size, i % self.chunk_size);
        if c < self.sealed.len() {
            self.sealed[c].get(o)
        } else if c == self.sealed.len() {
            self.tail.get(o)
        } else {
            None
        }
    }

    /// Every item, in order, as slices (sealed chunks, then the non-empty tail).
    pub fn chunks(&self) -> impl Iterator<Item = &[T]> {
        self.sealed
            .iter()
            .map(|c| c.as_slice())
            .chain((!self.tail.is_empty()).then_some(&*self.tail))
    }

    #[cfg(test)]
    fn chunk_ptr(&self, c: usize) -> *const T {
        if c < self.sealed.len() {
            self.sealed[c].as_ptr()
        } else {
            self.tail.as_ptr()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_snapshot_is_unchanged_by_later_pushes() {
        let mut c = Chunked::with_chunk(4);
        for i in 0..10 {
            assert_eq!(c.push(i), i);
        }
        let snap = c.snapshot();
        for i in 10..15 {
            c.push(i);
        }
        assert_eq!(snap.len(), 10);
        assert_eq!(snap.get(9), Some(&9));
        assert_eq!(snap.get(10), None);
        assert_eq!(c.len(), 15);
        assert_eq!(c.get(14), Some(&14));
    }

    #[test]
    fn get_mut_copies_only_the_touched_sealed_chunk() {
        let mut c = Chunked::with_chunk(4);
        for i in 0..10 {
            c.push(i);
        }
        let before = c.snapshot();
        *c.get_mut(5).unwrap() = 500;
        let after = c.snapshot();
        assert_eq!(before.get(5), Some(&5));
        assert_eq!(after.get(5), Some(&500));
        assert_eq!(before.chunk_ptr(0), after.chunk_ptr(0));
        assert_ne!(before.chunk_ptr(1), after.chunk_ptr(1));
    }

    #[test]
    fn get_mut_without_a_snapshot_does_not_copy() {
        let mut c = Chunked::with_chunk(2);
        for i in 0..4 {
            c.push(i);
        }
        drop(c.snapshot());
        let ptr = c.sealed[0].as_ptr();
        *c.get_mut(0).unwrap() = 9;
        assert_eq!(c.sealed[0].as_ptr(), ptr);
    }

    #[test]
    fn chunks_cover_every_item_in_order() {
        let mut c = Chunked::with_chunk(4);
        for i in 0..10 {
            c.push(i);
        }
        let snap = c.snapshot();
        let flat: Vec<i32> = snap.chunks().flatten().copied().collect();
        assert_eq!(flat, (0..10).collect::<Vec<_>>());
        assert_eq!(snap.chunks().count(), 3);
        assert_eq!(snap.chunk_size(), 4);
    }
}
