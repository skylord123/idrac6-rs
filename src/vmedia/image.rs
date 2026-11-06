//! Disk image backing for virtual media: ISO (CD/DVD) and IMG (floppy /
//! removable disk) files, including the geometry the Java client derives
//! from the image's boot sector.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Block device geometry announced to the iDRAC when mapping an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub block_size: u32,
    /// Block count sent in the map request. For ISO images this is
    /// `size / 2048 - 1` (the last LBA, as the Java client does); for IMG
    /// images it is the block count of the exposed area.
    pub blocks: u32,
    pub cylinders: u16,
    pub heads: u8,
    pub sectors: u8,
    /// Offset (in blocks) added to every read/write. Non-zero only for MBR
    /// images mapped on a removable ("floppy mode") drive, where only the
    /// first partition is exposed.
    pub lba_start: u64,
}

/// Something that can serve blocks: a file on disk, or memory in tests.
pub trait Backing: Send {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()>;
    fn len(&mut self) -> io::Result<u64>;
}

impl Backing for File {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.seek(SeekFrom::Start(offset))?;
        let mut n = 0;
        while n < buf.len() {
            match self.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(n)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.seek(SeekFrom::Start(offset))?;
        self.write_all(buf)?;
        self.flush()
    }
    fn len(&mut self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }
}

impl Backing for io::Cursor<Vec<u8>> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let data = self.get_ref();
        let off = (offset as usize).min(data.len());
        let n = buf.len().min(data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        Ok(n)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let data = self.get_mut();
        let end = offset as usize + buf.len();
        if end > data.len() {
            data.resize(end, 0);
        }
        data[offset as usize..end].copy_from_slice(buf);
        Ok(())
    }
    fn len(&mut self) -> io::Result<u64> {
        Ok(self.get_ref().len() as u64)
    }
}

/// An opened image ready to be mapped.
pub struct Image {
    backing: Box<dyn Backing>,
    pub read_only: bool,
    pub is_cd: bool,
    size: u64,
    pub geometry: Geometry,
}

impl Image {
    /// Open an ISO for a virtual CD/DVD. Always read-only.
    pub fn open_iso(path: &Path) -> io::Result<Image> {
        let f = File::open(path)?;
        Image::iso(Box::new(f))
    }

    /// Open an IMG for a virtual floppy / removable disk. `floppy_mode` is
    /// true when the target remote drive has type 4 (see `proto`).
    pub fn open_img(path: &Path, read_only: bool, floppy_mode: bool) -> io::Result<Image> {
        let f = OpenOptions::new().read(true).write(!read_only).open(path)?;
        Image::img(Box::new(f), read_only, floppy_mode)
    }

    pub fn iso(mut backing: Box<dyn Backing>) -> io::Result<Image> {
        let size = backing.len()?;
        if size < 2048 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "ISO image is too small"));
        }
        // The iDRAC treats this as the number of blocks. Dell's client sends
        // size/2048 - 1, which hides the last 2 KiB of every ISO from the
        // server; send the real count (a partial last block reads as zeros).
        let blocks = size.div_ceil(2048);
        Ok(Image {
            backing,
            read_only: true,
            is_cd: true,
            size,
            geometry: Geometry {
                block_size: 2048,
                blocks: blocks.min(u32::MAX as u64) as u32,
                cylinders: 0,
                heads: 0,
                sectors: 0,
                lba_start: 0,
            },
        })
    }

    pub fn img(mut backing: Box<dyn Backing>, read_only: bool, floppy_mode: bool) -> io::Result<Image> {
        let size = backing.len()?;
        if size < 512 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "disk image is too small"));
        }
        let mut sector0 = [0u8; 512];
        let n = backing.read_at(0, &mut sector0)?;
        if n < 512 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read of boot sector"));
        }
        let geometry = disk_geometry(&sector0, size / 512, floppy_mode);
        Ok(Image { backing, read_only, is_cd: false, size, geometry })
    }

    pub fn block_size(&self) -> u32 {
        self.geometry.block_size
    }

    /// Read `blocks` blocks starting at `start` (relative to the exposed
    /// area). Data past the end of the file reads as zeros; a read that
    /// starts past the end is an error.
    pub fn read_blocks(&mut self, start: u32, blocks: u32) -> io::Result<Vec<u8>> {
        let bs = self.block_size() as u64;
        let off = (start as u64 + self.geometry.lba_start) * bs;
        let len = blocks as u64 * bs;
        if off >= self.size && len > 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past end of image"));
        }
        let mut buf = vec![0u8; len as usize];
        self.backing.read_at(off, &mut buf)?;
        Ok(buf)
    }

    pub fn write_blocks(&mut self, start: u32, data: &[u8]) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "image is read-only"));
        }
        let bs = self.block_size() as u64;
        let off = (start as u64 + self.geometry.lba_start) * bs;
        if off + data.len() as u64 > self.size {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "write past end of image"));
        }
        self.backing.write_at(off, data)
    }
}

/// Port of `LocalDrive.getCHS` / `BootSector` for image files.
fn disk_geometry(s0: &[u8; 512], total_blocks: u64, floppy_mode: bool) -> Geometry {
    // (cylinders, heads, sectors, lba_start, partition_blocks)
    let (mut cyl, heads, sectors, lba_start, part_blocks): (u64, u32, u32, u64, u64) = if s0[0] == 0xEB {
        // FAT style BPB: sectors per track at 0x18, heads at 0x1A (LE).
        let spt = u16::from_le_bytes([s0[24], s0[25]]) as u32;
        let hd = u16::from_le_bytes([s0[26], s0[27]]) as u32;
        let c = if spt > 0 && hd > 0 { total_blocks / (spt as u64 * hd as u64) } else { 0 };
        (c, hd, spt, 0, total_blocks)
    } else if s0[0] == 0 {
        // No boot sector (unformatted).
        (0, 0, 0, 0, total_blocks)
    } else {
        // MBR: use the first partition with a non-zero start LBA.
        let mut found = None;
        for off in [446usize, 462, 478, 494] {
            let e = &s0[off..off + 16];
            let lba = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
            let size = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as u64;
            if lba > 0 {
                found = Some((lba, size, e[5] as u32 + 1, (e[6] & 0x3F) as u32));
                break;
            }
        }
        match found {
            Some((lba, size, hd, spt)) => {
                let end = lba + size;
                if end > 16_514_064 {
                    (16383, 16, 63, lba, size)
                } else if spt > 0 {
                    (end / hd as u64 / spt as u64, hd, spt, lba, size)
                } else {
                    (0, 0, 0, lba, size)
                }
            }
            None => (0, 0, 0, 0, total_blocks),
        }
    };
    let n3 = cyl;
    if heads != 0 && sectors != 0 {
        cyl = total_blocks / heads as u64 / sectors as u64;
    }
    if cyl < n3 {
        cyl = n3;
    }
    let (mut heads, mut sectors) = (heads, sectors);
    if total_blocks > 16_450_560 {
        cyl = 16383;
        heads = 16;
        sectors = 63;
    }
    let (blocks, lba) = if floppy_mode { (part_blocks, lba_start) } else { (total_blocks, 0) };
    Geometry {
        block_size: 512,
        blocks: blocks.min(u32::MAX as u64) as u32,
        cylinders: cyl.min(u16::MAX as u64) as u16,
        heads: heads.min(255) as u8,
        sectors: sectors.min(255) as u8,
        lba_start: lba,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floppy_144() -> Vec<u8> {
        let mut v = vec![0u8; 1_474_560];
        v[0] = 0xEB;
        v[24] = 18; // sectors per track
        v[26] = 2; // heads
        v
    }

    #[test]
    fn floppy_geometry() {
        let img = Image::img(Box::new(io::Cursor::new(floppy_144())), false, false).unwrap();
        assert_eq!(
            img.geometry,
            Geometry { block_size: 512, blocks: 2880, cylinders: 80, heads: 2, sectors: 18, lba_start: 0 }
        );
    }

    #[test]
    fn mbr_floppy_mode_exposes_partition() {
        let mut v = vec![0u8; 512 * 4096];
        v[0] = 0x33; // MBR code
        let e = 446;
        v[e + 5] = 15; // end head
        v[e + 6] = 63; // end sector
        v[e + 8..e + 12].copy_from_slice(&63u32.to_le_bytes());
        v[e + 12..e + 16].copy_from_slice(&4000u32.to_le_bytes());
        v[63 * 512] = 0xAB;
        let mut img = Image::img(Box::new(io::Cursor::new(v.clone())), true, true).unwrap();
        assert_eq!(img.geometry.blocks, 4000);
        assert_eq!(img.geometry.lba_start, 63);
        assert_eq!(img.read_blocks(0, 1).unwrap()[0], 0xAB);
        let mut whole = Image::img(Box::new(io::Cursor::new(v)), true, false).unwrap();
        assert_eq!(whole.geometry.blocks, 4096);
        assert_eq!(whole.read_blocks(0, 1).unwrap()[0], 0x33);
    }

    #[test]
    fn iso_reads_and_last_lba() {
        let mut v = vec![0u8; 2048 * 10];
        v[2048 * 3] = 7;
        let mut img = Image::iso(Box::new(io::Cursor::new(v))).unwrap();
        assert_eq!(img.geometry.blocks, 10);
        assert!(img.read_only);
        let d = img.read_blocks(3, 2).unwrap();
        assert_eq!(d.len(), 4096);
        assert_eq!(d[0], 7);
        assert!(img.read_blocks(10, 1).is_err());
        assert!(img.write_blocks(0, &[0; 2048]).is_err());
    }

    #[test]
    fn img_write() {
        let mut img = Image::img(Box::new(io::Cursor::new(floppy_144())), false, false).unwrap();
        img.write_blocks(5, &[9u8; 1024]).unwrap();
        assert_eq!(img.read_blocks(6, 1).unwrap()[511], 9);
        assert!(img.write_blocks(2880, &[0; 512]).is_err());
    }
}
