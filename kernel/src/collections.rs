//! Fixed-capacity collections, for use before (and without) a heap.

use core::ops::Deref;

/// A vector with a fixed maximum length, stored inline.
pub struct ArrayVec<T, const N: usize> {
    items: [T; N],
    len: usize,
}

impl<T: Copy + Default, const N: usize> ArrayVec<T, N> {
    pub fn new() -> Self {
        Self { items: [T::default(); N], len: 0 }
    }

    /// Appends an item, or gives it back if the vector is full.
    pub fn push(&mut self, item: T) -> Result<(), T> {
        if self.len == N {
            return Err(item);
        }
        self.items[self.len] = item;
        self.len += 1;
        Ok(())
    }
}

impl<T, const N: usize> Deref for ArrayVec<T, N> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.items[..self.len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn array_vec_holds_up_to_its_capacity() {
            let mut vec = ArrayVec::<u8, 2>::new();
            assert!(vec.is_empty());
            assert_eq!(vec.push(1), Ok(()));
            assert_eq!(vec.push(2), Ok(()));
            assert_eq!(vec.push(3), Err(3));
            assert_eq!(&vec[..], &[1, 2]);
        }
    }
}
