//! Non-standard variant of AES.
//!
//! AES is officially defined with key lengths of 128, 192 and 256 bits only,
//! but both the Rijndael itself and AES key derivation algorithm is flexible
//! enough that it can be generalized to any key sizes as long as it's
//! a multiple of 32 bits. It happens that CryptoJS, which Ridibooks uses,
//! supports larger keys in AES in an undocumented manner, and as Ridibooks
//! increased the base key length to 36 bytes in the schema version 4,
//! it silently started using one of such non-standard variant.
//!
//! Due to the added flexibility this is not a constant-time algorithm and
//! should be avoided unless absolutely required.

use std::fmt;

use aes::{
    cipher::{
        inout::InOut,
        typenum::{U16, U8},
        Block, BlockBackend, BlockCipher, BlockClosure, BlockDecrypt, BlockSizeUser, InvalidLength,
        Key, KeyInit, KeySizeUser, ParBlocksSizeUser,
    },
    hazmat::{equiv_inv_cipher_round, inv_mix_columns, mix_columns},
};

static SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

const RC: [u8; 11] = [
    // we need no more entries as the block size is fixed
    0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36,
];

#[inline]
fn xor_block(a: &mut [u8], b: &[u8]) {
    for (a, b) in a.iter_mut().zip(b.iter()) {
        *a ^= *b;
    }
}

#[derive(Clone)]
pub struct Aes32NDec {
    inv_round_keys: Vec<[u8; 16]>,
}

impl fmt::Debug for Aes32NDec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key_size = self.inv_round_keys.len() / 4 - 1;
        write!(f, "Aes32NDec {{ /* key size = {key_size} */ .. }}")
    }
}

// it seems that ciphers with variable key sizes still define KeySizeUser with
// a "preferred" key size and override `KeyInit::new_from_slices`; see e.g. `blowfish`
impl KeySizeUser for Aes32NDec {
    type KeySize = U16;
}

impl KeyInit for Aes32NDec {
    fn new(key: &Key<Self>) -> Self {
        Self::new_from_slice(&key[..]).unwrap()
    }

    fn new_from_slice(key: &[u8]) -> Result<Self, InvalidLength> {
        if !key.len().is_multiple_of(4) || key.len() < 16 {
            return Err(InvalidLength);
        }

        fn sub_word(w: u32) -> u32 {
            (SBOX[(w >> 24 & 0xff) as usize] as u32) << 24
                | (SBOX[(w >> 16 & 0xff) as usize] as u32) << 16
                | (SBOX[(w >> 8 & 0xff) as usize] as u32) << 8
                | (SBOX[(w & 0xff) as usize] as u32)
        }

        let n = key.len() / 4;
        let r = key.len() / 4 + 7;
        let mut words = Vec::with_capacity(r * 4);
        for word in key.chunks(4) {
            words.push(u32::from_le_bytes(word.try_into().unwrap()));
        }
        let mut w = *words.last().unwrap();
        for i in n..r * 4 {
            w = match i % n {
                0 => sub_word(w.rotate_right(8)) ^ RC[i / n] as u32,
                4 if n > 6 => sub_word(w),
                _ => w,
            };
            w ^= words[i - n];
            words.push(w);
        }

        let mut inv_round_keys: Vec<[u8; 16]> = words
            .chunks(4)
            .rev()
            .map(|w| {
                let a = w[0].to_le_bytes();
                let b = w[1].to_le_bytes();
                let c = w[2].to_le_bytes();
                let d = w[3].to_le_bytes();
                [
                    a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3], c[0], c[1], c[2], c[3], d[0],
                    d[1], d[2], d[3],
                ]
            })
            .collect();
        for round_key in &mut inv_round_keys[1..r - 1] {
            inv_mix_columns(round_key.into());
        }
        Ok(Self { inv_round_keys })
    }
}

impl BlockSizeUser for Aes32NDec {
    type BlockSize = U16;
}

impl BlockCipher for Aes32NDec {}

struct Aes32NDecBackend<'a>(&'a Aes32NDec);

impl<'a> BlockSizeUser for Aes32NDecBackend<'a> {
    type BlockSize = U16;
}

impl<'a> ParBlocksSizeUser for Aes32NDecBackend<'a> {
    type ParBlocksSize = U8;
}

impl<'a> BlockBackend for Aes32NDecBackend<'a> {
    fn proc_block(&mut self, mut inout: InOut<'_, '_, Block<Self>>) {
        let (first_inv_round_key, inv_round_keys) = self.0.inv_round_keys.split_first().unwrap();
        let (last_inv_round_key, inv_round_keys) = inv_round_keys.split_last().unwrap();

        let mut block = *inout.get_in();
        xor_block(&mut block, first_inv_round_key);
        for inv_round_key in inv_round_keys {
            equiv_inv_cipher_round(&mut block, inv_round_key.into());
        }
        // `aes::hazmat` doesn't have an AESDECLAST equivalent, but we can simulate it
        equiv_inv_cipher_round(&mut block, &[0u8; 16].into()); // so AddRoundKey is ignored
        mix_columns(&mut block); // so the last InvMixColumns is undone
        xor_block(&mut block, last_inv_round_key); // so AddRoundKey is reinstated
        *inout.get_out() = block;
    }

    // TODO: maybe we can also implement `proc_par_blocks` with a relative ease,
    // but this requires an annoying nested GenericArray conversion
}

impl BlockDecrypt for Aes32NDec {
    fn decrypt_with_backend(&self, f: impl BlockClosure<BlockSize = U16>) {
        f.call(&mut Aes32NDecBackend(self))
    }
}
