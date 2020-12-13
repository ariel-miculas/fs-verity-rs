#![feature(str_split_once)]
#![feature(slice_fill)]

// $LINUX/include/uapi/linux/fsverity.h
use sha2::digest::generic_array::typenum::Unsigned;
use sha2::{Sha512, digest::{BlockInput, DynDigest, FixedOutput, Reset, Update}};
use std::os::unix::prelude::AsRawFd;
use std::io::Write;
use sha2::{Digest, Sha256};
use num_enum::TryFromPrimitive;
use std::convert::TryFrom;

const FS_VERITY_HASH_ALG_SHA256: u8 = 1;
const FS_VERITY_HASH_ALG_SHA512: u8 = 2;

// FIXME these are calculated through complex macros that bindgen doesn't like.
//       it's also possible they are not the same on all architectures.
//       should really check this!!!
const FS_IOC_ENABLE_VERITY: u64 = 1082156677;
const FS_IOC_MEASURE_VERITY: u64 = 3221513862;

#[repr(C)]
pub struct fsverity_enable_arg {
    pub version: u32,
    pub hash_algorithm: u32,
    pub block_size: u32,
    pub salt_size: u32,
    pub salt_ptr: *const [u8],
    pub sig_size: u32,
    pub __reserved1: u32,
    pub sig_ptr: u64,
    pub __reserved2: [u64; 11],
}

const MAX_DIGEST_SIZE: usize = std::mem::size_of::<sha2::digest::Output<Sha512>>();
const MAX_SALT_SIZE: usize = 32;
const MAX_BLOCK_SIZE: usize = 4096;
const DEFAULT_BLOCK_SIZE: usize = 4096;

#[repr(C)]
struct fsverity_digest {
    digest_algorithm: u16,
    digest_size: u16,
    digest: [u8; MAX_DIGEST_SIZE],
}

fn f_enable_verity(fd: impl AsRawFd, block_size: usize, hash: VerityHashAlgorithm, salt: &[u8]) -> std::io::Result<()> {
    let fd = fd.as_raw_fd();

    assert!(salt.len() <= MAX_SALT_SIZE);
    assert!(block_size <= MAX_BLOCK_SIZE);
    assert!(block_size.is_power_of_two());

    let args = fsverity_enable_arg {
        version: 1,
        hash_algorithm: hash as u32,
        block_size: block_size as u32,
        salt_size: salt.len() as u32,
        salt_ptr: salt as *const _,
        sig_size: 0,
        __reserved1: Default::default(),
        sig_ptr: 0,
        __reserved2: Default::default(),
    };

    let ret = unsafe { libc::ioctl(fd, FS_IOC_ENABLE_VERITY, &args as *const _) };

    if ret != 0 {
        Err(std::io::Error::from_raw_os_error(ret))
    }
    else {
        Ok(())
    }
}

fn f_measure_verity(fd: impl AsRawFd) -> std::io::Result<(VerityHashAlgorithm, Vec<u8>)> {
    let fd = fd.as_raw_fd();

    let mut digest = fsverity_digest {
        digest_algorithm: 0,  // unset
        digest_size: MAX_DIGEST_SIZE as u16,
        digest: [0; MAX_DIGEST_SIZE],
    };

    let ret = unsafe { libc::ioctl(fd, FS_IOC_MEASURE_VERITY, &mut digest as *mut _) };

    if ret != 0 {
        Err(std::io::Error::from_raw_os_error(ret))
    }
    else {
        Ok((VerityHashAlgorithm::try_from(digest.digest_algorithm as u8).unwrap(), digest.digest[..digest.digest_size as usize].to_owned()))
    }
}

#[derive(Copy, Clone, PartialEq, Eq, parse_display::FromStr, parse_display::Display, Debug, TryFromPrimitive)]
#[display(style = "lowercase")]
#[repr(u8)]
pub enum VerityHashAlgorithm {
    Sha256 = FS_VERITY_HASH_ALG_SHA256,
    Sha512 = FS_VERITY_HASH_ALG_SHA512,
}

// FIXME make a FsVerity-specific trait implemented by both dyn and static versions.
//       also, this API does not allow to specify options such as salt.
trait DynDigestWrite: DynDigest + Write {}
impl<D: VerityDigestExt + 'static> DynDigestWrite for FsVerityDigest<D> {}

impl Into<Box<dyn DynDigestWrite>> for VerityHashAlgorithm {
    fn into(self) -> Box<dyn DynDigestWrite> {
        match self {
            VerityHashAlgorithm::Sha256 => Box::new(FsVeritySha256::new()),
            VerityHashAlgorithm::Sha512 => Box::new(FsVeritySha512::new()),
        }
    }
}

/// Extends a Digest with some extra information we need, as well as two useful utility methods.
pub trait VerityDigestExt: Update + FixedOutput + Reset + Default + Clone + BlockInput {
    const VERITY_HASH_ALGORITHM: VerityHashAlgorithm;

    fn update_padded(&mut self, data: &[u8], padded_size: usize) {
        self.update(data);
        self.update_zeroes(padded_size.checked_sub(data.len()).unwrap());
    }

    fn update_zeroes(&mut self, mut amount: usize) {
        let zeroes = [0u8; 64];
        while amount != 0 {
            let n = zeroes.len().min(amount);
            self.update(&zeroes[..n]);
            amount -= n;
        }
    }
}

impl VerityDigestExt for Sha256 {
    const VERITY_HASH_ALGORITHM: VerityHashAlgorithm = VerityHashAlgorithm::Sha256;
}

impl VerityDigestExt for Sha512 {
    const VERITY_HASH_ALGORITHM: VerityHashAlgorithm = VerityHashAlgorithm::Sha512;
}

/// Logically a fixed-size block of data to be hashed (padded with zeroes if needed.)
/// Actually remembers only the hash state and how many more bytes are needed.
#[derive(Clone)]
struct FixedSizeBlock<D: VerityDigestExt> {
    inner: D,
    remaining: usize,
}

impl<D: VerityDigestExt> FixedSizeBlock<D> {
    fn new(inner: D, block_size: usize) -> Self {
        Self { inner: inner.clone(), remaining: block_size }
    }

    /// Appends data to block, panics if it doesn't fit.
    fn append(&mut self, data: &[u8]) {
        self.inner.update(data);
        self.remaining = self.remaining.checked_sub(data.len()).unwrap();
    }

    /// Appends as much as possible to the block, returning the data that wouldn't fit.
    fn overflowing_append<'a>(&mut self, data: &'a [u8]) -> &'a [u8] {
        let (a, b) = data.split_at(self.remaining.min(data.len()));
        self.append(a);
        b
    }

    /// Convenience method for creating a clone of this block with some data appended to it.
    fn clone_and_append(&self, data: &[u8]) -> Self {
        let mut tmp = self.clone();
        tmp.append(data);
        tmp
    }

    /// Consume the block and returns its hash
    fn finalize(mut self) -> sha2::digest::Output<D> {
        self.inner.update_zeroes(self.remaining);
        self.inner.finalize()
    }
}

// https://www.kernel.org/doc/html/latest/filesystems/fsverity.html#userspace-utility
// https://git.kernel.org/pub/scm/linux/kernel/git/ebiggers/fsverity-utils.git/tree/lib/compute_digest.c

#[derive(Clone)]
pub struct FsVerityDigest<D: VerityDigestExt> {
    block_size: usize,
    salt: Box<[u8]>,
    /// Digest state pre-initialized with the salt. Cloned whenever we need that.
    salted_digest: D,
    /// Cloned whenever we need a new empty block.
    empty_block: FixedSizeBlock<D>,
    /// The currently relevant hierarchy of blocks in the Merkle Tree.
    /// Level 0 is filled with the input data. when the block at level n fills up, the hash of
    /// its contents is appended to the block at level n + 1, and it is reset to an empty state.
    /// (this process can repeat if that causes the next level to fill up and so on).
    /// We do not actually keep the content for each block, only the hash state.
    levels: Vec<FixedSizeBlock<D>>,
    /// Amount of input data processed so far
    total_size: usize,
}

impl<D: VerityDigestExt> Default for FsVerityDigest<D> {
    fn default() -> Self { Self::new() }
}

impl<D: VerityDigestExt> FsVerityDigest<D> {
    fn new_with_block_size_and_salt(block_size: usize, salt: &[u8]) -> Self {
        // TODO error instead of panic?
        assert!(block_size.is_power_of_two());
        assert!(block_size <= MAX_BLOCK_SIZE);
        assert!(D::OutputSize::to_usize() * 2 <= block_size);
        assert!(salt.len() <= MAX_SALT_SIZE);

        // salt should be padded up to the "nearest multiple" of the input block size.
        // assert that in practice this "multiple" is 0 or 1, due to the low MAX_SALT_SIZE.
        assert!(MAX_SALT_SIZE <= D::BlockSize::to_usize());

        let salted_digest = {
            let mut tmp = D::new();
            if salt.len() != 0 {
                tmp.update_padded(salt, D::BlockSize::to_usize());
            }
            tmp
        };

        let empty_block = FixedSizeBlock::new(salted_digest.clone(), block_size);

        Self {
            block_size,
            salt: salt.into(),
            salted_digest,
            empty_block,
            levels: vec![],
            total_size: 0,
        }
    }

    fn new() -> Self {
        FsVerityDigest::new_with_block_size_and_salt(DEFAULT_BLOCK_SIZE, &[])
    }
}


impl<D: VerityDigestExt> Update for FsVerityDigest<D> {

    fn update(&mut self, data: impl AsRef<[u8]>) {
        for chunk in data.as_ref().chunks(self.block_size) {

            // invariants that hold before and after this loop:
            // - level 0 is (once it's created) never empty. it *may* be completely full.
            // - levels 1..n are never full, they always have room for one more hash. they *may* be empty.
            // this is implemented using the 'keep_space_for_one_digest' flag which is false for block 0,
            // and true for all others. the reason for this asymmetry is that it makes flushing the final
            // state (at the end of file) a lot simpler.
            // note that due to multiple reasons, the overflowing_append call will only ever split the
            // data in overflow across two blocks when writing input data (into block 0.) splitting a
            // digest across two blocks would be incorrect, so it is good that this never happens.
            // (the first reason is that both block size and currently defined digest sizes are powers of
            // two, so the block size is always an exact multiple of the digest size. the second reason
            // is that (as mentioned) we always make sure there is room for an entire digest.)
            let mut keep_space_for_one_digest = false;
            let mut last_digest: sha2::digest::Output<Self>;
            let mut overflow = chunk;

            for level in self.levels.iter_mut() {

                overflow = level.overflowing_append(overflow);
                if overflow.len() == 0 {
                    if !keep_space_for_one_digest || level.remaining >= D::OutputSize::to_usize() {
                        break;
                    }
                }

                let new_block = self.empty_block.clone_and_append(overflow);
                last_digest = std::mem::replace(level, new_block).finalize();
                overflow = &last_digest;

                keep_space_for_one_digest = true;  // only false for the first loop iteration
            }

            if overflow.len() != 0 {
                self.levels.push(self.empty_block.clone_and_append(overflow));
            }

            self.total_size += chunk.len();
        }
    }

}

impl<D: VerityDigestExt> FixedOutput for FsVerityDigest<D> {
    type OutputSize = D::OutputSize;

    fn finalize_into(self, out: &mut sha2::digest::Output<Self>) {

        // flush all levels. zero length files are defined to have a root "hash" of all zeroes.
        // the root hash is ambiguous by itself, since it is simply a hash of block_size bytes
        // of data, and that data could have been either file data or digests of other blocks.
        // you always need the file size as well to properly interpret the root hash.
        // since a file size of 0 already uniquely identifies the file's contents there is no
        // point in even looking at the root hash. I guess fs-verity defines it as all zeroes
        // to avoid needing to do any hashing at all in that case.
        let mut last_digest: sha2::digest::Output<Self> = Default::default();
        let mut overflow: &[u8] = &[];
        for mut level in self.levels.into_iter() {
            level.append(overflow);
            last_digest = level.finalize();
            overflow = &last_digest;
        }

        // https://www.kernel.org/doc/html/latest/filesystems/fsverity.html
        // $LINUX/fs/verity/fsverity_private.h
        // #[repr(C)]
        // struct fsverity_descriptor {
        //     version: u8,           /* must be 1 */
        //     hash_algorithm: u8,    /* Merkle tree hash algorithm */
        //     log_blocksize: u8,     /* log2 of size of data and tree blocks */
        //     salt_size: u8,         /* size of salt in bytes; 0 if none */
        //     sig_size: u32,         /* must be 0 */
        //     data_size: u64,        /* little-endian size of file the Merkle tree is built over */
        //     root_hash: [u8; 64],   /* Merkle tree root hash */
        //     salt: [u8; 32],        /* salt prepended to each hashed block */
        //     reserved: [u8; 144],   /* must be 0's */
        // }

        let mut descriptor = self.salted_digest.clone();
        descriptor.update(&[1]);
        descriptor.update(&[D::VERITY_HASH_ALGORITHM as u8]);
        descriptor.update(&[self.block_size.trailing_zeros() as u8]);
        descriptor.update(&[self.salt.len() as u8]);
        descriptor.update(&0u32.to_le_bytes());
        descriptor.update(&(self.total_size as u64).to_le_bytes());
        descriptor.update_padded(&last_digest, 64);
        descriptor.update_padded(&self.salt, MAX_SALT_SIZE);
        descriptor.update_zeroes(144);

        descriptor.finalize_into(out);
    }

    fn finalize_into_reset(&mut self, out: &mut sha2::digest::Output<Self>) {
        std::mem::replace(self, Self::new_with_block_size_and_salt(self.block_size, &self.salt)).finalize_into(out);
    }
}

impl<D: VerityDigestExt> Reset for FsVerityDigest<D> {
    fn reset(&mut self) {
        *self = Self::new_with_block_size_and_salt(self.block_size, &self.salt);
    }
}

impl<D: VerityDigestExt> Write for FsVerityDigest<D> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Update::update(self, buf);  // FIXME why does self.update() complain about multiple impls?
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub type FsVeritySha256 = FsVerityDigest<Sha256>;
pub type FsVeritySha512 = FsVerityDigest<Sha512>;

#[cfg(test)]
mod tests {
use crate::DynDigestWrite;
use std::io::BufReader;
use std::fs::File;
use crate::VerityHashAlgorithm;

    #[test]
    fn test_testfiles() {

        // 'longfile' takes a while in debug mode, about 20 seconds for me.
        // in release mode it takes about a second.
        // sha256:e228078ebe9c4f7fe0c5d6a76fb2e317f5ea8bdfb227d7741e5c57cff739b5fa testfiles/longfile
        let testfiles = "
        sha256:3d248ca542a24fc62d1c43b916eae5016878e2533c88238480b26128a1f1af95 testfiles/empty
        sha256:f5c2b9ded1595acfe8a996795264d488dd6140531f6a01f8f8086a83fd835935 testfiles/hashblock_0_0
        sha256:5c00a54bd1d8341d7bbad060ff1b8e88ed2646d7bb38db6e752cd1cff66c0a78 testfiles/hashblock_0_-1
        sha256:a7abb76568871169a79104d00679fae6521dfdb2a2648e380c02b10e96e217ff testfiles/hashblock_0_1
        sha256:c4b519068d8c8c68fd5e362fc3526c5b11e15f8eb72d4678017906f9e7f2d137 testfiles/hashblock_-1_0
        sha256:09510d2dbb55fa16f2768165c42d19c4da43301dfaa05705b2ecb4aaa4a5686a testfiles/hashblock_1_0
        sha256:7aa0bb537c623562f898386ac88acd319267e4ab3200f3fd1cf648cfdb4a0379 testfiles/hashblock_-1_-1
        sha256:f804e9777f91d3697ca015303c23251ad3d80205184cfa3d1066ab28cb906330 testfiles/hashblock_-1_1
        sha256:26159b4fc68c63881c25c33b23f2583ffaa64fee411af33c3b03238eea56755c testfiles/hashblock_1_-1
        sha256:57bed0934bf3ab4610d54938f03cff27bd0d9d76c9a77e283f9fb2b7e29c5ab8 testfiles/hashblock_1_1
        sha256:3fd7a78101899a79cd337b1b4e5414be8bcb376b133370156ef6e65026d930ed testfiles/oneblock
        sha256:c0b9455d545b6b1ee5e7b227bd1ed463aaa530a4840dcd93465163a2b3aff0da testfiles/oneblockplusonebyte
        sha256:9845e616f7d2f7a1cd6742f0546a36d2e74d4eb8ae7d9bdc0b0df982c27861b7 testfiles/onebyte
        ".trim().lines().map(|l| {
            let l = l.trim();
            let (digest, path) = l.split_once(" ").unwrap();
            let (digest_type, digest) = digest.split_once(":").unwrap();
            let digest_type = digest_type.parse::<super::VerityHashAlgorithm>().unwrap();
            let digest = hex::decode(digest).unwrap();
            (digest_type, digest, path)
        }).collect::<Vec<_>>();

        for (digest_type, digest, path) in testfiles {
            assert!(digest_type == VerityHashAlgorithm::Sha256);
            let mut f = BufReader::new(File::open(path).unwrap());
            let mut tmp: Box<dyn DynDigestWrite> = digest_type.into();
            std::io::copy(&mut f, &mut tmp).unwrap();
            let out = tmp.finalize();

            let tmp = hex::encode(&digest);
            let tmp2 = hex::encode(&out);
            assert!(out.as_ref() == &digest, "expected: {} found: {} for file: {}", tmp, tmp2, path);
        }
    }
}
