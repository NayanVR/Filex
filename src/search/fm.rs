//! Selected FM representation: BWT wavelet rank with 512-bit checkpoints.
use super::wavelet::{MatrixImage, WaveletMatrix};
use crate::catalog::storage::{Reader, Writer};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::ops::Range;
pub struct FmIndex {
    matrix: WaveletMatrix,
    cumulative: Vec<usize>,
    len: usize,
    skip: usize,
}
#[derive(Serialize, Deserialize)]
pub struct FmImage {
    matrix: MatrixImage,
    cumulative: Vec<usize>,
    len: usize,
    skip: usize,
}
impl FmIndex {
    pub(crate) fn remap(&mut self, reader: &Reader) -> Result<()> {
        self.matrix.remap(reader)?;
        Ok(())
    }

    pub fn new(bwt: Vec<u16>, skip: usize) -> Self {
        let mut cumulative = vec![0; 257];
        for &symbol in &bwt {
            cumulative[symbol as usize] += 1;
        }
        let mut sum = 0;
        for count in &mut cumulative {
            let n = *count;
            *count = sum;
            sum += n;
        }
        let len = bwt.len();
        Self {
            matrix: WaveletMatrix::new(bwt.into_iter().map(u32::from).collect()),
            cumulative,
            len,
            skip,
        }
    }
    pub fn range(&self, needle: &[u8]) -> Range<usize> {
        if needle.is_empty() || needle.contains(&0) {
            return 0..0;
        }
        let (mut left, mut right) = (0, self.len);
        for &byte in needle.iter().rev() {
            let symbol = u32::from(byte) + 1;
            left = self.cumulative[symbol as usize] + self.matrix.rank(symbol, left);
            right = self.cumulative[symbol as usize] + self.matrix.rank(symbol, right);
            if left == right {
                return 0..0;
            }
        }
        left.saturating_sub(self.skip)..right.saturating_sub(self.skip)
    }
    pub fn bytes(&self) -> usize {
        self.matrix.bytes() + self.cumulative.capacity() * 8
    }
    pub fn save(&self, writer: &mut Writer) -> Result<FmImage> {
        Ok(FmImage {
            matrix: self.matrix.save(writer)?,
            cumulative: self.cumulative.clone(),
            len: self.len,
            skip: self.skip,
        })
    }
    pub fn load(reader: &Reader, image: FmImage) -> Result<Self> {
        let matrix = WaveletMatrix::load(reader, image.matrix)?;
        ensure!(
            image.cumulative.len() == 257 && image.skip <= image.len && matrix.len() == image.len,
            "invalid FM header"
        );
        let mut total = 0;
        for (symbol, &count) in image.cumulative.iter().enumerate() {
            ensure!(count == total, "invalid FM counts");
            total += matrix.rank(symbol as u32, image.len);
        }
        ensure!(total == image.len, "invalid FM alphabet");
        Ok(Self {
            matrix,
            cumulative: image.cumulative,
            len: image.len,
            skip: image.skip,
        })
    }
}
