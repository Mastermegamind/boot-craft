//! Byte-level access to a drive that only takes whole sectors.
//!
//! File system code (the FAT formatter and writer) reads and writes a few
//! bytes at a time: a FAT entry, a directory record. Raw drives on macOS and
//! Windows take only whole, aligned sectors, and USB sticks are slow at small
//! writes everywhere. [`Cache`] sits between the two: it keeps recently used
//! 1 MiB blocks in memory, sector by sector, reads only the sectors it hasn't
//! got, and writes back runs of changed sectors, all whole and aligned.
//! [`Slice`] then gives a file system one partition of it.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// The unit the cache keeps and evicts.
const BLOCK: u64 = 1 << 20;
/// Blocks kept in memory (32 MiB).
const MAX_BLOCKS: usize = 32;

struct Block {
    data: Vec<u8>,
    /// Per sector: holds the drive's bytes or newer ones.
    valid: Vec<bool>,
    /// Per sector: changed since it was last written to the drive.
    dirty: Vec<bool>,
    used: u64,
}

/// A write-back sector cache over a whole drive. Every read and write it
/// makes on `dev` is whole sectors at a sector boundary, inside `size`.
pub struct Cache<D> {
    dev: D,
    sector: u64,
    size: u64,
    blocks: HashMap<u64, Block>,
    clock: u64,
    pos: u64,
}

impl<D: Read + Write + Seek> Cache<D> {
    /// `size` is the drive's size in bytes and `sector` its sector size; the
    /// size must be a whole number of sectors.
    pub fn new(dev: D, size: u64, sector: usize) -> io::Result<Self> {
        let sector = sector.max(1) as u64;
        if !sector.is_power_of_two() || sector > BLOCK || size % sector != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("a {size}-byte drive with {sector}-byte sectors")));
        }
        Ok(Self { dev, sector, size, blocks: HashMap::new(), clock: 0, pos: 0 })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Writes every changed sector to the drive and flushes it.
    pub fn flush_all(&mut self) -> io::Result<()> {
        let mut indexes: Vec<u64> = self.blocks.keys().copied().collect();
        indexes.sort_unstable();
        for index in indexes {
            self.write_back(index)?;
        }
        self.dev.flush()
    }

    /// Flushes, then hands back the drive.
    pub fn into_inner(mut self) -> io::Result<D> {
        self.flush_all()?;
        Ok(self.dev)
    }

    /// Forgets every cached sector (after flushing), so later reads come
    /// from the drive.
    pub fn forget(&mut self) -> io::Result<()> {
        self.flush_all()?;
        self.blocks.clear();
        Ok(())
    }

    pub fn get_mut(&mut self) -> &mut D {
        &mut self.dev
    }

    fn block_len(&self, index: u64) -> usize {
        (self.size - index * BLOCK).min(BLOCK) as usize
    }

    /// The block, loaded (empty) if it isn't cached yet.
    fn block(&mut self, index: u64) -> io::Result<&mut Block> {
        self.clock += 1;
        if !self.blocks.contains_key(&index) {
            if self.blocks.len() >= MAX_BLOCKS {
                let oldest = self.blocks.iter().min_by_key(|(_, b)| b.used).map(|(&i, _)| i).expect("a block");
                self.write_back(oldest)?;
                self.blocks.remove(&oldest);
            }
            let len = self.block_len(index);
            let sectors = len / self.sector as usize;
            self.blocks.insert(index, Block { data: vec![0; len], valid: vec![false; sectors], dirty: vec![false; sectors], used: 0 });
        }
        let clock = self.clock;
        let block = self.blocks.get_mut(&index).expect("just inserted");
        block.used = clock;
        Ok(block)
    }

    /// Reads sectors `first..end` of a block from the drive where it doesn't
    /// hold them yet, in as few reads as possible.
    fn fill(&mut self, index: u64, first: usize, end: usize) -> io::Result<()> {
        let sector = self.sector as usize;
        let block = self.block(index)?;
        let mut s = first;
        let mut runs = Vec::new();
        while s < end {
            if block.valid[s] {
                s += 1;
                continue;
            }
            let start = s;
            while s < end && !block.valid[s] {
                s += 1;
            }
            runs.push((start, s));
        }
        for (start, stop) in runs {
            self.dev.seek(SeekFrom::Start(index * BLOCK + (start * sector) as u64))?;
            let block = self.blocks.get_mut(&index).expect("cached");
            self.dev.read_exact(&mut block.data[start * sector..stop * sector])?;
            block.valid[start..stop].iter_mut().for_each(|v| *v = true);
        }
        Ok(())
    }

    fn write_back(&mut self, index: u64) -> io::Result<()> {
        let sector = self.sector as usize;
        let Some(block) = self.blocks.get_mut(&index) else { return Ok(()) };
        let mut s = 0;
        while s < block.dirty.len() {
            if !block.dirty[s] {
                s += 1;
                continue;
            }
            let start = s;
            while s < block.dirty.len() && block.dirty[s] {
                s += 1;
            }
            self.dev.seek(SeekFrom::Start(index * BLOCK + (start * sector) as u64))?;
            self.dev.write_all(&block.data[start * sector..s * sector])?;
            block.dirty[start..s].iter_mut().for_each(|d| *d = false);
        }
        Ok(())
    }

    /// Reads `buf.len()` bytes at `offset`; fewer only at the drive's end.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let end = offset.saturating_add(buf.len() as u64).min(self.size);
        let mut at = offset;
        while at < end {
            let (index, within) = (at / BLOCK, (at % BLOCK) as usize);
            let n = ((end - at) as usize).min(self.block_len(index) - within);
            let sector = self.sector as usize;
            self.fill(index, within / sector, (within + n).div_ceil(sector))?;
            let block = self.block(index)?;
            let out = (at - offset) as usize;
            buf[out..out + n].copy_from_slice(&block.data[within..within + n]);
            at += n as u64;
        }
        Ok((end.saturating_sub(offset)) as usize)
    }

    /// Writes all of `buf` at `offset`; refuses to go past the drive's end.
    pub fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let end = offset.checked_add(buf.len() as u64).filter(|&e| e <= self.size).ok_or_else(|| {
            io::Error::new(io::ErrorKind::WriteZero, format!("a write at byte {offset} would go past the drive's end ({})", self.size))
        })?;
        let sector = self.sector as usize;
        let mut at = offset;
        while at < end {
            let (index, within) = (at / BLOCK, (at % BLOCK) as usize);
            let n = ((end - at) as usize).min(self.block_len(index) - within);
            // Sectors only partly overwritten need their other bytes first.
            let (first, last) = (within / sector, (within + n - 1) / sector);
            if within % sector != 0 {
                self.fill(index, first, first + 1)?;
            }
            if (within + n) % sector != 0 {
                self.fill(index, last, last + 1)?;
            }
            let block = self.block(index)?;
            let from = (at - offset) as usize;
            block.data[within..within + n].copy_from_slice(&buf[from..from + n]);
            for s in first..=last {
                block.valid[s] = true;
                block.dirty[s] = true;
            }
            at += n as u64;
        }
        Ok(())
    }
}

impl<D: Read + Write + Seek> Read for Cache<D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.read_at(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<D: Read + Write + Seek> Write for Cache<D> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_at(self.pos, buf)?;
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_all()
    }
}

impl<D: Read + Write + Seek> Seek for Cache<D> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = seek_to(pos, self.pos, self.size)?;
        Ok(self.pos)
    }
}

fn seek_to(pos: SeekFrom, at: u64, len: u64) -> io::Result<u64> {
    let target = match pos {
        SeekFrom::Start(n) => Some(n),
        SeekFrom::Current(d) => at.checked_add_signed(d),
        SeekFrom::End(d) => len.checked_add_signed(d),
    };
    target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))
}

/// One partition of a [`Cache`]: offsets start at the partition's first
/// byte, and nothing outside it can be read or written.
pub struct Slice<'a, D> {
    cache: &'a mut Cache<D>,
    start: u64,
    len: u64,
    pos: u64,
}

impl<'a, D: Read + Write + Seek> Slice<'a, D> {
    pub fn new(cache: &'a mut Cache<D>, start: u64, len: u64) -> io::Result<Self> {
        if start.checked_add(len).is_none_or(|end| end > cache.size()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the partition goes past the drive's end"));
        }
        Ok(Self { cache, start, len, pos: 0 })
    }
}

impl<D: Read + Write + Seek> Read for Slice<'_, D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        let n = self.cache.read_at(self.start + self.pos, &mut buf[..want])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<D: Read + Write + Seek> Write for Slice<'_, D> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.pos.saturating_add(buf.len() as u64) > self.len {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "a write past the partition's end"));
        }
        self.cache.write_at(self.start + self.pos, buf)?;
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.cache.flush_all()
    }
}

impl<D: Read + Write + Seek> Seek for Slice<'_, D> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = seek_to(pos, self.pos, self.len)?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A drive that, like raw disks on macOS and Windows, refuses anything
    /// but whole aligned sectors, and counts what it's asked to do.
    struct Strict {
        data: Cursor<Vec<u8>>,
        sector: u64,
        reads: usize,
        writes: usize,
    }

    impl Strict {
        fn check(&self, len: usize) -> io::Result<()> {
            if self.data.position() % self.sector != 0 || len as u64 % self.sector != 0 {
                return Err(io::Error::other(format!("unaligned: {len} bytes at {}", self.data.position())));
            }
            Ok(())
        }
    }

    impl Read for Strict {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.check(buf.len())?;
            self.reads += 1;
            self.data.read(buf)
        }
    }

    impl Write for Strict {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.check(buf.len())?;
            self.writes += 1;
            self.data.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for Strict {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.data.seek(pos)
        }
    }

    fn strict(size: usize, sector: u64) -> Strict {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        Strict { data: Cursor::new(data), sector, reads: 0, writes: 0 }
    }

    #[test]
    fn small_unaligned_writes_become_whole_sectors() {
        let size = 3 * BLOCK as usize + 4096; // the last block is short
        let mut expected = strict(size, 4096).data.into_inner();
        let mut cache = Cache::new(strict(size, 4096), size as u64, 4096).unwrap();
        for (offset, len, byte) in
            [(5u64, 3usize, 1u8), (4090, 20, 2), (BLOCK - 7, 50, 3), (3 * BLOCK + 4000, 96, 4), (100_000, 300_000, 5)]
        {
            cache.write_at(offset, &vec![byte; len]).unwrap();
            expected[offset as usize..offset as usize + len].fill(byte);
        }
        let mut back = vec![0u8; 9000];
        assert_eq!(cache.read_at(BLOCK - 4000, &mut back).unwrap(), 9000);
        assert_eq!(back, expected[(BLOCK - 4000) as usize..(BLOCK + 5000) as usize]);
        let dev = cache.into_inner().unwrap();
        assert_eq!(dev.data.into_inner(), expected);
    }

    #[test]
    fn whole_sectors_are_written_without_reading_first() {
        let mut cache = Cache::new(strict(4 << 20, 512), 4 << 20, 512).unwrap();
        cache.write_at(0, &vec![7u8; 2 << 20]).unwrap();
        let dev = cache.into_inner().unwrap();
        assert_eq!(dev.reads, 0);
        assert_eq!(dev.writes, 2, "one write per changed run in each block");
    }

    #[test]
    fn evicted_blocks_are_written_and_read_back() {
        let blocks = MAX_BLOCKS as u64 + 8;
        let size = blocks * BLOCK;
        let mut cache = Cache::new(strict(size as usize, 512), size, 512).unwrap();
        for i in 0..blocks {
            cache.write_at(i * BLOCK + 10, &[i as u8; 4]).unwrap();
        }
        for i in 0..blocks {
            let mut four = [0u8; 4];
            cache.read_at(i * BLOCK + 10, &mut four).unwrap();
            assert_eq!(four, [i as u8; 4]);
        }
    }

    #[test]
    fn slices_keep_to_their_partition() {
        let mut cache = Cache::new(strict(1 << 20, 512), 1 << 20, 512).unwrap();
        let mut part = Slice::new(&mut cache, 4096, 8192).unwrap();
        part.seek(SeekFrom::End(-2)).unwrap();
        part.write_all(&[9, 9]).unwrap();
        assert!(part.write_all(&[1]).is_err());
        part.seek(SeekFrom::Start(8190)).unwrap();
        let mut rest = Vec::new();
        part.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, [9, 9]);
        let mut one = [0u8];
        cache.read_at(4096 + 8191, &mut one).unwrap();
        assert_eq!(one, [9]);
        assert!(cache.write_at((1 << 20) - 1, &[0, 0]).is_err());
        assert!(Slice::new(&mut cache, 1 << 20, 1).is_err());
    }
}
