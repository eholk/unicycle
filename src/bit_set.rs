//! A hierarchical bit set with support for atomic operations.
//!
//! The idea is based on [hibitset], but dynamically growing instead of using a
//! fixed capacity.
//! By being careful with the data layout, we can also support structural
//! sharing between the local and atomic bitset variants.
//!
//! [hibitset]: https://docs.rs/hibitset

use std::{
    iter, mem, ops, slice,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Bits in a single usize.
const BITS: usize = mem::size_of::<usize>() * 8;
const BITS_SHIFT: usize = BITS.trailing_zeros() as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LayerLayout {
    /// The length of the layer.
    cap: usize,
}

/// A sparse, layered bit set.
///
/// [BitSet] and [AtomicBitSet]'s are guaranteed to have an identical memory
/// layout, so while it would require `unsafe`, transmuting or coercing between
/// the two is sound assuming the proper synchronization is respected.
///
/// A [BitSet] provides the following methods for converting to an
/// [AtomicBitSet]: [into_atomic] and [as_atomic].
///
/// [into_atomic]: BitSet::into_atomic
/// [as_atomic]: BitSet::as_atomic
#[repr(C)]
pub struct BitSet {
    /// Layers of bits.
    // TODO: Consider breaking this up into a (pointer, len, cap) tuple since
    // I'm not entirely sure this guarantees that the memory layout of `BitSet`
    // is the same as `AtomicBitSet`, even though `Layer` and `AtomicLayer` is.
    layers: Vec<Layer>,
    /// The capacity of the bitset in number of bits it can store.
    cap: usize,
}

impl BitSet {
    /// Construct a new, empty BitSet with an empty capacity.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let mut set = BitSet::new();
    /// assert!(set.is_empty());
    /// assert_eq!(0, set.capacity());
    /// ```
    pub const fn new() -> Self {
        Self {
            layers: Vec::new(),
            cap: 0,
        }
    }

    /// Construct a new, empty [BitSet] with the specified capacity.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let mut set = BitSet::with_capacity(1024);
    /// assert!(set.is_empty());
    /// assert_eq!(1024, set.capacity());
    /// ```
    pub fn with_capacity(capacity: usize) -> Self {
        let mut this = Self::new();
        this.reserve(capacity);
        this
    }

    /// Test if the bit set is empty.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let mut set = BitSet::with_capacity(64);
    /// assert!(set.is_empty());
    /// set.set(2);
    /// assert!(!set.is_empty());
    /// set.clear(2);
    /// assert!(set.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        if self.layers.is_empty() {
            return true;
        }

        self.layers[0].as_slice().iter().all(|b| *b == 0)
    }

    /// Get the current capacity of the bitset.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let mut set = BitSet::new();
    /// assert!(set.is_empty());
    /// assert_eq!(0, set.capacity());
    /// ```
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Return a view of the underlying, raw layers.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let mut set = BitSet::with_capacity(128);
    /// set.set(1);
    /// set.set(5);
    /// // Note: two layers since we specified a capacity of 128.
    /// assert_eq!(vec![&[0b100010, 0][..], &[1]], set.layers());
    /// ```
    pub fn layers(&self) -> Vec<&'_ [usize]> {
        self.layers.iter().map(Layer::as_slice).collect()
    }

    /// Convert in-place into an [AtomicBitSet].
    ///
    /// Atomic bit sets uses structural sharing with the current set, so this
    /// is a constant time `O(1)` operation.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let mut set = BitSet::with_capacity(1024);
    ///
    /// let atomic = set.into_atomic();
    /// atomic.set(42);
    ///
    /// let set = atomic.into_local();
    /// assert!(set.test(42));
    /// ```
    pub fn into_atomic(mut self) -> AtomicBitSet {
        AtomicBitSet {
            layers: unsafe { convert_vec(mem::replace(&mut self.layers, Vec::new())) },
            cap: mem::replace(&mut self.cap, 0),
        }
    }

    /// Convert in-place into a reference to an [AtomicBitSet].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let set = BitSet::with_capacity(1024);
    ///
    /// set.as_atomic().set(42);
    /// assert!(set.test(42));
    /// ```
    pub fn as_atomic(&self) -> &AtomicBitSet {
        // Safety: BitSet and AtomicBitSet are guaranteed to have identical
        // memory layouts.
        unsafe { &*(self as *const _ as *const AtomicBitSet) }
    }

    /// Set the given bit.
    ///
    /// # Panics
    ///
    /// Panics if the position does not fit within the capacity of the [BitSet].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let mut set = BitSet::with_capacity(64);
    ///
    /// assert!(set.is_empty());
    /// set.set(2);
    /// assert!(!set.is_empty());
    /// ```
    pub fn set(&mut self, mut position: usize) {
        assert!(
            position < self.cap,
            "position {} is out of bounds for capacity {}",
            position,
            self.cap
        );

        for layer in &mut self.layers {
            let slot = position / BITS;
            let offset = position % BITS;
            layer.set(slot, offset);
            position >>= BITS_SHIFT;
        }
    }

    /// Clear the given bit.
    ///
    /// # Panics
    ///
    /// Panics if the position does not fit within the capacity of the [BitSet].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let mut set = BitSet::with_capacity(64);
    ///
    /// set.clear(2);
    /// assert!(set.is_empty());
    /// set.set(2);
    /// assert!(!set.is_empty());
    /// set.clear(2);
    /// assert!(set.is_empty());
    /// set.clear(2);
    /// assert!(set.is_empty());
    /// ```
    pub fn clear(&mut self, mut position: usize) {
        assert!(
            position < self.cap,
            "position {} is out of bounds for capacity {}",
            position,
            self.cap
        );

        for layer in &mut self.layers {
            let slot = position / BITS;
            let offset = position % BITS;
            layer.clear(slot, offset);
            position >>= BITS_SHIFT;
        }
    }

    /// Test if the given position is set.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let mut set = BitSet::with_capacity(64);
    ///
    /// assert!(set.is_empty());
    /// set.set(2);
    /// assert!(!set.is_empty());
    /// assert!(set.test(2));
    /// assert!(!set.test(3));
    /// ```
    pub fn test(&self, position: usize) -> bool {
        assert!(position < self.cap);
        let slot = position / BITS;
        let offset = position % BITS;
        self.layers[0].test(slot, offset)
    }

    /// Reserve enough space to store the given number of elements.
    ///
    /// This will not reserve space for exactly as many elements specified, but
    /// will round up to the closest order of magnitude of 2.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    /// let mut set = BitSet::with_capacity(128);
    /// assert_eq!(128, set.capacity());
    /// set.reserve(250);
    /// assert_eq!(256, set.capacity());
    /// ```
    pub fn reserve(&mut self, cap: usize) {
        if self.cap >= cap {
            return;
        }

        let cap = round_capacity_up(cap);
        let mut new = bit_set_layout(cap).peekable();

        let mut old = self.layers.iter_mut();

        while let (Some(layer), Some(&LayerLayout { cap, .. })) = (old.next(), new.peek()) {
            debug_assert!(cap >= layer.cap);

            // Layer needs to grow.
            if cap > 0 {
                layer.grow(cap);
            }

            // Skip to next new layer.
            new.next();
        }

        let remaining = new.clone().count();

        // Add new layers!
        if remaining > 0 {
            self.layers.reserve_exact(remaining);
            self.layers.extend(new.map(|l| Layer::with_capacity(l.cap)));
        }

        self.cap = cap;
    }

    /// Create a draining iterator over the bitset.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let mut set = BitSet::with_capacity(128);
    /// set.set(127);
    /// set.set(32);
    /// set.set(3);
    ///
    /// assert_eq!(vec![3, 32, 127], set.drain().collect::<Vec<_>>());
    /// assert!(set.is_empty());
    /// ```
    pub fn drain(&mut self) -> Drain<'_> {
        let depth = self.layers.len().saturating_sub(1);

        Drain {
            layers: &mut self.layers,
            index: 0,
            depth,
            #[cfg(test)]
            op_count: 0,
        }
    }
}

impl Default for BitSet {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Drain<'a> {
    layers: &'a mut [Layer],
    index: usize,
    depth: usize,
    #[cfg(test)]
    pub(crate) op_count: usize,
}

impl Iterator for Drain<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.depth < self.layers.len() {
            #[cfg(test)]
            {
                self.op_count += 1;
            }

            let shift = self.depth * BITS_SHIFT;
            let offset = self.index / (BITS << shift);
            let layer = &mut self.layers[self.depth];

            let slot = &mut layer[offset];

            // We are at a layer which is zerod, move up one layer.
            if *slot == 0 {
                self.depth += 1;
                continue;
            }

            if self.depth > 0 {
                // Advance into a deeper layer. We set the base index to
                // the value of the deeper layer.
                //
                // We calculate the index based on the offset that we are
                // currently at and the information we get at the current
                // layer of bits.
                let new_index =
                    (offset * (BITS << shift)) + ((slot.trailing_zeros() as usize) << shift);
                self.index = new_index;
                self.depth -= 1;
                continue;
            }

            // We are now in layer 0. The number of trailing zeros indicates
            // the bit set.
            let trail = slot.trailing_zeros() as usize;

            // NB: if this doesn't hold, a prior layer lied and we ended up
            // here in vain.
            debug_assert!(trail < BITS);

            let index = self.index + trail;
            // Clear the current slot.
            *slot &= !(1 << trail);

            // Slot is not empty yet.
            if *slot != 0 {
                return Some(index);
            }

            // Clear upper layers until we find one that is not set again -
            // then use that as hour new depth.
            for (depth, layer) in (1..).zip(self.layers[1..].iter_mut()) {
                #[cfg(test)]
                {
                    self.op_count += 1;
                }

                let shift = depth * BITS_SHIFT;
                let offset = self.index / (BITS << shift);
                let mask = !(1 << ((index >> shift) % BITS));

                let slot = &mut layer[offset];
                *slot &= mask;

                if *slot == 0 {
                    continue;
                }

                // update the index to be the bottom of the next value set
                // layer.
                self.depth = depth;

                // We calculate the index based on the offset that we are
                // currently at and the information we get at the current
                // layer of bits.
                let new_index =
                    (offset * (BITS << shift)) + ((slot.trailing_zeros() as usize) << shift);
                self.index = new_index;
                return Some(index);
            }

            self.depth = self.layers.len();
            return Some(index);
        }

        None
    }
}

/// The same as [BitSet], except it provides atomic methods.
///
/// [BitSet] and [AtomicBitSet]'s are guaranteed to have an identical memory
/// layout, so while it would require `unsafe`, transmuting or coercing between
/// the two is sound assuming the proper synchronization is respected.
///
/// We provide the following methods to accomplish this from an atomic bitset,
/// to a local (non atomic) one: [as_local_mut] for borrowing mutably and
/// [into_local].
///
/// [as_local_mut]: AtomicBitSet::as_local_mut
/// [into_local]: AtomicBitSet::into_local
#[repr(C)]
pub struct AtomicBitSet {
    /// Layers of bits.
    layers: Vec<AtomicLayer>,
    /// The capacity of the bit set in number of bits it can store.
    cap: usize,
}

impl AtomicBitSet {
    /// Construct a new, empty atomic bit set.
    pub fn new() -> Self {
        Self {
            layers: Vec::new(),
            cap: 0,
        }
    }

    /// Set the given bit.
    pub fn set(&self, mut position: usize) {
        assert!(
            position < self.cap,
            "position {} is out of bounds for layer capacity {}",
            position,
            self.cap
        );

        for layer in &self.layers {
            let slot = position / BITS;
            let offset = position % BITS;
            layer.set(slot, offset);
            position >>= BITS_SHIFT;
        }
    }

    /// Convert in-place into a a [`BitSet`].
    ///
    /// [`BitSet`]: BitSet
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::BitSet;
    ///
    /// let mut set = BitSet::new();
    /// set.reserve(1024);
    ///
    /// let atomic = set.into_atomic();
    /// atomic.set(42);
    ///
    /// let set = atomic.into_local();
    /// assert!(set.test(42));
    /// ```
    pub fn into_local(mut self) -> BitSet {
        BitSet {
            layers: unsafe { convert_vec(mem::replace(&mut self.layers, Vec::new())) },
            cap: mem::replace(&mut self.cap, 0),
        }
    }

    /// Convert in-place into a reference to a [`BitSet`].
    ///
    /// [`BitSet`]: BitSet
    pub fn as_local(&self) -> &BitSet {
        // Safety: BitSet and AtomicBitSet are guaranteed to have identical
        // internal structures.
        unsafe { &*(self as *const _ as *const BitSet) }
    }

    /// Convert in-place into a mutable reference to a [`BitSet`].
    ///
    /// [`BitSet`]: BitSet
    pub fn as_local_mut(&mut self) -> &mut BitSet {
        // Safety: BitSet and AtomicBitSet are guaranteed to have identical
        // internal structures.
        unsafe { &mut *(self as *mut _ as *mut BitSet) }
    }
}

impl Default for AtomicBitSet {
    fn default() -> Self {
        Self::new()
    }
}

/// A single layer of bits.
///
/// Note: doesn't store capacity, so must be deallocated by a BitSet.
#[repr(C)]
struct Layer {
    /// Bits.
    bits: *mut usize,
    cap: usize,
}

impl Layer {
    /// Allocate a new raw layer with the specified capacity.
    pub fn with_capacity(cap: usize) -> Layer {
        // Create an already initialized layer of bits.
        let mut vec = mem::ManuallyDrop::new(vec![0usize; cap]);

        Layer {
            bits: vec.as_mut_ptr(),
            cap,
        }
    }

    /// Return the given layer as a slice.
    pub fn as_slice(&self) -> &[usize] {
        unsafe { slice::from_raw_parts(self.bits, self.cap) }
    }

    /// Return the given layer as a mutable slice.
    pub fn as_slice_mut(&mut self) -> &mut [usize] {
        unsafe { slice::from_raw_parts_mut(self.bits, self.cap) }
    }

    /// Reserve exactly the specified number of elements in this layer.
    pub fn grow(&mut self, new: usize) {
        // Nothing to do.
        if self.cap >= new {
            return;
        }

        let mut vec =
            mem::ManuallyDrop::new(unsafe { Vec::from_raw_parts(self.bits, self.cap, self.cap) });
        vec.reserve(new - self.cap);

        // Initialize new values.
        for _ in self.cap..new {
            vec.push(0usize);
        }

        debug_assert!(vec.len() == vec.capacity());
        self.bits = vec.as_mut_ptr();
        self.cap = vec.capacity();
    }

    /// Set the given bit in this layer.
    pub fn set(&mut self, slot: usize, offset: usize) {
        *self.slot_mut(slot) |= 1 << offset;
    }

    /// Clear the given bit in this layer.
    pub fn clear(&mut self, slot: usize, offset: usize) {
        *self.slot_mut(slot) &= !(1 << offset);
    }

    /// Set the given bit in this layer, where `element` indicates how many
    /// elements are affected per position.
    pub fn test(&self, slot: usize, offset: usize) -> bool {
        *self.slot(slot) & (1 << offset) > 0
    }

    #[inline(always)]
    fn slot(&self, slot: usize) -> &usize {
        assert!(slot < self.cap);
        // Safety: We check that the slot fits within the capacity.
        unsafe { &*self.bits.add(slot) }
    }

    #[inline(always)]
    fn slot_mut(&mut self, slot: usize) -> &mut usize {
        assert!(slot < self.cap);
        // Safety: We check that the slot fits within the capacity.
        unsafe { &mut *self.bits.add(slot) }
    }
}

impl<S> PartialEq<S> for Layer
where
    S: AsRef<[usize]>,
{
    fn eq(&self, other: &S) -> bool {
        other.as_ref() == self.as_slice()
    }
}

impl Eq for Layer {}

impl AsRef<[usize]> for Layer {
    fn as_ref(&self) -> &[usize] {
        self.as_slice()
    }
}

impl<I: slice::SliceIndex<[usize]>> ops::Index<I> for Layer {
    type Output = I::Output;

    #[inline]
    fn index(&self, index: I) -> &Self::Output {
        ops::Index::index(self.as_slice(), index)
    }
}

impl<I: slice::SliceIndex<[usize]>> ops::IndexMut<I> for Layer {
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        ops::IndexMut::index_mut(self.as_slice_mut(), index)
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        unsafe {
            drop(Vec::from_raw_parts(self.bits, self.cap, self.cap));
        }
    }
}

/// A single layer of the bitset, that can be atomically updated.
///
/// Note: This has the same memory layout as [Layer], so that coercing between
/// them is possible.
#[repr(C)]
struct AtomicLayer {
    bits: *mut AtomicUsize,
    cap: usize,
}

impl AtomicLayer {
    /// Return the given layer as a slice.
    pub fn as_slice(&self) -> &[AtomicUsize] {
        unsafe { slice::from_raw_parts(self.bits, self.cap) }
    }

    /// Set the given bit in this layer, where `element` indicates how many
    /// elements are affected per position.
    pub fn set(&self, slot: usize, offset: usize) {
        self.slot(slot).fetch_or(1 << offset, Ordering::AcqRel);
    }

    #[inline(always)]
    fn slot(&self, slot: usize) -> &AtomicUsize {
        assert!(slot < self.cap);
        // Safety: We check that the slot fits within the capacity.
        unsafe { &*self.bits.add(slot) }
    }
}

impl AsRef<[AtomicUsize]> for AtomicLayer {
    fn as_ref(&self) -> &[AtomicUsize] {
        self.as_slice()
    }
}

impl Drop for AtomicLayer {
    fn drop(&mut self) {
        // Safety: We keep track of the capacity internally.
        unsafe {
            drop(Vec::from_raw_parts(self.bits, self.cap, self.cap));
        }
    }
}

fn round_bits_up(value: usize) -> usize {
    let m = value % BITS;

    if m == 0 {
        value
    } else {
        value + (BITS - m)
    }
}

/// Helper function to generate the necessary layout of the bit set layers
/// given a desired `capacity`.
fn bit_set_layout(capacity: usize) -> impl Iterator<Item = LayerLayout> + Clone {
    let mut cap = round_bits_up(capacity);

    iter::from_fn(move || {
        if cap == 1 {
            return None;
        }

        cap = round_bits_up(cap) / BITS;

        if cap > 0 {
            Some(LayerLayout { cap })
        } else {
            None
        }
    })
}

/// Round up the capacity to be the closest power of 2.
fn round_capacity_up(cap: usize) -> usize {
    if cap == 0 {
        return 0;
    }

    let cap = if BITS as u32 - cap.leading_zeros() == cap.trailing_zeros() + 1 {
        cap
    } else {
        let leading = cap.leading_zeros();

        if leading == 64 {
            return std::usize::MAX;
        }

        1 << (64 - cap.leading_zeros() as usize)
    };

    usize::max(16, cap)
}

/// Convert a vector into a different type, assuming the constituent type has
/// an identical layout to the converted type.
///
/// # Safety
///
/// Caller must ensure that `T` and `U` are identical in their memory layouts.
unsafe fn convert_vec<T, U>(vec: Vec<T>) -> Vec<U> {
    debug_assert!(mem::size_of::<T>() == mem::size_of::<U>());
    let mut vec = mem::ManuallyDrop::new(vec);
    Vec::from_raw_parts(vec.as_mut_ptr() as *mut U, vec.len(), vec.capacity())
}

#[cfg(test)]
mod tests {
    use super::{bit_set_layout, BitSet};

    #[test]
    fn test_set_and_test() {
        let mut set = BitSet::new();
        set.reserve(1024);
        set.set(1);
        set.set(64);
        set.set(129);
        set.set(1023);

        assert!(set.test(1));
        assert!(set.test(64));
        assert!(set.test(129));
        assert!(set.test(1023));
        assert!(!set.test(1022));

        let mut layer0 = vec![0usize; 16];
        layer0[0] = 1 << 1;
        layer0[1] = 1;
        layer0[2] = 1 << 1;
        layer0[15] = 1 << 63;

        let mut layer1 = vec![0usize; 1];
        layer1[0] = 1 << 15 | 1 << 2 | 1 << 1 | 1;

        assert_eq!(vec![&layer0[..], &layer1[..]], set.layers());
    }

    #[test]
    fn test_bit_layout() {
        assert!(bit_set_layout(0).collect::<Vec<_>>().is_empty());
        assert_eq!(
            vec![1],
            bit_set_layout(64).map(|l| l.cap).collect::<Vec<_>>()
        );
        assert_eq!(
            vec![2, 1],
            bit_set_layout(128).map(|l| l.cap).collect::<Vec<_>>()
        );
        assert_eq!(
            vec![64, 1],
            bit_set_layout(4096).map(|l| l.cap).collect::<Vec<_>>()
        );
        assert_eq!(
            vec![65, 2, 1],
            bit_set_layout(4097).map(|l| l.cap).collect::<Vec<_>>()
        );
        assert_eq!(
            vec![2, 1],
            bit_set_layout(65).map(|l| l.cap).collect::<Vec<_>>()
        );
        assert_eq!(
            vec![2, 1],
            bit_set_layout(128).map(|l| l.cap).collect::<Vec<_>>()
        );
        assert_eq!(
            vec![3, 1],
            bit_set_layout(129).map(|l| l.cap).collect::<Vec<_>>()
        );
        assert_eq!(
            vec![65, 2, 1],
            bit_set_layout(4097).map(|l| l.cap).collect::<Vec<_>>()
        );
    }

    // NB: test to run through miri to make sure we reserve layers appropriately.
    #[test]
    fn test_reserve() {
        let mut b = BitSet::new();
        b.reserve(1_000);
        b.reserve(10_000);

        assert_ne!(
            bit_set_layout(1_000).collect::<Vec<_>>(),
            bit_set_layout(10_000).collect::<Vec<_>>()
        );
    }

    macro_rules! drain_test {
        ($cap:expr, $sample:expr, $expected_op_count:expr) => {{
            let mut set = BitSet::new();
            set.reserve($cap);

            let positions: Vec<usize> = $sample;

            for p in positions.iter().copied() {
                set.set(p);
            }

            let mut drain = set.drain();
            assert_eq!(positions, (&mut drain).collect::<Vec<_>>());
            let op_count = drain.op_count;

            // Assert that all layers are zero.
            assert_eq!(
                0,
                set.layers()
                    .into_iter()
                    .map(|l| l.iter().copied().sum::<usize>())
                    .sum::<usize>()
            );

            assert_eq!($expected_op_count, op_count);
        }};
    }

    #[test]
    fn test_drain() {
        drain_test!(64, vec![0], 1);
        drain_test!(64, vec![0, 1], 2);
        drain_test!(64, vec![0, 1, 63], 3);
        drain_test!(128, vec![64], 3);
        drain_test!(128, vec![0, 32, 64], 7);
        drain_test!(4096, vec![0, 32, 64, 3030, 4095], 13);
        drain_test!(
            1_000_000,
            vec![0, 32, 64, 3030, 4095, 50_000, 102110, 203020, 500000, 803020, 900900],
            51
        );
        drain_test!(
            10_000_000,
            vec![0, 32, 64, 3030, 4095, 50_000, 102110, 203020, 500000, 803020, 900900, 9_009_009],
            58
        );
    }

    #[test]
    fn test_round_capacity_up() {
        use super::round_capacity_up;
        assert_eq!(0, round_capacity_up(0));
        assert_eq!(16, round_capacity_up(1));
        assert_eq!(32, round_capacity_up(17));
        assert_eq!(32, round_capacity_up(32));
        assert_eq!(
            (std::usize::MAX >> 1) + 1,
            round_capacity_up(std::usize::MAX >> 1)
        );
    }
}
