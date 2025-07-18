use bytes::{Bytes, BytesMut};
use std::cell::{Cell, UnsafeCell};

#[derive(Default)]
pub(crate) struct BytesExt {
    // The only reason we use `UnsafeCell` here is to provide `extend_by_ref` method
    // in the sound way. After stabilization of the polonius borrow checker, it
    // will be replaced with simple `Bytes`. See `RowCursor::next()` for details.
    bytes: UnsafeCell<Bytes>,

    // Points to the real start of the remaining slice.
    // `Cell` allows us to mutate this value while keeping references to `bytes`.
    // Also, the dedicated counter is faster than using `Bytes::advance()`.
    cursor: Cell<usize>,
}

impl BytesExt {
    /// Returns a remaining slice of bytes.
    #[inline(always)]
    pub(crate) fn slice(&self) -> &[u8] {
        &self.bytes()[self.cursor.get()..]
    }

    /// Returns the number of remaining bytes.
    #[inline(always)]
    pub(crate) fn remaining(&self) -> usize {
        self.bytes().len() - self.cursor.get()
    }

    /// Overrides the number of remaining bytes by moving the cursor.
    /// Note: it's valid to call this method while holding `slice()` reference.
    #[inline(always)]
    pub(crate) fn set_remaining(&self, n: usize) {
        self.cursor.set(self.bytes().len() - n);
    }

    #[cfg(any(test, feature = "lz4"))]
    #[inline(always)]
    pub(crate) fn advance(&mut self, n: usize) {
        debug_assert!(n <= self.remaining());
        *self.cursor.get_mut() += n;
    }

    /// Adds the provided chunk into available bytes.
    #[inline(always)]
    pub(crate) fn extend(&mut self, chunk: Bytes) {
        *self.bytes.get_mut() = merge_bytes(self.slice(), chunk);
        self.cursor.set(0);
    }

    /// Adds the provided chunk into available bytes.
    ///
    /// See `RowCursor::next()` for details on why this method exists.
    ///
    /// # Safety
    ///
    /// The caller MUST ensure that there are no active references from `slice()` calls.
    #[inline(always)]
    pub(crate) unsafe fn extend_by_ref(&self, chunk: Bytes) {
        let new_bytes = merge_bytes(self.slice(), chunk);

        // No active references to `bytes` are held at this point (ensured by the caller).
        *self.bytes.get() = new_bytes;
        self.cursor.set(0);
    }

    fn bytes(&self) -> &Bytes {
        // SAFETY: all possible incorrect accesses are ensured by caller's of `extend_by_ref()`.
        unsafe { &*self.bytes.get() }
    }
}

fn merge_bytes(lhs: &[u8], rhs: Bytes) -> Bytes {
    if lhs.is_empty() {
        // Most of the time, we read the next chunk after consuming the previous one.
        rhs
    } else {
        // Some bytes are left in the buffer, we need to merge them with the next chunk.
        merge_bytes_slow(lhs, rhs)
    }
}

#[cold]
#[inline(never)]
fn merge_bytes_slow(lhs: &[u8], rhs: Bytes) -> Bytes {
    let new_len = lhs.len() + rhs.len();
    let mut new_bytes = BytesMut::with_capacity(new_len);
    let capacity = new_bytes.capacity();
    new_bytes.extend_from_slice(lhs);
    new_bytes.extend_from_slice(&rhs);
    debug_assert_eq!(new_bytes.capacity(), capacity);
    new_bytes.freeze()
}

#[cfg(test)]
mod tests_miri {
    use super::*;

    #[test]
    fn smoke() {
        let mut bytes = BytesExt::default();
        assert!(bytes.slice().is_empty());
        assert_eq!(bytes.remaining(), 0);

        // zero cursor, fast path
        bytes.extend(Bytes::from_static(b"hello"));
        assert_eq!(bytes.slice(), b"hello");
        assert_eq!(bytes.remaining(), 5);

        bytes.advance(3);
        assert_eq!(bytes.slice(), b"lo");
        assert_eq!(bytes.remaining(), 2);

        // non-zero cursor, slow path
        bytes.extend(Bytes::from_static(b"l"));
        assert_eq!(bytes.slice(), b"lol");
        assert_eq!(bytes.remaining(), 3);

        bytes.set_remaining(1);
        assert_eq!(bytes.slice(), b"l");
        assert_eq!(bytes.remaining(), 1);

        bytes.advance(1);
        assert_eq!(bytes.remaining(), 0);
        assert_ne!(bytes.cursor.get(), 0);

        // non-zero cursor, but fast path
        bytes.extend(Bytes::from_static(b"l"));
        assert_eq!(bytes.slice(), b"l");
        assert_eq!(bytes.remaining(), 1);
    }

    // Unfortunately, we cannot run miri against async code in order to check
    // the unsafe code in `RowCursor::next()`. However, we can at least
    // check that the valid usage of `extend_by_ref()` is free of UB.
    #[test]
    fn extend_by_ref() {
        fn next(buffer: &mut BytesExt) -> &[u8] {
            loop {
                if let Some(slice) = decode(buffer.slice()) {
                    buffer.set_remaining(buffer.remaining() - 3);
                    return slice;
                }

                let more = read_more();

                // Compilation error:
                /*
                buffer.extend(more);
                */

                // SAFETY: we're checking it right now in miri =)
                unsafe { buffer.extend_by_ref(more) };
            }
        }

        fn decode(buffer: &[u8]) -> Option<&[u8]> {
            if buffer.len() > 3 {
                Some(&buffer[..3])
            } else {
                None
            }
        }

        fn read_more() -> Bytes {
            Bytes::from_static(b"aaaa")
        }

        let mut buffer = BytesExt::default();
        for _ in 0..10 {
            assert_eq!(next(&mut buffer), b"aaa");
        }
    }
}
