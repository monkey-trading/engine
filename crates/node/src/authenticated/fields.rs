//! A stored fixed-width form's fields, read in order.

use anyhow::{Result, ensure};

/// A stored value's fixed-width fields, read in order; every integer is big-endian.
pub(crate) struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    /// Reads `bytes` from their first field.
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    /// The next `n` bytes.
    /// Error cases:
    /// - fewer are left
    pub(crate) fn slice(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.0.len() >= n, "short stored value");
        let (taken, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(taken)
    }

    /// The next `N` bytes.
    /// Error cases:
    /// - fewer are left
    pub(crate) fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.slice(N)?.try_into()?)
    }

    /// The next byte.
    /// Error cases:
    /// - none is left
    pub(crate) fn byte(&mut self) -> Result<u8> {
        Ok(self.take::<1>()?[0])
    }

    /// The next `u64`.
    /// Error cases:
    /// - fewer than eight bytes are left
    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take()?))
    }

    /// Checks no byte is left.
    /// Error cases:
    /// - a byte is left
    pub(crate) fn end(&self) -> Result<()> {
        ensure!(self.0.is_empty(), "bytes past a stored value");
        Ok(())
    }
}
